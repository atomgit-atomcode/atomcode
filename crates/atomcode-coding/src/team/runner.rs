use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_capabilities::team::{
    role_by_id, TeamDifficulty, TeamPermission, TeamRoleProfile, TeamTaskSpec,
};
use atomcode_capabilities::tools::team_child_middlewares_for_policy;
#[cfg(test)]
use atomcode_capabilities::tools::team_child_middlewares;
use atomcode_kernel::agent::{Agent, AutoRespond, ToolLoopPolicy};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{MountedTools, Tool, ToolCall};
use tokio_util::sync::CancellationToken;

use super::{TeamActivitySink, TeamJobFactory, TeamMemberOutcome, TeamModelFactory};

pub type TeamProviderFactory = Arc<dyn Fn(TeamDifficulty) -> Arc<dyn LlmProvider> + Send + Sync>;
pub type TeamToolsFactory = Arc<dyn Fn(TeamPermission) -> MountedTools + Send + Sync>;

#[derive(Clone)]
pub struct TeamRunnerFactory {
    providers: TeamProviderFactory,
    tools: TeamToolsFactory,
    working_dir: std::path::PathBuf,
    max_rounds: Option<u32>,
    tool_loop_policy: Option<ToolLoopPolicy>,
    stream_timeout: Option<Duration>,
    request_timeout: Option<Duration>,
    inherited_worker_middlewares: Vec<Arc<dyn ToolMiddleware>>,
    credential_shell_policy: atomcode_capabilities::tools::CredentialShellPolicy,
}

impl TeamRunnerFactory {
    pub fn new(
        providers: TeamProviderFactory,
        tools: TeamToolsFactory,
        working_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            providers,
            tools,
            working_dir,
            max_rounds: None,
            tool_loop_policy: None,
            stream_timeout: None,
            request_timeout: None,
            inherited_worker_middlewares: Vec::new(),
            credential_shell_policy: Default::default(),
        }
    }

    pub fn with_runtime_policy(
        mut self,
        max_rounds: Option<u32>,
        tool_loop_policy: Option<ToolLoopPolicy>,
        stream_timeout: Option<Duration>,
        request_timeout: Option<Duration>,
    ) -> Self {
        self.max_rounds = max_rounds.filter(|rounds| *rounds > 0);
        self.tool_loop_policy = tool_loop_policy;
        self.stream_timeout = stream_timeout;
        self.request_timeout = request_timeout;
        self
    }

    pub fn with_worker_middleware(mut self, middleware: Arc<dyn ToolMiddleware>) -> Self {
        self.inherited_worker_middlewares.push(middleware);
        self
    }

    pub fn with_credential_shell_policy(
        mut self,
        policy: atomcode_capabilities::tools::CredentialShellPolicy,
    ) -> Self {
        self.credential_shell_policy = policy;
        self
    }

    pub fn job_factory(&self) -> TeamJobFactory {
        let runner = self.clone();
        Arc::new(move |task, cancel, activity| {
            let runner = runner.clone();
            Box::pin(async move { runner.run(task, cancel, activity).await })
        })
    }

    pub fn model_factory(&self) -> TeamModelFactory {
        let providers = Arc::clone(&self.providers);
        Arc::new(move |task| (providers)(task.difficulty).model_name().to_string())
    }

    async fn run(
        &self,
        task: TeamTaskSpec,
        cancel: CancellationToken,
        activity: TeamActivitySink,
    ) -> TeamMemberOutcome {
        let Some(profile) = role_by_id(task.role.as_str()) else {
            return TeamMemberOutcome::failed(format!("unknown team role: {}", task.role));
        };
        let provider = (self.providers)(task.difficulty);
        let tools = (self.tools)(task.permission);
        let progress = Arc::new(TeamProgressHook::new(activity));
        let mut builder = Agent::builder()
            .provider(provider)
            .tools(tools)
            .persona(team_member_persona(profile, &task.scope))
            .working_dir(self.working_dir.clone())
            .cancel_token(cancel)
            .hook(progress.clone())
            .middleware(Arc::new(DenyTeamBash));
        for middleware in team_child_middlewares_for_policy(
            task.permission == TeamPermission::Worker,
            &task.scope,
            &self.working_dir,
            &self.inherited_worker_middlewares,
            self.credential_shell_policy,
        ) {
            builder = builder.middleware(middleware);
        }
        if let Some(rounds) = self.max_rounds {
            builder = builder.max_rounds(rounds);
        }
        if let Some(policy) = self.tool_loop_policy {
            builder = builder.tool_loop_policy(policy);
        }
        if let Some(timeout) = self.stream_timeout {
            builder = builder.stream_timeout(timeout);
        }
        if let Some(timeout) = self.request_timeout {
            builder = builder.request_timeout(timeout);
        }
        let outcome = builder
            .build()
            .run_to_completion(task.prompt, AutoRespond::AllowAll)
            .await;
        let output = if !outcome.text.is_empty() {
            outcome.text
        } else if let Some(error) = outcome.error {
            error
        } else {
            outcome
                .tool_results
                .into_iter()
                .map(|result| result.content)
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Carry the final accumulated token total out via the outcome: the closing
        // round has no tool call, so on_model_response surfaced no activity for it.
        let output_tokens = progress.total_tokens();
        let member = match outcome.stop {
            StopReason::Stopped => TeamMemberOutcome::completed(output),
            StopReason::Cancelled => TeamMemberOutcome {
                success: false,
                stop: "stopped".into(),
                output,
                output_tokens: 0,
            },
            stop => TeamMemberOutcome {
                success: false,
                stop: format!("{stop:?}"),
                output,
                output_tokens: 0,
            },
        };
        TeamMemberOutcome {
            output_tokens,
            ..member
        }
    }
}

