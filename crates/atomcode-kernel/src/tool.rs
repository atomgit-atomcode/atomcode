//! Tools mounted into the kernel — and the kernel's **trust-model contract**.
//!
//! # Trust model (read before mounting a tool)
//!
//! The kernel is a neutral, embeddable SDK. It does **NOT sandbox** the tools it
//! hosts. MOUNTING a tool GRANTS its `execute` the host process's **full ambient
//! authority** — the same environment variables, filesystem, network, and
//! secrets the host process itself holds. There is no privilege boundary between
//! a mounted tool and the embedder.
//!
//! * [`RiskLevel`] is **advisory metadata only**. A tool declares it; a
//!   specialization's approval middleware MAY read it to decide whether to gate a
//!   call. It is NOT an enforcement boundary — the kernel never blocks, drops, or
//!   confines a call based on its `RiskLevel`. Rating a call `Safe` confines
//!   nothing; rating it `Risky` stops nothing on its own.
//! * The kernel's ONE built-in safety mechanism at this altitude is the
//!   **tool-result size cap** (`agent::AgentBuilder::max_tool_result_bytes`,
//!   default 64 KiB): it bounds how many bytes a tool result can inject into the
//!   context window / host memory. That is the limit of what the kernel can own
//!   here.
//! * **OS-level isolation is the EMBEDDER's responsibility**, not the kernel's.
//!   seccomp, namespaces, containers, a separate child process, a restricted
//!   user, network egress controls — these live at the OS / driver / an L1
//!   capability layer. The kernel deliberately does not implement them: doing so
//!   would be out of its altitude (it has no OS-specific knowledge and must stay
//!   portable). An embedder mounting untrusted tools MUST provide isolation
//!   itself.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string from the model.
    pub arguments: String,
}

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Risk classification a tool declares about itself. **Advisory metadata, NOT an
/// enforcement boundary** (see the module-level trust-model contract): the kernel
/// only *knows* risk; it does nothing about it — it never blocks, drops, or
/// sandboxes a call based on its `RiskLevel`. "Approval" is a specialization
/// concept built on top (see testkit::ApprovalMiddleware) that MAY read this to
/// decide whether to gate. This boundary keeps approval OUT of the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskLevel {
    Safe,
    Risky,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
    /// Inline images a tool produced for a VISION model to SEE (e.g. `read_file`
    /// returning a picture instead of the "binary, cannot display" dead-end). A
    /// TRANSIENT carrier on the result, NOT persisted onto the tool-result message:
    /// the agent loop lifts these onto a follow-up `Role::User` message (the only
    /// role a provider serializes images on — OpenAI rejects images in a `tool`
    /// message), exactly mirroring how user-pasted images already reach the model.
    /// Empty for every text tool. ADDITIVE: `#[serde(default)]` so an older snapshot
    /// (no `images`) still deserializes (→ empty). See [`crate::message::ImageContent`].
    #[serde(default)]
    pub images: Vec<crate::message::ImageContent>,
}

/// What the LLM sees for a mounted tool.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Execution context passed to tools. Deliberately minimal: NO semantic/graph/lsp
/// services — proving the kernel needs none. NOTE the trust model (module doc):
/// the kernel does not sandbox; `execute` runs with the host process's full
/// ambient authority. The only bound the kernel imposes on a tool is the size of
/// the `ToolResult.content` it may inject (`AgentBuilder::max_tool_result_bytes`).
///
/// `cancel` is the per-turn cooperative-cancellation token. A long-running tool
/// SHOULD poll `ctx.cancel.is_cancelled()` or `select!` on `ctx.cancel.cancelled()`
/// to bail out and RELEASE ITS RESOURCES. On cancel the kernel drops the execute
/// future as a backstop, but dropping only STOPS POLLING — it is NOT cleanup: any
/// subprocess / fd / partial write the tool spawned is the TOOL's responsibility
/// to reclaim, via cooperative cancel-polling or an RAII `Drop` guard on the
/// resource (e.g. a child-process handle that SIGKILLs on drop). A tool that does
/// neither may leak on cancel, and a side effect already in flight when the future
/// is dropped is reported to the model as cancelled even though it may have landed.
/// A live progress channel a long-running tool MAY use to report incremental status to
/// the DRIVER mid-execution — e.g. a sub-agent tool reporting per-task progress, or a
/// batch editor reporting per-file. Each [`emit`](ProgressSink::emit) becomes an
/// `AgentEvent::ToolProgress` tagged with the executing call's id. Cheap to clone; the
/// `noop()` sink (the DEFAULT, installed when no driver wires one) silently discards, so
/// a tool can always call `emit` without branching.
///
/// NEUTRALITY: the kernel knows nothing about "sub-agents" (those are an L2 composition
/// — a tool running a child session). This is the GENERIC observability seam such a tool
/// builds on to surface child/sub-task progress; the kernel only forwards the bytes.
#[derive(Clone)]
pub struct ProgressSink {
    inner: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl ProgressSink {
    /// A sink that discards (no driver listening). The default.
    pub fn noop() -> Self {
        Self { inner: None }
    }
    /// A sink backed by `f`. The kernel installs one that forwards to the driver event
    /// stream, tagging each message with the executing call's id.
    pub fn new(f: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        Self { inner: Some(f) }
    }
    /// Report incremental progress to the driver. No-op if no driver is listening.
    pub fn emit(&self, message: impl Into<String>) {
        if let Some(f) = &self.inner {
            f(message.into());
        }
    }
}

impl Default for ProgressSink {
    fn default() -> Self {
        Self::noop()
    }
}

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressSink")
            .field("active", &self.inner.is_some())
            .finish()
    }
}

