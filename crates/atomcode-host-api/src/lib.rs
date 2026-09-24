//! Host control: what a front end asks of whatever hosts its agents
//! (`docs/adr/0021` §2).
//!
//! The handle protocol ([`atomcode_kernel::event`]) speaks to one agent about its own
//! conversation. What is here is above any one agent: which session is live, and
//! settings a person expects to outlive the agent that carries them today. A
//! front end reaches it through [`HostControl`] and never learns who the host is
//! — a coding runtime, a daemon, a test.
//!
//! Three rules shape every item:
//!
//! - **The payload is an intent, not an implementation.** "Resume that session",
//!   never a configuration or a conversation to rebuild one from.
//! - **No session model leaks through.** Nothing here says how a host replaces a
//!   session. A front end sees a session's identity change, and that is all.
//! - **Commands are addressed** (`docs/adr/0021` §9). One that acts on the live
//!   session names the session the caller is looking at; if the host has moved
//!   on to another, the command is refused rather than applied to a session the
//!   caller never saw.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use atomcode_kernel::provider::ReasoningEffort;
use atomcode_kernel::session::SeqNo;

/// What a front end asks the host to do.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostCommand {
    /// Put an empty session in place of `session`, in the same working
    /// directory.
    NewSession { session: String },
    /// Put the stored session `target` in place of `session`.
    Resume { session: String, target: String },
    /// The thinking level `session`'s requests carry from now on. `None` is no
    /// opinion: the endpoint's own default stands.
    SetReasoningEffort {
        session: String,
        level: Option<ReasoningEffort>,
    },
    /// The stored sessions a person could resume, newest first — those of one
    /// working directory, or all of them.
    ListSessions { working_dir: Option<String> },
    /// The last few exchanges of a stored session, for someone deciding whether
    /// to come back to it. Answered from the log the session already keeps —
    /// nothing is opened or resumed by asking.
    PreviewSession { session: String },
    /// Throw a stored session away, with whatever was delegated from it.
    ///
    /// Not addressed at a live session the way the rest are: the argument is a
    /// session on disk, which is usually *not* the one running. The host
    /// refuses the one that is — a person cannot mean "delete the conversation
    /// I am having", and a half-deleted live session is the worst of both.
    DeleteSession { session: String },
    /// Take the conversation back to before the person's message that opened
    /// `turn` — the last one they sent, when none is named — and hand that
    /// message back (`docs/adr/0024` §17). `based_on` is the last fact the
    /// caller saw: a message or a turn since then makes it `Stale`.
    Undo {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        based_on: SeqNo,
    },
    /// The turns a rewind can go back to, newest first.
    RewindPoints { session: String },
    /// Take back what `turn` and everything after it did: the conversation,
    /// the workspace, or both. Restoring a snapshot is this, over the
    /// conversation.
    Rewind {
        session: String,
        turn: u64,
        scope: atomcode_kernel::session::RewindScope,
        based_on: SeqNo,
    },
    /// The model `session`'s requests go to from now on, by the id a person
    /// picks it by.
    SwitchModel { session: String, model: String },
    /// How much `session` may do without asking, from now on
    /// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A1).
    SetMode { session: String, mode: Mode },
    /// How much `session` may do without asking, as it stands now.
    ///
    /// [`HostCommand::SetMode`]'s reading half, and it is not redundant with
    /// [`HostEvent::ModeChanged`]: the mode can be set before a front end has
    /// subscribed — a host's own `--dangerously-skip-permissions` seeds it at
    /// startup — and an event pushed to nobody is an event nobody heard. So a
    /// front end that draws the mode asks once and follows the events from
    /// then on, which is the bargain [`HostCommand::Readiness`] and
    /// [`HostCommand::Autonomy`] already strike.
    Mode { session: String },
    /// Work in `directory` from now on. A new conversation, because what a
    /// session read and wrote belongs to where it ran (A2).
    ChangeDirectory { session: String, directory: String },
    /// The settings a person may change, with what each is set to now
    /// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A3).
    ///
    /// The host's own configuration, not the running row graph: this is the
    /// file a person edits, and it is deliberately **not** the development
    /// commands 0022 §7 removed — those patched live rows and could turn
    /// approval off under a running turn.
    Settings { session: String },
    /// Set one of them. What it takes effect on is the setting's own business:
    /// some apply now, some at the next start.
    SetSetting {
        session: String,
        id: String,
        value: String,
    },
    /// Put one setting back to what this build would do if nobody had said.
    ///
    /// A separate command from `SetSetting` with the default's value, because
    /// the two write different things: this **removes** the key, so the setting
    /// follows the build from then on, while writing today's default pins it to
    /// a value that stops following. A person who asks for "default" means the
    /// first.
    ResetSetting { session: String, id: String },
    /// The models a person may pick from, as the host resolves them now.
    ///
    /// The catalog is the host's: only it knows what is configured, and a
    /// screen that had to be told a model id to switch to could only offer
    /// typing it out (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
    /// A12).
    Models { session: String },
    /// 这一会话花了多少 token,**按哪个模型花的分开**。
    ///
    /// 和 `Usage` 不同:那一条问的是账号还剩多少额度,这一条问的是
    /// **这一段对话自己**花了多少 —— 跨账号、离线也答得出来,因为它就在
    /// 会话自己的记录里。按模型分开是重点:一段对话中途换过模型时,
    /// 合起来的总数什么也回答不了。
    Cost { session: String },
    /// Name `session`. What a person calls a conversation when the title it
    /// took from its first message is not what it turned out to be about (A4).
    Rename { session: String, title: String },
    /// The MCP servers `session` was given, and how each one is.
    McpStatus { session: String },
    /// The tools one MCP server put on `session`'s model (A11).
    McpTools { session: String, server: String },
    /// Every configured MCP server for `session`, **disabled ones included**,
    /// with what a management screen groups and counts by
    /// (`docs/mcp-panel-design.md` §4.1). Distinct from `McpStatus`, which
    /// reports only what the running session actually has.
    McpManage { session: String },
    /// One configured server in full, for the detail page. `server` is the
    /// configured key, not a tool name.
    McpDetail { session: String, server: String },
    /// Do one thing to one configured MCP server (`docs/mcp-panel-design.md`
    /// §4.1).
    ///
    /// Answers with the refreshed list rather than this one server, because half
    /// of these change the whole project's picture — trust is project-wide. A
    /// caller that stays on a detail page re-reads that server with `McpDetail`.
    McpAct {
        session: String,
        server: String,
        action: McpAction,
    },
    /// Everything in `session`'s tool catalog and what is true of each: on, off
    /// because the person said so, or absent because the tree was configured
    /// without it (`docs/tool-catalog-policy.md`).
    ToolCatalog { session: String },
    /// Turn one tool off or back on for `session`. `pattern` is a tool name or
    /// a glob — `mcp__github__*` is one server's tools — so hiding a whole MCP
    /// server and hiding one of its tools are the same command. The connection
    /// is untouched either way.
    SwitchTool {
        session: String,
        pattern: String,
        on: bool,
    },
    /// Take every MCP tool off `session`'s model now — what has to happen
    /// before anything changes which servers are trusted.
    WithdrawMcpTools { session: String },
    /// Read skills, MCP servers and configuration again, for the same session.
    Reload { session: String },
    /// Take `session`'s credentials out of the process. The session stays.
    SignOut { session: String },
    /// Sign `session` back in, with the credentials configured now.
    SignIn { session: String },
    /// Who is signed in, as the host knows it.
    WhoAmI { session: String },
    /// The files this session was configured from, and whether each was there.
    ///
    /// Neutral by the rule this contract is kept to: every host reads *some*
    /// set of files to make a session what it is, and "which ones, and did you
    /// find them" is the question a person asks when the agent is not behaving
    /// the way their files say it should. What the groups are called is the
    /// host's — only it knows what its files mean.
    ///
    /// A file that is **not** there is still reported. That is the answer the
    /// question is usually asked for: "my instructions are being ignored" is
    /// almost always "that file is not where you think it is".
    Sources { session: String },
    /// Whether the session is driving itself — a goal or a loop — and how far
    /// it has got.
    ///
    /// Neutral by the rule this contract is kept to: a host that runs an agent
    /// unattended cares whether it is mid-goal, whatever it is an agent *of*.
    Autonomy { session: String },
    /// The providers this host is configured with.
    ///
    /// Switching to one is [`HostCommand::SwitchModel`] with its id — a provider
    /// and a model are resolved by the same call, so there is one switch rather
    /// than two that must agree.
    Providers { session: String },
    /// What `session` has changed in the workspace. `file` asks for that one
    /// file's diff instead of the list.
    ///
    /// Two levels in one command because they are one question asked at two
    /// depths, and a front end that showed the list would otherwise have to
    /// know a second command's name to open a row of it.
    Changes {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        /// Which two things to compare. Omitted is [`ChangeScope::Session`],
        /// so a front end that predates this asks what it always asked.
        #[serde(default, skip_serializing_if = "ChangeScope::is_default")]
        scope: ChangeScope,
    },
    /// How much of the model's context this session is using.
    ///
    /// The budget, not the bill: [`HostCommand::Usage`] says what the account
    /// may still do, this says how close this conversation is to the window it
    /// has to fit in. A front end that could not ask this could only count what
    /// it had seen, which is not the same number — the host packs a system
    /// prompt, instructions and tool definitions the screen never sees.
    Context { session: String },
    /// What this person has typed into this project before, newest first.
    ///
    /// **Folded from the sessions' own logs, not a second store.** A session's
    /// conversation is its event log (`docs/adr/0024`), and what was typed is
    /// part of it — so the history a composer arrows back through is a *read*
    /// over those logs rather than a file kept alongside them. A separate
    /// file is what the other front end kept, and it drifts the moment a turn
    /// is undone: the words stay in the history after the conversation stops
    /// having them.
    ///
    /// Scoped to the project the session works in, which is how the sessions
    /// are stored anyway. `limit` caps what comes back, newest first.
    History { session: String, limit: u32 },
    /// What the account has left to spend, as rolling windows.
    ///
    /// Separate from the token counts a turn reports: those say what this
    /// conversation cost, this says what the account may still do and when a
    /// spent window comes back. A host that meters nothing answers with an
    /// empty list — which is an answer, not a failure.
    Usage {
        session: String,
        /// Ask for the windows alone.
        ///
        /// **The cheap form: one call on the account instead of three.** The
        /// full answer carries the plan behind the windows and what has been
        /// spent, and each is a separate round trip — right for a page a person
        /// opened, wrong for something asked on a timer. A periodic check that
        /// wanted only "how much is left" would otherwise triple the cost of
        /// asking.
        ///
        /// Defaults to false, so a caller that does not know about this gets
        /// what it always got.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        windows_only: bool,
    },
    /// Whether `session`'s requests carry thinking at all, as a setting to read.
    Thinking { session: String },
    /// Turn thinking on or off for `session` from now on.
    ///
    /// A separate knob from [`HostCommand::SetReasoningEffort`] on purpose: one
    /// says whether the model thinks before it answers, the other how hard. A
    /// host whose models have no such switch may refuse it.
    SetThinking { session: String, on: bool },
    /// Whether a turn would be accepted right now, and what to do if not.
    ///
    /// Asked before anything is typed, which is the whole point: a front end
    /// that only learns a provider is missing by submitting a turn tells the
    /// person after they have written one. What the old driver protocol did
    /// with three separate pre-flight checks (`is_stopped`,
    /// `provider_unavailable_reason`, `accepts`), the contract does with one
    /// question — and unlike those, the answer carries what to do about it.
    Readiness { session: String },
}

