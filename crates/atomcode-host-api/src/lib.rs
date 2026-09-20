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
    /// Name `session`. What a person calls a conversation when the title it
    /// took from its first message is not what it turned out to be about (A4).
    Rename { session: String, title: String },
    /// The MCP servers `session` was given, and how each one is.
    McpStatus { session: String },
    /// The tools one MCP server put on `session`'s model (A11).
    McpTools { session: String, server: String },
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
    },
    /// How much of the model's context this session is using.
    ///
    /// The budget, not the bill: [`HostCommand::Usage`] says what the account
    /// may still do, this says how close this conversation is to the window it
    /// has to fit in. A front end that could not ask this could only count what
    /// it had seen, which is not the same number — the host packs a system
    /// prompt, instructions and tool definitions the screen never sees.
    Context { session: String },
    /// What the account has left to spend, as rolling windows.
    ///
    /// Separate from the token counts a turn reports: those say what this
    /// conversation cost, this says what the account may still do and when a
    /// spent window comes back. A host that meters nothing answers with an
    /// empty list — which is an answer, not a failure.
    Usage { session: String },
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
            | Self::ChangeDirectory { session, .. }
            | Self::Models { session }
            | Self::Rename { session, .. }
            | Self::McpStatus { session }
            | Self::McpTools { session, .. }
            | Self::WithdrawMcpTools { session }
            | Self::Reload { session }
            | Self::SignOut { session }
            | Self::SignIn { session }
            | Self::WhoAmI { session }
            | Self::Changes { session, .. }
            | Self::Providers { session }
            | Self::Autonomy { session }
            | Self::Context { session }
            | Self::Usage { session }
            | Self::Thinking { session }
            | Self::SetThinking { session, .. }
            | Self::Readiness { session } => Some(session),
            Self::ListSessions { .. } => None,
        }
    }
}

/// What a command that went through produced.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostReply {
    /// Done. The live session is the one it was.
    Done,
    /// The live session is now `session`. A front end drops the stream of the
    /// one it replaced and follows this one (`docs/adr/0022` §6).
    SessionChanged {
        session: String,
    },
    Sessions {
        sessions: Vec<StoredSession>,
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
        code_unavailable: Option<String>,
    },
    McpServers {
        servers: Vec<McpServer>,
    },
    /// The tools one MCP server put on the model, by the names the model calls
    /// them by.
    McpTools {
        tools: Vec<String>,
    },
    /// The settings a person may change, each with what it is set to now.
    Settings {
        settings: Vec<Setting>,
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
    /// What the account has left, window by window. Empty for a host that
    /// meters nothing.
    Usage {
        windows: Vec<UsageWindow>,
        /// What went through, when the host meters it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<UsageStats>,
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

/// One file a session changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Relative to the working directory.
    pub path: String,
    pub added: u64,
    pub removed: u64,
    /// A file with no line counts to give. Shown as changed, not as `+0 -0`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
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
    /// How many workspace files it changed.
    #[serde(default)]
    pub files: usize,
    /// Whether the workspace can be put back to before it.
    #[serde(default)]
    pub code: bool,
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
    Failed {
        message: String,
    },
    Disconnected,
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
            HostCommand::ChangeDirectory {
                session: "a".into(),
                directory: "/w/other".into(),
            },
            HostCommand::Models {
                session: "a".into(),
            },
            HostCommand::Usage {
                session: "a".into(),
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
        ];
        for c in &all {
            match c {
                HostCommand::NewSession { .. }
                | HostCommand::Resume { .. }
                | HostCommand::SetReasoningEffort { .. }
                | HostCommand::ListSessions { .. }
                | HostCommand::Undo { .. }
                | HostCommand::RewindPoints { .. }
                | HostCommand::Rewind { .. }
                | HostCommand::SwitchModel { .. }
                | HostCommand::Settings { .. }
                | HostCommand::SetSetting { .. }
                | HostCommand::SetMode { .. }
                | HostCommand::ChangeDirectory { .. }
                | HostCommand::Models { .. }
                | HostCommand::Usage { .. }
                | HostCommand::Context { .. }
                | HostCommand::Rename { .. }
                | HostCommand::McpStatus { .. }
                | HostCommand::McpTools { .. }
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
                    files: 1,
                    code: true,
                }],
                code_unavailable: Some("not a repository".into()),
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
                ],
            },
            HostReply::McpTools {
                tools: vec!["fs__read".into(), "fs__write".into()],
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
                }],
                diff: Some("@@ -1 +1 @@\n-a\n+b\n".into()),
                unavailable: None,
            },
            HostReply::Identity {
                signed_in: true,
                who: Some("lichao".into()),
                detail: Some("atomgit".into()),
            },
            HostReply::Readiness {
                ready: false,
                why: Some("还没有配置任何 provider".into()),
                fix: Some("login".into()),
            },
            HostReply::Context {
                window: 200_000,
                used: 48_000,
                model: "glm-5".into(),
                working_dir: "/w".into(),
            },
            HostReply::Usage {
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
        ];
        for r in &all {
            match r {
                HostReply::Done
                | HostReply::SessionChanged { .. }
                | HostReply::Sessions { .. }
                | HostReply::Undone { .. }
                | HostReply::RewindPoints { .. }
                | HostReply::McpServers { .. }
                | HostReply::McpTools { .. }
                | HostReply::Settings { .. }
                | HostReply::Models { .. }
                | HostReply::Changes { .. }
                | HostReply::Providers { .. }
                | HostReply::Autonomy { .. }
                | HostReply::Usage { .. }
                | HostReply::Context { .. }
                | HostReply::Identity { .. }
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
        ];
        for e in &all {
            match e {
                HostEvent::SessionChanged { .. }
                | HostEvent::Autonomy { .. }
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
}
