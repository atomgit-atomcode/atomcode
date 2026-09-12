//! The capability seams: what can be replaced, and the face a consumer sees.
//!
//! Each seam is three roles — a **definition** (here), one or more **providers**
//! (plugins that fill the slot), and **consumers** (plugins that read it). No
//! consumer names an implementation, which is why swapping a provider changes the
//! product without touching anything that uses it.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_capabilities::codeintel::CodeIndex;
use atomcode_capabilities::mcp::McpRegistry;
use atomcode_capabilities::skills::SkillRegistry;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{Tool, ToolDef};
use atomcode_plexus::{plexus_service, Context};

use crate::agent::{Agent, Agents};
use crate::session::{LoggedEvent, SessionLog, SessionProjections};
pub use atomcode_capabilities::tools::Opener;
pub use atomcode_capabilities::world::{FileSystem, Shell};

plexus_service!(LlmSvc => dyn LlmProvider, "llm", Seam, "Model adapter");
plexus_service!(LlmUtilitySvc => dyn LlmProvider, "llm-utility", Seam, "The model for side calls whose result a program consumes — titles, summaries, suggestions — not the conversation");
plexus_service!(ToolsSvc => ToolBox, "tools", Core, "The live tool catalog");
plexus_service!(SystemPromptSvc => PromptRegistry, "system-prompt", Core, "Ordered prompt fragments");
// Reuses `PromptRegistry` because the shape is identical — ranked fragments
// keyed by id, removed with their row — and a second implementation of "ordered
// contributions" would be a second thing to keep correct. The *slot* is what
// differs: this one is never sent to the model unprompted. It is answered when
// asked, so a row can describe a knob in as much detail as the knob deserves
// without that detail costing tokens on every single request.
plexus_service!(OperationsSvc => PromptRegistry, "operations", Core, "How to work the running system, described by the rows that own each knob");
plexus_service!(SessionSvc => SessionLog, "sessions", Core, "The append-only session log of the agent whose realm this is");
plexus_service!(SessionDefaultsSvc => SessionDefaults, "session-defaults", Core, "What the front end's own agent is told about its session: an id, whether to resume it");
plexus_service!(SessionProjectionsSvc => SessionProjections, "session-projections", Core, "Incremental folds over the log");
plexus_service!(SessionPersistenceSvc => dyn SessionPersistence, "session-persistence", Seam, "Durable session storage");
plexus_service!(SkillsSvc => SkillRegistry, "skills", Core, "Markdown skill catalog");
plexus_service!(CodeIndexSvc => CodeIndex, "code-index", Core, "Shared lazily-built code graph");
plexus_service!(FsSvc => dyn FileSystem, "fs", Seam, "One execution world's view of files");
plexus_service!(ShellSvc => dyn Shell, "shell", Seam, "Shell execution for one world: spawn, stream, kill the tree");
plexus_service!(OpenerSvc => dyn Opener, "opener", Seam, "Where a file or URL is shown to the person — the front end's to provide");
plexus_service!(CompactionSvc => dyn Compaction, "compaction", Seam, "History compaction strategy");
plexus_service!(SessionTitleSvc => dyn SessionTitle, "session-title", Seam, "How a session gets named");
plexus_service!(UserQuestionsSvc => dyn UserQuestions, "user-questions", Seam, "Asking a human");
plexus_service!(McpSvc => McpRegistry, "mcp", Core, "Connected MCP servers");
plexus_service!(AgentsSvc => Agents, "agents", Core, "Live agent registry");
plexus_service!(UiSvc => dyn UserInterface, "ui", Seam, "The interaction front end");
plexus_service!(ControlSvc => dyn Control, "control", Core, "Reconfiguring the running tree");
plexus_service!(FindingsSvc => dyn Findings, "findings", Seam, "Where structured findings are collected");
plexus_service!(SubagentsSvc => dyn Subagents, "subagents", Seam, "Delegating work to a child agent");
plexus_service!(AgentLoopSvc => dyn AgentLoop, "agent-loop", Seam, "The turn driver");
plexus_service!(ApprovalSvc => dyn ApprovalPolicy, "approval", Seam, "Whether a tool call may run");
plexus_service!(AgentHandleSvc => dyn AgentHandleSource, "agent-handle", Seam, "A driver-protocol handle on this harness");