impl HostCommand {
    /// The live session this command acts on, if it acts on one.
    pub fn addressed(&self) -> Option<&str> {
        match self {
            Self::NewSession { session }
            | Self::Resume { session, .. }
            | Self::SetReasoningEffort { session, .. }
            | Self::Undo { session, .. }
            | Self::RewindPoints { session }
            | Self::Rewind { session, .. }
            | Self::SwitchModel { session, .. }
            | Self::Settings { session }
            | Self::SetSetting { session, .. }
            | Self::ResetSetting { session, .. }
            | Self::SetMode { session, .. }
            | Self::Mode { session }
            | Self::ChangeDirectory { session, .. }
            | Self::Models { session }
            | Self::Cost { session }
            | Self::Rename { session, .. }
            | Self::McpStatus { session }
            | Self::McpTools { session, .. }
            | Self::McpManage { session }
            | Self::McpDetail { session, .. }
            | Self::McpAct { session, .. }
            | Self::ToolCatalog { session }
            | Self::SwitchTool { session, .. }
            | Self::WithdrawMcpTools { session }
            | Self::Reload { session }
            | Self::SignOut { session }
            | Self::SignIn { session }
            | Self::WhoAmI { session }
            | Self::Sources { session }
            | Self::Changes { session, .. }
            | Self::Providers { session }
            | Self::Autonomy { session }
            | Self::Context { session }
            | Self::History { session, .. }
            | Self::Usage { session, .. }
            | Self::Thinking { session }
            | Self::SetThinking { session, .. }
            | Self::Readiness { session } => Some(session),
            // Addressed at a stored session, not the live one — see the
            // variant's own note.
            Self::ListSessions { .. }
            | Self::DeleteSession { .. }
            | Self::PreviewSession { .. } => None,
        }
    }
}

