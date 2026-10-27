//! An advisory about a few common status-losing wrappers, never an execution policy.
//! Unknown/dynamic syntax is left alone. This is not proof of a command's success.

use atomcode_kernel::tool::ToolResult;
use tree_sitter::Node;

pub(super) const MARKER: &str = "[execution note: wrapped command status]";

pub(super) fn append_note(result: &mut ToolResult, preview_available: bool) {
    result.content.push_str("\n\n");
    result.content.push_str(MARKER);
    result.content.push_str(
        "\nThe reported status belongs to the entire pipeline/wrapper. Unless the original \
         build/test/install status was explicitly preserved, zero does not establish its success. \
         Check existing logs or status evidence before claiming success. Do not rerun completed \
         work solely to recover its exit status.",
    );
    if preview_available {
        result.content.push_str(
            " For future runs, execute the original command directly; use output_tail_lines on \
             bash or bash_poll instead of shell output filters, and fetch_output for saved output.",
        );
    }
}

pub(super) fn may_mask_status(source: &str) -> bool {
    // This optional diagnostic must not do unbounded work on generated scripts.
    if source.len() > 64 * 1024 {
        return false;
    }
    let Some(tree) = super::parse_bash(source) else {
        return false;
    };
    let root = tree.root_node();
    !root.has_error() && scan(root, source.as_bytes(), false, 0)
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

fn invocation<'a>(mut node: Node<'_>, source: &'a [u8]) -> Option<(&'a str, Vec<&'a str>)> {
    if node.kind() == "redirected_statement" {
        node = node.child_by_field_name("body")?;
    }
    if node.kind() != "command" {
        return None;
    }
    let name = literal(node.child_by_field_name("name")?.utf8_text(source).ok()?)?;
    let mut cursor = node.walk();
    let args = node
        .children_by_field_name("argument", &mut cursor)
        .map(|n| n.utf8_text(source).unwrap_or_default())
        .collect();
    Some((basename(name), args))
}

fn literal(text: &str) -> Option<&str> {
    let text = if (text.starts_with('\'') && text.ends_with('\''))
        || (text.starts_with('"') && text.ends_with('"'))
    {
        text.get(1..text.len().checked_sub(1)?)?
    } else {
        text
    };
    (!text.contains(['$', '`', '\\'])).then_some(text)
}

fn basename(name: &str) -> &str {
    let name = name.rsplit('/').next().unwrap_or(name);
    name.strip_suffix(".exe").unwrap_or(name)
}

fn is_work(node: Node<'_>, source: &[u8]) -> bool {
    let Some((name, args)) = invocation(node, source) else {
        return false;
    };
    let arg = |i: usize| args.get(i).and_then(|s| literal(s)).unwrap_or("");
    match name {
        "cargo" => matches!(
            arg(0),
            "build" | "test" | "check" | "clippy" | "bench" | "install"
        ),
        "npm" | "pnpm" | "yarn" | "bun" => matches!(
            arg(0),
            "install" | "i" | "ci" | "add" | "test" | "build" | "run"
        ),
        "pip" | "pip3" => matches!(arg(0), "install" | "wheel"),
        "go" => matches!(arg(0), "build" | "test" | "install"),
        "dotnet" => matches!(arg(0), "build" | "test" | "restore" | "publish"),
        "cmake" => arg(0) == "--build",
        "make" | "gmake" | "ninja" | "mvn" | "mvnw" | "gradle" | "gradlew" | "pytest" | "tox"
        | "nox" | "rustc" | "tsc" => !matches!(arg(0), "--help" | "--version" | "-h"),
        // Only standard Python build/test/install entry points, not arbitrary scripts
        // or `python -c` data-processing commands.
        name if name == "python"
            || name.strip_prefix("python").is_some_and(|s| {
                !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
            }) =>
        {
            let i = args
                .iter()
                .take_while(|s| matches!(literal(s), Some("-u" | "-B" | "-I" | "-O")))
                .count();
            if arg(i) == "-m" {
                matches!(arg(i + 1), "pytest" | "build" | "unittest")
                    || (arg(i + 1) == "pip" && matches!(arg(i + 2), "install" | "wheel"))
            } else {
                basename(arg(i)) == "setup.py"
                    && args.iter().skip(i + 1).any(|s| {
                        matches!(
                            literal(s),
                            Some(
                                "build"
                                    | "build_ext"
                                    | "install"
                                    | "develop"
                                    | "bdist_wheel"
                                    | "sdist"
                            )
                        )
                    })
            }
        }
        _ => false,
    }
}

