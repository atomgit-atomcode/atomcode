//! Deterministic review planning from the diff itself.
//!
//! The reviewer still reasons with the model, but the first exploration target should be
//! stable: changed symbols and high-risk lifecycle/API seams from the diff, not whatever
//! file the model happens to grep first.

use std::collections::BTreeSet;

/// Render a compact impact-oriented review plan for the provided unified diff.
pub fn render_review_impact_plan(diff: &str) -> String {
    let files = changed_files(diff);
    let targets = extract_targets(diff);
    let contract_checks = lifecycle_contract_checks(diff);

    let mut out = String::from(
        "## Review impact plan (deterministic, code-graph guided)\n\n\
         Use this plan to keep exploration bounded. Prefer code-intelligence tools over \
         broad grep/read loops. The DIFF remains authoritative.\n\n",
    );

    if files.is_empty() {
        out.push_str("- Changed files: none parsed from the diff.\n");
    } else {
        out.push_str("- Changed files:\n");
        for f in &files {
            out.push_str(&format!("  - {f}\n"));
        }
    }

    if targets.is_empty() {
        render_contract_checks(&mut out, &contract_checks);
        out.push_str("\nNo high-risk symbol targets were inferred. Review the changed hunks and only use deeper code-graph tools if a hunk changes runtime behavior, dependency resolution, or public contracts.\n");
        return out;
    }

    render_contract_checks(&mut out, &contract_checks);

    out.push_str(
        "\nHigh-risk review targets. For each target, first inspect the changed hunk, then use \
         `list_symbols`/`read_symbol` for local context and `find_references` + \
         `trace_callers`/`trace_callees` or `file_dependencies`/`blast_radius` for the \
         affected path. Do not conclude clean until these target contracts are checked:\n",
    );
    for t in targets {
        out.push_str(&format!(
            "\n- `{}` in `{}` [{}]\n  - why: {}\n  - code-graph recipe: `list_symbols`/`read_symbol` in this file; `find_references` for `{}`; `trace_callers`/`trace_callees` if the graph has this symbol; `file_dependencies` or `blast_radius` for the file when symbol graph is unavailable.\n",
            t.symbol, t.file, t.kind, t.reason, t.symbol
        ));
    }
    out
}

fn render_contract_checks(out: &mut String, checks: &[String]) {
    if checks.is_empty() {
        return;
    }

    out.push_str("\nLifecycle contract checks:\n");
    for check in checks {
        out.push_str(&format!("- {check}\n"));
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct Target {
    file: String,
    symbol: String,
    kind: &'static str,
    reason: &'static str,
}

fn changed_files(diff: &str) -> Vec<String> {
    crate::rules::changed_files_from_diff(diff)
}

fn extract_targets(diff: &str) -> Vec<Target> {
    let mut current_file = String::new();
    let mut set = BTreeSet::new();

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim().strip_prefix("b/").unwrap_or(rest.trim());
            current_file = if path == "/dev/null" {
                String::new()
            } else {
                path.to_string()
            };
            continue;
        }
        if current_file.is_empty() || crate::rules::is_low_signal_file(&current_file) {
            continue;
        }

        if line.starts_with("@@") {
            for sym in symbols_from_hunk_header(line) {
                set.insert(Target {
                    file: current_file.clone(),
                    symbol: sym,
                    kind: "changed symbol",
                    reason: "the hunk changes a named code region; check callers, callees, and contract consumers",
                });
            }
            continue;
        }

        if let Some(code) = hunk_code_line(line) {
            for sym in state_machine_tokens(code) {
                set.insert(Target {
                    file: current_file.clone(),
                    symbol: sym.clone(),
                    kind: "state/lifecycle",
                    reason: "event, phase, status, or turn lifecycle changes require checking producers and consumers in order",
                });
                for related in related_lifecycle_symbols(&sym) {
                    set.insert(Target {
                        file: current_file.clone(),
                        symbol: related.to_string(),
                        kind: "state/lifecycle",
                        reason: "paired lifecycle events and terminal notifications must be checked together for ordering regressions",
                    });
                }
            }
        }
        if let Some(code) = changed_code_line(line) {
            for sym in symbols_from_code(code) {
                let (kind, reason) = classify_symbol(&sym, code);
                set.insert(Target {
                    file: current_file.clone(),
                    symbol: sym,
                    kind,
                    reason,
                });
            }
        }
    }

    set.into_iter().take(24).collect()
}