pub struct ToolContext {
    pub working_dir: PathBuf,
    pub cancel: tokio_util::sync::CancellationToken,
    /// Live progress channel (see [`ProgressSink`]). Default `noop()` — a tool reports
    /// progress only if it wants to, and only a driver that cares receives it.
    pub progress: ProgressSink,
    /// Request seam so a tool can ask the driver a structured question and await the
    /// answer. `None` in tests/headless → `request()` returns Null and callers degrade.
    pub requester: Option<crate::request::Requester>,
}

impl ToolContext {
    pub async fn request(&self, kind: &str, payload: serde_json::Value) -> serde_json::Value {
        match &self.requester {
            Some(r) => r.request(kind, payload).await,
            None => serde_json::Value::Null,
        }
    }
}

/// A mounted tool. Its `execute` runs with the host process's FULL ambient
/// authority — the kernel does not sandbox it (see the module-level trust-model
/// contract). The kernel's only built-in bound on a tool is the size of the
/// result it may return (`agent::AgentBuilder::max_tool_result_bytes`); a tool
/// returning a huge `ToolResult.content` is TRUNCATED to that cap before the
/// model / history / driver see it, so a runaway tool cannot blow the context
/// window. A tool MAY also self-cap, but need not — the kernel cap is a central
/// backstop for third-party tools that do not.
///
/// # PANIC CONTRACT (must-not-panic)
///
/// An `execute` (or any trait method) **MUST NOT panic**. The kernel does **NOT**
/// isolate panics: under the workspace `panic = "abort"` profile a panic ABORTS
/// THE HOST PROCESS (and `catch_unwind` is a no-op there), and under an unwind
/// profile a panicking tool is not currently caught either — so a panicking tool
/// takes down the whole session / process. This is the SAME trust posture as the
/// sandbox contract above: treat all injected code as must-not-panic. A tool that
/// can fail must return `ToolResult { is_error: true, .. }`, never panic.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    /// Risk classification for THIS call — arg-aware, so e.g. a bash tool can rate
    /// `rm -rf` Risky and `ls` Safe. Conservative default: Safe. The tool owns this
    /// (intrinsic knowledge of its args); a specialization's approval middleware
    /// reads it to decide whether to gate.
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }
    /// Whether this tool is KNOWN to be read-only (no side effects) — an intrinsic
    /// property of the tool, distinct from `risk()` (which folds in trust/approval
    /// state). Default `false` (unknown). An MCP tool sets this from the server's
    /// `annotations.readOnlyHint`. A specialization (e.g. plan mode) reads it to allow
    /// read-only external queries that it would otherwise gate — a read-only tool
    /// cannot modify anything, so it is safe during read-only exploration.
    fn read_only_hint(&self) -> bool {
        false
    }
    /// Whether THIS call (with these args) may run CONCURRENTLY with other tools in
    /// the same assistant message. Arg-aware: a tool's safety can depend on its
    /// arguments — `bash` is parallel-safe only for provably read-only commands, so
    /// it inspects `_args`. Arg-independent tools ignore `_args` and defer to
    /// `read_only_hint()` (the single "no side effects" property, also read by plan
    /// mode). A side-effecting tool leaves this `false` and is serialized behind the
    /// write-lock by the executor.
    fn parallel_safe(&self, _args: &str) -> bool {
        self.read_only_hint()
    }
    /// The scope under which an "always" approval grant ("总是 / Always") is
    /// remembered for THIS call. Two calls that yield the SAME scope string share a
    /// single grant — approving "always" on one auto-approves the other for the
    /// session. The conservative DEFAULT is the exact `args`, so each distinct call
    /// is remembered on its own; this is correct for a tool like `bash`, where every
    /// destructive command must be approved individually (approving `rm -rf foo`
    /// must NOT blanket-approve `rm -rf bar`). A tool whose calls always differ in
    /// args but whose approval is meaningfully tool-wide (`edit_file`, `write_file`,
    /// …) overrides this to a constant so "Always" covers ALL its future calls this
    /// session — matching v1's tool-wide `grant_session(&call.name)`. Advisory
    /// metadata only: a specialization's approval middleware reads it; the kernel
    /// itself never gates.
    fn always_grant_scope(&self, args: &str) -> String {
        args.to_string()
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult;
}

