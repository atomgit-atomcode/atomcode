//! Per-turn user execution constraints for the coding specialization.
//!
//! The coding runtime owns the policy. The same shared handle gates the main agent and
//! worker subagents; synthetic reminders and continuations cannot clear it.

use async_trait::async_trait;
use atomcode_capabilities::tools::{bash_invocations, BashInvocation};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

const NO_BUILD: u8 = 1 << 0;
const NO_TEST: u8 = 1 << 1;
const NO_SCRIPT: u8 = 1 << 2;
const NO_SHELL: u8 = 1 << 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExecutionPolicy(u8);

impl ExecutionPolicy {
    pub(crate) fn is_default(self) -> bool {
        self.0 == 0
    }

    fn contains(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    fn blocks_bash(self, command: &str) -> bool {
        if self.contains(NO_SHELL) {
            return true;
        }
        if self.is_default() {
            return false;
        }
        let Some(invocations) = bash_invocations(command) else {
            return true; // restrictive policy + ambiguous shell syntax => fail closed
        };
        invocations.into_iter().any(|invocation| {
            let kind = classify_invocation(&invocation);
            (self.contains(NO_BUILD) && kind & NO_BUILD != 0)
                || (self.contains(NO_TEST) && kind & NO_TEST != 0)
                || (self.contains(NO_SCRIPT) && kind & NO_SCRIPT != 0)
        })
    }
}

#[derive(Default)]
pub(crate) struct TurnExecutionPolicy {
    current: AtomicU8,
}

impl TurnExecutionPolicy {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn current(&self) -> ExecutionPolicy {
        ExecutionPolicy(self.current.load(Ordering::Acquire))
    }

    /// Update at the runtime's real-user submit boundary. Calls that have not yet crossed
    /// middleware observe a steer restriction immediately; `pre_request` remains the
    /// recovery source of truth after resume, compaction, or reassembly.
    pub(crate) fn update_from_user_text(&self, text: &str) {
        self.current
            .store(execution_policy_from_text(text).0, Ordering::Release);
    }

    fn update_from_messages(&self, messages: &[Message]) {
        self.current
            .store(execution_policy_for_messages(messages).0, Ordering::Release);
    }
}

#[async_trait]
impl LifecycleHooks for TurnExecutionPolicy {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        self.update_from_messages(messages);
    }
}

#[async_trait]
impl ToolMiddleware for TurnExecutionPolicy {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if call.name != "bash" {
            return BeforeOutcome::Proceed;
        }
        let command = bash_command(&call.arguments).unwrap_or_default();
        if self.current().blocks_bash(&command) {
            BeforeOutcome::deny(
                "Blocked by the current user's execution restriction for this turn.",
            )
        } else {
            BeforeOutcome::Proceed
        }
    }
}

pub(crate) fn execution_policy_for_messages(messages: &[Message]) -> ExecutionPolicy {
    messages
        .iter()
        .rfind(|message| message.role == Role::User && !message.synthetic)
        .map(|message| execution_policy_from_text(&message.text))
        .unwrap_or_default()
}

