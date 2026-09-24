//! The screen's English table.
//!
//! Register: terse, lower-case, terminal. These lines sit in a status bar or a
//! key legend where there are twelve columns to say something in, so they read
//! as labels rather than sentences — the same voice the product table already
//! uses for `(↑↓ to navigate, Enter to confirm, Esc to skip)`.
//!
//! Key names (`enter`, `esc`, `Tab`, `Ctrl+C`) are what is printed on the key,
//! so they are not translated in either direction.
//!
//! **Where a line already exists in [`crate::product`], this table says it the
//! same way.** The two front ends are two renderings of one product, and a
//! person moving between them should not be told the same thing in two
//! wordings — `Allow once`, `Deny`, `Accounts`, `Models`, `{n}m ago`, the four
//! policy-recovery options and `{n} completed` are all lifted from the table
//! tuix reads. `screen_and_product_say_the_same_things_the_same_way` in
//! `tests/consistency.rs` is the one that keeps it that way.

use super::messages::Msg;
use std::borrow::Cow;

pub(super) fn en(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        // ── the command dispatcher ──
        Msg::CmdNoSuch { name } => {
            format!("no /{name} — type /help to see what there is").into()
        }
        Msg::CmdNoSuchDidYouMean { name, near } => {
            format!("no /{name}; did you mean {near}?").into()
        }

        // ── layout errors ──
        Msg::LayoutNoSuchModule { name, available } => {
            format!("no module called `{name}`; there is: {available}").into()
        }
        Msg::LayoutNotOnScreen { module } => format!("`{module}` was not on screen").into(),
        Msg::LayoutAlreadyOnScreen { module } => format!("`{module}` is already on screen").into(),
        Msg::LayoutDrawnTwice { module } => {
            format!("`{module}` is written twice: at the tail of the stream and as its own panel — it would be drawn twice").into()
        }

        // ── the moment ──
        Msg::MomentQuitAgain => "press Ctrl+C again to quit".into(),

        // ── overlays ──
        Msg::OverlayEmptyFile => "  (empty file)".into(),
        Msg::OverlayFilterHint => "type to filter".into(),
        Msg::OverlayNoMatch => "  nothing matches".into(),

        // ── completion menu and policy interventions ──
        Msg::MenuFolder => "folder".into(),

        // ── the providers panel ──

        // ── when a setting takes effect ──
        Msg::AppliesImmediately => "now".into(),
        Msg::AppliesNextTurn => "next turn".into(),
        Msg::AppliesReload => "on reload".into(),
        Msg::AppliesReprepare => "on rebuilding capabilities".into(),
        Msg::AppliesRestart => "on restart".into(),

        // ── shared widgets ──
        Msg::ListEmpty => "(empty)".into(),
        Msg::ListMore { count } => format!("  …{count} more").into(),

        // ── times and durations ──
        Msg::LastedSeconds { s } => format!("{s}s").into(),
        Msg::LastedMinutes { m } => format!("{m}m").into(),
        Msg::LastedMinutesSeconds { m, s } => format!("{m}m {s}s").into(),
        Msg::LastedHours { h } => format!("{h}h").into(),
        Msg::LastedHoursMinutes { h, m } => format!("{h}h {m}m").into(),

        // ── the tools tree ──
        Msg::ToolsStateOn => "the model can call".into(),
        Msg::ToolsStateOff => "off for this session".into(),
        Msg::ToolsStateExcluded => "excluded by config".into(),
        Msg::ToolsExcludedByConfig { name } => {
            format!("`{name}` is excluded by this tree's config — only the config can put it back").into()
        }
        Msg::ToolsTurningOff { name } => format!("turning {name} off…").into(),
        Msg::ToolsTurningOn { name } => format!("putting {name} back…").into(),

        // ── the step-by-step wizard ──
        Msg::WizardBack => " · ← back".into(),
        Msg::WizardNoteKeys { back } => format!("enter to go on{back} · esc to drop it").into(),
        Msg::WizardChooseKeys { back } => {
            format!("↑↓ to pick · enter to confirm{back} · esc to drop it").into()
        }
        Msg::WizardTypeKeys { back } => format!("enter to confirm{back} · esc to drop it").into(),
        Msg::WizardWaitSkippableKeys => "enter to skip · esc to drop it".into(),
        Msg::WizardWaitKeys => "esc to drop it".into(),
        Msg::WizardWaiting { spinner } => format!("  {spinner} waiting").into(),

        // ── what the agent asks a person ──
        Msg::AskStepLimitQuestion => "this turn has taken a lot of steps. Go on?".into(),
        Msg::AskStepLimitTitle => "step limit".into(),
        Msg::AskTruncatedQuestion => {
            "the answer keeps being cut off and the automatic continuations are used up. Go on?"
                .into()
        }
        Msg::AskTruncatedTitle => "output cut off".into(),
        Msg::AskContinue => "go on".into(),
        Msg::AskStop => "stop".into(),
        Msg::AskYes => "yes".into(),
        Msg::AskNo => "no".into(),
        Msg::AskAlwaysAllow => "Always allow".into(),
        Msg::AskMemberRequests { name } => format!("member {name} asks to ").into(),
        Msg::AskLinesChars { lines, chars } => {
            format!("{lines} lines · {chars} chars").into()
        }

        // ── the ask panel ──
        Msg::AskLegendChoose => "select".into(),
        Msg::AskLegendConfirm => "confirm".into(),
        Msg::AskFromMember { who } => format!("from member {who}").into(),
        Msg::AskGrantWholeTool => "every call of this tool".into(),
        Msg::AskGrantOnly { what } => format!("only {what}").into(),

        // ── the input line ──
        Msg::InputAnswerKeys => "enter to send · esc to withhold".into(),
        Msg::InputHistoryNth { nth, total } => format!("history {nth}/{total}").into(),
        Msg::InputSearchNth { query, nth, total } =>
            format!("search '{query}' {nth}/{total}").into(),
        Msg::InputSearchNone { query } => format!("search '{query}' no match").into(),
        Msg::ComposerInterrupted => "Interrupted · what next?".into(),

        // ── the status bar ──
        Msg::StatusMember { name } => format!("member {name}").into(),
        Msg::StatusStopping => "stopping".into(),
        Msg::StatusGoal => "goal".into(),
        Msg::StatusLoop => "loop".into(),
        Msg::StatusRoundsHeld { kind, rounds, why } => {
            format!("{kind} round {rounds} · held: {why}").into()
        }
        Msg::StatusRounds { kind, rounds } => format!("{kind} round {rounds}").into(),

        // ── the team panel ──
        Msg::TeamHeaderFocused { count } => {
            format!("team · {count} members · ↑↓ to pick · Enter to switch · Esc to go back").into()
        }
        Msg::TeamHeader { count } => format!("team · {count} members · Tab to switch view").into(),
        Msg::TeamLead => "lead".into(),
        Msg::TeamViewing => " viewing".into(),
        Msg::TeamWorkingRound { round } => format!("round {round}").into(),

        // ── the todo fold ──
        Msg::TodoCounts {
            completed,
            in_progress,
            open,
        } => format!("({completed} completed, {in_progress} in progress, {open} to do)").into(),

        // ── the tools panel ──
        Msg::ToolsPanelCounts { on, off } => format!("{on} callable · {off} off").into(),
        Msg::ToolsPanelNoneMounted => "  this tree has no tools mounted at all".into(),
        Msg::ToolsPanelNoMatch => "  no tool matches".into(),
        Msg::ToolsPanelStopWaiting => "stop waiting".into(),
        Msg::ToolsLegendChoose => "select".into(),
        Msg::ToolsLegendToggle => "on / off".into(),
        Msg::ToolsLegendTyping => "type".into(),
        Msg::ToolsLegendFilter => "filter".into(),
        Msg::ToolsLegendClose => "close".into(),

        Msg::RewindPanelAbout => "  restore the code and/or the conversation to the point before…".into(),
        Msg::RewindPanelReading => "reading the turns of this session…".into(),
        Msg::RewindPanelGoing { turn } => format!("going back to before turn {turn}…").into(),
        Msg::RewindPanelNoPoints => "  nothing to go back to yet".into(),
        Msg::RewindPanelStopWaiting => "stop watching".into(),
        Msg::RewindPanelCurrent => "(current)".into(),
        Msg::RewindPanelNoCodeChanges => "no code changes".into(),
        Msg::RewindPanelFiles { files } => format!("{files} files").into(),
        Msg::RewindPanelScopeAsk => "  going back there takes what with it?".into(),
        Msg::RewindCodeNotEnabled => "set ATOMCODE_CODE_REWIND=1 to rewind the workspace too".into(),
        Msg::RewindCodeNoSession => "this session is not written down, so the workspace cannot go back".into(),
        Msg::RewindCodeFailed { why } => format!("the workspace cannot go back: {why}").into(),
        Msg::RewindPanelTurnNoFiles => "this turn changed no file — only the conversation can go back".into(),
        Msg::RewindLegendChoose => "choose".into(),
        Msg::RewindLegendContinue => "continue".into(),
        Msg::RewindLegendGo => "go back".into(),
        Msg::RewindLegendBack => "one step back".into(),
        Msg::RewindLegendClose => "cancel".into(),
        Msg::RewindPointsUnreadable { why } => format!("the turns could not be read: {why}").into(),
        Msg::RewindFailed { why } => format!("it did not go back: {why}").into(),
        Msg::NoRewindPanel => "this screen has no rewind panel: the launcher provided no `tui-panel-rewind`".into(),
        Msg::NoResumeStore => {
            "this screen cannot throw a session away: the launcher provided no `tui-resume-store`"
                .into()
        }
        Msg::NoPlaces => {
            "this screen cannot keep bookmarks: the launcher provided no `tui-places`".into()
        }
        Msg::CdBookmarked => "marked".into(),
        Msg::CdRecent => "worked in recently".into(),
        Msg::CdPinned { dir } => format!("{dir} is marked — /cd offers it first").into(),
        Msg::CdUnpinned { dir } => format!("{dir} is no longer marked").into(),
        Msg::ResumeDeleteArmed => "Delete again to throw it away".into(),
        Msg::ResumePreviewWaiting => "reading what it last talked about…".into(),
        Msg::ResumeDeleted { id } => format!("Session {id} is gone").into(),
        Msg::ResumeDeleteFailed { why } => format!("It was not deleted: {why}").into(),
        Msg::NoResumePanel => "this screen has no resume panel: the launcher provided no `tui-panel-resume`".into(),
        Msg::NoRewind => "this screen cannot go back: the launcher provided no `tui-rewind`".into(),
        Msg::ScreenNotConnectedRewind => "no agent on screen, so there are no turns to go back through".into(),

        // ── notes in the transcript ──
        Msg::TranscriptCompacted { through } => {
            format!("the conversation before this is folded into one summary (through #{through})")
                .into()
        }
        Msg::TranscriptShortened { count } => {
            format!("{count} tool outputs were shortened for the model; what is shown here is still the original").into()
        }
        Msg::TranscriptDropped { through } => {
            format!("tool results through #{through} are no longer sent to the model").into()
        }
        Msg::TranscriptRateLimited { until } => format!("rate limited, waiting until {until}").into(),
        Msg::TranscriptMemberEnded => "this member has finished and will not speak again".into(),

        // ── what each command is for (`commands.rs`) ──
        Msg::CmdAboutQuit => "quit".into(),
        Msg::CmdAboutReasoning => "reasoning: one line, in full, folded — cycles".into(),
        Msg::CmdAboutTools => "tool output: all of it, one summary each, summarised in groups; cycles with no argument".into(),
        Msg::CmdAboutShowInject => "injected context: folded, label only, in full — cycles; all of them with no name".into(),
        Msg::CmdAboutMouse => "hand the mouse back to the terminal, or take it back".into(),
        Msg::CmdAboutKeys => "list the key bindings".into(),
        Msg::CmdAboutTodo => "open or fold the plan".into(),
        Msg::CmdAboutTeam => "open or fold the team panel".into(),
        Msg::CmdAboutPaste => "put the clipboard (or a file) into the composer; for when the terminal or the system eats Ctrl+V".into(),
        Msg::CmdAboutConfig => "open the settings panel: search, change a value; esc closes it".into(),
        Msg::CmdAboutProviderPanel => "open the provider panel: accounts and models, add/edit/remove; ⏎ switches, esc closes".into(),
        Msg::CmdAboutCopy => "copy a code block from the model's last reply; N picks which one, all takes every one".into(),
        Msg::CmdAboutSave => "save this conversation as markdown".into(),
        Msg::CmdAboutView => "open a file in a read-only overlay; costs no turn and does not enter the conversation".into(),
        Msg::CmdAboutCompact => "fold the history to make room in the context".into(),
        Msg::CmdAboutCancelAll => "stop the turn running here and in every team member; the members stay on the team".into(),
        Msg::CmdAboutContext => "how much this session has used".into(),
        Msg::CmdAboutAgents => "every agent under this session: the lead and each member, stopped ones included; pick one to read its conversation".into(),
        Msg::CmdAboutTranscript => "list the conversation the way the model sees it".into(),
        Msg::CmdAboutClear => "start a new session: this conversation is put down and a clean one begins".into(),
        Msg::CmdAboutSession => "start a new session (same as /clear)".into(),
        Msg::CmdAboutResume => "go back to a stored session; pick one from a list with no id".into(),
        Msg::CmdAboutEffort => "change this session's reasoning effort (independent of the model)".into(),
        Msg::CmdAboutUndo => "take back the last thing said (or one turn) and everything after it; the words go back into the composer".into(),
        Msg::CmdAboutRewind => "go back to before a turn: the conversation, the workspace or both; with no argument it opens the panel, which a double-tap on Esc opens too".into(),
        Msg::CmdAboutModel => "which model this session uses from now on; pick one from a list with no id".into(),
        Msg::CmdAboutAutonomy => "whether it is running on its own (goal / loop), which round it is on and how long it has taken".into(),
        Msg::CmdAboutRename => "give this session another name".into(),
        Msg::CmdAboutDiff => "what this session did to the workspace; with no file it lists the changed ones to pick from".into(),
        Msg::CmdAboutMode => "change what gets asked: plan looks without touching, ask asks first, edits writes files without asking, auto asks nothing; with no argument it says which one is on".into(),
        Msg::CmdAboutCd => "work in another directory (starts a new session); pin / unpin marks the ones you come back to".into(),
        Msg::CmdAboutPlan => "look without touching (same as /mode plan)".into(),
        Msg::CmdAboutBuild => "ask before touching (same as /mode ask)".into(),
        Msg::CmdAboutAuto => "ask nothing (same as /mode auto)".into(),
        Msg::CmdAboutStatus => "where this session stands: model, mode, directory, which turn it is on".into(),
        Msg::CmdAboutCost => "how many tokens this session has used (same as /context)".into(),
        Msg::CmdAboutUsage => "what is left on the account: which window is spent and when it comes back".into(),
        Msg::CmdAboutMcp => "the MCP servers and their state; tools lists what one server mounted; withdraw takes every MCP tool away at once".into(),
        Msg::CmdAboutLanguage => "which language the screen and the model answer in; with no argument it says which one is set and what it takes".into(),
        Msg::CmdAboutReload => "read the skills, the MCP servers and the config again; the session stays".into(),
        Msg::CmdAboutLogout => "take the credentials out of the process; the session stays".into(),
        Msg::CmdAboutLogin => "sign in again with the configured credentials".into(),
        Msg::CmdAboutWhoami => "who is signed in".into(),
        Msg::CmdAboutThink => "whether to think at all (a different knob from /effort, which is how hard); with no argument it says which one is set".into(),
        Msg::CmdAboutLook => "put the screen on that agent".into(),
        Msg::CmdAboutHelp => "list every command".into(),

        // ── what a command takes, as it is shown after the name (`commands.rs`) ──
        Msg::CmdTakesPath => "[path]".into(),
        Msg::CmdTakesPathRequired => "<path>".into(),
        Msg::CmdTakesFilename => "[filename]".into(),
        Msg::CmdTakesFile => "[file]".into(),
        Msg::CmdTakesSessionId => "[session id]".into(),
        Msg::CmdTakesSessionIdRequired => "<session id>".into(),
        Msg::CmdTakesTurn => "[turn]".into(),
        Msg::CmdTakesTurnScope => "[turn [conversation|code|both]]".into(),
        Msg::CmdTakesModelId => "[model id]".into(),
        Msg::CmdTakesName => "<name>".into(),
        Msg::CmdTakesDirectory => "<directory | pin | unpin>".into(),
        Msg::CmdTakesMcp => "[tools <server>|withdraw]".into(),
        Msg::CmdTakesLanguage => "[language]".into(),

        // ── when the host refuses (`commands.rs`) ──
        Msg::HostBusy { reason } => format!("not now: {reason}").into(),
        Msg::HostNotFound => "not found: the session has been changed, or there is no such session".into(),
        Msg::HostSessionInUse { id } => format!("session {id} is in use elsewhere").into(),
        Msg::HostUnavailable => "the host is unavailable right now".into(),
        Msg::HostNoProvider { reason } => format!("no model available: {reason}").into(),
        Msg::HostSaidSomethingElse { reply } => format!("the host answered something else: {reply}").into(),

        // ── the screen's own commands (`commands.rs`) ──
        Msg::NoClipboard => "this screen has no clipboard".into(),
        Msg::NoAgent => "this screen is not connected to an agent".into(),
        Msg::NoHost => "this screen is not connected to a host".into(),
        Msg::CommandCarriesNoPictures { count } =>
            format!("a command carries no pictures, so the {count} attached went nowhere; send them in a message instead").into(),
        Msg::FoldUsage { name, other } =>
            format!("`/{name} {other}`? it takes nothing (toggle), `show`, or `hide`").into(),
        Msg::DiffAdded => "added".into(),
        Msg::DiffAddedStaged => "added · staged".into(),
        Msg::DiffModified => "modified".into(),
        Msg::DiffModifiedStaged => "modified · staged".into(),
        Msg::DiffDeleted => "deleted".into(),
        Msg::DiffDeletedStaged => "deleted · staged".into(),
        Msg::DiffRenamed => "renamed".into(),
        Msg::DiffUntracked => "untracked".into(),
        Msg::DiffConflicted => "conflicted".into(),
        Msg::ClipboardHasNothing => "there is nothing on the clipboard to paste; `/paste <path>` takes a file instead — a picture attaches, anything else goes in as text".into(),
        Msg::FileIsEmpty { path } => format!("{path} is empty").into(),
        Msg::FileUnreadable { path, error } => format!("cannot read {path}: {error}").into(),
        Msg::KeysHelp => "enter sends · shift+enter a new line (or ctrl-j) · ctrl-d quits · ctrl-w deletes a word\n\
             esc in order: drop the selection -> clear the composer -> stop the turn · ctrl-c stops the turn outright\n\
             up/down move the caret, and page through history once at the end · click to put the caret where you clicked\n\
             pgup/pgdn and the wheel scroll the conversation\n\
             alt-r reasoning (one line / in full / folded, cycles) · ctrl-t tool output (all / one summary each / summarised in groups, cycles) · ctrl-l redraws\n\
             ctrl-r searches what you have typed in this project before; type to narrow, ctrl-r again for older, enter takes it, esc gives your draft back\n\
             shift+tab steps to the next execution mode (plan/ask/edits/auto; with no completion menu up) · set ui.mode_switch_key=tab in /config to cycle with tab instead, leaving tab for completion\n\
             /showinject [name] injected context (hidden by default; all of them with no name, `all` includes peers' reports)\n\
             drag to select and copy · esc drops the selection · click a thought or a tool call to fold or open that one\n\
             ctrl-o hands the mouse back to the terminal (use its own selection instead)".into(),
        Msg::ToolOutputUnknown { what } => format!("there is no `{what}` shape for tool output; it takes full (all of it) / head (20 lines each end) / each (one summary each) / group (summarised in groups)").into(),
        Msg::InjectionUnknown { what, names } => format!("there is no `{what}` injection; it takes {names} or all").into(),
        Msg::CopyWhichBlock { count } => format!("there are {count}; `/copy N` picks one, `/copy all` takes every one").into(),
        Msg::CopyNoSuchBlock { count, asked } => format!("there are only {count}; there is no block {asked}").into(),
        Msg::CopyNoBlocks => "there is no code block in the last reply".into(),
        Msg::CopiedLines { lines } => format!("copied {lines} lines").into(),
        Msg::SaveNothingYet => "there is nothing in this conversation to save yet".into(),
        Msg::SavedTo { path } => format!("saved to {path}").into(),
        Msg::SaveWouldOverwrite { path } => format!("{path} already exists and is not a .md — pick another name, or remove it first").into(),
        Msg::SaveFailed { error } => format!("could not save: {error}").into(),
        Msg::AllowanceNear { label, percent } => format!("{label} {percent}% used").into(),
        Msg::AllowanceNearWithReset {
            label,
            percent,
            resets_in,
        } => format!("{label} {percent}% used · back in {resets_in}").into(),
        Msg::ViewWhichFile => "which file? `/view <path>`".into(),
        Msg::ViewNotText { path } => format!("{path} is not a text file").into(),
        Msg::ViewTooBig { mb } => format!("first {mb} MB").into(),
        Msg::ViewOnlyFirstLines { lines } => format!("first {lines} lines").into(),
        Msg::ViewLongLinesCut { lines } => match lines {
            1 => "1 long line cut".into(),
            n => format!("{n} long lines cut").into(),
        },

        // ── the conversation's own commands (`commands.rs`) ──
        Msg::LookWhichSession => "switch to which session?".into(),
        Msg::CancelledTurn => "the turn was stopped".into(),
        Msg::CancelledTurnAndMembers { members } => format!("the turn was stopped, and so were {members} members' ").into(),
        Msg::NoCompaction => "this agent has no compaction policy".into(),
        Msg::ContextCounts { turn, messages, facts } => format!("{turn} turns · {messages} messages the model can see · {facts} facts").into(),
        Msg::NothingSaidYet => "nothing has been said yet".into(),
        Msg::NoRoster => "this screen has no agent roster: the launcher provided no `tui-team-roster`".into(),
        Msg::AgentsLead => "lead · this session itself".into(),
        Msg::AgentsStopped { name } => format!("{name} · stopped; its log is still here").into(),
        Msg::AgentsNoneYet => "there is no other agent under this session yet".into(),
        Msg::AgentsPickerHint => "read which one · enter switches to it".into(),
        Msg::SessionNeedsNewerVersion { id } => format!("a newer version is needed to open it · {id}").into(),
        Msg::SessionTurnsWhenWhere { turns, when, dir } => format!("{turns} turns · {when} · {dir}").into(),
        Msg::SessionTurnsWhen { turns, when } => format!("{turns} turns · {when}").into(),
        Msg::ResumeNoOthers => "there is no other stored session".into(),
        Msg::ResumePickerHint => "back to which session · enter opens it · Delete throws it away".into(),

        // ── reasoning effort, undo and rewind (`commands.rs`) ──
        Msg::EffortAbout => "this session's reasoning effort".into(),
        Msg::EffortDefaultAbout => "leave it to the endpoint".into(),
        Msg::EffortPickerTitle { level } => format!("reasoning effort · now {level} · enter changes it").into(),
        Msg::EffortPickerTitleDefault => "reasoning effort · left to the endpoint · enter changes it".into(),
        Msg::EffortUnknown { wanted, levels } => format!("no such effort `{wanted}`; it takes {levels}, default").into(),
        Msg::EffortSet { wanted } => format!("reasoning effort → {wanted}").into(),
        Msg::EffortCurrent { now, levels } => {
            format!("reasoning effort · now {now} · pick one: {levels}").into()
        }
        Msg::UndoLeadOnly => "undo is the lead's: switch back to the lead first".into(),
        Msg::NotATurnNumber { what } => format!("`{what}` is not a turn number").into(),
        Msg::RewindScopeUnknown { what } => format!("`{what}` is not a scope; it takes conversation, code or both").into(),
        Msg::RewindRestored { files } => format!("{files} files restored").into(),

        // ── model, mode and working directory (`commands.rs`) ──
        Msg::ModelOnlyCurrent { current } => format!("current model: {current}; there is no other to pick").into(),
        Msg::ModelNoneConfigured => "no model is configured to pick from".into(),
        Msg::ModelPickerHint => "switch to which model · enter switches".into(),
        Msg::ModelSet { wanted } => format!("model → {wanted}").into(),
        Msg::ModeWhatEachDoes => "plan looks without touching · ask asks first · edits writes files without asking · auto asks nothing".into(),
        Msg::ModeUnknown { what } => format!("`{what}` is not a mode; it takes plan, ask, edits or auto").into(),
        Msg::ModeSet { mode } => format!("now on {mode}").into(),
        Msg::CdUpOneLevel => "up one level".into(),
        Msg::CdStepInto => "step into it".into(),
        Msg::CdStayHere => "work right here".into(),
        Msg::CdPickerHint { here } => format!("work in which directory · now in {here}").into(),
        Msg::CdMovedNewSession { directory, session } => format!("working in {directory} now · new session {session}").into(),
        Msg::CdMoved { directory } => format!("working in {directory} now").into(),

        // ── what changed, the language, and what is left on the account (`commands.rs`) ──
        Msg::DiffNoChangeIn { what } => format!("{what} has no changes").into(),
        Msg::DiffNothingChanged => "this session has not changed a file in the workspace yet".into(),
        Msg::DiffBinary => "binary".into(),
        Msg::DiffPickerHint { count, added, removed } => format!("{count} files changed · +{added} -{removed} · enter opens one").into(),
        Msg::NoLanguageSetting => "this host has no language setting".into(),
        Msg::LanguageNow { value, accepts } => format!("language: {value} · it takes {accepts} · `/language <value>` changes it").into(),
        Msg::LanguageSet { wanted, applies } => format!("language: {wanted} (applies {applies})").into(),
        Msg::UsageNotCounted => "this host does not count an allowance".into(),
        Msg::UsageCallLimit { n } => format!(" · {n} calls at most").into(),
        Msg::UsageResetsIn { duration } => format!("in {duration}").into(),
        Msg::UsageResetsAt { at } => format!("at {at} ").into(),
        Msg::UsageExhausted { label, when, cap } => format!("{label} is spent · back {when}{cap}").into(),
        Msg::UsageLeft { label, cap } => format!("{label} has room left{cap}").into(),

        // ── running on its own, where the session stands, and who is signed in (`commands.rs`) ──
        Msg::AutonomyIdle => "it is not running on its own".into(),
        Msg::AutonomyGoal { what } => format!("goal: {what}").into(),
        Msg::AutonomyLoop { what } => format!("loop: {what}").into(),
        Msg::AutonomyRoundOf { round, of } => format!("round {round}/{of}").into(),
        Msg::AutonomyRound { round } => format!("round {round}").into(),
        Msg::AutonomyLine { what, rounds, took } => format!("{what} · {rounds} · {took} so far").into(),
        Msg::AutonomyHeld { line, why } => format!("{line} · held: {why}").into(),
        Msg::StatusNoModel => "no model mounted".into(),
        Msg::StatusEffortDefault => "the endpoint's default".into(),
        Msg::StatusSessionLine { session } => format!("session {session}").into(),
        Msg::StatusModelLine { model, effort } => format!("model {model} · reasoning effort {effort}").into(),
        Msg::StatusWhereLine { where_ } => format!("in {where_}").into(),
        Msg::StatusAutonomyLine { what, round, took } => format!("running on its own: {what} · round {round} · {took} so far").into(),
        Msg::VisionFailedBecause { reason } => {
            format!("the picture was not recognised: {reason}").into()
        }
        Msg::RuntimeStopped { how } => {
            format!("the runtime stopped: {how}. nothing more will arrive in this session.").into()
        }
        Msg::ModelNotKept { error } => {
            format!("but it was not written down; the next start will use the old one: {error}").into()
        }
        Msg::RefusedStaleQuestion => "that question is no longer waiting for an answer".into(),
        Msg::RefusedNotRunning => "there is no turn running to act on".into(),
        Msg::RefusedUnavailable => {
            "it cannot be taken now — no usable provider, or one is being swapped".into()
        }
        Msg::RefusedUnsupported => "this host has no answer for that command".into(),
        Msg::CompactionInterrupted => {
            "the compaction was interrupted — the context is as long as it was".into()
        }
        Msg::GoalMet { condition } => format!("goal met: {condition}").into(),
        Msg::GoalGaveUp { condition } => {
            format!("the goal stopped without being able to tell whether it was met: {condition}")
                .into()
        }
        Msg::WhoAmIUnnamed => "signed in, but the host did not say as whom".into(),
        Msg::WhoAmIStoredAt { path } => format!("kept in {path}").into(),
        Msg::WhoAmINobody => "nobody is signed in; this configuration uses its own credentials".into(),
        Msg::ThinkingNow { value } => format!("thinking: {value}; /think on or /think off changes it").into(),
        Msg::NoThinkingSwitch => "this host has no thinking switch".into(),
        Msg::NotOnOrOff { what } => format!("`{what}` is not on or off").into(),
        Msg::ThinkingSet { value } => format!("thinking: {value}").into(),
        Msg::RenameNeedsName => "it needs a name: /rename <name>".into(),
        Msg::RenamedTo { title } => format!("this session is now called “{title}”").into(),

        // ── MCP servers, reloading and signing in (`commands.rs`) ──
        Msg::McpNoneConfigured => "no MCP server is configured".into(),
        Msg::McpConnecting => "connecting".into(),
        Msg::McpConnected => "connected".into(),
        Msg::McpUntrusted => "untrusted project — not started".into(),
        Msg::McpNeedsAuthentication => "needs authentication".into(),
        Msg::McpFailed { message } => format!("failed: {message}").into(),
        Msg::McpDisconnected => "disconnected".into(),
        Msg::McpDisabled => "disabled in its config file".into(),
        Msg::McpUnknownState => "unknown".into(),
        Msg::McpWithdrawn => "every MCP tool was withdrawn".into(),
        Msg::McpNeedsServerName => "it needs a server name: /mcp tools <server>".into(),
        Msg::McpServerHasNoTools { server } => format!("{server} mounted no tools").into(),
        Msg::McpUnknownSubcommand { what } => format!("`/mcp {what}` is not one of them; there is /mcp, /mcp tools <server>, /mcp withdraw").into(),
        Msg::Reloaded => "the skills, the MCP servers and the config were read again".into(),
        Msg::SignedOut => "signed out; /login signs in again".into(),
        Msg::SignedIn => "signed in".into(),

        // ── the MCP panel (`mcp.rs`, `modules/mcp.rs`) ──
        Msg::McpPanelTitle => "Manage MCP servers".into(),
        Msg::McpPanelServers { n } => format!("{n} servers").into(),
        Msg::McpPanelEmpty => "No MCP servers configured".into(),
        Msg::McpPanelNoMatch => "no configured server matches".into(),
        Msg::McpPanelUnavailable => "this build has no MCP panel".into(),
        Msg::McpDetailPending => "Fetching details…".into(),
        // Where a server came from: the product's `/help` names the same two
        // places in its source column, so these read its words instead of
        // writing them a second time (`tests/tables.rs`).
        Msg::McpGroupGlobal => {
            crate::product::t_with(crate::Locale::En, crate::product::Msg::HelpSourceGlobal)
        }
        Msg::McpGroupProject => {
            crate::product::t_with(crate::Locale::En, crate::product::Msg::HelpSourceProject)
        }
        Msg::McpGroupDriver => "Supplied by the client".into(),
        Msg::McpLabelState => "Status".into(),
        Msg::McpLabelAuth => "Auth".into(),
        Msg::McpLabelEndpoint => "Endpoint".into(),
        Msg::McpLabelSource => "Config location".into(),
        Msg::McpLabelTools { n } => format!("{n} tools").into(),
        Msg::McpAuthNone => "Not required".into(),
        Msg::McpAuthAuthenticated => "authenticated".into(),
        Msg::McpAuthNotAuthenticated => "not authenticated".into(),
        Msg::McpActionTrust => "Trust this project".into(),
        Msg::McpActionUntrust => "Untrust".into(),
        Msg::McpActionLogin => "Authenticate".into(),
        Msg::McpActionLogout => "Sign out".into(),
        Msg::McpActionEnable => "Enable".into(),
        Msg::McpActionDisable => "Disable".into(),
        Msg::McpLegendList => "↑/↓ move · Enter details · Esc close".into(),
        Msg::McpLegendDetail => "↑/↓ move · Enter run · Esc back".into(),
        Msg::McpLegendBusy => "Esc cancel".into(),
        Msg::McpLegendBusyHide => "Esc hide".into(),
        Msg::McpLegendCancelling => "cancelling… · Esc hide".into(),
        Msg::McpSignInCancelled => "sign-in cancelled".into(),

        // ── the toolbox and the plugins (`commands.rs`) ──
        Msg::CmdTakesToolbox => "[off <name or mcp__server__*> | on <the same>]".into(),
        Msg::CmdAboutToolbox => "the toolbox: with no argument it opens the panel (what there is, and switching it); with one it switches directly".into(),
        Msg::NoToolCatalog => "this screen has no tool catalogue: the launcher provided no `tui-tools`".into(),
        Msg::ToolboxUnknownVerb { what } => format!("no such word `{what}` — there is `off` and `on`").into(),
        Msg::ToolboxNeedsPattern { verb } => format!("`{verb}` needs a name or a pattern, e.g. `mcp__github__*`").into(),
        Msg::ToolboxNothingMoved { pattern } => format!("no tool moved — either `{pattern}` matched nothing, or what it matched is excluded by the config").into(),
        Msg::ToolboxPutBack { names } => format!("put back: {names}").into(),
        Msg::ToolboxTurnedOff { names } => format!("turned off: {names}").into(),
        Msg::ToolboxNameJoiner => ", ".into(),
        Msg::CmdTakesPlugin => "[list | install <name> | uninstall <name> | update <name> | marketplace …]".into(),
        Msg::CmdAboutPlugin => "plugins: with no argument it opens the panel (install, remove, add a marketplace); with one it does it directly".into(),
        Msg::PluginNoSuch { typed } => format!("there is no plugin called {typed}").into(),
        Msg::PluginAmbiguous { name, lines } => format!("there are several called {name}; say which one:\n{lines}").into(),
        Msg::NoPluginPort => "this screen has no plugins: the launcher provided no `tui-plugins`".into(),
        Msg::NoMcpPort => "this screen has no MCP port: the launcher provided no `tui-mcp`".into(),
        Msg::CmdTakesSetup => "[focus area, e.g. hooks, mcp, skills, all]".into(),
        Msg::CmdAboutSetup => "analyze this project, install the seed skill, and recommend which automations to set up".into(),
        Msg::NoSetupPort => "this screen installs no seeds: the launcher provided no `tui-setup`, so there is nothing to unseal".into(),
        Msg::SetupJobDidNotStart { error } => format!("the seed installation did not start: {error}").into(),
        Msg::SetupFailed { error } => format!("installing the seeds failed: {error}").into(),
        Msg::SetupInstalling => "installing the seed files…".into(),
        Msg::SetupRunningSkill => "the seeds are in place — having the model look over this project…".into(),
        Msg::PluginNothingInstalled => "nothing is installed yet".into(),
        Msg::PluginInstalledList { lines } => format!("installed:\n{lines}").into(),
        Msg::PluginInstallWhich => "install which one? `/plugin install <name>`".into(),
        Msg::PluginAlreadyInstalled { id } => format!("{id} is already installed. To install it again, `/plugin uninstall {id}` first").into(),
        Msg::PluginInstalling { plugin, market } => format!("installing {plugin}@{market} …").into(),
        Msg::PluginUninstallWhich => "remove which one? `/plugin uninstall <name>`".into(),
        Msg::PluginNotInstalled { typed } => format!("no plugin called {typed} is installed").into(),
        Msg::PluginUninstalling { id } => format!("removing {id} …").into(),
        Msg::PluginUpdateWhich => "update which one? `/plugin update <name>`".into(),
        Msg::PluginUpdating { id } => format!("updating {id} …").into(),
        Msg::MarketNoneYet => "there is no marketplace yet".into(),
        Msg::MarketRow { name, source, plugins, installed } => format!("  {name}  {source}  {plugins} plugins, {installed} installed").into(),
        Msg::MarketList { lines } => format!("marketplaces on the list:\n{lines}").into(),
        Msg::MarketAddWhich => "add which one? `/plugin marketplace add <address>`".into(),
        Msg::MarketFetching { what } => format!("fetching {what} …").into(),
        Msg::MarketRemoveWhich => "remove which one? `/plugin marketplace remove <name>`".into(),
        Msg::MarketRemoving { what } => format!("removing marketplace {what} …").into(),
        Msg::MarketUpdateWhich => "update which one? `/plugin marketplace update <name>`".into(),
        Msg::MarketUpdating { what } => format!("updating marketplace {what} …").into(),
        Msg::MarketUnknownAction { what } => format!("`/plugin marketplace` has no {what}; there is list, add, remove, update").into(),
        Msg::PluginUnknownAction { what } => format!("`/plugin` has no {what}; there is list, install, uninstall, update, marketplace, reload — or no argument at all, which opens the panel").into(),
        Msg::ReloadFailedAfter { said, why } => format!("{said}\nbut the session could not be reloaded, so what arrived takes effect at the next start: {why}").into(),
        Msg::MarkdownUser => "## Me".into(),
        Msg::MarkdownAssistant => "## The model".into(),

        // ── the settings panel and what it shows about usage (`modules/settings.rs`) ──
        Msg::SettingsNoMatch => "  no setting matches".into(),
        Msg::SettingsAbove { above } => format!("{above} more above").into(),
        Msg::SettingsBelow { below, arrow } => format!("{below} more below {arrow}").into(),
        Msg::SettingsTitle => "Settings".into(),
        Msg::AskingHost => "asking the host…".into(),
        Msg::UsageThisSession => "this session".into(),
        Msg::UsageContextBar { percent, used, window } => format!("context {percent}% used · {used} / {window}").into(),
        Msg::UsageModelNote { model } => format!("model {model}").into(),
        Msg::UsageAllowanceHead => "allowance".into(),
        Msg::UsageCallsUsedOfLimit { used, limit } => format!(" · {used} / {limit} calls").into(),
        Msg::UsageSpentPercent { percent, counted } => format!("{percent}% spent{counted}").into(),
        Msg::UsageWindowNotReported => "this window reported no usage".into(),
        Msg::UsageSpent => "spent".into(),
        Msg::UsageResetsAtNote { at } => format!("resets {at}").into(),
        Msg::UsagePlanHead { plan, state } => format!("{plan} · {state}").into(),
        Msg::UsagePlanTermPercent { percent } => format!("{percent}%").into(),

        // ── what this build is running as (`modules/settings.rs`) ──
        Msg::StatusRowVersion => "version".into(),
        Msg::StatusRowSession => "session".into(),
        Msg::StatusRowSessionId => "session id".into(),
        Msg::StatusRowDirectory => "directory".into(),
        Msg::StatusRowSignedIn => "signed in".into(),
        Msg::StatusRowPlan => "plan".into(),
        Msg::StatusRowUsage => "usage".into(),
        Msg::StatusNoAccount => "no account (it uses the credentials in the config)".into(),
        Msg::StatusWhoDetail { who, detail } => format!("{who} · {detail}").into(),
        Msg::StatusModelEffort { model, effort } => format!("{model} · reasoning effort {effort}").into(),
        Msg::StatusPlanExpires { at } => format!(" · expires {at}").into(),
        Msg::StatusPlanDaysLeft { remaining, total } => format!(" ({remaining}/{total} days left)").into(),
        Msg::StatusWindowSpent { percent } => format!("{percent}% of this window spent").into(),
        Msg::StatusWindowNotReported => "this window reported no usage".into(),
        Msg::StatusWindowResetsIn { duration } => format!(" · resets in {duration}").into(),
        Msg::McpTallyFailed { n } => format!("{n} cannot connect").into(),
        Msg::McpTallyUntrusted { n } => format!("{n} awaiting trust").into(),
        Msg::McpTallyNeedsAuthentication { n } => format!("{n} awaiting authentication").into(),
        Msg::McpTallyConnecting { n } => format!("{n} connecting").into(),
        Msg::McpTallyConnected { n } => format!("{n} connected").into(),
        Msg::McpTallyOff { n } => format!("{n} not connected").into(),
        Msg::McpTallyDisabled { n } => format!("{n} disabled").into(),
        Msg::StatsNotKept => "this host keeps no account".into(),

        // ── the account's figures (`modules/settings.rs`) ──
        Msg::StatsNoDaily => "there is no daily record".into(),
        Msg::StatsDailyHead => "used per day".into(),
        Msg::StatsNoModels => "there is no per-model record".into(),
        Msg::StatsRange { from, to } => format!("{from} to {to}").into(),
        Msg::StatsDays { n } => format!("{n} days").into(),
        Msg::StatsNoneInPeriod => "nothing was used in this period".into(),
        Msg::StatsColTokens => "tokens".into(),
        Msg::StatsColRequests => "requests".into(),
        Msg::StatsColShare => "share".into(),
        Msg::SettingUnset => "(unset)".into(),
        Msg::SettingHintToggle => "enter toggles".into(),
        Msg::SettingHintEdit => "enter edits".into(),
        Msg::SettingsNotFound => " not found".into(),
        Msg::LegendSave => "save".into(),
        Msg::LegendCancel => "cancel".into(),
        Msg::LegendChangePage => "change page".into(),
        Msg::LegendPagesHere => "the pages here".into(),
        Msg::LegendPageKeys => "page keys".into(),
        Msg::LegendScroll => "scroll".into(),
        Msg::LegendClose => "close".into(),
        Msg::LegendPressAgainToReset => "press again to restore the default".into(),
        Msg::LegendAnyOtherKey => "any other key".into(),
        Msg::LegendSelect => "select".into(),
        Msg::LegendEdit => "change".into(),
        Msg::LegendRestoreDefault => "restore the default".into(),
        Msg::LegendClearSearch => "clear the search".into(),

        // ── the host loop: what it says while it works (`plugin.rs`) ──
        Msg::SwitchedToSession { session } => format!("switched to session {session}").into(),
        Msg::TurnNotStored { message } => format!("this turn could not be stored: {message}").into(),
        Msg::McpServerNotConfigured { server } => format!("no MCP server named {server} is configured").into(),
        Msg::McpSignInLost => "the sign-in ended without an answer".into(),
        Msg::McpSignedInReloadLater { server } => format!(
            "signed in to {server} and saved the token; a turn is running, so run /mcp reload when it ends to connect it"
        )
        .into(),
        Msg::McpLoginUrl { server, url } => {
            format!("Authenticating MCP server {server}. If the browser did not open, open this link:\n{url}").into()
        }
        Msg::McpSignInAsking { host } => format!("connecting to {host}…").into(),
        Msg::McpSignInWaiting => "waiting for the browser…".into(),
        Msg::MouseTakenBackAuto => "the terminal took the mouse back; it has been asked for again. If it happens again, ctrl-o switches by hand".into(),
        Msg::ScreenNotConnectedProviders => "the screen is not connected; providers cannot be changed".into(),
        Msg::NoProviderPort => "this screen has no providers: the launcher provided no `tui-providers`".into(),
        Msg::ProviderEdited { id } => format!("{id} changed").into(),
        Msg::ProviderAddedAddModel { id } => format!("{id} added — now give it a model").into(),
        Msg::ProviderAdded { id } => format!("{id} added").into(),
        Msg::ProviderDeletedWithModels { id } => format!("{id} removed, and the models under it").into(),
        Msg::ProviderDeleted { id } => format!("{id} removed").into(),
        Msg::ConfigWrittenReloadFailed { error } => format!("the config was written, but the session could not be reloaded: {error}").into(),
        Msg::ToolCatalogUnreadable { why } => format!("cannot read the tool catalogue: {why}").into(),
        Msg::ScreenNotConnectedTools => "the screen is not connected; tools cannot be switched".into(),
        Msg::SwitchFailed { why } => format!("it did not work: {why}").into(),
        Msg::ScreenNotConnectedPlugins => "the screen is not connected; plugins cannot be changed".into(),
        Msg::PluginJobCancelled => "no longer waiting. It will clean up after itself once it lands".into(),
        Msg::PluginInstallingAt { plugin, marketplace } => format!("installing {plugin}@{marketplace} …").into(),
        Msg::PluginUpdatingAt { plugin, marketplace } => format!("updating {plugin}@{marketplace} …").into(),
        Msg::PluginUninstallingAt { plugin, marketplace } => format!("removing {plugin}@{marketplace} …").into(),
        Msg::ReloadFailedAfterPlugin { why } => format!("but the session could not be reloaded, so what arrived takes effect at the next start: {why}").into(),
        Msg::ScreenNotConnectedSettings => "the screen is not connected; settings cannot be changed".into(),
        Msg::NoSettingsPort => "this screen has no settings: the launcher provided no `tui-settings`".into(),
        Msg::SettingWrittenReloadFailed { why } => format!("the setting was written, but reloading failed: {why}").into(),
        Msg::NowViewing { name } => format!("viewing {name}").into(),
        Msg::PolicyBlockedNoWayOut => "the policy boundary stopped this step and offered no way on this time".into(),
        Msg::PolicyQuestion => "policy stopped this step. What now?".into(),
        Msg::PolicyAsker => "policy".into(),
        Msg::NotDelivered { error } => format!("not delivered: {error}").into(),
        Msg::Compacted => "folded".into(),
        Msg::NothingWorthCompacting => "there is nothing worth folding yet".into(),
        Msg::CompactFailed { error } => format!("folding did not work: {error}").into(),
        Msg::ClipboardHasNoImage => "there is no image on the clipboard".into(),
        Msg::ImagePreviewFailed { reason } => format!("could not open the image: {reason}").into(),
        Msg::NoOpener => "this front end cannot open files".into(),
        Msg::ImageGone => "that image is no longer available".into(),
        Msg::ImageCorrupt => "the image data is corrupt".into(),
        Msg::MouseTaken => "the mouse is taken: drag to select and copy, click a thought or a tool call to fold it, wheel to scroll, esc to drop the selection".into(),
        Msg::MouseHandedBack => "the mouse is the terminal's: use its own selection (which reaches into the scrollback). Fold with ctrl-t, reasoning with alt-r (hidden by default), scroll with pgup/pgdn, ctrl-o takes the mouse back".into(),
        Msg::NoProviderPanel => "this screen has no provider panel: the launcher provided no `tui-panel-providers`".into(),
        Msg::NoPluginPanel => "this screen has no plugin panel: the launcher provided no `tui-panel-plugins`".into(),
        Msg::NoToolPanel => "this screen has no tool panel: the launcher provided no `tui-panel-tools`".into(),
        Msg::CopiedSelection => "the selection was copied".into(),
        Msg::MenuCopySelection => "copy the selection".into(),
        Msg::MenuCopySelectionAbout => "put the selected text on the clipboard".into(),
        Msg::MenuCopyAll => "copy all of it".into(),
        Msg::MenuCopyAllAbout => "put the composer on the clipboard".into(),
        Msg::MenuPaste => "paste".into(),
        Msg::MenuPasteAbout => "insert from the clipboard".into(),
        Msg::MenuClear => "clear".into(),
        Msg::MenuClearAbout => "throw away the draft and what is attached".into(),
        Msg::MenuSend => "send".into(),
        Msg::MenuSendAbout => "hand this one to the model".into(),
        Msg::SelectionHasNoText => "the selection has no text to copy".into(),
        Msg::NothingToCopy => "there is nothing to copy".into(),
        Msg::ClipboardHasNoTextShort => "there is no text on the clipboard".into(),
        Msg::ModelCannotSeeImages { model } => format!("the model in use, `{model}`, cannot see images: pasting one would only drop it when the message is sent, so it was not pasted.\nSwitch to a model that can see images first.").into(),
        Msg::ModelUnknownForImages => "what model this agent uses is not known yet, so an image has nowhere to go and was not pasted.".into(),

        // ── installing a plugin: where it goes and what each action does (`plugins.rs`) ──
        Msg::ScopeUserName => "this machine".into(),
        Msg::ScopeProjectName => "this project".into(),
        Msg::ScopeLocalName => "only me".into(),
        Msg::ScopeUserAbout => "into ~/.atomcode/plugins — every project has it".into(),
        Msg::ScopeProjectAbout => "into .atomcode/plugins — it travels with the repository, so colleagues have it too".into(),
        Msg::ScopeLocalAbout => "into .atomcode/plugins/local — not in git, so only you have it".into(),
        Msg::ScopeUserShort => "machine".into(),
        Msg::ScopeLocalShort => "me".into(),
        Msg::PluginTabAll => "all".into(),
        Msg::PluginTabInstalled => "installed".into(),
        Msg::PluginTabMarkets => "marketplaces".into(),
        Msg::PluginActionUpdateAbout => "fetch it from the marketplace again: the old one out, the new one in".into(),
        Msg::PluginActionUninstallAbout => "take it away, and the skills and hooks it brought with it".into(),
        Msg::MarketActionBrowse => "see what it carries".into(),
        Msg::MarketActionRemove => "remove this marketplace".into(),
        Msg::MarketActionBrowseAbout => "back to the all page, filtered to its own".into(),
        Msg::MarketActionUpdateAbout => "pull it again and see whether it has new plugins".into(),
        Msg::MarketActionRemoveAbout => "take it away, and the plugins installed from it".into(),

        // ── how the conversation reads (`content.rs`) ──
        Msg::ThoughtLines { gutter, n } => format!("{gutter} thought, {n} lines").into(),
        Msg::ToolsRun { count } => format!("{count} tools were run").into(),
        Msg::ToolsFailed { failed } => format!(" · {failed} failed").into(),
        Msg::VerbSkill => "skill".into(),
        Msg::VerbMemory => "memory".into(),
        Msg::OutcomeInterrupted => "stopped".into(),
        Msg::OutcomeFailedWith { first } => format!("failed · {first}").into(),
        Msg::OutcomeLines { lines } => format!("{lines} lines").into(),
        Msg::RewindScopeConversation => "the conversation".into(),
        Msg::RewindScopeCode => "the workspace".into(),
        Msg::RewindScopeBoth => "the conversation and the workspace".into(),
        Msg::RewoundToTurn { what, turn } => format!("↶ {what} was taken back to before turn {turn}").into(),
        Msg::RewoundEarlier { what } => format!("↶ {what} was taken back to before an earlier turn").into(),
        Msg::AskRefuseChoice => "refuse".into(),
        Msg::TurnRounds { steps } => format!("{steps} rounds").into(),
        Msg::TurnTools { tools } => format!("{tools} tools").into(),
        Msg::StopCancelled => "stopped".into(),
        Msg::StopMaxRounds => "stopped · the rounds ran out".into(),
        Msg::StopByPolicy => "stopped · a stopping policy ended it (a deadline or a budget)".into(),
        Msg::StopRunawayFuse => "stopped · the backstop fuse blew; this tree has no stopping policy at all".into(),
        Msg::StopToolLoop => {
            "stopped · the model kept repeating the same step with no progress — send a new \
             message (rephrase or add a hint) to continue"
                .into()
        }
        Msg::StopPromptRejected => "stopped · the input was refused".into(),
        Msg::StopPolicyDenied => "stopped · a security policy blocked this step".into(),
        Msg::StopRateLimited => "paused · rate limited".into(),
        Msg::StopTimeout => "stopped · the model did not answer for a long time".into(),
        Msg::StopInvariantViolated => "stopped · an internal invariant was broken; this session should not be carried on".into(),
        Msg::StopMaxContinuations => "stopped · the automatic continuations ran out".into(),

        // ── the live strip and the folded-lines notes (`modules/live.rs`, `host.rs`) ──
        Msg::LiveStopping => "stopping".into(),
        Msg::LiveRecognizingImage => "recognizing image".into(),
        Msg::LiveWaiting => "waiting for the model".into(),
        Msg::LiveThinking => "thinking".into(),
        Msg::LiveWriting => "writing".into(),
        Msg::LiveSilentFor { secs } => format!("nothing new for {secs}s").into(),
        Msg::LiveRunningTools { n } => format!("running {n} tools").into(),
        Msg::LiveElapsed { took } => format!("{took} elapsed").into(),
        Msg::LiveIn { tokens } => format!("in {tokens}").into(),
        Msg::LiveOut { tokens } => format!("out {tokens}").into(),
        Msg::LiveCached { hit } => format!("cached {hit}").into(),
        Msg::LiveStep { step } => format!("step {step}").into(),
        Msg::FoldedLines { hidden } => format!("⋯ {hidden} lines folded — click to open").into(),
        Msg::MoreBelow { arrow, lines } => format!(" {arrow} {lines} more lines · click to go back to the bottom ").into(),

        // ── the plugin panel (`modules/plugins.rs`) ──
        Msg::AddMarketNotesHead => "it can be:".into(),
        Msg::AddMarketNoteHttps => "  · https://atomgit.com/someone/some-repo.git".into(),
        Msg::AddMarketNoteSsh => "  · git@atomgit.com:someone/some-repo.git".into(),
        Msg::AddMarketNoteLocal => "  · ./a/local/directory".into(),
        Msg::PluginsNoMarketsYet => "  there is no marketplace yet — add one on the marketplaces page".into(),
        Msg::PluginsNoMatch => "  no plugin matches".into(),
        Msg::PluginsNothingInstalled => "  nothing is installed yet".into(),
        Msg::PluginsNoMatchInstalled => "  nothing installed matches".into(),
        Msg::PluginsNoMatchMarkets => "  no marketplace matches".into(),
        Msg::PluginFormScopeTitle { plugin, marketplace } => format!("installing {plugin}@{marketplace} — where should it go").into(),
        Msg::PluginFormAddMarketTitle => "add a marketplace".into(),
        Msg::MarketPluginCount { n } => format!("{n} plugins").into(),
        Msg::MarketInstalledCount { n } => format!("{n} installed").into(),
        Msg::ArmedRemoveMarket => "press ^d again to remove this marketplace".into(),
        Msg::ArmedRemoveMarketWithPlugins { n } => format!("press ^d again to remove it, and the {n} plugins installed from it").into(),
        Msg::ArmedUninstall => "press ^d again to remove it".into(),
        Msg::FieldAddress => "address".into(),
        Msg::LegendStopWaiting => "stop waiting".into(),
        Msg::LegendAdd => "add it".into(),
        Msg::LegendThisOne => "this one".into(),
        Msg::LegendBack => "back".into(),
        Msg::LegendPressAgain => "press again".into(),
        Msg::LegendInstallOrOpen => "install it / look at it".into(),
        Msg::LegendUpdateOrRemove => "update or remove".into(),
        Msg::LegendOpen => "open".into(),
        Msg::LegendAddMarket => "add a marketplace".into(),
        Msg::LegendTakeAway => "take it away".into(),

        // ── the provider panel (`modules/providers.rs`) ──
        Msg::ProvidersNoMatch => "  no provider matches".into(),
        Msg::ProviderFormEdit { id } => format!("change {id}").into(),
        Msg::ProviderFormAddAccount => "add a provider".into(),
        Msg::ProviderFormAddModelTo { account } => format!("add a model to {account}").into(),
        Msg::ProviderFormAddModel => "add a model".into(),
        Msg::ProviderModelCount { n } => format!("{n} models").into(),
        Msg::ProviderHasKey => "has a key".into(),
        Msg::ProviderNoKey => "no key".into(),
        Msg::ProviderManaged => "managed by signing in".into(),
        Msg::ProviderUnconfigured => "not configured".into(),
        Msg::ProviderVision => "vision".into(),
        Msg::ArmedDelete => "press ^d again to delete".into(),
        Msg::FieldName => "name".into(),
        Msg::FieldProtocol => "protocol".into(),
        Msg::FieldKey => "key".into(),
        Msg::FieldVision => "sees images".into(),
        Msg::FieldEffort => "reasoning effort".into(),
        Msg::FieldLevels => "efforts it takes".into(),
        Msg::FieldWindow => "context".into(),
        Msg::FieldUseAfterSaving => "use it once saved".into(),
        Msg::VisionYes => "yes".into(),
        Msg::VisionNo => "no".into(),
        Msg::EffortUnsupported => "unsupported".into(),
        Msg::YesWord => "yes".into(),
        Msg::NoWord => "no".into(),
        Msg::KeyLeaveBlankToKeep => "(leave blank to keep it)".into(),
        Msg::LegendNextField => "next field".into(),
        Msg::LegendChangeValue => "change".into(),
        Msg::LegendPressAgainToDelete => "press again to delete".into(),
        Msg::LegendSeeItsModels => "see its models".into(),
        Msg::LegendSwitchToIt => "switch to it".into(),
        Msg::LegendChange => "change".into(),

        // ── the host side of the screen (`atomcode-cli`) ──
        Msg::HostConfigNotEditable => "this host's configuration cannot be changed from the screen".into(),
        Msg::HostHeld => "held".into(),
        Msg::SourceConfigFile => "config file".into(),
        Msg::SourceSettings => "settings".into(),
        Msg::SourceInstructionFiles => "instruction files".into(),
        Msg::SourceMemoryFiles => "memory files".into(),
        Msg::HostNoEditableConfig => "this host has no configuration that can be changed".into(),
        Msg::HostNoModelCatalog => "this host has no model catalogue".into(),
        Msg::NameCannotBeEmpty => "a name cannot be empty".into(),
        Msg::SettingThinking => "thinking".into(),
        Msg::RuntimeAlreadyStopped => "this session's runtime has already stopped".into(),
        Msg::NoProviderConfigured => "no provider is configured — add one before you can start".into(),
        Msg::LoginExpired => "the login has expired; sign in again".into(),
        Msg::ProviderUnsupportedByBuild => "this build does not support the configured provider".into(),
        Msg::ScreenNotConnectedAgent => "the screen is not connected to an agent".into(),
        Msg::HostHasNoControl => "this host has no control surface".into(),
        Msg::HostNotFoundShort => "not found: the session has been changed".into(),
        Msg::NoModelSelected => "no model is selected, so there is nowhere to write this".into(),
        Msg::RetryCountFor { selection } => format!("retries for {selection}").into(),
        Msg::NoSuchSetting { id } => format!("there is no setting called `{id}`").into(),

        // ── the provider forms' refusals (`atomcode-cli`) ──
        Msg::ProviderNameRules => "give it a name: letters, digits, `-`, `_`, `.`".into(),
        Msg::ProtocolNeedsEndpoint => "this protocol has no default address; one has to be given".into(),
        Msg::ManagedCannotEditUseLogin { id } => format!("{id} is managed by signing in and cannot be changed here; use /login").into(),
        Msg::ManagedCannotDeleteUseLogout { id } => format!("{id} is managed by signing in and cannot be removed here; use /logout").into(),
        Msg::NotInConfig { id } => format!("{id} is not in the config").into(),
        Msg::AccountModelsManaged { account } => format!("{account}'s models are managed by signing in").into(),
        Msg::ModelNameCannotBeEmpty => "a model name cannot be empty".into(),
        Msg::ManagedCannotEdit { id } => format!("{id} is managed by signing in and cannot be changed here").into(),
        Msg::ManagedCannotDelete { id } => format!("{id} is managed by signing in and cannot be removed here").into(),

        // ── installing plugins, from the launcher's side (`atomcode-cli`) ──
        Msg::SeedMarketFetched { name, plugins } => format!("fetched the marketplace this build ships with, {name} ({plugins} plugins)").into(),
        Msg::SeedPluginInstalled { plugin, marketplace } => format!("installed {plugin}@{marketplace}").into(),
        Msg::UpdatedToday => "updated today".into(),
        Msg::UpdatedYesterday => "updated yesterday".into(),
        Msg::UpdatedDaysAgo { days } => format!("updated {days} days ago").into(),
        Msg::PluginInstalledVerb => "installed".into(),
        Msg::PluginUpdatedVerb => "updated".into(),
        Msg::UninstallFailed { error } => format!("could not remove it: {error}").into(),
        Msg::Uninstalled { id } => format!("removed {id}").into(),
        Msg::MarketAddFailed { error } => format!("could not add it: {error}").into(),
        Msg::CancelledNothingLeft { what } => format!("cancelled — {what} was not left behind").into(),
        Msg::MarketCarriesNothing => "it carries no plugins".into(),
        Msg::MarketCarriesSome { n, names } => format!("it carries {n} plugins: {names} …").into(),
        Msg::MarketCarriesAll { n, names } => format!("it carries {n} plugins: {names}").into(),
        Msg::MarketAdded { name, source, carries } => format!("added the marketplace {name} ({source}). {carries}. Installing is still one at a time: `/plugin install <name>`, or ⏎ in the panel").into(),
        Msg::MarketUpdateFailed { error } => format!("could not update it: {error}").into(),
        Msg::MarketUpdated { name, commit, plugins } => format!("marketplace {name} updated to {commit}, carrying {plugins} plugins").into(),
        Msg::MarketRemoveFailed { error } => format!("could not remove it: {error}").into(),
        Msg::MarketRemoved { name } => format!("removed the marketplace {name}, and the plugins installed from it").into(),
        Msg::MarketRemovedWithLeftovers { name, failed } => format!("removed the marketplace {name}, but these were not cleaned up: {failed}").into(),
        Msg::PluginInstallFailed { error } => format!("it was not installed: {error}").into(),
        Msg::TallySkills { n } => format!("{n} skills").into(),
        Msg::TallyCommands { n } => format!("{n} commands").into(),
        Msg::TallyHooks => "hooks".into(),
        Msg::TallyBrought { what } => format!(", bringing {what}").into(),
        Msg::JobDidNotStart { error } => format!("the job did not start: {error}").into(),
        Msg::ListJoiner => ", ".into(),

        // ── signing in, from the screen (`atomcode-cli`) ──
        Msg::CmdAboutTuiLogin => "sign in and set up a provider; if already signed in, refresh the CodingPlan configuration".into(),
        Msg::SetupThreadDied => "the CodingPlan setup thread died".into(),
        Msg::ConfigWrittenReloadFailedCli { error } => format!("the config was written, but reloading failed: {error}").into(),
        Msg::SignedInSettingUpProvider => "signed in — setting up a provider…".into(),
        Msg::AlreadySignedInRefreshing => "already signed in — refreshing the CodingPlan configuration…".into(),
        Msg::TokenRejectedSigningInAgain => "the server no longer accepts this token — signing in again…".into(),
        Msg::SignedInAgainRerunningSetup => "signed in again — running setup once more…".into(),
        Msg::ConfigNotWritten { error } => format!("the config was not written: {error}").into(),
        Msg::LoginCouldNotStart { error } => format!("the login could not be started: {error}").into(),
        Msg::LoginFailed { error } => format!("the login failed: {error}").into(),
        Msg::LoginNoAnswer => "the login went quiet".into(),
        Msg::TokenExchangeFailed { error } => format!("exchanging the token failed: {error}").into(),
        Msg::SignedInCredentialsNotSaved { error } => format!("signed in, but the credentials were not saved: {error}").into(),
        Msg::ScanOrOpen => "scan it with your phone, or open it in a browser:".into(),
        Msg::InternalError { error } => format!("internal error: {error}").into(),

        // ── the first-run wizard (`atomcode-cli`) ──
        Msg::OnboardIntroTitle => "let us set this machine up first".into(),
        Msg::OnboardIntroLine1 => "there is no usable provider yet, so there is no work to be done.".into(),
        Msg::OnboardIntroLine2 => "pick the language, decide where the provider comes from, and look at what came of it.".into(),
        Msg::OnboardIntroLine3 => "esc leaves at any step; /onboarding comes back to it.".into(),
        Msg::OnboardLanguageTitle => "interface language".into(),
        Msg::OnboardLanguageChinese => "中文".into(),
        Msg::OnboardLanguageFollowSystem => "follow the system".into(),
        Msg::OnboardLanguageFollowSystemAbout => "decided by the environment variables".into(),
        Msg::OnboardSetupTitle => "where the provider comes from".into(),
        Msg::OnboardSetupCodingPlan => "sign in to CodingPlan".into(),
        Msg::OnboardSetupCodingPlanAbout => "scan a code; the free allowance comes with it".into(),
        Msg::OnboardSetupManual => "configure one myself".into(),
        Msg::OnboardSetupManualAbout => "for an API key of your own, or a model you host".into(),
        Msg::OnboardSetupSkip => "not now".into(),
        Msg::OnboardSetupSkipAbout => "this machine still cannot do any work".into(),
        Msg::OnboardSetupByHand => "opening the provider panel next: the API key, the address and the model all go in there".into(),
        Msg::OnboardWouldClearTitle => "this clears the screen".into(),
        Msg::OnboardWouldClearLine1 => "there is a conversation going here. a sign-in that succeeds opens a new session, and what is on screen now will not be in it.".into(),
        Msg::OnboardWouldClearLine2 => "enter goes on, esc leaves it alone.".into(),
        Msg::OnboardLoginTitle => "sign in".into(),
        Msg::OnboardFetchingLoginUrl => "fetching the sign-in address…".into(),
        Msg::OnboardConfirmTitle => "that is it".into(),
        Msg::OnboardModalTitle => "before you start".into(),
        Msg::OnboardAlreadyFinished => "the walkthrough is over".into(),
        Msg::OnboardLanguageSet { language } => format!("interface language: {language}").into(),
        Msg::OnboardLanguageNotWritten { error } => format!("the interface language was not written: {error}").into(),
        Msg::OnboardLoginSkipped => "signing in was skipped — there is still no provider, and /onboarding comes back to it".into(),
        Msg::OnboardSkipHint => "not now? enter skips it.".into(),
        Msg::OnboardSignedInConfigNotWritten { error } => format!("signed in, but the config was not written: {error}").into(),
        Msg::CmdAboutOnboarding => "set this machine up to work: language, sign in, and a look at the result".into(),
        Msg::CmdAboutOnboardingFinished => "the walkthrough is done".into(),
        Msg::ClassicOnlyForNow { command } => format!(
            "/{command} is only on the classic screen for now: quit and run `atomcode --classic` \
             (same sessions). It is coming to this screen."
        )
        .into(),
        Msg::CmdAboutClassicOnly => "only on the classic screen for now".into(),
        Msg::LivesInTheCli { command, run } => {
            format!("/{command} belongs to the command line: quit and run `{run}`.").into()
        }
        Msg::CmdAboutInTheCli => "belongs to the command line".into(),
        Msg::ShareNoModel => "no model is configured yet — run /login or /model first".into(),
        Msg::CmdAboutWebui => "share this session with a browser (lan to expose it; stop to end)".into(),
        Msg::WebuiTakes => "[lan | --host <addr> | stop]".into(),
        Msg::CmdAboutSync => "share this session without opening anything (off to stop)".into(),
        Msg::SyncTakes => "[off]".into(),
        Msg::CmdAboutDesktop => "open the desktop app".into(),
        Msg::CmdAboutApp => "share this session with the phone app (stop to end)".into(),
        Msg::AppTakes => "[stop]".into(),
        Msg::AppRelayDisabled => "remote access is off in this deployment (ATOMCODE_ENABLE_RELAY=0)".into(),
        Msg::AppRelayNotStarted { error, path } => {
            format!("the relay client would not start ({error}); tried `{path}`").into()
        }
        Msg::AppStopped => "The phone app can no longer reach this session.".into(),
        Msg::AppWasNotOn => "It was not reachable from the phone.".into(),
        Msg::AppPairTitle => "Pair the phone".into(),
        Msg::AppPairScan => "In the GitCode app: home → AtomCode → scan this code.".into(),
        Msg::AppPairType => "Or paste this pairing password into the app:".into(),
        Msg::AppPaired => {
            "The pairing code is out — once the app scans it, this session is on the phone; \
             `/app stop` ends it."
                .into()
        }
        Msg::CmdAboutAppPaired => "The pairing screen was closed.".into(),
        Msg::RelayNeedsLogin => {
            "the relay client is fetched from a signed-in release — run /login first".into()
        }
        Msg::RelayUnsupportedPlatform { os, arch, dir } => format!(
            "no relay client is published for {os}/{arch} — build one and put it in {dir}"
        )
        .into(),
        Msg::RelayDownloadOff { dir } => format!(
            "automatic download is off (ATOMCODE_RELAY_CLIENT_SKIP_DOWNLOAD=1) — put the relay client in {dir}"
        )
        .into(),
        Msg::RelayDownloadFailed {
            error,
            dir,
            releases,
            install,
        } => format!(
            "the relay client could not be downloaded: {error}\n\n\
             Get it yourself:\n\
             1. open {releases}\n\
             2. download the binary for this platform\n\
             3. save it as {dir}/atomcode-relay-client and make it executable\n\
             4. run /app again\n\n\
             Or in one line:\n{install}"
        )
        .into(),
        Msg::ShareStarted => "This session is shared — a browser or the phone app sees the same conversation.".into(),
        Msg::ShareStopped => "This session is no longer shared.".into(),
        Msg::ShareWasNotOn => "It was not being shared.".into(),
        Msg::DesktopOpening { name, path } => format!("Opening {name} ({path})").into(),
        Msg::DesktopLaunchFailed { path, error } => {
            format!("{path} would not start: {error}").into()
        }
        Msg::DesktopNotInstalled { url } => {
            format!("The desktop app is not installed here — {url}").into()
        }
        Msg::CmdAboutSchedule => "the scheduled tasks, and when each runs next".into(),
        Msg::CmdAboutOpenRouter => "connect OpenRouter's free models".into(),
        Msg::OpenRouterTakes => "[api key]".into(),
        Msg::OpenRouterConnecting => "Connecting OpenRouter…".into(),
        Msg::OpenRouterAuthorise { url } => {
            format!("Authorise in the browser. If it did not open: {url}").into()
        }
        Msg::OpenRouterNoAnswer => "No answer from the browser — cancelled or timed out.".into(),
        Msg::OpenRouterNoFreeModels => "OpenRouter returned no free models.".into(),
        Msg::OpenRouterConnected { added, default } => {
            format!("OpenRouter is connected: {added} free model(s) added, `{default}` is the one in force.").into()
        }
        Msg::OpenRouterNotReloaded { error } => format!(
            "OpenRouter is connected and saved, but this session was not reloaded ({error}); it takes effect on the next launch."
        )
        .into(),
        Msg::OpenRouterFailed { error } => format!("OpenRouter was not connected: {error}").into(),
        Msg::ScheduleNone => {
            "Nothing is scheduled. `atomcode schedule add` sets one up.".into()
        }
        Msg::ScheduleTaskLine {
            id,
            title,
            next,
            last,
            state,
        } => format!("{id} · {title} · next {next} · last {last} · {state}").into(),
        Msg::ScheduleOn => "on".into(),
        Msg::ScheduleOff => "off".into(),
        Msg::ScheduleEditInTheCli => {
            "Adding and removing is `atomcode schedule add` / `remove`; the OS scheduler runs them."
                .into()
        }
        Msg::CmdAboutProxy => "outbound proxy: follow the system, pin the current one, or none".into(),
        Msg::ProxyTakes => "[follow_system | default_proxy | no_proxy]".into(),
        Msg::ProxyPickerTitle { current } => format!("Outbound proxy (now: {current})").into(),
        Msg::ProxyFollowSystemAbout => {
            "the launch environment's proxy, with the system proxy filling gaps (default)".into()
        }
        Msg::ProxyDefaultProxyAbout { captured } => {
            format!("pin the proxy this launch started with into the config: {captured}").into()
        }
        Msg::ProxyNoProxyAbout => "no outbound request goes through a proxy".into(),
        Msg::ProxySet { summary } => {
            format!("Outbound proxy is now {summary}; the model connection was rebuilt.").into()
        }
        Msg::ProxySetNotReconnected { summary, error } => format!(
            "Outbound proxy is now {summary} and saved, but the model connection was not \
             rebuilt ({error}); it takes effect on the next connection."
        )
        .into(),
        Msg::ProxyUnknown { wanted } => format!(
            "No proxy mode called `{wanted}`: follow_system, default_proxy or no_proxy."
        )
        .into(),
        Msg::ProxySaveFailed { error } => {
            format!("The proxy setting was not written to the config file: {error}").into()
        }
        Msg::SteeringQueued => {
            "Queued — sent after the next tool call (press Ctrl+B to interrupt and send now)"
                .into()
        }
    }
}
