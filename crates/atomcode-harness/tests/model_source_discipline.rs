//! 一条纪律:模型/环境的解析只住在 `model_source`。
//!
//! 扫的是**这个 crate 自己的 `src/`**(`CARGO_MANIFEST_DIR`),所以它必须留在
//! harness —— 它原来和几条建树的判据同住一个文件,那几条跟着 `bundle::base()`
//! 搬到了 coding,这两条不能跟着走:搬过去就变成"扫 coding 的源码",而 coding
//! 加载配置是它的本分,断言立刻变成假的。

/// Every offender under `src/`, with `model_source.rs` as the one allowed file.
///
/// Narrow on purpose, in one direction: `current_dir()`, `temp_dir()` and
/// `args()` are facts about the process, not places configuration comes from.
fn offenders_in_src(flag: impl Fn(&str) -> bool) -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // The one module allowed to know where configuration lives.
            if path.file_name().and_then(|n| n.to_str()) == Some("model_source.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for (n, line) in text.lines().enumerate() {
                // Comments and doc prose explain the rule; they do not break it.
                let code = line.split("//").next().unwrap_or("");
                if flag(code) {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
    }
    offenders
}

/// **Where configuration lives is decided in one place, and it is `model_source`.**
///
/// Before it existed, four rows each read the environment and the config file
/// with their own fallbacks and their own error text — the same gateway resolved
/// two ways in one process.
///
/// The first version of this check looked for `std::env::var` and nothing else,
/// and a public `env_var` helper walked straight through it: the *spelling* had
/// moved into one module while the *decision* stayed at every call site. The
/// second version flagged the variable *names*, which is a false positive — a
/// help line telling someone to `export ATOMCODE_API_KEY` is documentation.
///
/// So it flags the two things that actually are resolution:
///
/// 1. any way of reaching the environment, whatever helper is used;
/// 2. a default variable name chosen at a call site
///    (`…unwrap_or("ATOMCODE_API_KEY")`), which is a policy decision made far
///    from the policy.
///
/// Not covered, deliberately: a bare name inside prose, and a `const` declared
/// elsewhere. The first is documentation; the second would need a name match
/// that cannot tell prose from code.
#[test]
fn nothing_outside_the_source_module_reaches_the_environment() {
    let offenders = offenders_in_src(|code| {
        let reaches_env = ["env::var", "env::vars", "var_os(", "env_var("]
            .iter()
            .any(|needle| code.contains(needle));
        // A default decided here rather than in the module that owns it.
        let decides_default = code.contains(r#"unwrap_or("ATOMCODE"#)
            || code.contains(r#"unwrap_or_else(|| "ATOMCODE"#);
        reaches_env || decides_default
    });
    assert!(
        offenders.is_empty(),
        "resolution lives in `model_source`; these do it from somewhere else:\n{}",
        offenders.join("\n")
    );
}

/// And the same for the config file: one loader, so no row can resolve a
/// `[models.*]` selection by a rule of its own.
#[test]
fn only_one_module_loads_the_model_config() {
    let offenders =
        offenders_in_src(|code| code.contains("Config::load") || code.contains("resolve_model("));
    assert!(
        offenders.is_empty(),
        "loading the model config belongs in `model_source`, not here:\n{}",
        offenders.join("\n")
    );
}
