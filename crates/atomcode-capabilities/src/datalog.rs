//! Best-effort per-turn datalogging for the native kernel lifecycle.
//!
//! The writer is observation-only: it records the final neutral request seen by
//! [`LifecycleHooks::on_request`] and never mutates or owns runtime state.
//!
//! Privacy note: records include the full request body (system prompt, messages,
//! tools). On Unix the output directory and files are created private (0o700/0o600).
//! On Windows there is no equivalent mode bit, so files are created with the
//! directory's inherited ACLs — which already deny other standard users when the
//! datalog lives under the user profile (`$ATOMCODE_HOME`, the default). If a
//! Windows user points `datalog.dir` at a world-readable location on a shared
//! machine, the request bodies are readable by other local users. The feature is
//! opt-in and disabled by default; keep `datalog.dir` inside your user profile.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use atomcode_config::config::{Config, DatalogConfig};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message};
use atomcode_kernel::middleware::{AfterOutcome, ToolMiddleware};
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall, ToolDef, ToolResult};

static HOOK_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const IO_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Native-runtime datalog writer. All filesystem failures are deliberately ignored:
/// observability must never change a turn's behavior or terminal.
pub struct DatalogHook {
    working_dir: PathBuf,
    configured_dir: Option<String>,
    model: String,
    context_window: u32,
    state: Mutex<TurnLog>,
    writer: DatalogWriter,
    instance_id: u64,
}

#[derive(Default)]
struct TurnLog {
    prompt: String,
    markdown_path: Option<PathBuf>,
    jsonl_path: Option<PathBuf>,
    initialization_attempted: bool,
    started: Option<Instant>,
    rounds: u32,
    tool_calls: usize,
    total_tokens: u64,
    active: bool,
    tool_names: HashMap<String, String>,
}

#[derive(Clone)]
struct DatalogWriter {
    tx: mpsc::Sender<WriteOp>,
}

enum WriteOp {
    Initialize {
        directory: PathBuf,
        filename_stem: String,
        markdown: String,
        reply: tokio::sync::oneshot::Sender<Option<(PathBuf, PathBuf)>>,
    },
    Append {
        path: PathBuf,
        content: String,
    },
    Barrier {
        reply: tokio::sync::oneshot::Sender<()>,
    },
}