fn lifecycle_contract_checks(diff: &str) -> Vec<String> {
    let mut checks = BTreeSet::new();
    let mut current_file = String::new();
    let mut hunk_lines: Vec<&str> = Vec::new();

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            flush_lifecycle_hunk(&current_file, &hunk_lines, &mut checks);
            let path = rest.trim().strip_prefix("b/").unwrap_or(rest.trim());
            current_file = if path == "/dev/null" {
                String::new()
            } else {
                path.to_string()
            };
            hunk_lines.clear();
            continue;
        }
        if line.starts_with("@@") {
            flush_lifecycle_hunk(&current_file, &hunk_lines, &mut checks);
            hunk_lines.clear();
        }
        if !current_file.is_empty() && !crate::rules::is_low_signal_file(&current_file) {
            hunk_lines.push(line);
        }
    }
    flush_lifecycle_hunk(&current_file, &hunk_lines, &mut checks);

    checks.into_iter().collect()
}

fn flush_lifecycle_hunk(file: &str, hunk_lines: &[&str], checks: &mut BTreeSet<String>) {
    if file.is_empty() || hunk_lines.is_empty() {
        return;
    }

    let added_terminal = hunk_lines
        .iter()
        .any(|line| changed_code_line(line).is_some_and(terminal_event_line));
    let removed_terminal = hunk_lines.iter().any(|line| {
        line.strip_prefix('-')
            .filter(|_| !line.starts_with("---"))
            .map(str::trim)
            .is_some_and(terminal_event_line)
    });
    let mentions_follow_up_update = hunk_lines.iter().any(|line| {
        hunk_code_line(line).is_some_and(|code| {
            let lower = code.to_ascii_lowercase();
            lower.contains("goal_update")
                || lower.contains("goalupdate")
                || lower.contains("phasechange")
                || lower.contains("tokenusage")
                || lower.contains("emit(")
        })
    });

    if added_terminal && removed_terminal && mentions_follow_up_update {
        checks.insert(format!(
            "`{file}` reorders a terminal event around a follow-up update. Verify consumers still receive and process the follow-up update after the terminal event, especially non-goal turns and UIs that stop reading on `TurnComplete`/idle phase."
        ));
    } else if added_terminal && mentions_follow_up_update {
        checks.insert(format!(
            "`{file}` emits a terminal event in the same hunk as follow-up update/event emission. Verify terminal handling does not close the turn before later updates are delivered, especially non-goal paths."
        ));
    }
}

fn terminal_event_line(code: &str) -> bool {
    let lower = code.to_ascii_lowercase();
    lower.contains("finish_turn")
        || lower.contains("turncomplete")
        || lower.contains("phasechange(agentphase::idle")
}

fn changed_code_line(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('+')?;
    if line.starts_with("+++") {
        return None;
    }
    Some(rest.trim())
}

fn hunk_code_line(line: &str) -> Option<&str> {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
        return None;
    }
    line.strip_prefix('+')
        .or_else(|| line.strip_prefix(' '))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn symbols_from_hunk_header(header: &str) -> Vec<String> {
    let Some(after) = header.rsplit("@@").next() else {
        return Vec::new();
    };
    symbol_words(after)
        .into_iter()
        .filter(|s| looks_like_symbol(s))
        .take(3)
        .collect()
}

fn symbols_from_code(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let patterns = [
        "fn ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "pub fn ",
        "pub struct ",
        "pub enum ",
    ];
    for pat in patterns {
        if let Some(idx) = code.find(pat) {
            let rest = &code[idx + pat.len()..];
            if let Some(sym) = symbol_words(rest)
                .into_iter()
                .find(|s| looks_like_symbol(s))
            {
                out.push(sym);
            }
        }
    }
    out
}

fn state_machine_tokens(code: &str) -> Vec<String> {
    symbol_words(code)
        .into_iter()
        .filter(|s| {
            let lower = s.to_ascii_lowercase();
            s.ends_with("Event")
                || s.ends_with("Update")
                || s.ends_with("Phase")
                || s.ends_with("State")
                || lower.contains("finish")
                || lower.contains("turncomplete")
                || lower.contains("cancel")
                || lower.contains("goal")
                || lower.contains("status")
        })
        .filter(|s| looks_like_symbol(s))
        .take(8)
        .collect()
}

fn related_lifecycle_symbols(symbol: &str) -> &'static [&'static str] {
    let lower = symbol.to_ascii_lowercase();
    if lower.contains("finish_turn") || lower.contains("finish") {
        &["TurnComplete", "GoalUpdate", "PhaseChange", "Idle"]
    } else if lower.contains("goalupdate") || symbol == "GoalUpdate" {
        &["TurnComplete", "finish_turn", "PhaseChange"]
    } else if lower.contains("turncomplete") || symbol == "TurnComplete" {
        &["GoalUpdate", "finish_turn", "PhaseChange"]
    } else {
        &[]
    }
}