struct TeamProgressHook {
    activity: TeamActivitySink,
    /// Output+reasoning tokens finalized from completed rounds — the provider's
    /// reported `completion` count when available, else a chars/4 estimate. Mirrors
    /// the legacy `task` subagent panel so both show a real live token count.
    total_tokens: std::sync::atomic::AtomicU64,
    /// Chars streamed in the CURRENT (unfinished) round, reset at each round end.
    round_chars: std::sync::atomic::AtomicU64,
}

impl TeamProgressHook {
    fn new(activity: TeamActivitySink) -> Self {
        Self {
            activity,
            total_tokens: std::sync::atomic::AtomicU64::new(0),
            round_chars: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Finalized tokens plus a running estimate for the in-progress round.
    fn live_tokens(&self) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        self.total_tokens.load(Relaxed) + self.round_chars.load(Relaxed) / 4
    }

    fn add_chars(&self, delta: &str) {
        self.round_chars.fetch_add(
            delta.chars().count() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

#[async_trait]
impl LifecycleHooks for TeamProgressHook {
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        (self.activity)("thinking".to_string(), self.live_tokens());
    }

    async fn on_text_delta(&self, delta: &mut String) {
        self.add_chars(delta);
    }

    async fn on_reasoning_delta(&self, delta: &mut String) {
        self.add_chars(delta);
    }

    async fn on_model_response(&self, response: &mut Message) {
        use std::sync::atomic::Ordering::Relaxed;
        // Finalize this round: prefer the provider's reported completion count,
        // falling back to the chars/4 estimate when usage is unavailable.
        let estimated = self.round_chars.swap(0, Relaxed) / 4;
        let reported = response
            .meta
            .as_ref()
            .map(|meta| meta.tokens.completion as u64)
            .unwrap_or(0);
        self.total_tokens.fetch_add(reported.max(estimated), Relaxed);
        // Only surface an activity when the model is about to use a tool. A
        // response WITHOUT a tool call ends the turn — emitting "thinking" here
        // would just overwrite the last real activity and double the event rate;
        // the final token total is carried out via the member outcome instead.
        if let Some(call) = response.tool_calls.first() {
            (self.activity)(format!("using {}", call.name), self.total_tokens.load(Relaxed));
        }
    }
}

impl TeamProgressHook {
    fn total_tokens(&self) -> u64 {
        self.total_tokens.load(std::sync::atomic::Ordering::Relaxed)
    }
}

struct DenyTeamBash;

#[async_trait]
impl ToolMiddleware for DenyTeamBash {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if call.name == "bash" {
            BeforeOutcome::deny_turn(
                "team child may not run bash; verification remains owned by the parent agent",
            )
        } else {
            BeforeOutcome::Proceed
        }
    }
}

fn team_member_persona(profile: &TeamRoleProfile, scope: &[String]) -> String {
    let authority = match profile.permission {
        TeamPermission::Explore => "You are read-only. Investigate with the mounted read tools and report concise findings.".to_string(),
        TeamPermission::Worker => format!(
            "You may edit only within the assigned scope [{}]. Do not run shell commands. Make the focused change and report what remains for the parent to verify.",
            scope.join(", ")
        ),
    };
    format!(
        "You are the {} Team Agent role.\n{}\n{}\n{}",
        profile.display_name, authority, profile.persona, profile.when_to_use
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::team::TeamRoleId;
    use atomcode_kernel::hook::TurnCtx;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::{ToolCall, ToolContext, ToolDef, ToolRegistry, ToolResult};
    use futures::stream::BoxStream;

    struct DummyTool;
    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "dummy"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult::default()
        }
    }

    fn request_ctx() -> RequestCtx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RequestCtx::new(tx, None)
    }