/// What a command that went through produced.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostReply {
    /// Done. The live session is the one it was.
    Done,
    /// Done — and something about it is worth saying.
    ///
    /// For a command whose live half went through while a durable half did
    /// not: switching the model takes effect at once and is also written to
    /// the configuration, and the write can fail on its own. Refusing would be
    /// a lie in one direction (the switch *did* happen) and a bare `Done` is a
    /// lie in the other (the next start will not remember it) — which is how
    /// "it reverts to the old model on restart" reached a person as silence.
    ///
    /// `note` is the host's own words, shown as they stand.
    DoneWithNote {
        note: String,
    },
    /// The live session is now `session`. A front end drops the stream of the
    /// one it replaced and follows this one (`docs/adr/0022` §6).
    SessionChanged {
        session: String,
    },
    Sessions {
        sessions: Vec<StoredSession>,
    },
    /// What a stored session last talked about: a few lines, newest last,
    /// already in the order a person reads them.
    SessionPreview {
        lines: Vec<String>,
    },
    /// An undo or a rewind went through. `prompt` is the person's message the
    /// conversation went back to before — for where they type, to edit and
    /// send again; `restored_files` the workspace files put back.
    Undone {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        restored_files: Vec<String>,
    },
    RewindPoints {
        points: Vec<RewindPoint>,
        /// Why the workspace cannot be rewound here, when it cannot: only the
        /// conversation can.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code_unavailable: Option<CodeUnavailable>,
    },
    McpServers {
        servers: Vec<McpServer>,
    },
    /// The tools one MCP server put on the model, by the names the model calls
    /// them by.
    McpTools {
        tools: Vec<String>,
    },
    McpRows {
        rows: Vec<McpRow>,
    },
    McpDetail {
        detail: McpServerDetail,
    },
    /// The tool catalog, as a screen offering the switch needs it.
    ToolCatalog {
        tools: Vec<CatalogTool>,
    },
    /// The settings a person may change, each with what it is set to now.
    Settings {
        settings: Vec<Setting>,
    },
    /// 每个模型一行,加上归不了属的那一块。
    Cost {
        models: Vec<ModelCost>,
        /// 没能归到哪个模型名下的 token。单列而不是摄进某一行:推给
        /// 任一个模型都是编的。
        #[serde(default)]
        unattributed: u64,
    },
    /// What a person may switch to — the host's own catalog. `current` is the
    /// one this conversation runs on, when the host knows it.
    Models {
        models: Vec<ModelChoice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<String>,
    },
    /// What the session is doing on its own, if anything. `None` is idle.
    Autonomy {
        running: Option<Running>,
    },
    /// How much the session may do without asking, as the host found it.
    ///
    /// `None` is "this host cannot say", and it is a different answer from
    /// [`Mode::Ask`] — a host that governs no execution mode at all (one whose
    /// tree carries no approval rows) has no mode rather than the most careful
    /// one, and a front end that drew `ask` for it would be reporting a policy
    /// nobody configured. Same distinction [`HostReply::Autonomy`] keeps
    /// between `None` and a stopped goal.
    Mode {
        mode: Option<Mode>,
    },
    /// How much of the window this session occupies.
    Context {
        /// Tokens the model may take in one request, as the host resolves it
        /// for the model in use. `0` when the host does not know.
        window: u32,
        /// Tokens this session would send now.
        used: u32,
        /// The model the window belongs to — the two travel together because a
        /// window without its model is a number a person cannot act on.
        model: String,
        /// Where the session works. Part of the same answer because "what am I
        /// carrying" and "what am I carrying it over" are asked together.
        working_dir: String,
    },
    /// What was typed into this project before, newest first and de-duplicated.
    History {
        entries: Vec<String>,
    },
    /// What the account has left, window by window.
    ///
    /// Still best effort: a meter that is slow or down does not make this
    /// fail, because `/usage` would then be the one command that breaks when
    /// the network hiccups. But the two empty answers are **not** the same
    /// answer, and `unavailable` is which one it was — see its own note.
    Usage {
        /// Empty with no `unavailable` means a host that meters nothing.
        windows: Vec<UsageWindow>,
        /// Why there is no answer, when there is none.
        ///
        /// **「不计额度」和「问不到」必须读起来不一样。** Both arrive as an
        /// empty list, and drawing the second as the first tells a person the
        /// opposite of the truth: someone who is being held back BY the
        /// allowance reads "this host does not count one" and goes looking
        /// somewhere else for the reason. Same shape as `WorkspaceChanges`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unavailable: Option<String>,
        /// What the account is subscribed to, when the host has a notion of it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<Entitlement>,
        /// What went through, when the host meters it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<UsageStats>,
    },
    /// The files the session was configured from, in the host's own grouping.
    Sources {
        groups: Vec<SourceGroup>,
    },
    /// The providers a person may switch between. `current` is the one this
    /// conversation runs on, when the host knows it.
    Providers {
        providers: Vec<ProviderChoice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<String>,
    },
    /// What a session has done to the workspace.
    ///
    /// `unavailable` is why there is no answer, when there is none — a session
    /// with no workspace checkpointing is an ordinary session, and "nothing
    /// changed" and "cannot tell" must read differently on screen.
    Changes {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<ChangedFile>,
        /// The one file's unified diff, when one was asked for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unavailable: Option<String>,
    },
    /// Who is signed in.
    ///
    /// `signed_in: false` is an answer, not a failure — a build that runs on a
    /// key in a file has nobody signed in and works fine. Never a credential:
    /// what comes back is what a person would put on a name badge.
    Identity {
        signed_in: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        who: Option<String>,
        /// Anything worth showing beside the name — an email, an organisation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// Where the host keeps the thing that says so, if it keeps it
        /// anywhere.
        ///
        /// A path, never its contents. The question it answers is the one
        /// asked after "who am I" comes back wrong — which file do I delete,
        /// which one did I copy to the other machine — and a host that signs
        /// in some other way leaves it out rather than inventing one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stored_at: Option<String>,
    },
    /// The answer to [`HostCommand::Readiness`].
    Readiness {
        /// Whether a turn submitted now would be taken.
        ready: bool,
        /// Why not, in words the screen shows as they stand.
        ///
        /// The host's own words because only the host knows what went wrong;
        /// a front end that phrased this itself would be guessing at a set of
        /// causes it does not have.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
        /// A command the screen can run to put it right, without the leading
        /// slash.
        ///
        /// The host names it rather than describing it, because what puts it
        /// right is the host's own command — this front end dispatches it the
        /// way it dispatches a typed one, and a host with nothing to offer
        /// says `None` rather than a name that does nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fix: Option<String>,
    },
}

/// A session driving itself: what it is working towards, and how far it has got.
///
/// One shape for a goal and a loop, because a front end draws them the same way
/// and the difference is a word. `of` is how many rounds it may take at most,
/// when there is a cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    /// `goal` or `loop`. A word rather than an enum: a host with a third kind
    /// of autonomy should not need this contract changed to say so.
    pub kind: String,
    /// The condition being worked towards, or the prompt being repeated.
    pub what: String,
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<u32>,
    /// Seconds since it started. A duration rather than a start time: the
    /// screen and the host need not agree about what time it is
    /// (`docs/adr/0008`).
    pub elapsed_secs: u64,
    /// Why it is not running right now, when it is registered but paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
}