/// The live tool catalog.
///
/// Deliberately mutable at runtime rather than a snapshot taken at assembly: a
/// tool plugin registers on apply and *unregisters on unload*, so unmounting
/// `tool-bash` removes bash from the model's schema list mid-session with nobody
/// rebuilding anything.
#[derive(Default)]
pub struct ToolBox {
    tools: RwLock<BTreeMap<String, Arc<dyn Tool>>>,
}

impl ToolBox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Pair every call with a `ctx.effect(.. unregister ..)` so the
    /// tool leaves when its plugin does.
    ///
    /// A duplicate name is an error, not last-write-wins: two rows claiming
    /// `read_file` means the config is ambiguous about which execution world the
    /// model is talking to, and silently picking one would be the worst answer.
    pub fn register(&self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let name = tool.name().to_string();
        let mut tools = self.tools.write().expect("toolbox poisoned");
        if tools.contains_key(&name) {
            return Err(format!(
                "tool `{name}` is already registered; disable the row that owns it before mounting another"
            ));
        }
        tools.insert(name, tool);
        Ok(())
    }

    pub fn unregister(&self, name: &str) {
        self.tools.write().expect("toolbox poisoned").remove(name);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .expect("toolbox poisoned")
            .get(name)
            .cloned()
    }

    /// What the model is shown this round. Read fresh every request, so a
    /// plugin mounted mid-session is visible on the very next one.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .read()
            .expect("toolbox poisoned")
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
            })
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .expect("toolbox poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

/// Prompt fragments, contributed by whoever owns the behaviour they describe.
///
/// A tool plugin that needs usage guidance ships it here rather than the persona
/// growing a paragraph about a tool it does not own. Fragments are ordered by an
/// explicit rank so the assembled prompt is stable no matter what order plugins
/// activated in — which also keeps the prefix cacheable.
#[derive(Default)]
pub struct PromptRegistry {
    fragments: RwLock<Vec<Fragment>>,
}

struct Fragment {
    id: String,
    rank: i32,
    text: String,
}

impl PromptRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contribute(&self, id: impl Into<String>, rank: i32, text: impl Into<String>) {
        let id = id.into();
        let mut fragments = self.fragments.write().expect("prompt registry poisoned");
        fragments.retain(|f| f.id != id);
        fragments.push(Fragment {
            id,
            rank,
            text: text.into(),
        });
        fragments.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.id.cmp(&b.id)));
    }

    pub fn remove(&self, id: &str) {
        self.fragments
            .write()
            .expect("prompt registry poisoned")
            .retain(|f| f.id != id);
    }

    pub fn render(&self) -> String {
        self.fragments
            .read()
            .expect("prompt registry poisoned")
            .iter()
            .map(|f| f.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn ids(&self) -> Vec<String> {
        self.fragments
            .read()
            .expect("prompt registry poisoned")
            .iter()
            .map(|f| f.id.clone())
            .collect()
    }
}

/// Durable session storage. A seam: the shipped provider appends JSONL under
/// the harness home, but the same interface covers a database, an object store,
/// or nothing at all.
///
/// It receives *events*, not messages — persistence that stored the projection
/// instead of the facts could not reproduce a UI replay or a different
/// compaction after the fact.
#[async_trait]
pub trait SessionPersistence: Send + Sync {
    /// Record a session's header, once, before any of its events. A session
    /// the store already holds keeps the header it has: this is where a new
    /// file gets its first line, not where an old one is rewritten.
    async fn begin(&self, _header: &crate::session::SessionHeader) -> Result<(), String> {
        Ok(())
    }

    /// The header a stored session was created with — `None` for a store that
    /// keeps none, or a file written before there was one.
    async fn header(
        &self,
        _session_id: &str,
    ) -> Result<Option<crate::session::SessionHeader>, String> {
        Ok(None)
    }

    /// What `session/list` wants to say about one session without replaying
    /// it into an agent: the header, the name, and how much happened.
    async fn describe(&self, session_id: &str) -> Result<Option<SessionSummary>, String> {
        let header = self.header(session_id).await?;
        let events = self.load(session_id).await?;
        if header.is_none() && events.is_empty() {
            return Ok(None);
        }
        let inherited = header.as_ref().map(|h| h.inherited).unwrap_or(0);
        let title = events
            .iter()
            .skip(inherited)
            .rev()
            .find_map(|e| match &e.event {
                crate::session::SessionEvent::Titled { title, .. } => Some(title.clone()),
                _ => None,
            });
        let turns = events
            .iter()
            .filter(|e| matches!(e.event, crate::session::SessionEvent::TurnStart { .. }))
            .count();
        Ok(Some(SessionSummary {
            header,
            title,
            turns,
            events: events.len(),
        }))
    }

    /// Append everything after `cursor`. Implementations must be idempotent for
    /// a re-sent range: a crash between write and cursor update is normal.
    async fn append(&self, session_id: &str, events: &[LoggedEvent]) -> Result<(), String>;

    /// Replay a session's events in order.
    async fn load(&self, session_id: &str) -> Result<Vec<LoggedEvent>, String>;

    /// Session ids this store holds, newest first where the backend can tell.
    async fn list(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// Where this session's events actually land, in whatever terms the backend
    /// uses — a path, a URL, a table name.
    ///
    /// The store answers because the store is the only thing that knows. A
    /// caller that recomputed the path from the same config would be a second
    /// copy of the rule, and the copy is what drifts.
    fn location(&self, _session_id: &str) -> Option<String> {
        None
    }
}

/// The `session` row's answer for the agent a front end creates on its own:
/// which id it gets and whether that id's stored log is replayed first. Read
/// by [`crate::agent::CreateAgent::root`]; an agent created any other way — a
/// delegated child, an ACP session — names its own.
#[derive(Clone, Debug, Default)]
pub struct SessionDefaults {
    pub id: Option<String>,
    pub resume: bool,
}

/// One stored session, as a list would show it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    /// `None` for a file written before headers existed.
    pub header: Option<crate::session::SessionHeader>,
    pub title: Option<String>,
    pub turns: usize,
    pub events: usize,
}

/// Where to cut the history, and what to leave in its place.
#[derive(Clone, Debug)]
pub struct CompactionDecision {
    /// Everything at or below this sequence number stops being model-visible.
    pub through: crate::session::SeqNo,
    /// What the model sees instead.
    pub summary: String,
}

/// History compaction. A seam because the right answer differs by deployment:
/// a model-free summary is cheap and always available, a model-written one is
/// better and costs a call, and a token-budget-aware pruner is different again.
#[async_trait]
pub trait Compaction: Send + Sync {
    fn describe(&self) -> String;
    /// `None` means "nothing worth compacting yet".
    async fn compact(&self, log: &crate::session::SessionLog) -> Option<CompactionDecision>;
}

/// Reconfiguring the tree while it runs.
///
/// `App::patch` can already replace any row without disturbing the rest — the
/// missing piece was a way to reach it from inside. A front end holds a
/// `Context`, not the `App`, so the host puts this in the tree and every front
/// end gets the same capability without knowing how the host is structured.
///
/// **Not reentrant.** A patch unloads and remounts fibers, so calling it from
/// inside a plugin's `apply` would deadlock; the implementation refuses rather
/// than hanging.
#[async_trait]
pub trait Control: Send + Sync {
    /// Apply a patch layer, given as TOML. Returns a description of what moved.
    async fn patch(&self, toml: &str) -> Result<String, String>;

    /// The running tree, as `--dump-config` prints it.
    async fn dump(&self) -> String;

    /// Check the running composition. Empty means consistent.
    async fn audit(&self) -> Vec<String>;

    /// Row ids currently in the tree, with whether each is enabled.
    async fn rows(&self) -> Vec<(String, String, bool)>;
}

/// Handing out a driver-protocol handle.
///
/// A seam rather than a return value, because whoever embeds this harness holds
/// a [`Context`], not the plugin — the same way every other front end resolves
/// what it needs from the tree. Its consumer is outside the tree by
/// construction; that is what makes it a handle.
pub trait AgentHandleSource: Send + Sync {
    /// The handle, once. A second caller gets `None`: two owners of one command
    /// channel is two drivers fighting over one conversation.
    fn take(&self) -> Option<atomcode_kernel::agent::AgentHandle>;
}

/// The front end: whoever drives agents and talks to a person.
///
/// A seam, so "run one prompt and exit", "a terminal session", "a web server"
/// and "an ACP endpoint" are four rows rather than four binaries. The runtime
/// hands it the context and gets out of the way; everything a front end needs —
/// the agent registry, the session log, the event stream — it resolves for
/// itself.
#[async_trait]
pub trait UserInterface: Send + Sync {
    fn describe(&self) -> String;

    /// Drive the interaction to completion.
    ///
    /// `initial` is whatever the launcher was given on the command line, which
    /// a one-shot front end treats as the whole job and an interactive one
    /// treats as the first message.
    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String>;
}

/// Naming a session. A seam because the cheap answer (the first prompt) and the
/// good answer (a model call) are different trade-offs, not different quality
/// levels of one implementation.
#[async_trait]
pub trait SessionTitle: Send + Sync {
    fn describe(&self) -> String;
    async fn title(&self, log: &SessionLog) -> Option<String>;
}

/// The three answers an approval can have. The spelling is
/// `atomcode_capabilities::tools::approval`'s, so a decision means the same
/// thing whichever gate asked and whatever carries it.
pub const ANSWER_ALLOW: &str = "allow";
pub const ANSWER_ALWAYS: &str = "allow_always";
pub const ANSWER_DENY: &str = "deny";

/// One answer: what comes back, and what a plain front end prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Returned by [`UserQuestions::ask`] when this one is picked.
    pub value: String,
    /// What a front end with nothing better to show prints. A front end that
    /// knows the answer's meaning is free to word it its own way.
    pub label: String,
}

