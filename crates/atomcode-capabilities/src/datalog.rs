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

/// On-disk record format version written into every `.jsonl` line. `1` (or a
/// missing field) is the legacy full-snapshot layout where each line embeds the
/// entire `messages`/`tools` arrays; `2` is the content-addressed layout where a
/// line carries `message_refs`/`tool_refs` — arrays of small per-turn integer
/// blob ids — and the bodies live once in the sibling `<stem>.cas.jsonl`.
/// Readers branch on this — see [`rehydrate_record`].
const RECORD_FORMAT_VERSION: u32 = 2;

/// Serialize each body and intern it against `seen` (content-addressed by the
/// `sha256` of its bytes, kept only as a compact 32-byte map key). The first
/// time a body is seen this turn it is assigned the next integer id from
/// `next_id` and appended as one `<stem>.cas.jsonl` line
/// (`{"i":<id>,"k":<kind>,"c":<body>}`); repeats reuse the existing id. Returns
/// the ordered id list to store in the record — small integers, NOT 64-char
/// hashes, so a record listing the whole growing history stays cheap. `kind`
/// tags the blob (`"m"` message / `"t"` tool). An unserializable body yields
/// `None` (a `null` ref) so the record stays positionally correct.
///
/// Best-effort note: an id is minted (and `seen` updated) when the cas line is
/// queued, not when it lands. If that queued append later fails on the writer
/// thread (disk full / EACCES), the body is lost for the rest of the turn and
/// every record referencing that id rehydrates to `null` — this layer never
/// changes a turn's behavior, so it does not retry.
fn intern_bodies<T: serde::Serialize>(
    seen: &mut HashMap<[u8; 32], u32>,
    next_id: &mut u32,
    kind: &str,
    bodies: &[T],
    cas_lines: &mut String,
) -> Vec<Option<u32>> {
    use sha2::{Digest, Sha256};
    bodies
        .iter()
        .map(|body| {
            let json = serde_json::to_string(body).ok()?;
            let key: [u8; 32] = Sha256::digest(json.as_bytes()).into();
            if let Some(id) = seen.get(&key) {
                return Some(*id);
            }
            let id = *next_id;
            *next_id = next_id.saturating_add(1);
            seen.insert(key, id);
            let _ = writeln!(cas_lines, "{{\"i\":{id},\"k\":\"{kind}\",\"c\":{json}}}");
            Some(id)
        })
        .collect()
}

/// Parse a `<stem>.cas.jsonl` body into an `id → body` map. Blank or malformed
/// lines are skipped — the store is a best-effort debug artifact.
pub fn build_cas_index(cas_contents: &str) -> HashMap<u32, serde_json::Value> {
    let mut index = HashMap::new();
    for line in cas_contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let (Some(id), Some(body)) = (
            value.get("i").and_then(|v| v.as_u64()),
            value.get("c"),
        ) {
            index.insert(id as u32, body.clone());
        }
    }
    index
}