/// One rolling window of an account's allowance.
///
/// Phrased as a person reads it rather than as a provider bills it: a name, an
/// exhausted flag, and how long until it comes back. No money and no
/// percentages — what a front end needs to say "you are out until 14:30", and
/// nothing a host would have to invent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// What the window is called, in the person's own terms — "5 小时", "每周".
    pub label: String,
    /// Nothing left in it right now.
    pub exhausted: bool,
    /// When it comes back, as the host words it. Empty when it is not waiting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resets_at: String,
    /// Seconds until then, so a screen can count down without agreeing with the
    /// host about what time it is (`docs/adr/0008`). `0` when nothing is waiting.
    pub resets_in_seconds: i64,
    /// How long the whole rolling window is, in seconds. `0` when the host does
    /// not know.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub window_seconds: i64,
    /// How much of the allowance is gone, as whole percent (0..=100). `None`
    /// when the host does not know — and a screen must then say nothing rather
    /// than draw an empty bar, which reads as "none used".
    ///
    /// Whole percent rather than a float: this is a number a person reads off a
    /// bar, the extra digits are noise, and a wire type that can be compared
    /// for equality is worth more here than the last decimal.
    ///
    /// This is the number a bar on this page is *of*. It comes from the account
    /// service, which is the only thing that counts requests; a front end that
    /// derived one from the reset countdown would be drawing time and calling
    /// it allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<u8>,
    /// How many requests of the window are gone, when the host knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls_used: Option<i64>,
    /// How many model requests the window allows, when the host knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_limit: Option<i64>,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

/// What an account has spent, as the service that meters it counts.
///
/// Separate from [`UsageWindow`]: a window is an allowance and its reset, this
/// is what went through. A front end that shows both shows them on one page,
/// which is why one reply carries both.
///
/// The day series is carried rather than summarised, because summarising it
/// would fix the shape of a chart in a crate that draws nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageStats {
    /// The span, as the service words it (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
    /// Per model, biggest first.
    pub models: Vec<ModelUse>,
    /// One per day, oldest first.
    pub daily: Vec<DayUse>,
    /// The same days again, split by model — one entry per model in `models`
    /// and in the same order, each as long as `daily`.
    ///
    /// Separate from `daily` rather than nested inside it because a chart reads
    /// it the other way round: one line per model across every day, not one day
    /// at a time. Empty when the service does not break the days down, which is
    /// a thing a reader can be told rather than guessed at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<ModelSeries>,
    pub total_tokens: u64,
    pub total_requests: u64,
}

/// One group of configuration files — what the host calls them, and what is in
/// it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceGroup {
    pub label: String,
    pub files: Vec<SourceFile>,
}

/// One file the host reads, and whether it found it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    /// What this one is for, in the host's words — the scope, usually.
    pub label: String,
    pub path: String,
    pub present: bool,
}

/// What the account is subscribed to.
///
/// Separate from the windows because the two run on different clocks and answer
/// different questions: a window resets in minutes, a plan in years. A screen
/// with only the windows can say "you are throttled now" and never "your plan
/// runs out next week", which is the one of the two a person can act on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entitlement {
    pub plan: String,
    /// Whether the service still honours it. An expired plan is still reported:
    /// a person whose plan lapsed needs to be told, and saying nothing looks
    /// like never having had one.
    pub active: bool,
    /// `YYYY-MM-DD`, or empty when the service did not say — a claim that was
    /// never activated has no date, which is not day zero.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub claimed_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expires_at: String,
    pub remaining_days: i32,
    pub total_days: i32,
}

/// One model's day-by-day tokens, aligned with [`UsageStats::daily`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSeries {
    pub name: String,
    pub daily: Vec<u64>,
}

/// One model's share of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUse {
    pub name: String,
    pub tokens: u64,
    pub requests: u64,
}

/// One day of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayUse {
    /// `YYYY-MM-DD`.
    pub date: String,
    pub tokens: u64,
    pub requests: u64,
}

/// One provider a person may switch to.
///
/// `about` is what it is — its kind and its model — and **never a credential**.
/// A provider entry in a configuration file carries an `api_key`; this type is
/// what a screen prints and a log keeps, so the key has no field to travel in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderChoice {
    pub id: String,
    pub about: String,
}

/// What a request for changes is asking about.
///
/// **One command with a scope, not two commands.** Both are "what has changed
/// here" — they differ in what "here" is, and a front end showing one has to be
/// able to offer the other without learning a second command's name
/// (`docs/adr/0021`: a question asked at two depths is one question).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeScope {
    /// What this session did: the workspace as it was before its first prompt,
    /// against now. The default, because in a coding session it is the more
    /// frequently useful of the two — "what did this agent touch".
    ///
    /// A session with no workspace checkpointing cannot answer it, and says so
    /// through `unavailable`.
    #[default]
    Session,
    /// What the workspace has that the host has not taken in: everything
    /// outstanding, whoever did it and whenever — including work done before
    /// this session opened and work a person did by hand.
    ///
    /// Named for the workspace rather than for whatever keeps it, because a
    /// host that is not driving a repository can still be asked this and can
    /// still answer "I cannot tell" — which is the rule this whole contract is
    /// held to (`tests/contract.rs`). What a front end *calls* it is the front
    /// end's own business; the screen says `/diff git`.
    ///
    /// **It needs no session history**, which is why `unavailable` for "this
    /// session keeps no snapshots" belongs to [`Session`](Self::Session) alone.
    Workspace,
}

impl ChangeScope {
    fn is_default(&self) -> bool {
        matches!(self, Self::Session)
    }
}

/// What happened to a file, in the words a person uses rather than git's
/// letters.
///
/// `Other` rather than an error for a letter this build does not know: a future
/// git must be able to add one without making the whole listing refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    /// Not in the index: a file git has never been told about.
    Untracked,
    Conflicted,
    Other,
}

/// One file a session — or the checkout — changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Relative to the working directory.
    pub path: String,
    pub added: u64,
    pub removed: u64,
    /// A file with no line counts to give. Shown as changed, not as `+0 -0`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// What happened to it, when the answer knows. `None` from a scope that
    /// only counts lines — the session's own diff is a comparison of two trees
    /// and has no index to ask about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<FileChange>,
    /// Whether any of it is in the index. The two sections a listing is split
    /// into, and the difference between what a commit would take and what it
    /// would leave behind.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub staged: bool,
}

/// How much an agent may do before it asks.
///
/// Four steps, from "read, do not touch" to "do not ask at all". Named by what
/// a person means rather than by a host's internals: a front end offers these
/// four and the host maps them onto whatever it calls them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Explore and plan; writes and commands are refused, not asked about.
    Plan,
    /// Ask before anything that changes the workspace or runs a command.
    #[default]
    Ask,
    /// Edits go through; commands still ask.
    AcceptEdits,
    /// Nothing asks. For a sandbox, an eval, a CI run.
    Auto,
}

impl Mode {
    /// The next one along, for a key that cycles rather than names.
    ///
    /// The order is the reference front end's: ask → accept edits → auto → plan
    /// → ask. It runs from the most ordinary mode to the most permissive and
    /// then to the most careful, so a person stepping through it passes the
    /// dangerous one on the way rather than landing on it from a screen they
    /// were reading.
    ///
    /// Here rather than in a front end because it is a fact about these four
    /// modes and not about any one screen: two front ends stepping in two
    /// orders would be two products.
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::AcceptEdits,
            Self::AcceptEdits => Self::Auto,
            Self::Auto => Self::Plan,
            Self::Plan => Self::Ask,
        }
    }
}