impl Answer {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
        }
    }
    pub fn labelled(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// The call an approval is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AboutCall {
    pub tool: String,
    /// The exact bytes that will run. A front end summarises them for the eye,
    /// but this is what executes — approve what runs, not a paraphrase of it.
    pub arguments: String,
    /// What an `allow_always` would cover, or `None` when this call is not
    /// something to remember. Empty means every call of this tool; otherwise
    /// it is the tool's own scope — `bash` reports the command, so approving
    /// one destructive command never blanket-approves another. Shown, because
    /// a person saying "always" is owed the scope they are saying it to.
    pub grant: Option<String>,
}

/// A question put to a person — data, not a sentence.
///
/// A string was enough while one agent asked and the answers were yes and no.
/// It stopped being enough the moment a delegated member could ask: "allow
/// `write_file`?" with no way to say *who* wants to write is a question a
/// person cannot answer honestly. So everything a front end needs to lay a
/// question out is here, and everything it gets to decide — wording, colour,
/// which key means which answer — is not.
#[derive(Clone, Debug, Default)]
pub struct Question {
    /// The ask, phrased, for a front end that renders nothing else.
    pub prompt: String,
    /// The answers, in the order they should be offered.
    pub options: Vec<Answer>,
    /// Which agent is asking, when it is not the one the person is driving —
    /// a team member's name. `None` is this conversation itself.
    pub asker: Option<String>,
    /// The call under review, when this is an approval.
    pub about: Option<AboutCall>,
}