impl DatalogHook {
    pub fn new(
        working_dir: impl Into<PathBuf>,
        config: &DatalogConfig,
        model: impl Into<String>,
        context_window: u32,
    ) -> Option<Self> {
        config.enabled.then(|| Self {
            working_dir: working_dir.into(),
            configured_dir: config.dir.clone(),
            model: model.into(),
            context_window,
            state: Mutex::new(TurnLog::default()),
            writer: DatalogWriter::start(),
            instance_id: HOOK_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Resolve `<configured-root>/<project-basename>-<hash8>`.
    pub fn resolve_log_dir(working_dir: &Path, configured_dir: Option<&str>) -> PathBuf {
        let root = match configured_dir.filter(|value| !value.trim().is_empty()) {
            // `DatalogConfig::default` materializes this value into config.toml.
            // Treat it as the semantic default so ATOMCODE_HOME keeps working.
            None | Some("~/.atomcode/datalog") => Config::config_dir().join("datalog"),
            Some("~") => {
                atomcode_config::util::real_home_dir().unwrap_or_else(|| PathBuf::from("."))
            }
            Some(value) if value.starts_with("~/") => atomcode_config::util::real_home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(value.trim_start_matches("~/")),
            Some(value) => {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    path
                } else {
                    working_dir.join(path)
                }
            }
        };
        root.join(project_slug(working_dir))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnLog> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn start_turn(&self, prompt: &str) {
        let mut state = self.lock();
        state.prompt.clear();
        state.prompt.push_str(prompt);
        state.markdown_path = None;
        state.jsonl_path = None;
        state.initialization_attempted = false;
        state.started = Some(Instant::now());
        state.rounds = 0;
        state.tool_calls = 0;
        state.total_tokens = 0;
        state.active = true;
        state.tool_names.clear();
    }

    async fn initialize_turn(&self, ctx: &TurnCtx) -> bool {
        let prompt = {
            let mut state = self.lock();
            if state.markdown_path.is_some() && state.jsonl_path.is_some() {
                return true;
            }
            if state.initialization_attempted {
                return false;
            }
            state.initialization_attempted = true;
            state.prompt.clone()
        };

        let now = chrono::Local::now();
        let display_timestamp = now.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let timestamp = now.format("%Y-%m-%d_%H-%M-%S_%3f");
        let session = sanitize_component(
            ctx.session_id
                .as_deref()
                .map(AsRef::as_ref)
                .filter(|value: &&str| !value.is_empty())
                .unwrap_or("sessionless"),
        );
        let filename_stem = format!(
            "{timestamp}-{session}-t{}-p{}-i{}",
            ctx.turn_id,
            std::process::id(),
            self.instance_id
        );
        let directory = Self::resolve_log_dir(&self.working_dir, self.configured_dir.as_deref());
        let build_id = option_env!("ATOMCODE_BUILD_ID").unwrap_or("dev");
        let mut markdown = String::new();
        let _ = writeln!(markdown, "# Turn {display_timestamp} [build:{build_id}]");
        let _ = writeln!(
            markdown,
            "**env:** model={}, ctx_window={}, cwd={}\n",
            self.model,
            self.context_window,
            self.working_dir.display()
        );
        let _ = writeln!(markdown, "## User\n```\n{prompt}\n```\n");
        let _ = writeln!(markdown, "## Agent\n");

        let Some((markdown_path, jsonl_path)) = self
            .writer
            .initialize(directory, filename_stem, markdown)
            .await
        else {
            return false;
        };
        let mut state = self.lock();
        if !state.active {
            return false;
        }
        state.markdown_path = Some(markdown_path);
        state.jsonl_path = Some(jsonl_path);
        true
    }

    fn append_markdown(&self, content: String) {
        let path = self.lock().markdown_path.clone();
        if let Some(path) = path {
            self.writer.append(path, content);
        }
    }
}

#[async_trait]
impl LifecycleHooks for DatalogHook {
    async fn user_prompt_submit(&self, text: &mut String) -> Result<(), String> {
        self.start_turn(text);
        Ok(())
    }

    async fn on_request(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
        ctx: &TurnCtx,
    ) {
        if !self.initialize_turn(ctx).await {
            return;
        }
        let mut state = self.lock();
        if !state.active {
            return;
        }
        let estimated_tokens: u64 = messages
            .iter()
            .map(|message| u64::from(message.estimate_tokens()))
            .sum();
        state.rounds = state.rounds.max(ctx.round);
        let record = serde_json::json!({
            "step": ctx.round,
            "session_id": ctx.session_id.as_deref().map(|id| id.as_ref()).unwrap_or(""),
            "turn_id": ctx.turn_id,
            "request_id": ctx.request_id,
            "model": self.model,
            "context_window": self.context_window,
            "message_count": messages.len(),
            "estimated_tokens": estimated_tokens,
            "tool_count": tools.len(),
            "messages": messages,
            "tools": tools,
            "options": options,
            "cache_epoch": ctx.cache_epoch,
        });
        if let (Some(path), Ok(line)) = (&state.jsonl_path, serde_json::to_string(&record)) {
            self.writer.append(path.clone(), format!("{line}\n"));
        }
        let mut markdown = String::new();
        let _ = writeln!(markdown, "### Turn {}", ctx.round);
        let _ = writeln!(
            markdown,
            "  _[request: {}msgs · {}tok · {}tools]_\n",
            messages.len(),
            estimated_tokens,
            tools.len()
        );
        drop(state);
        self.append_markdown(markdown);
    }

    async fn on_model_response(&self, response: &mut Message) {
        let mut state = self.lock();
        if !state.active {
            return;
        }
        let mut markdown = String::new();
        if let Some(reasoning) = response
            .reasoning
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            let _ = writeln!(markdown, "**Reasoning:**\n{reasoning}\n");
        }
        for call in &response.tool_calls {
            state.tool_names.insert(call.id.clone(), call.name.clone());
            let _ = writeln!(
                markdown,
                "- {} `{}`",
                call.name,
                call.arguments.replace('`', "\\`")
            );
        }
        if !response.text.is_empty() {
            if response.tool_calls.is_empty() {
                let _ = writeln!(markdown, "**Response:**\n{}\n", response.text.trim());
            } else {
                let display = response.text.trim().replace('\n', "\n  > ");
                let _ = writeln!(markdown, "  > {display}\n");
            }
        }
        state.tool_calls = state.tool_calls.saturating_add(response.tool_calls.len());
        if let Some(meta) = &response.meta {
            state.total_tokens = state.total_tokens.saturating_add(u64::from(
                meta.tokens.prompt.saturating_add(meta.tokens.completion),
            ));
            let _ = writeln!(
                markdown,
                "  _[tokens: prompt={}+completion={}, cache={}tok]_\n",
                meta.tokens.prompt, meta.tokens.completion, meta.tokens.cached
            );
        }
        drop(state);
        self.append_markdown(markdown);
    }

    async fn on_error(&self, error: &str) {
        if !self.lock().active {
            return;
        }
        self.append_markdown(format!("**Error:** {error}\n\n"));
    }

    async fn turn_complete(&self, _convo: &Conversation, reason: &StopReason, _ctx: &TurnCtx) {
        let markdown = {
            let mut state = self.lock();
            if !state.active {
                return;
            }
            let duration = state
                .started
                .map(|started| started.elapsed().as_secs_f64())
                .unwrap_or_default();
            let rounds = state.rounds;
            let tool_calls = state.tool_calls;
            let total_tokens = state.total_tokens;
            let mut markdown = String::new();
            let _ = writeln!(
                markdown,
                "---\n**Stats:** {rounds} turns, {tool_calls} tool calls, {duration:.1}s, {total_tokens} tokens\n\
                 **End:** reason={reason:?}",
            );
            state.active = false;
            markdown
        };
        self.append_markdown(markdown);
        self.writer.barrier().await;
    }
}

#[async_trait]
impl ToolMiddleware for DatalogHook {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> atomcode_kernel::middleware::BeforeOutcome {
        let mut state = self.lock();
        if state.active {
            state.tool_names.insert(call.id.clone(), call.name.clone());
        }
        atomcode_kernel::middleware::BeforeOutcome::Proceed
    }

    async fn after(&self, result: &mut ToolResult) -> AfterOutcome {
        let mut state = self.lock();
        if !state.active {
            return AfterOutcome::Proceed;
        }
        let name = state
            .tool_names
            .remove(&result.call_id)
            .unwrap_or_else(|| "unknown".to_string());
        drop(state);
        let status = if result.is_error { "error" } else { "ok" };
        self.append_markdown(format!(
            "**Tool result:** `{name}` (`{}`, {status})\n```\n{}\n```\n\n",
            result.call_id, result.content
        ));
        AfterOutcome::Proceed
    }
}

impl DatalogWriter {
    fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("atomcode-datalog".into())
            .spawn(move || writer_loop(rx));
        Self { tx }
    }

    async fn initialize(
        &self,
        directory: PathBuf,
        filename_stem: String,
        markdown: String,
    ) -> Option<(PathBuf, PathBuf)> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.tx
            .send(WriteOp::Initialize {
                directory,
                filename_stem,
                markdown,
                reply,
            })
            .ok()?;
        tokio::time::timeout(IO_WAIT_TIMEOUT, receive)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    fn append(&self, path: PathBuf, content: String) {
        let _ = self.tx.send(WriteOp::Append { path, content });
    }

    async fn barrier(&self) {
        let (reply, receive) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(WriteOp::Barrier { reply });
        let _ = tokio::time::timeout(IO_WAIT_TIMEOUT, receive).await;
    }
}

fn writer_loop(rx: mpsc::Receiver<WriteOp>) {
    while let Ok(operation) = rx.recv() {
        match operation {
            WriteOp::Initialize {
                directory,
                filename_stem,
                markdown,
                reply,
            } => {
                let result = initialize_files(&directory, &filename_stem, markdown.as_bytes());
                if let Err(Some((markdown_path, jsonl_path))) = reply.send(result) {
                    let _ = fs::remove_file(markdown_path);
                    let _ = fs::remove_file(jsonl_path);
                }
            }
            WriteOp::Append { path, content } => {
                if let Ok(mut file) = open_private_append(&path) {
                    let _ = file.write_all(content.as_bytes());
                }
            }
            WriteOp::Barrier { reply } => {
                let _ = reply.send(());
            }
        }
    }
}

fn initialize_files(
    directory: &Path,
    filename_stem: &str,
    markdown: &[u8],
) -> Option<(PathBuf, PathBuf)> {
    ensure_private_directory(directory).ok()?;
    for suffix in 0..1000 {
        let stem = if suffix == 0 {
            filename_stem.to_string()
        } else {
            format!("{filename_stem}-{suffix}")
        };
        let markdown_path = directory.join(format!("{stem}.md"));
        let jsonl_path = directory.join(format!("{stem}.jsonl"));
        let Ok(mut markdown_file) = create_private_file(&markdown_path) else {
            continue;
        };
        if markdown_file.write_all(markdown).is_err() {
            let _ = fs::remove_file(&markdown_path);
            continue;
        }
        match create_private_file(&jsonl_path) {
            Ok(_) => return Some((markdown_path, jsonl_path)),
            Err(_) => {
                let _ = fs::remove_file(&markdown_path);
            }
        }
    }
    None
}

fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    set_private_create_mode(&mut options);
    options.open(path)
}