/// One setting a person may change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    /// What `SetSetting` takes.
    pub id: String,
    /// What it is called, in the person's own language.
    pub label: String,
    /// What it is set to now.
    pub value: String,
    /// What it accepts, phrased for a person: `true | false`, a list of words,
    /// a range. Empty when anything goes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accepts: String,
    /// When a change takes effect, in a person's terms — "now", "next start".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub applies: String,
}

/// 一个模型在这一会话里花掉的 token。
///
/// `account` 而不是原始的选择 id:同一个账号下的好几个选择折成一个名字,
/// 而那是人认得出来的那个。宿主解析,屏幕原样画。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCost {
    pub account: String,
    pub model: String,
    /// 发出去的,含命中缓存的那部分。
    pub prompt: u64,
    /// 收回来的。
    pub completion: u64,
    /// 上面那个 `prompt` 里命中缓存的那部分 —— 不是另一笔。
    pub cached: u64,
}

/// One model a person can pick, as the host lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// What [`HostCommand::SwitchModel`] takes — the id a person picks it by.
    pub id: String,
    /// What to show beside the id: the provider, the context window, whatever
    /// the host thinks tells two of them apart. Empty when there is nothing to
    /// add.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub about: String,
}

/// A turn a rewind can go back to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindPoint {
    pub turn: u64,
    /// The person's message that opened it, shortened for a list.
    pub prompt: String,
    /// What it changed in the workspace, one entry per file.
    ///
    /// The files themselves and not a count: a screen offering the turn has to
    /// answer "is this the one?", and `rewind.rs +484` answers it while
    /// `3 files` does not. The count is `changes.len()`, so carrying both would
    /// be two truths about one thing. [`ChangedFile`] is the same type `/diff`
    /// hands back — one contract, one notion of "a file a turn touched".
    #[serde(default)]
    pub changes: Vec<ChangedFile>,
    /// Whether the workspace can be put back to before it.
    #[serde(default)]
    pub code: bool,
}

/// Why the workspace half of a rewind is not on offer.
///
/// **A kind, not a sentence.** A front end draws this for a person, in that
/// person's language — so what travels is which case it is, and the host's own
/// words only where they are a fact about this machine (a checkpoint that
/// failed, and why). It arrived here as an English sentence once, which left
/// every screen able only to pass it through untranslated.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeUnavailable {
    /// This build keeps workspace checkpoints off by default, to protect disk
    /// space. The person can turn them on.
    NotEnabled,
    /// The session is not written down, so there is nothing to checkpoint
    /// against.
    NoSession,
    /// Turned on, but the checkpoint could not be set up — with the cause.
    Failed { message: String },
}

/// One name in the tool catalog, as a screen offering the switch needs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogTool {
    pub name: String,
    /// The row that offered it, empty when whoever registered it did not say.
    /// A person deciding whether to turn `read_file` off wants to know which
    /// world it reads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    pub state: ToolState,
}

/// Why a tool is or is not on offer to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolState {
    On,
    /// A person turned it off for this session and can put it back.
    OffInSession,
    /// The tree was configured without it: only editing that changes it, so a
    /// screen offers no switch here.
    ExcludedByConfig,
}

/// One MCP server, as a status list shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub state: McpServerState,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerState {
    Connecting,
    Connected,
    /// Not started: the project is not trusted.
    Untrusted,
    /// HTTP with OAuth auth, and no usable token stored for this server. Derived
    /// by the runtime from the token store, not reported by the connection: the
    /// connection is never attempted without credentials to try.
    NeedsAuthentication,
    /// `disabled: true` in the file that defines it. The server is not started
    /// and is not in the running session's catalog — it is listed so the switch
    /// back on is reachable (`crates/atomcode-capabilities/src/mcp/config.rs:212`
    /// filters these out of the runtime's own read).
    Disabled,
    Failed {
        message: String,
    },
    Disconnected,
}

/// How a server is reached, with nothing that could authenticate as anyone.
///
/// Deliberately not `atomcode_capabilities::mcp::McpTransportConfig`: that type's
/// payload carries headers and OAuth material, and a screen showing "this one is
/// an HTTP server" has no business holding them.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
}

/// Whether a server authenticates, and whether it currently can.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpAuth {
    /// The transport authenticates by nothing the person manages here.
    None,
    OAuth {
        /// A usable token is stored for this server.
        authenticated: bool,
    },
}

/// One row of the `/mcp` list, as a screen draws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRow {
    pub name: String,
    pub state: McpServerState,
    /// `McpConfigSource::as_str()` — `"global"`, `"project"` or `"driver"`.
    /// A plain string rather than the enum: the capability type is not this
    /// crate's to publish, and a screen only groups by it.
    pub source: String,
    /// Tools this server has on the session's model, by the names the model
    /// calls them by. Zero for a server that is disabled or not connected.
    pub tool_count: usize,
    /// The file it is defined in, when it is backed by one. `None` for a
    /// driver-supplied server, which never had a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
}

/// Everything the `/mcp` detail page shows about one server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDetail {
    pub name: String,
    pub state: McpServerState,
    /// See [`McpRow::source`].
    pub source: String,
    pub transport: McpTransport,
    pub auth: McpAuth,
    pub tool_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
}

/// What a person can do to one MCP server from the management panel.
///
/// Signing in is not a host command. It opens a browser and writes a token —
/// nothing a running session owns — so a front end runs it on its own side and
/// then asks for [`HostCommand::Reload`], the way a provider's browser
/// authorisation is done; a host command for it would have to hold a reply
/// open for as long as a person takes in a browser.
///
/// `Enable` and `Disable` are named from the person's point of view, not the
/// file's: **`Disable` writes `disabled: true`, `Enable` removes that key.**
/// Getting this backwards silently inverts every switch in the panel, so the
/// mapping is spelled out here once.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpAction {
    /// Trust this project, so its `.mcp.json` servers may connect at all.
    Trust,
    /// Withdraw that trust. The project's tools come off the session first.
    Untrust,
    /// Forget the stored token for this server. Its tools come off first.
    Logout,
    /// Let this server run again: remove `disabled` from the file that defines it.
    Enable,
    /// Switch this server off: write `disabled: true` into that file.
    Disable,
}

/// One stored session, as a picker shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Unix milliseconds.
    #[serde(default)]
    pub created_at: u64,
    /// Unix milliseconds.
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub turns: u32,
    /// Written by a newer build than the host's: listed so a person knows it is
    /// there, refused if they try to resume it (`docs/adr/0024` §16).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_newer_version: bool,
}