impl Question {
    /// A question with nothing behind it: a prompt and some answers.
    pub fn plain(prompt: impl Into<String>, options: &[&str]) -> Self {
        Self {
            prompt: prompt.into(),
            options: options.iter().map(|o| Answer::new(*o)).collect(),
            ..Self::default()
        }
    }
    /// Just the values, for an asker that only echoes them.
    pub fn values(&self) -> Vec<String> {
        self.options.iter().map(|o| o.value.clone()).collect()
    }
    /// The answer whose value is this, if it is one of them.
    pub fn has(&self, value: &str) -> bool {
        self.options.iter().any(|o| o.value == value)
    }
}

/// Asking a human. `None` means "no answer" — every caller must treat that as a
/// refusal, never as consent.
#[async_trait]
pub trait UserQuestions: Send + Sync {
    fn describe(&self) -> String;
    /// Put the question and wait. The returned string is an [`Answer::value`].
    async fn ask(&self, question: &Question) -> Option<String>;
}

/// What a delegated task produced.
#[derive(Clone, Debug)]
pub struct SubagentOutcome {
    pub text: String,
    pub rounds: u32,
    pub tool_calls: u32,
    pub stop: StopReason,
    pub error: Option<String>,
    /// Events the child's own log accumulated. Reported, not returned: the
    /// point of delegation is that the parent does not carry the transcript.
    pub transcript_len: usize,
}