/// Reconstruct a full request record from a `v:2` content-addressed `.jsonl`
/// line: replace `message_refs`/`tool_refs` (arrays of integer blob ids) with
/// the `messages`/`tools` arrays looked up in `index` (built via
/// [`build_cas_index`] from the sibling cas file). A legacy v1 record — one that
/// already embeds `messages` and has no `*_refs` — is returned unchanged. A ref
/// that is `null` or has no matching blob rehydrates to `null`, preserving arity
/// and order.
pub fn rehydrate_record(
    record: &serde_json::Value,
    index: &HashMap<u32, serde_json::Value>,
) -> serde_json::Value {
    let mut out = record.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    for (ref_key, body_key) in [("message_refs", "messages"), ("tool_refs", "tools")] {
        let Some(refs) = object.get(ref_key).and_then(|v| v.as_array()).cloned() else {
            continue;
        };
        let bodies: Vec<serde_json::Value> = refs
            .iter()
            .map(|reference| {
                reference
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                    .and_then(|id| index.get(&id).cloned())
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect();
        object.remove(ref_key);
        object.insert(body_key.to_string(), serde_json::Value::Array(bodies));
    }
    out
}

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
    /// Sibling content-addressed store for this turn: `<stem>.cas.jsonl`. Each
    /// unique message/tool body is appended once; `.jsonl` records reference them
    /// by hash. Deleted alongside the `.jsonl`/`.md` by any retention sweep.
    cas_path: Option<PathBuf>,
    /// Interning table for `cas_path` this turn: `sha256(body) → integer blob id`.
    /// The 32-byte key dedups (one write per unique body); the small id is what
    /// records store, so listing the whole growing history stays cheap. Together
    /// they turn the old O(n²) full-history rewrite into one write per unique body.
    blob_ids: HashMap<[u8; 32], u32>,
    /// Next integer blob id to hand out this turn (shared across message/tool
    /// blobs so every id in `cas_path` is unique). Reset per turn.
    next_blob_id: u32,
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
        reply: tokio::sync::oneshot::Sender<Option<(PathBuf, PathBuf, PathBuf)>>,
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
        state.cas_path = None;
        state.blob_ids.clear();
        state.next_blob_id = 0;
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
            if state.markdown_path.is_some()
                && state.jsonl_path.is_some()
                && state.cas_path.is_some()
            {
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

        let Some((markdown_path, jsonl_path, cas_path)) = self
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
        state.cas_path = Some(cas_path);
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
        // Content-address the two arrays that dominate this record. Round N+1's
        // history is round N's plus a few new messages, so nearly every hash is
        // already interned — the body is written to `<stem>.cas.jsonl` once and the
        // record only carries the (small) ordered hash list. This is what removes
        // the old O(n²) full-history rewrite. An unserializable body degrades to an
        // empty ref rather than aborting the record (rehydration tolerates it).
        let mut cas_lines = String::new();
        let log = &mut *state;
        let message_refs =
            intern_bodies(&mut log.blob_ids, &mut log.next_blob_id, "m", messages, &mut cas_lines);
        let tool_refs =
            intern_bodies(&mut log.blob_ids, &mut log.next_blob_id, "t", tools, &mut cas_lines);
        let record = serde_json::json!({
            "v": RECORD_FORMAT_VERSION,
            "step": ctx.round,
            "session_id": ctx.session_id.as_deref().map(|id| id.as_ref()).unwrap_or(""),
            "turn_id": ctx.turn_id,
            "request_id": ctx.request_id,
            "model": self.model,
            "context_window": self.context_window,
            "message_count": messages.len(),
            "estimated_tokens": estimated_tokens,
            "tool_count": tools.len(),
            "message_refs": message_refs,
            "tool_refs": tool_refs,
            "options": options,
            "cache_epoch": ctx.cache_epoch,
        });
        // Blobs before the record that references them: both go through the single
        // writer thread, so appending the cas lines first keeps a reader from ever
        // seeing a ref whose body has not landed yet.
        if let Some(cas) = &state.cas_path {
            if !cas_lines.is_empty() {
                self.writer.append(cas.clone(), cas_lines);
            }
        }
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

    async fn after(
        &self,
        result: &mut ToolResult,
        _tool: Option<&std::sync::Arc<dyn atomcode_kernel::tool::Tool>>,
    ) -> AfterOutcome {
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
    ) -> Option<(PathBuf, PathBuf, PathBuf)> {
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
                if let Err(Some((markdown_path, jsonl_path, cas_path))) = reply.send(result) {
                    let _ = fs::remove_file(markdown_path);
                    let _ = fs::remove_file(jsonl_path);
                    let _ = fs::remove_file(cas_path);
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
) -> Option<(PathBuf, PathBuf, PathBuf)> {
    ensure_private_directory(directory).ok()?;
    for suffix in 0..1000 {
        let stem = if suffix == 0 {
            filename_stem.to_string()
        } else {
            format!("{filename_stem}-{suffix}")
        };
        let markdown_path = directory.join(format!("{stem}.md"));
        let jsonl_path = directory.join(format!("{stem}.jsonl"));
        let cas_path = directory.join(format!("{stem}.cas.jsonl"));
        let Ok(mut markdown_file) = create_private_file(&markdown_path) else {
            continue;
        };
        if markdown_file.write_all(markdown).is_err() {
            let _ = fs::remove_file(&markdown_path);
            continue;
        }
        if create_private_file(&jsonl_path).is_err() {
            let _ = fs::remove_file(&markdown_path);
            continue;
        }
        match create_private_file(&cas_path) {
            Ok(_) => return Some((markdown_path, jsonl_path, cas_path)),
            Err(_) => {
                let _ = fs::remove_file(&markdown_path);
                let _ = fs::remove_file(&jsonl_path);
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
        hook.after(&mut tool_result, None).await;
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
        // `.cas.jsonl` also has extension `jsonl`, so match on the full name.
        let name_ends = |path: &&PathBuf, suffix: &str| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(suffix))
        };
        let markdown_path = files.iter().find(|p| name_ends(p, ".md")).unwrap();
        let cas_path = files.iter().find(|p| name_ends(p, ".cas.jsonl")).unwrap();
        let jsonl_path = files
            .iter()
            .find(|p| name_ends(p, ".jsonl") && !name_ends(p, ".cas.jsonl"))
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

        // Two rounds → two records, and each is the content-addressed v2 shape:
        // refs in the record, no inline `messages`.
        let jsonl = fs::read_to_string(jsonl_path).unwrap();
        assert_eq!(jsonl.lines().count(), 2);
        let record: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(record["v"], RECORD_FORMAT_VERSION);
        assert!(record.get("messages").is_none());
        assert_eq!(record["message_refs"].as_array().unwrap().len(), 1);

        // Both rounds sent the SAME single message, so the cas holds exactly one
        // blob — the write-amplification fix, proven directly.
        let cas = fs::read_to_string(cas_path).unwrap();
        assert_eq!(cas.lines().count(), 1);

        // And a single record + its cas file fully reconstructs the request.
        let full = rehydrate_record(&record, &build_cas_index(&cas));
        assert!(full.get("message_refs").is_none());
        assert!(full["messages"].to_string().contains("inspect this"));
    }

    #[test]
    fn intern_dedupes_repeats_and_rehydrate_round_trips() {
        // Round 1 sends [a]; round 2 sends [a, b]. `a` is interned once even though
        // it is sent in both rounds — cas grows by the delta (b), not the full set.
        let mut seen = HashMap::new();
        let mut next_id = 0u32;
        let mut cas = String::new();
        let a = serde_json::json!({"role":"user","text":"a"});
        let b = serde_json::json!({"role":"user","text":"b"});
        let refs1 = intern_bodies(&mut seen, &mut next_id, "m", std::slice::from_ref(&a), &mut cas);
        let refs2 = intern_bodies(&mut seen, &mut next_id, "m", &[a.clone(), b.clone()], &mut cas);
        assert_eq!(refs2[0], refs1[0], "identical body → same blob id");
        assert_eq!(cas.lines().count(), 2, "a interned once despite two sends");

        let record = serde_json::json!({ "v": 2, "message_refs": refs2, "tool_refs": [] });
        let full = rehydrate_record(&record, &build_cas_index(&cas));
        assert_eq!(full["messages"][0], a);
        assert_eq!(full["messages"][1], b);
        assert_eq!(full["tools"], serde_json::json!([]));
        assert!(full.get("message_refs").is_none());
    }

    #[test]
    fn rehydrate_leaves_legacy_v1_record_untouched() {
        // A pre-existing full-snapshot record has no `*_refs` — pass it through as-is.
        let v1 = serde_json::json!({ "step": 1, "messages": [{"text":"x"}], "tools": [] });
        assert_eq!(rehydrate_record(&v1, &HashMap::new()), v1);
    }

    #[test]
    fn rehydrate_missing_blob_becomes_null_preserving_arity() {
        let record = serde_json::json!({
            "v": 2, "message_refs": [7, 9], "tool_refs": []
        });
        let full = rehydrate_record(&record, &HashMap::new());
        assert_eq!(full["messages"].as_array().unwrap().len(), 2);
        assert_eq!(full["messages"][0], serde_json::Value::Null);
        assert_eq!(full["messages"][1], serde_json::Value::Null);
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
        // Two hooks × three files each (`.md` + `.jsonl` + `.cas.jsonl`).
        assert_eq!(files.len(), 6);
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