/// Something that happened on the host, whoever caused it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostEvent {
    /// The live session was replaced — at this front end's request, another's,
    /// or the host's own.
    SessionChanged {
        session: String,
        previous: Option<String>,
    },
    /// A turn finished but its record could not be written.
    ///
    /// Separate from the turn's own completion because they are separate
    /// things: the turn did finish, and it has a reason for finishing. What
    /// failed is keeping it. The log is the only authority a session has
    /// (`docs/adr/0024`), so "the authority did not get it" is exactly the kind
    /// of thing a person must be told rather than left to discover later.
    ///
    /// **Not a fact in the log**, for the reason it exists: writing to the log
    /// is what just failed. A fact about the failure would need the same write.
    ///
    /// On `HostEvent` rather than the handle protocol because persistence is
    /// the host's job — it owns the store — and the kernel stays as it is.
    PersistenceFailed {
        session: String,
        /// What went wrong, for a person.
        message: String,
    },
    /// What the session is doing on its own changed — a round finished, a goal
    /// ended, a loop paused.
    ///
    /// The same payload [`HostReply::Autonomy`] answers with, so a status line
    /// that follows this and a `/autonomy` that asks cannot end up saying
    /// different things. `None` is "not any more".
    Autonomy {
        session: String,
        running: Option<Running>,
    },
    /// How much the session may do without asking changed — at this front end's
    /// request ([`HostCommand::SetMode`]), another's, or a host's own startup
    /// flag.
    ///
    /// The mode is session state rather than a fact in the log: the log records
    /// what happened, and a run under one mode looks the same as a run under
    /// another. So a screen that had to fold it out of the conversation could
    /// not draw it, and this is the road it travels instead — the same bargain
    /// [`HostEvent::Autonomy`] strikes, and for the same reason.
    ModeChanged { session: String, mode: Mode },
}

/// Why a host refused or failed a command (`docs/adr/0021` §8).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostError {
    /// Not now: something the host is doing has to finish first.
    Busy {
        reason: String,
    },
    /// Stopped before it took effect.
    Cancelled,
    /// Another host holds that session.
    SessionInUse {
        id: String,
    },
    /// The host cannot take commands now, or at all any more.
    Unavailable,
    /// There is no model to talk to.
    ProviderUnavailable {
        reason: ProviderUnavailableReason,
    },
    /// Tried and failed. The message is for a person.
    Failed {
        message: String,
    },
    InvalidWorkingDirectory {
        message: String,
    },
    UndoOutOfRange {
        requested: usize,
        available: usize,
    },
    RewindPointNotFound {
        turn: u64,
    },
    CodeRewindUnavailable {
        message: String,
    },
    /// The conversation moved on since the fact the command was based on.
    Stale {
        current: SeqNo,
    },
    /// Nothing by that id here: the addressed session is no longer live, or
    /// the one asked for does not exist.
    NotFound,
}

/// Why there is no model to talk to.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderUnavailableReason {
    NotConfigured,
    AuthenticationRequired,
    UnsupportedBuild,
}

/// The host, as a front end holds it. Filled by the host; the service key that
/// carries it is declared by whoever consumes it (`docs/adr/0021` §6).
#[async_trait]
pub trait HostControl: Send + Sync {
    async fn call(&self, command: HostCommand) -> Result<HostReply, HostError>;
    /// Every event from now on.
    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent>;
}

/// Everything a front end gets from a host: the handle protocol to what it runs
/// and host control over it.
///
/// The pair outlives any one agent. When the host replaces the live session it
/// rewires underneath: the front end keeps these channels, hears
/// [`HostEvent::SessionChanged`], and subscribes to the new session
/// (`docs/adr/0022` §2, §3).
pub struct HostConnection {
    /// The session live when the connection was made.
    pub session: String,
    pub commands: mpsc::UnboundedSender<atomcode_kernel::event::AgentCommand>,
    pub events: mpsc::UnboundedReceiver<atomcode_kernel::event::AgentEvent>,
    pub control: std::sync::Arc<dyn HostControl>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each command, reply, event and error exactly once. The matches are
    /// exhaustive on purpose: a new variant does not compile until it is
    /// sampled here.
    fn commands() -> Vec<HostCommand> {
        let all = vec![
            HostCommand::NewSession {
                session: "a".into(),
            },
            HostCommand::Resume {
                session: "a".into(),
                target: "b".into(),
            },
            HostCommand::SetReasoningEffort {
                session: "a".into(),
                level: Some(ReasoningEffort::XHigh),
            },
            HostCommand::ListSessions {
                working_dir: Some("/w".into()),
            },
            HostCommand::Undo {
                session: "a".into(),
                turn: Some(3),
                based_on: 40,
            },
            HostCommand::RewindPoints {
                session: "a".into(),
            },
            HostCommand::Rewind {
                session: "a".into(),
                turn: 2,
                scope: atomcode_kernel::session::RewindScope::Both,
                based_on: 40,
            },
            HostCommand::SwitchModel {
                session: "a".into(),
                model: "glm-5".into(),
            },
            HostCommand::Settings {
                session: "a".into(),
            },
            HostCommand::SetSetting {
                session: "a".into(),
                id: "ui.theme".into(),
                value: "dark".into(),
            },
            HostCommand::SetMode {
                session: "a".into(),
                mode: Mode::Plan,
            },
            HostCommand::Mode {
                session: "a".into(),
            },
            HostCommand::ChangeDirectory {
                session: "a".into(),
                directory: "/w/other".into(),
            },
            HostCommand::Models {
                session: "a".into(),
            },
            HostCommand::History {
                session: "a".into(),
                limit: 200,
            },
            HostCommand::Usage {
                session: "a".into(),
                windows_only: false,
            },
            HostCommand::Usage {
                session: "a".into(),
                windows_only: true,
            },
            HostCommand::Context {
                session: "a".into(),
            },
            HostCommand::Rename {
                session: "a".into(),
                title: "配置重构".into(),
            },
            HostCommand::McpStatus {
                session: "a".into(),
            },
            HostCommand::McpTools {
                session: "a".into(),
                server: "fs".into(),
            },
            HostCommand::McpManage {
                session: "a".into(),
            },
            HostCommand::McpDetail {
                session: "a".into(),
                server: "fs".into(),
            },
            HostCommand::McpAct {
                session: "a".into(),
                server: "fs".into(),
                action: McpAction::Disable,
            },
            HostCommand::WithdrawMcpTools {
                session: "a".into(),
            },
            HostCommand::Reload {
                session: "a".into(),
            },
            HostCommand::SignOut {
                session: "a".into(),
            },
            HostCommand::SignIn {
                session: "a".into(),
            },
            HostCommand::WhoAmI {
                session: "a".into(),
            },
            HostCommand::Changes {
                session: "a".into(),
                file: Some("src/parser.rs".into()),
                scope: ChangeScope::Workspace,
            },
            HostCommand::Providers {
                session: "a".into(),
            },
            HostCommand::Autonomy {
                session: "a".into(),
            },
            HostCommand::Thinking {
                session: "a".into(),
            },
            HostCommand::SetThinking {
                session: "a".into(),
                on: true,
            },
            HostCommand::Readiness {
                session: "a".into(),
            },
            HostCommand::ResetSetting {
                session: "a".into(),
                id: "theme".into(),
            },
            HostCommand::Sources {
                session: "a".into(),
            },
            HostCommand::ToolCatalog {
                session: "a".into(),
            },
            HostCommand::SwitchTool {
                session: "a".into(),
                pattern: "mcp__github__*".into(),
                on: false,
            },
        ];
        for c in &all {
            match c {
                HostCommand::NewSession { .. }
                | HostCommand::Resume { .. }
                | HostCommand::SetReasoningEffort { .. }
                | HostCommand::ListSessions { .. }
                | HostCommand::PreviewSession { .. }
                | HostCommand::DeleteSession { .. }
                | HostCommand::History { .. }
                | HostCommand::Undo { .. }
                | HostCommand::RewindPoints { .. }
                | HostCommand::Rewind { .. }
                | HostCommand::SwitchModel { .. }
                | HostCommand::Settings { .. }
                | HostCommand::SetSetting { .. }
                | HostCommand::SetMode { .. }
                | HostCommand::Mode { .. }
                | HostCommand::ChangeDirectory { .. }
                | HostCommand::Models { .. }
                | HostCommand::Cost { .. }
                | HostCommand::Sources { .. }
                | HostCommand::Usage { .. }
                | HostCommand::Context { .. }
                | HostCommand::Rename { .. }
                | HostCommand::McpStatus { .. }
                | HostCommand::McpTools { .. }
                | HostCommand::McpManage { .. }
                | HostCommand::McpDetail { .. }
                | HostCommand::McpAct { .. }
                | HostCommand::ToolCatalog { .. }
                | HostCommand::SwitchTool { .. }
                | HostCommand::WithdrawMcpTools { .. }
                | HostCommand::Reload { .. }
                | HostCommand::SignOut { .. }
                | HostCommand::SignIn { .. }
                | HostCommand::WhoAmI { .. }
                | HostCommand::Changes { .. }
                | HostCommand::Providers { .. }
                | HostCommand::Autonomy { .. }
                | HostCommand::Thinking { .. }
                | HostCommand::SetThinking { .. }
                | HostCommand::Readiness { .. }
                | HostCommand::ResetSetting { .. } => {}
            }
        }
        all
    }