fn contains_work(node: Node<'_>, source: &[u8]) -> bool {
    let mut stack = vec![node];
    for _ in 0..4096 {
        let Some(n) = stack.pop() else {
            return false;
        };
        if n.kind() == "function_definition" {
            continue;
        }
        if is_work(n, source) {
            return true;
        }
        stack.extend(children(n));
    }
    false
}

fn scan(node: Node<'_>, source: &[u8], mut pipefail: bool, depth: usize) -> bool {
    if depth >= 64 || node.kind() == "function_definition" {
        return false;
    }
    let nodes = children(node);
    if node.kind() == "pipeline" && !pipefail {
        let mut work = false;
        for n in &nodes {
            if work
                && invocation(*n, source)
                    .is_some_and(|(name, _)| matches!(name, "head" | "tail" | "tee"))
            {
                return true;
            }
            work |= contains_work(*n, source);
        }
    }
    if matches!(
        node.kind(),
        "program" | "list" | "subshell" | "compound_statement"
    ) {
        if let Some(last) = nodes.last() {
            if invocation(*last, source).is_some_and(|(name, _)| matches!(name, "echo" | "printf"))
            {
                if let Some(prior) = nodes
                    .get(nodes.len().saturating_sub(2))
                    .filter(|_| nodes.len() > 1)
                {
                    // && preserves failure by skipping the final printer. A semicolon,
                    // newline or || can run it after the work failed.
                    let mut cursor = node.walk();
                    let guarded = node.children(&mut cursor).any(|n| {
                        n.start_byte() >= prior.end_byte()
                            && n.end_byte() <= last.start_byte()
                            && n.kind() == "&&"
                    });
                    if !guarded
                        && nodes[..nodes.len() - 1]
                            .iter()
                            .any(|n| contains_work(*n, source))
                    {
                        return true;
                    }
                }
            }
        }
        for n in nodes {
            // A straight-line explicit pipefail setting in this scope is evidence
            // against the pipeline warning. Conditional/nested changes are not assumed.
            if let Some(("set", args)) = invocation(n, source) {
                if args.len() == 2 && literal(args[1]) == Some("pipefail") {
                    match literal(args[0]) {
                        Some("-o") => pipefail = true,
                        Some("+o") => pipefail = false,
                        _ => {}
                    }
                }
            }
            if scan(n, source, pipefail, depth + 1) {
                return true;
            }
        }
        false
    } else {
        nodes
            .into_iter()
            .any(|n| scan(n, source, pipefail, depth + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_and_deep_scripts_do_not_expand_diagnostic_work() {
        assert!(!may_mask_status(&format!(
            "{}\ncargo test | tail",
            "#".repeat(64 * 1024)
        )));
        let nested = format!("{}cargo test | tail{}", "(".repeat(256), ")".repeat(256));
        assert!(!may_mask_status(&nested));
    }

    #[test]
    fn diagnoses_only_supported_execution_structure() {
        for command in [
            "cargo test | tail -20",
            "cargo build 2>&1 | tee build.log",
            "CFLAGS=-O2 .venv/bin/python setup.py build_ext --inplace | head -30",
            ".venv/bin/python3.9 -m pytest -q | tail",
            "python -m pip install dep | tail",
            "npm test | tail",
            "go test ./... | tail",
            "cmake --build build | tail",
            "(make test) | tail",
            "cargo test > build.log 2>&1; echo \"EXIT:$?\"",
            "cargo test; rc=$?; printf '%s' \"$rc\"",
            "cargo test || echo failed",
            "cargo test && echo done; echo finished",
            "cargo test; echo '&& is only data'",
            "cargo test; # && is only a comment\necho done",
            "echo 'set -o pipefail'; cargo test | tail",
            "if false; then set -o pipefail; fi; make test | tail",
            "set -o pipefail; cargo test | tail; echo done",
            "set -o pipefail; set +o pipefail; cargo test | tail",
        ] {
            assert!(may_mask_status(command), "missed: {command}");
        }
        for command in [
            "cargo test",
            "grep ERROR build.log | tail -20",
            "cat data | head",
            "printf 'cargo test | tail'",
            "echo \"cargo test | tail\"",
            "# cargo test | tail\necho done",
            "cat <<'EOF'\ncargo test | tail\nEOF",
            "cargo test && echo done",
            "cargo test; rc=$?; tail build.log; exit \"$rc\"",
            "cargo test |",
            "python -c 'print(1)' | head",
            "pip list | tail",
            "set -o pipefail; cargo test | tail",
            "set -o pipefail && cargo test | tail",
            "cat build.log; echo $?",
            "npm --version | head",
            "check() { cargo test | tail; }; echo defined",
        ] {
            assert!(!may_mask_status(command), "unexpected note: {command}");
        }
    }
}