/// Holds *all* available tools. Clones share the same registry so a runtime-owned
/// background capability reconciler can register tools before publishing a new
/// mounted snapshot. BTreeMap preserves deterministic prompt ordering.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let mut tools = match self.tools.write() {
            Ok(tools) => tools,
            Err(poisoned) => poisoned.into_inner(),
        };
        tools.insert(tool.name().to_string(), tool);
    }
    /// Select the subset exposed to the LLM. Unmounted tools never produce a
    /// ToolDef and are not resolvable during a turn → zero effect on the agent.
    pub fn mount(&self, names: &[&str]) -> MountedTools {
        let (mounted, _publisher) = self.mount_updatable(names);
        mounted
    }

    /// Select a subset whose complete contents may later be atomically replaced.
    ///
    /// The publisher is deliberately separate from [`MountedTools`]: the agent
    /// only reads snapshots, while the embedding runtime remains the sole writer.
    pub fn mount_updatable(&self, names: &[&str]) -> (MountedTools, MountedToolsPublisher) {
        let snapshot = Arc::new(MountedToolsSnapshot::new(
            ToolCatalogRevision(0),
            self.select(names),
        ));
        let current = Arc::new(RwLock::new(snapshot));
        (
            MountedTools {
                current: current.clone(),
            },
            MountedToolsPublisher { current },
        )
    }

    fn select(&self, names: &[&str]) -> BTreeMap<String, Arc<dyn Tool>> {
        let tools = match self.tools.read() {
            Ok(tools) => tools,
            Err(poisoned) => poisoned.into_inner(),
        };
        names
            .iter()
            .filter_map(|n| tools.get(*n).map(|t| (n.to_string(), t.clone())))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ToolCatalogRevision(pub u64);

#[derive(Clone)]
pub struct MountedTools {
    current: Arc<RwLock<Arc<MountedToolsSnapshot>>>,
}

/// The exact tool definitions and implementations used for one agent turn.
///
/// A snapshot stays valid after a newer catalog is published, preventing a
/// model request and its subsequent tool execution from observing different
/// tool sets.
pub struct MountedToolsSnapshot {
    revision: ToolCatalogRevision,
    selected: BTreeMap<String, Arc<dyn Tool>>,
    defs: Arc<[ToolDef]>,
}

impl MountedToolsSnapshot {
    fn new(revision: ToolCatalogRevision, selected: BTreeMap<String, Arc<dyn Tool>>) -> Self {
        let defs = selected
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
            })
            .collect::<Vec<_>>()
            .into();
        Self {
            revision,
            selected,
            defs,
        }
    }

    pub fn revision(&self) -> ToolCatalogRevision {
        self.revision
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.defs.to_vec()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.selected.get(name).cloned()
    }
}

/// Runtime-owned write side of an updatable tool mount.
///
/// Publishing always replaces the full selected set under one write lock. Clones
/// are writer capabilities for tasks spawned by that same runtime owner; revision
/// assignment remains serialized by the shared lock.
#[derive(Clone)]
pub struct MountedToolsPublisher {
    current: Arc<RwLock<Arc<MountedToolsSnapshot>>>,
}

impl MountedToolsPublisher {
    pub fn publish(&self, registry: &ToolRegistry, names: &[&str]) -> ToolCatalogRevision {
        let selected = registry.select(names);
        let mut current = match self.current.write() {
            Ok(current) => current,
            Err(poisoned) => poisoned.into_inner(),
        };
        let revision = ToolCatalogRevision(current.revision.0.saturating_add(1));
        *current = Arc::new(MountedToolsSnapshot::new(revision, selected));
        revision
    }
}