fn open_private_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true);
    set_private_create_mode(&mut options);
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn set_private_create_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Non-Unix (Windows) has no create-mode bit here: the file inherits the parent
    // directory's ACLs. See the module-header privacy note — this is safe under the
    // default per-user `$ATOMCODE_HOME`, not for a world-readable `datalog.dir`.
    #[cfg(not(unix))]
    let _ = options;
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "sessionless".to_string()
    } else {
        sanitized
    }
}

fn project_slug(working_dir: &Path) -> String {
    let basename = working_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project");
    let sanitized: String = basename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let hash = atomcode_config::util::stable_project_hash(working_dir);
    let hash8 = hash.get(..8).unwrap_or(hash.as_str());
    format!("{sanitized}-{hash8}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use tempfile::tempdir;

    #[test]
    fn disabled_config_does_not_create_a_hook() {
        let config = DatalogConfig {
            enabled: false,
            dir: None,
        };
        assert!(DatalogHook::new("/repo", &config, "model", 128_000).is_none());
    }

    #[test]
    fn relative_roots_are_project_scoped_and_collision_safe() {
        let first = DatalogHook::resolve_log_dir(Path::new("/work/foo"), Some("logs"));
        let second = DatalogHook::resolve_log_dir(Path::new("/personal/foo"), Some("logs"));
        assert!(first.starts_with("/work/foo/logs"));
        assert!(second.starts_with("/personal/foo/logs"));
        assert_ne!(first.file_name(), second.file_name());
    }

    /// `DatalogConfig::default` materializes a literal into config.toml and
    /// [`DatalogHook::resolve_log_dir`] special-cases that same literal back to
    /// "unset". They are one contract, but they live in two crates with nothing
    /// linking them except a copied string.
    ///
    /// Reword either side and the default silently demotes to an ordinary `~/…`
    /// path: it stops following `$ATOMCODE_HOME` and every datalog moves to
    /// `$HOME/.atomcode/datalog`. No error, no failing test, and config.toml
    /// still reads exactly the same — which is why this needs pinning.
    #[test]
    fn the_materialized_default_resolves_the_same_as_an_unset_dir() {
        let materialized = DatalogConfig::default().dir;
        assert!(
            materialized.is_some(),
            "the default is materialized into config.toml on first save; if that \
             stopped, this contract and its comment in `resolve_log_dir` are stale"
        );

        let working_dir = Path::new("/work/foo");
        assert_eq!(
            DatalogHook::resolve_log_dir(working_dir, materialized.as_deref()),
            DatalogHook::resolve_log_dir(working_dir, None),
            "the string written to config.toml ({:?}) is no longer the one \
             `resolve_log_dir` treats as the default — the two crates have drifted",
            materialized
        );
    }

    /// The default root is `$ATOMCODE_HOME`-relative, not `$HOME`-relative.
    /// The harness `#[ctor]` points `$ATOMCODE_HOME` at a temp dir for the whole
    /// binary, so this asserts against a location that is provably not the
    /// built-in `~/.atomcode` — the case the `resolve_log_dir` special-case
    /// exists for.
    #[test]
    fn the_default_root_follows_atomcode_home() {
        let configured = Config::config_dir();
        assert!(
            !configured.ends_with(".atomcode"),
            "precondition: the harness must have moved the config dir off the \
             default, else this test cannot tell the two roots apart — got {}",
            configured.display()
        );

        let resolved = DatalogHook::resolve_log_dir(
            Path::new("/work/foo"),
            DatalogConfig::default().dir.as_deref(),
        );
        assert!(
            resolved.starts_with(configured.join("datalog")),
            "datalogs must land under $ATOMCODE_HOME, got {}",
            resolved.display()
        );
        // The project slug is still appended, so two projects never share a bucket.
        assert_ne!(resolved, configured.join("datalog"));
    }

    /// The special case is exact-match on purpose: any OTHER `~/…` value is a
    /// user-authored path and must expand against the real home, not the config
    /// dir. This is also what makes the default's spelling load-bearing — a
    /// stray space or trailing slash falls through to this arm.
    #[test]
    fn other_tilde_paths_are_not_the_default() {
        let Some(home) = atomcode_config::util::real_home_dir() else {
            return; // no resolvable home on this box; nothing to compare against
        };
        let resolved = DatalogHook::resolve_log_dir(Path::new("/work/foo"), Some("~/elsewhere"));
        assert!(
            resolved.starts_with(home.join("elsewhere")),
            "an explicit `~/…` must expand against $HOME, got {}",
            resolved.display()
        );

        // Same string as the default but with a trailing space: NOT the sentinel.
        let sloppy =
            DatalogHook::resolve_log_dir(Path::new("/work/foo"), Some("~/.atomcode/datalog "));
        assert!(
            sloppy.starts_with(home.join(".atomcode")),
            "only the exact literal is the sentinel, got {}",
            sloppy.display()
        );
    }

    #[tokio::test]
    async fn writes_markdown_and_one_jsonl_record_per_round() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let output = root.path().join("logs");
        let config = DatalogConfig {
            enabled: true,
            dir: Some(output.display().to_string()),
        };
        let hook = DatalogHook::new(&project, &config, "test-model", 128_000).unwrap();

        let mut prompt = "inspect this".to_string();
        hook.user_prompt_submit(&mut prompt).await.unwrap();
        let messages = vec![Message::user("inspect this")];
        let tools = Vec::new();
        let options = ChatOptions::default();
        for round in 1..=2 {
            let ctx = TurnCtx {
                round,
                request_id: u64::from(round),
                turn_id: 1,
                ..TurnCtx::default()
            };
            hook.on_request(&messages, &tools, &options, &ctx).await;
        }
        let mut response = Message::assistant(
            "done",
            vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"README.md"}"#.into(),
            }],
        );
        hook.on_model_response(&mut response).await;
        let mut tool_result = ToolResult {
            call_id: "call-1".into(),
            content: "tool output".into(),
            is_error: false,
            images: Vec::new(),
        };
        hook.after(&mut tool_result).await;
        hook.on_error("sample failure").await;
        hook.turn_complete(
            &Conversation::new(),
            &StopReason::ProviderError,
            &TurnCtx::default(),
        )
        .await;

        let project_dir = fs::read_dir(output)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let files: Vec<PathBuf> = fs::read_dir(project_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let markdown_path = files
            .iter()
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
            .unwrap();
        let jsonl_path = files
            .iter()
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .unwrap();
        let markdown = fs::read_to_string(markdown_path).unwrap();
        assert!(markdown.contains("## User"));
        assert!(markdown.contains("### Turn 2"));
        assert!(markdown.contains("- read_file"));
        assert!(markdown.contains("**Tool result:** `read_file` (`call-1`, ok)"));
        assert!(markdown.contains("tool output"));
        assert!(markdown.contains("**Error:** sample failure"));
        assert!(markdown.contains("**Stats:** 2 turns, 1 tool calls"));
        assert!(markdown.contains("reason=ProviderError"));
        assert_eq!(fs::read_to_string(jsonl_path).unwrap().lines().count(), 2);
    }

    #[tokio::test]
    async fn concurrent_hooks_create_distinct_session_scoped_file_pairs() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let output = root.path().join("logs");
        let config = DatalogConfig {
            enabled: true,
            dir: Some(output.display().to_string()),
        };
        let first = DatalogHook::new(&project, &config, "model", 128_000).unwrap();
        let second = DatalogHook::new(&project, &config, "model", 128_000).unwrap();
        let ctx = TurnCtx {
            session_id: Some(Arc::from("shared/session")),
            turn_id: 1,
            request_id: 1,
            round: 1,
            ..TurnCtx::default()
        };
        for hook in [&first, &second] {
            hook.user_prompt_submit(&mut "prompt".to_string())
                .await
                .unwrap();
            hook.on_request(
                &[Message::user("prompt")],
                &[],
                &ChatOptions::default(),
                &ctx,
            )
            .await;
            hook.turn_complete(&Conversation::new(), &StopReason::Stopped, &ctx)
                .await;
        }

        let project_dir = fs::read_dir(output)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let files: Vec<PathBuf> = fs::read_dir(project_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 4);
        assert!(files.iter().all(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("shared-session-t1")
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn creates_private_directory_and_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let output = root.path().join("logs");
        let config = DatalogConfig {
            enabled: true,
            dir: Some(output.display().to_string()),
        };
        let hook = DatalogHook::new(&project, &config, "model", 128_000).unwrap();
        hook.user_prompt_submit(&mut "prompt".to_string())
            .await
            .unwrap();
        let ctx = TurnCtx {
            turn_id: 1,
            request_id: 1,
            round: 1,
            ..TurnCtx::default()
        };
        hook.on_request(
            &[Message::user("prompt")],
            &[],
            &ChatOptions::default(),
            &ctx,
        )
        .await;
        hook.turn_complete(&Conversation::new(), &StopReason::Stopped, &ctx)
            .await;

        let project_dir = fs::read_dir(output)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let directory_mode = fs::metadata(&project_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(directory_mode, 0o700);
        for entry in fs::read_dir(project_dir).unwrap() {
            let mode = entry.unwrap().metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