fn classify_symbol(symbol: &str, code: &str) -> (&'static str, &'static str) {
    let lower = format!(
        "{} {}",
        symbol.to_ascii_lowercase(),
        code.to_ascii_lowercase()
    );
    if lower.contains("event")
        || lower.contains("phase")
        || lower.contains("state")
        || lower.contains("finish")
        || lower.contains("turn")
        || lower.contains("goal")
    {
        (
            "state/lifecycle",
            "state-machine or event lifecycle changes can break producer/consumer ordering and cancellation paths",
        )
    } else {
        (
            "changed symbol",
            "the hunk changes a named code region; check callers, callees, and contract consumers",
        )
    }
}

fn symbol_words(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_matches('_').to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

fn looks_like_symbol(s: &str) -> bool {
    if s.len() < 3 {
        return false;
    }
    if matches!(
        s,
        "pub"
            | "fn"
            | "let"
            | "mut"
            | "self"
            | "Self"
            | "Some"
            | "None"
            | "true"
            | "false"
            | "String"
            | "Option"
            | "Result"
            | "Vec"
            | "use"
            | "mod"
            | "impl"
    ) {
        return false;
    }
    s.chars().any(|c| c.is_ascii_alphabetic())
        && (s.contains('_') || s.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::render_review_impact_plan;

    #[test]
    fn goal_lifecycle_diff_gets_state_machine_and_code_graph_plan() {
        let diff = r#"diff --git a/crates/atomcode-core/src/agent/mod.rs b/crates/atomcode-core/src/agent/mod.rs
--- a/crates/atomcode-core/src/agent/mod.rs
+++ b/crates/atomcode-core/src/agent/mod.rs
@@ -413,6 +413,7 @@ pub enum AgentEvent {
     GoalUpdate {
         active: bool,
         round: u32,
+        tokens_used: u64,
         condition: String,
     }
diff --git a/crates/atomcode-bridge/src/runtime.rs b/crates/atomcode-bridge/src/runtime.rs
--- a/crates/atomcode-bridge/src/runtime.rs
+++ b/crates/atomcode-bridge/src/runtime.rs
@@ -444,9 +444,9 @@ impl Bridge {
                 if let Some(g) = self.goal.as_mut() {
                     g.active = false;
                 }
+                self.finish_turn(reason, messages);
                 if let Some(g) = self.goal.take() {
                     self.emit(goal_update_ev(&g));
                 }
-                self.finish_turn(reason, messages);
             }
diff --git a/crates/atomcode-tuix/src/event_loop/mod.rs b/crates/atomcode-tuix/src/event_loop/mod.rs
--- a/crates/atomcode-tuix/src/event_loop/mod.rs
+++ b/crates/atomcode-tuix/src/event_loop/mod.rs
@@ -9001,6 +9001,7 @@ fn handle_agent_event(...) {
         AgentEvent::GoalUpdate { active, round, elapsed_secs, tokens_used, condition, last_reason } => {
             if active {
                 state.goal_condition = Some(condition);
             }
         }
"#;

        let plan = render_review_impact_plan(diff);

        assert!(plan.contains("Review impact plan"), "{plan}");
        assert!(plan.contains("state/lifecycle"), "{plan}");
        assert!(plan.contains("find_references"), "{plan}");
        assert!(plan.contains("trace_callers"), "{plan}");
        assert!(plan.contains("GoalUpdate"), "{plan}");
        assert!(plan.contains("finish_turn"), "{plan}");
        assert!(plan.contains("TurnComplete"), "{plan}");
        assert!(
            plan.contains("crates/atomcode-bridge/src/runtime.rs"),
            "{plan}"
        );
    }

    #[test]
    fn terminal_event_reordering_gets_contract_questions() {
        let diff = r#"diff --git a/crates/atomcode-bridge/src/runtime.rs b/crates/atomcode-bridge/src/runtime.rs
--- a/crates/atomcode-bridge/src/runtime.rs
+++ b/crates/atomcode-bridge/src/runtime.rs
@@ -445,11 +445,11 @@ impl Bridge {
                     g.active = false;
                     g.last_eval_reason = Some(verdict);
                 }
+                self.finish_turn(reason, messages);
                 if let Some(g) = self.goal.take() {
                     let ev = goal_update_ev(&g);
                     self.emit(ev);
                 }
-                self.finish_turn(reason, messages);
             }
"#;

        let plan = render_review_impact_plan(diff);

        assert!(plan.contains("Lifecycle contract checks"), "{plan}");
        assert!(
            plan.contains("terminal event") && plan.contains("follow-up update"),
            "{plan}"
        );
        assert!(plan.contains("non-goal"), "{plan}");
    }

    #[test]
    fn manifest_only_diff_does_not_create_high_risk_symbol_targets() {
        let diff = r#"diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,5 +1,5 @@
 version = 3
-name = "a"
+name = "b"
"#;

        let plan = render_review_impact_plan(diff);

        assert!(plan.contains("Review impact plan"), "{plan}");
        assert!(plan.contains("No high-risk symbol targets"), "{plan}");
        assert!(!plan.contains("trace_callers"), "{plan}");
    }
}