    fn replies() -> Vec<HostReply> {
        let all = vec![
            HostReply::Done,
            HostReply::SessionChanged {
                session: "b".into(),
            },
            HostReply::Sessions {
                sessions: vec![StoredSession {
                    id: "b".into(),
                    title: Some("fix the parser".into()),
                    working_dir: Some("/w".into()),
                    created_at: 1,
                    updated_at: 2,
                    turns: 3,
                    needs_newer_version: true,
                }],
            },
            HostReply::Undone {
                prompt: Some("fix the parser".into()),
                restored_files: vec!["src/parser.rs".into()],
            },
            HostReply::RewindPoints {
                points: vec![RewindPoint {
                    turn: 2,
                    prompt: "fix the parser".into(),
                    changes: vec![ChangedFile {
                        path: "src/parser.rs".into(),
                        added: 12,
                        removed: 3,
                        binary: false,
                        change: Some(FileChange::Modified),
                        staged: true,
                    }],
                    code: true,
                }],
                code_unavailable: Some(CodeUnavailable::Failed {
                    message: "not a repository".into(),
                }),
            },
            HostReply::McpServers {
                servers: vec![
                    McpServer {
                        name: "docs".into(),
                        state: McpServerState::Connected,
                    },
                    McpServer {
                        name: "db".into(),
                        state: McpServerState::Failed {
                            message: "refused".into(),
                        },
                    },
                    McpServer {
                        name: "a".into(),
                        state: McpServerState::Connecting,
                    },
                    McpServer {
                        name: "b".into(),
                        state: McpServerState::Untrusted,
                    },
                    McpServer {
                        name: "c".into(),
                        state: McpServerState::Disconnected,
                    },
                    McpServer {
                        name: "needs-auth".into(),
                        state: McpServerState::NeedsAuthentication,
                    },
                    McpServer {
                        name: "off".into(),
                        state: McpServerState::Disabled,
                    },
                ],
            },
            HostReply::McpTools {
                tools: vec!["fs__read".into(), "fs__write".into()],
            },
            HostReply::McpRows {
                rows: vec![McpRow {
                    name: "fs".into(),
                    state: McpServerState::Connected,
                    source: "project".into(),
                    tool_count: 3,
                    config_path: Some("/w/.mcp.json".into()),
                }],
            },
            HostReply::McpDetail {
                detail: McpServerDetail {
                    name: "fs".into(),
                    state: McpServerState::Disabled,
                    source: "project".into(),
                    transport: McpTransport::Stdio {
                        command: "npx".into(),
                        args: vec!["-y".into(), "srv".into()],
                        timeout_ms: None,
                    },
                    auth: McpAuth::None,
                    tool_count: 0,
                    config_path: Some("/w/.mcp.json".into()),
                },
            },
            HostReply::Settings {
                settings: vec![Setting {
                    id: "ui.theme".into(),
                    label: "主题".into(),
                    value: "auto".into(),
                    accepts: "auto | dark | light".into(),
                    applies: "下次启动".into(),
                }],
            },
            HostReply::Models {
                models: vec![
                    ModelChoice {
                        id: "glm-5".into(),
                        about: "zhipu · 200k".into(),
                    },
                    ModelChoice {
                        id: "local/qwen".into(),
                        about: String::new(),
                    },
                ],
                current: Some("glm-5".into()),
            },
            HostReply::Autonomy {
                running: Some(Running {
                    kind: "goal".into(),
                    what: "the tests pass".into(),
                    round: 3,
                    of: Some(20),
                    elapsed_secs: 252,
                    paused: None,
                }),
            },
            HostReply::Providers {
                providers: vec![ProviderChoice {
                    id: "zhipu".into(),
                    about: "openai_compat · glm-5".into(),
                }],
                current: Some("zhipu".into()),
            },
            HostReply::Changes {
                files: vec![ChangedFile {
                    path: "src/parser.rs".into(),
                    added: 12,
                    removed: 3,
                    binary: false,
                    change: Some(FileChange::Modified),
                    staged: false,
                }],
                diff: Some("@@ -1 +1 @@\n-a\n+b\n".into()),
                unavailable: None,
            },
            HostReply::Identity {
                signed_in: true,
                stored_at: Some("~/.atomcode/auth.json".into()),
                who: Some("lichao".into()),
                detail: Some("atomgit".into()),
            },
            HostReply::Readiness {
                ready: false,
                why: Some("还没有配置任何 provider".into()),
                fix: Some("login".into()),
            },
            HostReply::Mode {
                mode: Some(Mode::Plan),
            },
            // And the host that cannot say, which is a different answer from
            // the most careful mode.
            HostReply::Mode { mode: None },
            HostReply::Context {
                window: 200_000,
                used: 48_000,
                model: "glm-5".into(),
                working_dir: "/w".into(),
            },
            HostReply::Usage {
                unavailable: None,
                plan: Some(Entitlement {
                    plan: "CodingPlan Pro".into(),
                    active: true,
                    claimed_at: "2026-07-30".into(),
                    expires_at: "2036-07-30".into(),
                    remaining_days: 3601,
                    total_days: 3653,
                }),
                windows: vec![UsageWindow {
                    label: "5 小时".into(),
                    exhausted: true,
                    resets_at: "14:30".into(),
                    resets_in_seconds: 3600,
                    window_seconds: 18_000,
                    used_percent: Some(42),
                    calls_used: Some(420),
                    call_limit: Some(1000),
                }],
                stats: Some(UsageStats {
                    from: "2026-08-21".into(),
                    to: "2026-09-20".into(),
                    models: vec![ModelUse {
                        name: "glm-5".into(),
                        tokens: 221_100_000,
                        requests: 1604,
                    }],
                    daily: vec![DayUse {
                        date: "2026-09-20".into(),
                        tokens: 216_600_000,
                        requests: 1600,
                    }],
                    series: vec![ModelSeries {
                        name: "glm-5".into(),
                        daily: vec![216_600_000],
                    }],
                    total_tokens: 221_100_000,
                    total_requests: 1604,
                }),
            },
            HostReply::Sources {
                groups: vec![SourceGroup {
                    label: "指令文件".into(),
                    files: vec![SourceFile {
                        label: "项目共享".into(),
                        path: "/w/AGENTS.md".into(),
                        present: true,
                    }],
                }],
            },
            HostReply::ToolCatalog {
                tools: vec![CatalogTool {
                    name: "write_file".into(),
                    owner: "tool-fs-world".into(),
                    state: ToolState::OffInSession,
                }],
            },
        ];
        let all: Vec<HostReply> = all
            .into_iter()
            .chain(std::iter::once(HostReply::DoneWithNote {
                note: "switched, but not written".into(),
            }))
            .collect();
        for r in &all {
            match r {
                HostReply::Done
                | HostReply::DoneWithNote { .. }
                | HostReply::SessionChanged { .. }
                | HostReply::Sessions { .. }
                | HostReply::SessionPreview { .. }
                | HostReply::History { .. }
                | HostReply::Undone { .. }
                | HostReply::RewindPoints { .. }
                | HostReply::McpServers { .. }
                | HostReply::McpTools { .. }
                | HostReply::McpRows { .. }
                | HostReply::McpDetail { .. }
                | HostReply::Settings { .. }
                | HostReply::Models { .. }
                | HostReply::Cost { .. }
                | HostReply::Changes { .. }
                | HostReply::Providers { .. }
                | HostReply::Autonomy { .. }
                | HostReply::Mode { .. }
                | HostReply::Usage { .. }
                | HostReply::Context { .. }
                | HostReply::Identity { .. }
                | HostReply::Sources { .. }
                | HostReply::ToolCatalog { .. }
                | HostReply::Readiness { .. } => {}
            }
        }
        all
    }