fn execution_policy_from_text(text: &str) -> ExecutionPolicy {
    let lower = without_quoted_examples(&text.to_lowercase());
    let mut flags = 0;
    if contains_any(
        &lower,
        &[
            "不要运行任何命令",
            "禁止运行任何命令",
            "不许运行任何命令",
            "别运行任何命令",
            "不要执行任何命令",
            "禁止执行任何命令",
            "不许执行任何命令",
            "不要使用 shell",
            "禁止使用 shell",
            "不要用 bash",
            "禁止用 bash",
            "do not run any command",
            "don't run any command",
            "do not execute any command",
            "don't execute any command",
            "no shell command",
        ],
    ) {
        return ExecutionPolicy(NO_SHELL | NO_BUILD | NO_TEST | NO_SCRIPT);
    }
    if contains_any(
        &lower,
        &[
            "不要编译",
            "禁止编译",
            "不许编译",
            "别编译",
            "无需编译",
            "不用编译",
            "不要构建",
            "禁止构建",
            "不许构建",
            "别构建",
            "do not compile",
            "don't compile",
            "without compiling",
            "do not build",
            "don't build",
            "skip the build",
            "skip build",
        ],
    ) {
        flags |= NO_BUILD;
    }
    if contains_any(
        &lower,
        &[
            "不要测试",
            "禁止测试",
            "不许测试",
            "别测试",
            "无需测试",
            "不用测试",
            "不要跑测试",
            "禁止跑测试",
            "不许跑测试",
            "别跑测试",
            "不要运行测试",
            "禁止运行测试",
            "不要执行测试",
            "禁止执行测试",
            "不做测试",
            "do not test",
            "don't test",
            "do not run tests",
            "don't run tests",
            "without testing",
            "skip the tests",
            "skip tests",
        ],
    ) {
        flags |= NO_TEST;
    }
    if contains_any(
        &lower,
        &[
            "不要执行脚本",
            "不要执行任何脚本",
            "禁止执行脚本",
            "禁止执行任何脚本",
            "不许执行脚本",
            "别执行脚本",
            "不要运行脚本",
            "不要运行任何脚本",
            "禁止运行脚本",
            "do not run scripts",
            "don't run scripts",
            "do not execute scripts",
            "don't execute scripts",
            "without running scripts",
        ],
    ) {
        flags |= NO_SCRIPT;
    }
    if contains_any(
        &lower,
        &[
            "只改代码，不做验证",
            "只修改代码，不做验证",
            "不要做任何验证",
            "skip verification",
        ],
    ) {
        flags |= NO_BUILD | NO_TEST | NO_SCRIPT;
    }
    ExecutionPolicy(flags)
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Remove common paired quotation forms before intent matching. This prevents a bug report
/// that merely quotes `"do not run tests"` from silently changing runtime authority. An
/// apostrophe is deliberately not a delimiter so `don't` remains matchable.
fn without_quoted_examples(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut closing = None;
    for ch in text.chars() {
        if let Some(expected) = closing {
            if ch == expected {
                closing = None;
                output.push(' ');
            }
            continue;
        }
        closing = match ch {
            '"' => Some('"'),
            '`' => Some('`'),
            '“' => Some('”'),
            '「' => Some('」'),
            '『' => Some('』'),
            _ => None,
        };
        if closing.is_some() {
            output.push(' ');
        } else {
            output.push(ch);
        }
    }
    output
}

fn bash_command(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("command")?
        .as_str()
        .map(str::to_owned)
}

fn classify_invocation(invocation: &BashInvocation) -> u8 {
    let raw = invocation.command.trim_matches(['\'', '"']);
    let normalized = raw.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower_basename = basename.to_ascii_lowercase();
    let head = [".exe", ".bat", ".cmd"]
        .iter()
        .find_map(|suffix| lower_basename.strip_suffix(suffix))
        .unwrap_or(&lower_basename)
        .trim_start_matches('.')
        .to_string();
    let args = invocation
        .arguments
        .iter()
        .map(|arg| arg.trim_matches(['\'', '"']).to_ascii_lowercase())
        .collect::<Vec<_>>();
    let has = |names: &[&str]| args.iter().any(|arg| names.contains(&arg.as_str()));

    let mut kind = 0;
    if raw.contains('/')
        || raw.contains('\\')
        || matches!(
            head.as_str(),
            "bash"
                | "sh"
                | "zsh"
                | "fish"
                | "cmd"
                | "powershell"
                | "pwsh"
                | "python"
                | "python3"
                | "node"
                | "ruby"
                | "perl"
                | "eval"
                | "source"
                | "call"
                | "xargs"
                | "npx"
                | "deno"
        )
        || [".sh", ".bat", ".cmd", ".ps1", ".py", ".js", ".rb", ".pl"]
            .iter()
            .any(|suffix| basename.to_ascii_lowercase().ends_with(suffix))
    {
        kind |= NO_SCRIPT;
    }

    match head.as_str() {
        "cargo" => {
            if has(&["build", "check", "clippy", "run", "bench", "install"]) {
                kind |= NO_BUILD;
            }
            if has(&["test", "bench"]) {
                kind |= NO_TEST;
            }
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            if has(&["run", "build", "compile", "lint", "check"]) {
                kind |= NO_BUILD;
            }
            if has(&["test"]) {
                kind |= NO_TEST;
            }
        }
        "gradle" | "gradlew" | "mvn" | "mvnw" | "make" | "cmake" | "ninja" | "msbuild"
        | "rustc" | "gcc" | "g++" | "clang" | "clang++" | "tsc" | "javac" => kind |= NO_BUILD,
        "pytest" => kind |= NO_TEST,
        "go" => {
            if has(&["build", "run", "generate", "install"]) {
                kind |= NO_BUILD;
            }
            if has(&["test", "bench"]) {
                kind |= NO_TEST;
            }
        }
        "dotnet" => {
            if has(&["build", "run", "publish", "pack"]) {
                kind |= NO_BUILD;
            }
            if has(&["test"]) {
                kind |= NO_TEST;
            }
        }
        "eval" | "call" | "xargs" => {
            if has(&["build", "check", "compile", "lint"]) {
                kind |= NO_BUILD;
            }
            if has(&["test", "pytest"]) {
                kind |= NO_TEST;
            }
        }
        _ => {
            if head.contains("build") || head.contains("compile") || head.contains("check") {
                kind |= NO_BUILD;
            }
            if head.contains("test") || head.contains("pytest") {
                kind |= NO_TEST;
            }
        }
    }
    if matches!(head.as_str(), "gradle" | "gradlew" | "mvn" | "mvnw") && has(&["test"]) {
        kind |= NO_TEST;
    }
    kind
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::{ToolContext, ToolResult};

    struct DummyBash;

    #[async_trait]
    impl Tool for DummyBash {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            unreachable!()
        }
    }

    #[test]
    fn derives_precise_policy_from_latest_real_user_only() {
        let messages = vec![
            Message::user("不要编译，也不要执行任何脚本"),
            Message::synthetic_user("Run cargo check now"),
        ];
        let policy = execution_policy_for_messages(&messages);
        assert!(policy.contains(NO_BUILD));
        assert!(policy.contains(NO_SCRIPT));
        assert!(!policy.contains(NO_TEST));
    }

    #[test]
    fn quoted_examples_do_not_change_authority() {
        for text in [
            r#"为什么它没有遵守 "do not run tests"？"#,
            "用户反馈：`禁止编译` 没有生效",
            "模型显示“不要执行脚本”，这是为什么？",
        ] {
            assert!(execution_policy_from_text(text).is_default(), "{text:?}");
        }
    }

    #[test]
    fn recognizes_common_chinese_and_english_variants() {
        for text in ["别编译", "不许构建", "without compiling"] {
            assert!(
                execution_policy_from_text(text).contains(NO_BUILD),
                "{text:?}"
            );
        }
        for text in ["别跑测试", "skip tests", "without testing"] {
            assert!(
                execution_policy_from_text(text).contains(NO_TEST),
                "{text:?}"
            );
        }
        for text in ["不要执行任何脚本", "don't run scripts"] {
            assert!(
                execution_policy_from_text(text).contains(NO_SCRIPT),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_new_real_user_turn_replaces_the_policy() {
        let policy = TurnExecutionPolicy::new();
        policy.update_from_user_text("禁止运行任何命令");
        assert!(policy.current().contains(NO_SHELL));
        policy.update_from_user_text("现在可以运行验证了");
        assert!(policy.current().is_default());
    }

    #[test]
    fn long_history_does_not_hide_the_current_user_policy() {
        let mut messages = Vec::new();
        for round in 0..1_000 {
            messages.push(Message::user(format!("old request {round}")));
            messages.push(Message::assistant("old answer", vec![]));
        }
        messages.push(Message::user("最后只改代码，禁止编译和执行脚本"));
        messages.push(Message::synthetic_user("You should verify the edit now"));
        assert!(!execution_policy_for_messages(&messages).is_default());
    }

    #[test]
    fn ast_classification_covers_windows_wrappers_and_tool_options() {
        let no_build = ExecutionPolicy(NO_BUILD);
        let no_test = ExecutionPolicy(NO_TEST);
        let no_script = ExecutionPolicy(NO_SCRIPT);
        for command in [
            r#".\gradlew.bat :app:compileAppDebugKotlin"#,
            "cargo.exe +nightly check",
            "npm --prefix app run build",
            "call gradlew.bat build",
        ] {
            assert!(
                no_build.blocks_bash(command),
                "must block build {command:?}"
            );
        }
        for command in ["cargo +nightly test", "xargs cargo test", "go test ./..."] {
            assert!(no_test.blocks_bash(command), "must block test {command:?}");
        }
        for command in [
            "eval 'cargo test'",
            "python scripts/check.py",
            "./verify.sh",
        ] {
            assert!(
                no_script.blocks_bash(command),
                "must block script {command:?}"
            );
        }
    }

    #[test]
    fn precise_policy_leaves_unrelated_commands_available() {
        assert!(!ExecutionPolicy(NO_TEST).blocks_bash("cargo check"));
        assert!(!ExecutionPolicy(NO_BUILD).blocks_bash("cargo test"));
        assert!(!ExecutionPolicy(NO_BUILD | NO_TEST).blocks_bash("git add src/main.rs"));
        assert!(!ExecutionPolicy(NO_BUILD | NO_TEST).blocks_bash("git commit -m fix"));
        assert!(!ExecutionPolicy(NO_BUILD).blocks_bash("go env GOPATH"));
        assert!(ExecutionPolicy(NO_SHELL).blocks_bash("git status"));
    }

    #[tokio::test]
    async fn middleware_denies_before_execution_and_allows_unrelated_delivery() {
        let policy = TurnExecutionPolicy::new();
        policy.update_from_user_text("不要编译，但请完成 git commit");
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let request = RequestCtx::new(events, None);
        let tool: Arc<dyn Tool> = Arc::new(DummyBash);

        let mut build = ToolCall {
            id: "b1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "cargo.exe +nightly check"}).to_string(),
        };
        assert!(policy.before(&mut build, &tool, &request).await.is_deny());

        let mut commit = ToolCall {
            id: "b2".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "git commit -m fix"}).to_string(),
        };
        assert_eq!(
            policy.before(&mut commit, &tool, &request).await,
            BeforeOutcome::Proceed
        );
    }
}