impl SubagentOutcome {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            rounds: 0,
            tool_calls: 0,
            stop: StopReason::ProviderError,
            error: Some(message.into()),
            transcript_len: 0,
        }
    }

    /// What the parent model sees. The child's transcript stays in the child.
    pub fn report(&self) -> String {
        if let Some(error) = &self.error {
            return format!("Subagent failed: {error}");
        }
        format!(
            "{}\n\n[subagent: {} round(s), {} tool call(s), {:?}]",
            self.text, self.rounds, self.tool_calls, self.stop
        )
    }
}

/// Where a review or audit puts what it found.
///
/// A seam because the destination differs by deployment: a CLI prints, CI posts
/// to a pull request, a web front end renders, an eval counts.
pub trait Findings: Send + Sync {
    fn describe(&self) -> String;
    /// Everything reported so far.
    fn all(&self) -> Vec<atomcode_capabilities::tools::Finding>;
    /// Everything reported, clearing the sink.
    fn take(&self) -> Vec<atomcode_capabilities::tools::Finding>;
}

/// Delegation. A seam because "a child agent in this process", "a fork of this
/// session" and "another product entirely" are all legitimate answers behind
/// one interface.
#[async_trait]
pub trait Subagents: Send + Sync {
    fn describe(&self) -> String;
    async fn spawn(&self, task: &str, instructions: &str) -> SubagentOutcome;
}

/// The turn driver. A seam like any other: the shipped loop is one row in the
/// config tree, and a different loop (a plan-first driver, a replay harness, a
/// remote delegator) is a different row filling the same slot.
///
/// It drives an [`Agent`] rather than taking a prompt, because a turn is not a
/// function call: input arrives through the agent's inbox, and a message that
/// lands mid-turn belongs to the turn already running.
#[async_trait]
pub trait AgentLoop: Send + Sync {
    /// Run one turn for `agent`: open, claim, step until nothing is owed, close.
    /// Returns immediately with an empty turn when the inbox has nothing waking.
    async fn drive(&self, agent: &Agent) -> TurnOutcome;
}

/// Whether a tool call may run, asked before execution.
#[async_trait]
pub trait ApprovalPolicy: Send + Sync {
    async fn decide(
        &self,
        call: &atomcode_kernel::tool::ToolCall,
        tool: &Arc<dyn Tool>,
    ) -> Decision;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

#[derive(Clone, Debug, Default)]
pub struct TurnOutcome {
    pub text: String,
    /// Which turn this was, 1-based within the session.
    pub turn: u64,
    /// Steps taken. A step is one model request plus the tools it called.
    pub steps: u32,
    /// Kept as an alias for `steps` while callers migrate.
    pub rounds: u32,
    pub tool_calls: u32,
    pub stop: StopReason,
    pub error: Option<String>,
}

/// Why a turn ended.
///
/// Serialized by variant name, which is what `format!("{:?}")` produced when
/// the log stored a rendering of this instead of the value — so a session
/// written before it was typed still loads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StopReason {
    /// The model answered with no further tool calls.
    #[default]
    Stopped,
    /// The round budget ran out.
    MaxRounds,
    /// The provider failed.
    ProviderError,
    /// A `agent/turn-stopping` listener asked for the turn to end — a round
    /// budget, a deadline, a cost ceiling.
    StoppedByPolicy,
    /// The loop's own runaway fuse. Not a policy: the fuse exists so a tree with
    /// no stopping policy at all still terminates.
    RunawayFuse,
    /// A tool-loop guard saw no progress and ended the turn.
    ToolLoopDetected,
    /// The agent was asked to stop.
    Cancelled,
    /// A `agent/pre-step` listener rejected the claimed input, so no step ran.
    InputRejected,
    /// Something reached the model that the session log cannot explain. The
    /// turn is stopped rather than continued: a prompt nobody can reconstruct
    /// makes resume, fork and compaction unsound from here on.
    InvariantViolated,
}