    async fn apply(
        middlewares: &[Arc<dyn ToolMiddleware>],
        name: &str,
        args: &str,
    ) -> BeforeOutcome {
        let mut call = ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments: args.into(),
        };
        for middleware in middlewares {
            let outcome = middleware
                .before(
                    &mut call,
                    &(Arc::new(DummyTool) as Arc<dyn Tool>),
                    &request_ctx(),
                )
                .await;
            if outcome != BeforeOutcome::Proceed {
                return outcome;
            }
        }
        BeforeOutcome::Proceed
    }

    #[tokio::test]
    async fn worker_denies_bash_and_out_of_scope_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut middleware = vec![Arc::new(DenyTeamBash) as Arc<dyn ToolMiddleware>];
        middleware.extend(team_child_middlewares(
            true,
            &["src/**".into()],
            dir.path(),
            &[],
        ));
        assert!(apply(&middleware, "bash", r#"{"command":"cargo test"}"#)
            .await
            .is_deny());
        assert!(
            apply(&middleware, "write_file", r#"{"file_path":"../escape.rs"}"#)
                .await
                .is_deny()
        );
        assert_eq!(
            apply(&middleware, "write_file", r#"{"file_path":"src/ok.rs"}"#).await,
            BeforeOutcome::Proceed
        );
        assert!(apply(&middleware, "read_file", r#"{"file_path":"tests/outside.rs"}"#)
            .await
            .is_deny());
        assert_eq!(
            apply(&middleware, "read_file", r#"{"file_path":"src/ok.rs"}"#).await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn worker_denies_sensitive_paths() {
        let dir = tempfile::tempdir().unwrap();
        let middleware = team_child_middlewares(true, &["src/**".into()], dir.path(), &[]);
        assert!(apply(&middleware, "read_file", r#"{"file_path":".env"}"#)
            .await
            .is_deny());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_denies_write_through_symlink_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("src/link")).unwrap();
        let middleware = team_child_middlewares(true, &["src/**".into()], dir.path(), &[]);
        assert!(apply(
            &middleware,
            "write_file",
            r#"{"file_path":"src/link/escape.rs"}"#
        )
        .await
        .is_deny());
    }

    #[tokio::test]
    async fn progress_hook_estimates_tokens_chars_over_four() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let activity: TeamActivitySink = Arc::new(move |text, tokens| {
            let _ = tx.send((text, tokens));
        });
        let hook = TeamProgressHook::new(activity);
        // 8 chars → 2 tokens（chars/4 估算）。
        let mut delta = "abcdefgh".to_string();
        hook.on_text_delta(&mut delta).await;
        assert_eq!(hook.live_tokens(), 2);
        // pre_request 发布 "thinking" 并携带当前 token 估算。
        hook.pre_request(&mut vec![], &TurnCtx::default()).await;
        let (text, tokens) = rx.try_recv().unwrap();
        assert_eq!(text, "thinking");
        assert_eq!(tokens, 2);
        // on_model_response 发布 "using <tool>"。
        let mut response = Message::assistant(
            "",
            vec![ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        );
        hook.on_model_response(&mut response).await;
        let (text, _) = rx.try_recv().unwrap();
        assert_eq!(text, "using read_file");
    }

    struct NamedProvider(&'static str);
    #[async_trait]
    impl LlmProvider for NamedProvider {
        fn model_name(&self) -> &str {
            self.0
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[test]
    fn model_factory_maps_difficulty() {
        let providers: TeamProviderFactory = Arc::new(|difficulty| {
            let name = match difficulty {
                TeamDifficulty::Simple => "fast-model",
                TeamDifficulty::Hard => "capable-model",
            };
            Arc::new(NamedProvider(name)) as Arc<dyn LlmProvider>
        });
        let runner = TeamRunnerFactory::new(
            providers,
            Arc::new(|_| ToolRegistry::new().mount(&[])),
            std::env::temp_dir(),
        );
        let models = runner.model_factory();
        let simple = TeamTaskSpec {
            description: "d".into(),
            prompt: "p".into(),
            role: TeamRoleId::Explorer,
            permission: TeamPermission::Explore,
            difficulty: TeamDifficulty::Simple,
            scope: vec![],
        };
        let hard = TeamTaskSpec {
            difficulty: TeamDifficulty::Hard,
            ..simple.clone()
        };
        assert_eq!(models(&simple), "fast-model");
        assert_eq!(models(&hard), "capable-model");
    }

    #[test]
    fn persona_embeds_scope_and_authority() {
        let worker = role_by_id("implementer").unwrap();
        let persona = team_member_persona(worker, &["src/**".into()]);
        assert!(persona.contains("src/**"), "{persona}");
        assert!(persona.contains("Do not run shell commands"), "{persona}");
        let explorer = role_by_id("explorer").unwrap();
        let persona = team_member_persona(explorer, &[]);
        assert!(persona.contains("read-only"), "{persona}");
        assert!(!persona.contains("Do not run shell commands"), "{persona}");
    }
}