    fn events() -> Vec<HostEvent> {
        let all = vec![
            HostEvent::SessionChanged {
                session: "b".into(),
                previous: Some("a".into()),
            },
            HostEvent::PersistenceFailed {
                session: "b".into(),
                message: "磁盘满了".into(),
            },
            HostEvent::Autonomy {
                session: "b".into(),
                running: Some(Running {
                    kind: "goal".into(),
                    what: "测试全绿".into(),
                    round: 3,
                    of: Some(40),
                    elapsed_secs: 90,
                    paused: None,
                }),
            },
            HostEvent::ModeChanged {
                session: "b".into(),
                mode: Mode::Plan,
            },
        ];
        for e in &all {
            match e {
                HostEvent::SessionChanged { .. }
                | HostEvent::Autonomy { .. }
                | HostEvent::ModeChanged { .. }
                | HostEvent::PersistenceFailed { .. } => {}
            }
        }
        all
    }

    fn errors() -> Vec<HostError> {
        let all = vec![
            HostError::Busy {
                reason: "a turn is running".into(),
            },
            HostError::Cancelled,
            HostError::SessionInUse { id: "b".into() },
            HostError::Unavailable,
            HostError::ProviderUnavailable {
                reason: ProviderUnavailableReason::AuthenticationRequired,
            },
            HostError::Failed {
                message: "no".into(),
            },
            HostError::InvalidWorkingDirectory {
                message: "gone".into(),
            },
            HostError::UndoOutOfRange {
                requested: 4,
                available: 2,
            },
            HostError::RewindPointNotFound { turn: 9 },
            HostError::CodeRewindUnavailable {
                message: "not a repository".into(),
            },
            HostError::Stale { current: 42 },
            HostError::NotFound,
        ];
        for e in &all {
            match e {
                HostError::Busy { .. }
                | HostError::Cancelled
                | HostError::SessionInUse { .. }
                | HostError::Unavailable
                | HostError::ProviderUnavailable { .. }
                | HostError::Failed { .. }
                | HostError::InvalidWorkingDirectory { .. }
                | HostError::UndoOutOfRange { .. }
                | HostError::RewindPointNotFound { .. }
                | HostError::CodeRewindUnavailable { .. }
                | HostError::Stale { .. }
                | HostError::NotFound => {}
            }
        }
        all
    }

    fn crosses<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value, "{json}");
    }

    /// The contract can go over a wire: every command, reply, event and error
    /// comes back as it left.
    #[test]
    fn every_host_variant_crosses_the_wire_unchanged() {
        commands().iter().for_each(crosses);
        replies().iter().for_each(crosses);
        events().iter().for_each(crosses);
        errors().iter().for_each(crosses);
        let result: Result<HostReply, HostError> = Err(HostError::NotFound);
        crosses(&result);
    }

    /// A command about the live session says which one; a question about the
    /// store does not have to.
    #[test]
    fn a_command_on_the_live_session_names_it() {
        for command in commands() {
            let expected = match &command {
                HostCommand::ListSessions { .. } => None,
                _ => Some("a"),
            };
            assert_eq!(command.addressed(), expected, "{command:?}");
        }
    }

    #[test]
    fn a_transport_crossing_the_wire_carries_no_credentials() {
        // The config it is built from holds headers and OAuth material
        // (`caps::mcp::McpTransportConfig`). The wire type must not.
        let http = McpTransport::Http {
            url: "https://mcp.example.com/mcp".into(),
            timeout_ms: Some(60_000),
        };
        let json = serde_json::to_string(&http).unwrap();
        for leaked in ["header", "authorization", "client_secret", "token"] {
            assert!(
                !json.to_lowercase().contains(leaked),
                "the wire type must not carry {leaked}: {json}"
            );
        }
        let back: McpTransport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, http);
    }

    #[test]
    fn a_row_and_a_detail_survive_the_wire() {
        let row = McpRow {
            name: "context7".into(),
            state: McpServerState::Connected,
            source: "global".into(),
            tool_count: 8,
            config_path: Some("/home/u/.atomcode/mcp.json".into()),
        };
        let detail = McpServerDetail {
            name: "figma".into(),
            state: McpServerState::NeedsAuthentication,
            source: "project".into(),
            transport: McpTransport::Http {
                url: "https://mcp.figma.com/mcp".into(),
                timeout_ms: Some(60_000),
            },
            auth: McpAuth::OAuth {
                authenticated: false,
            },
            tool_count: 0,
            config_path: None,
        };
        crosses(&row);
        crosses(&detail);
    }
}