impl MountedTools {
    pub fn snapshot(&self) -> Arc<MountedToolsSnapshot> {
        match self.current.read() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.snapshot().defs()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.snapshot().get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;

    #[test]
    fn parallel_safe_defaults_to_read_only_hint() {
        struct Plain;
        #[async_trait::async_trait]
        impl Tool for Plain {
            fn name(&self) -> &str {
                "plain"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
                ToolResult {
                    call_id: String::new(),
                    content: String::new(),
                    is_error: false,
                    images: vec![],
                }
            }
        }
        struct RO;
        #[async_trait::async_trait]
        impl Tool for RO {
            fn name(&self) -> &str {
                "ro"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn read_only_hint(&self) -> bool {
                true
            }
            async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
                ToolResult {
                    call_id: String::new(),
                    content: String::new(),
                    is_error: false,
                    images: vec![],
                }
            }
        }
        assert!(
            !Plain.parallel_safe("{}"),
            "default (no read_only_hint) is NOT parallel-safe"
        );
        assert!(RO.parallel_safe("{}"), "a read-only tool IS parallel-safe");
    }

    struct Dummy(&'static str, RiskLevel);

    #[async_trait]
    impl Tool for Dummy {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "dummy"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk(&self, _args: &str) -> RiskLevel {
            self.1
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: "ok".into(),
                is_error: false,
                images: vec![],
            }
        }
    }

    #[test]
    fn progress_noop_sink_is_silent() {
        ProgressSink::noop().emit("ignored"); // no listener → must not panic
        ProgressSink::default().emit("also ignored");
    }

    #[test]
    fn progress_sink_forwards_each_message() {
        use std::sync::Mutex;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let c2 = captured.clone();
        let sink = ProgressSink::new(Arc::new(move |m| c2.lock().unwrap().push(m)));
        sink.emit("a");
        sink.emit("b");
        assert_eq!(
            *captured.lock().unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn only_mounted_tools_are_exposed_or_resolvable() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Dummy("echo", RiskLevel::Safe)));
        reg.register(Arc::new(Dummy("risky_write", RiskLevel::Risky)));

        let mounted = reg.mount(&["echo"]);

        let defs = mounted.defs();
        assert_eq!(defs.len(), 1, "unmounted tool must not appear in ToolDefs");
        assert_eq!(defs[0].name, "echo");

        assert!(mounted.get("echo").is_some());
        assert!(
            mounted.get("risky_write").is_none(),
            "unmounted tool must be inert/invisible"
        );
    }

    #[test]
    fn updatable_mount_publishes_an_atomic_new_snapshot() {
        let mut initial = ToolRegistry::new();
        initial.register(Arc::new(Dummy("echo", RiskLevel::Safe)));
        let (mounted, publisher) = initial.mount_updatable(&["echo"]);
        let turn_one = mounted.snapshot();

        let mut replacement = ToolRegistry::new();
        replacement.register(Arc::new(Dummy("risky_write", RiskLevel::Risky)));
        let revision = publisher.publish(&replacement, &["risky_write"]);
        let turn_two = mounted.snapshot();

        assert_eq!(revision, ToolCatalogRevision(1));
        assert_eq!(turn_one.revision(), ToolCatalogRevision(0));
        assert!(turn_one.get("echo").is_some());
        assert!(turn_one.get("risky_write").is_none());
        assert_eq!(turn_two.revision(), ToolCatalogRevision(1));
        assert!(turn_two.get("echo").is_none());
        assert!(turn_two.get("risky_write").is_some());
    }

    #[test]
    fn mounted_tools_reads_the_latest_published_snapshot() {
        let mut initial = ToolRegistry::new();
        initial.register(Arc::new(Dummy("echo", RiskLevel::Safe)));
        let (mounted, publisher) = initial.mount_updatable(&["echo"]);

        let mut replacement = ToolRegistry::new();
        replacement.register(Arc::new(Dummy("risky_write", RiskLevel::Risky)));
        publisher.publish(&replacement, &["risky_write"]);

        let defs = mounted.defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "risky_write");
        assert!(mounted.get("echo").is_none());
        assert!(mounted.get("risky_write").is_some());
    }

    #[tokio::test]
    async fn tool_context_without_requester_returns_null() {
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("/"),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: ProgressSink::noop(),
            requester: None,
        };
        assert_eq!(
            ctx.request("ask", serde_json::json!({})).await,
            serde_json::Value::Null
        );
    }
}
