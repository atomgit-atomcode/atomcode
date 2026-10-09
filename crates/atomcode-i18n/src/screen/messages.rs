//! One variant per sentence the screen says.
//!
//! Named `<Surface><Detail>`, where the surface is the module that draws it —
//! `docs/i18n-style.md`. Variants carry **named** fields, never positional
//! ones: a translator reading `{rounds}` knows what it is, and a reordered
//! sentence in the other language cannot silently swap two arguments.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg<'a> {
    // ── the command dispatcher (`command.rs`) ──
    /// Typed `/xyz` and there is no such command, with nothing close either.
    CmdNoSuch {
        name: &'a str,
    },
    /// Typed `/xyz` and something close exists; `near` is already a
    /// space-separated list of `/name`s.
    CmdNoSuchDidYouMean {
        name: &'a str,
        near: &'a str,
    },

    // ── layout errors (`layout.rs`) ──
    LayoutNoSuchModule {
        name: &'a str,
        available: &'a str,
    },
    LayoutNotOnScreen {
        module: &'a str,
    },
    LayoutAlreadyOnScreen {
        module: &'a str,
    },
    LayoutDrawnTwice {
        module: &'a str,
    },

    // ── the moment (`moment.rs`) ──
    /// After the first Ctrl+C: press it again and the screen closes.
    MomentQuitAgain,

    // ── overlays (`overlay.rs`) ──
    OverlayEmptyFile,
    OverlayNoMatch,
    /// The key legend under a list in the bottom sheet (`crate::sheet`).
    /// `typed`: Enter on nothing matching takes what was typed (`/cd`).
    SheetListLegend {
        typed: bool,
    },
    /// The key legend under text being read in the bottom sheet. `back`: Esc
    /// returns to the list it was opened from, rather than closing.
    SheetReadLegend {
        back: bool,
    },
    /// The bottom sheet's document page (`/changelog`'s release): `tabs` when
    /// there is more than one page to switch between, `links` when the page
    /// showing has links to click.
    SheetDocLegend {
        back: bool,
        tabs: bool,
        links: bool,
    },
    /// `/diff`'s list: what it lists, the agent's changes or the working tree's.
    DiffListTitle {
        workspace: bool,
    },
    /// `/diff`'s summary line, before the `+a -r` totals.
    DiffFilesChanged {
        count: usize,
    },
    /// Bare `/view`: the list of files to pick one from.
    ViewPickerTitle,
    /// Bare `/view` with nothing in the file index.
    ViewPickerEmpty,

    // ── completion menu and policy interventions (`plugin.rs`) ──
    /// The one-word note beside a directory in the `@`-completion menu.
    MenuFolder,

    // ── the providers panel (`providers.rs`) ──

    // ── when a setting takes effect (`settings.rs`) ──
    AppliesImmediately,
    AppliesNextTurn,
    AppliesReload,
    AppliesReprepare,
    AppliesRestart,

    // ── shared widgets (`widget.rs`) ──
    ListEmpty,
    ListMore {
        count: usize,
    },

    // ── times and durations (`text.rs`) ──
    LastedSeconds {
        s: u64,
    },
    LastedMinutes {
        m: u64,
    },
    LastedMinutesSeconds {
        m: u64,
        s: u64,
    },
    LastedHours {
        h: u64,
    },
    LastedHoursMinutes {
        h: u64,
        m: u64,
    },

    // ── the tools tree (`tools.rs`) ──
    ToolsStateOn,
    ToolsStateOff,
    ToolsStateExcluded,
    ToolsExcludedByConfig {
        name: &'a str,
    },
    ToolsTurningOff {
        name: &'a str,
    },
    ToolsTurningOn {
        name: &'a str,
    },

    // ── the step-by-step wizard (`wizard.rs`) ──
    /// The ` · ← previous` tail, present only when there is a step to go back to.
    WizardBack,
    WizardNoteKeys {
        back: &'a str,
    },
    WizardChooseKeys {
        back: &'a str,
    },
    WizardTypeKeys {
        back: &'a str,
    },
    WizardWaitSkippableKeys,
    WizardWaitKeys,
    WizardWaiting {
        spinner: &'a str,
    },

    // ── what the agent asks a person (`ask.rs`) ──
    AskStepLimitQuestion,
    AskStepLimitTitle,
    AskTruncatedQuestion,
    AskTruncatedTitle,
    AskContinue,
    AskStop,
    AskYes,
    AskNo,
    AskAlwaysAllow,
    /// Prefix on a request that came from a team member rather than the lead.
    AskMemberRequests {
        name: &'a str,
    },
    AskLinesChars {
        lines: usize,
        chars: usize,
    },

    // ── the ask panel (`modules/ask.rs`) ──
    AskLegendChoose,
    AskLegendConfirm,
    AskFromMember {
        who: &'a str,
    },
    AskGrantWholeTool,
    AskGrantOnly {
        what: &'a str,
    },
    /// The row a person types an answer of their own on, while it is empty.
    AskTypeSomething,
    /// The row that gives up answering here and talks it over instead.
    AskChatInstead,
    /// The row that sends a multiple choice, and a batch's last page tab.
    AskSubmit,
    /// The row that keeps a multiple choice's answer and turns to the next
    /// question of a batch.
    AskNext,
    AskReviewTitle,
    AskReviewReady,
    AskReviewSend,
    AskReviewCancel,
    /// A batch question with no answer yet, on the review page.
    AskUnanswered,
    AskLegendToggle,
    AskLegendSwitch,
    /// Left and right, on a typing row with words in it.
    AskLegendCaret,

    // ── the input line (`modules/input.rs`) ──
    InputAnswerKeys,
    InputHistoryNth {
        nth: usize,
        total: usize,
    },
    /// A picture is on the clipboard, and `ctrl+v` takes it.
    InputClipboardImage,
    /// A picture is on the clipboard, on a terminal that keeps `ctrl+v` for its
    /// own text paste: `ctrl+alt+v` or `/paste` take it.
    InputClipboardImageAltOrCommand,
    /// The `Ctrl+R` search, with a hit: which of the history it landed on.
    InputSearchNth {
        query: &'a str,
        nth: usize,
        total: usize,
    },
    /// The same search with nothing matching — a state, not an error, so it is
    /// said in the same place and the same colour.
    InputSearchNone {
        query: &'a str,
    },
    /// The search, just opened: nothing typed yet, so nothing matched — say
    /// what to do rather than show a count of nothing.
    InputSearchPrompt,
    /// The dim line under the composer after you stop a turn yourself.
    ComposerInterrupted,

    // ── the status bar (`modules/status.rs`) ──
    StatusMember {
        name: &'a str,
    },
    StatusStopping,
    StatusGoal,
    StatusLoop,
    /// `kind` is already localised — [`Msg::StatusGoal`] or [`Msg::StatusLoop`].
    StatusRoundsHeld {
        kind: &'a str,
        /// Already composed by the caller: `4/12` when there is a cap, `4`
        /// when there is not.
        rounds: &'a str,
        why: &'a str,
    },
    StatusRounds {
        kind: &'a str,
        rounds: &'a str,
    },

    // ── the team panel (`modules/team.rs`) ──
    TeamHeaderFocused {
        count: usize,
        background: usize,
    },
    TeamHeader {
        count: usize,
        background: usize,
    },
    /// Every member idle and the lead's turn over: the panel as one line.
    TeamHeaderAllIdle {
        count: usize,
    },
    /// A member's report in the conversation, folded to this head.
    MemberReportedBack {
        name: &'a str,
    },
    /// A member whose turn was cancelled, folded to this head.
    MemberTurnCancelled {
        name: &'a str,
    },
    /// A member whose turn ended any other way than finishing.
    MemberTurnEndedEarly {
        name: &'a str,
        why: &'a str,
    },
    /// The lead's own row, first in the list.
    TeamLead,
    /// The role column of a background session's row.
    TeamBackgroundRole,
    /// A background session stopped on a question for the person.
    TeamBackgroundWaiting,
    /// Marks the member whose stream the screen is showing.
    TeamViewing,
    TeamWorkingRound {
        round: u64,
    },

    // ── the todo fold (`modules/todo.rs`) ──
    /// `/todo`: the plan, printed — finished items too, since that is when the
    /// panel has left and the list can be seen no other way.
    TodoListed,
    /// `/todo` with no plan in this conversation (or one that was cancelled).
    TodoNoPlan,
    /// `/todo` with a word it does not know.
    TodoUsage {
        other: &'a str,
    },
    TodoCounts {
        completed: usize,
        in_progress: usize,
        open: usize,
    },

    // ── the tools panel (`modules/tools.rs`) ──
    ToolsPanelCounts {
        on: usize,
        off: usize,
    },
    ToolsPanelNoneMounted,
    ToolsPanelNoMatch,
    ToolsPanelStopWaiting,
    ToolsLegendChoose,
    ToolsLegendToggle,
    ToolsLegendTyping,
    ToolsLegendFilter,
    ToolsLegendClose,

    // ── the rewind panel (`modules/rewind.rs`) ──
    /// The line under the panel's name: what a press in here would do.
    RewindPanelAbout,
    RewindPanelReading,
    RewindPanelGoing {
        turn: u64,
    },
    RewindPanelNoPoints,
    RewindPanelStopWaiting,
    /// The last row of the list: where the session is now, and the way out for
    /// someone who opened the panel and changed their mind.
    RewindPanelCurrent,
    RewindPanelNoCodeChanges,
    RewindPanelFiles {
        files: usize,
    },
    /// The second step's question: this turn goes back — carrying what?
    RewindPanelScopeAsk,
    /// The workspace half is off in this build, and how to turn it on.
    RewindCodeNotEnabled,
    /// This session is not written down, so there is nothing to put back.
    RewindCodeNoSession,
    /// It is on, but the checkpoint could not be set up — the host's own words
    /// about this machine, which are a fact rather than a sentence to write.
    RewindCodeFailed {
        why: &'a str,
    },
    RewindPanelTurnNoFiles,
    RewindLegendChoose,
    RewindLegendContinue,
    RewindLegendGo,
    RewindLegendBack,
    RewindLegendClose,
    RewindPointsUnreadable {
        why: &'a str,
    },
    RewindFailed {
        why: &'a str,
    },
    NoRewindPanel,
    NoResumePanel,
    NoSheetPanel,
    /// The launcher mounted the panel but no way to throw a session away.
    NoResumeStore,
    /// The launcher mounted no place to keep `/cd` bookmarks.
    NoPlaces,
    CdBookmarked,
    CdRecent,
    CdPinned {
        dir: &'a str,
    },
    CdUnpinned {
        dir: &'a str,
    },
    /// On the row a second Delete would throw away.
    ResumeDeleteArmed,
    /// While the last words of the selected session are being fetched.
    ResumePreviewWaiting,
    ResumeDeleted {
        id: &'a str,
    },
    ResumeDeleteFailed {
        why: &'a str,
    },
    NoRewind,
    ScreenNotConnectedRewind,

    // ── notes in the transcript (`modules/transcript.rs`) ──
    TranscriptCompacted {
        through: u64,
    },
    TranscriptShortened {
        count: usize,
    },
    /// A provider request failed and is being sent again. `reason` is the
    /// provider's own error, verbatim.
    TranscriptProviderRetry {
        reason: &'a str,
        seconds: u64,
        attempt: u32,
        max: u32,
    },
    /// The chip on an injected block: another session — one this tree has never
    /// held — reporting back (`modules/transcript.rs`).
    InjectedFromBackground,
    TranscriptDropped {
        through: u64,
    },
    /// A 429 the runtime is waiting out on its own; the turn goes on after it.
    TranscriptRateLimitWaiting {
        secs: u64,
    },
    /// The CodingPlan 5-hour window is used up and the turn stopped. `until` is
    /// the reset time the server gave (may be empty); `left` how long until it,
    /// already formatted (`2h11m`), when known.
    TranscriptWindowExhausted {
        until: &'a str,
        left: Option<&'a str>,
    },
    /// A 429 that is not the plan's window (a person's own model, or a gateway
    /// 429 with no window data) and stopped the turn. `reason` is the provider's
    /// own words when it gave any; `left` how long until a retry may work.
    TranscriptRateLimitedElsewhere {
        reason: Option<&'a str>,
        left: Option<&'a str>,
    },
    TranscriptMemberEnded,

    // ── what each command is for (`commands.rs`) ──
    CmdAboutQuit,
    CmdAboutReasoning,
    CmdAboutTools,
    CmdAboutShowInject,
    CmdAboutMouse,
    /// `/provider`'s model picker: the listing had nothing the account lacks.
    ProviderDiscoverNothingNew,
    /// …this account's protocol (or this screen) lists no models.
    ProviderDiscoverUnsupported,
    /// …while the listing is on its way.
    ProviderDiscovering,
    /// …beside an empty model field: the key that opens it.
    ProviderDiscoverHint,
    /// …its keys.
    ProviderPickKeys,
    /// …its filter line.
    ProviderPickFilter {
        query: &'a str,
    },
    /// …the listing could not be had, and why.
    ProviderDiscoverFailed {
        why: &'a str,
    },
    DiscoverNotAListing,
    DiscoverTimedOut,
    DiscoverTooLarge,
    /// `key`: a 401/403, which is the key's fault.
    DiscoverStatus {
        status: u16,
        key: bool,
    },
    DiscoverUnreachable,
    /// The word a `※ recap:` line opens with.
    RecapLabel,
    /// Ctrl+Z where it cannot stop: no job control (Windows), or started with
    /// suspending switched off.
    SuspendUnsupported,
    CmdAboutRaw,
    CmdAboutKeys,
    CmdAboutTodo,
    CmdAboutTeam,
    CmdAboutPaste,
    CmdAboutConfig,
    CmdAboutProviderPanel,
    CmdAboutCopy,
    CmdAboutSave,
    CmdAboutView,
    CmdAboutCompact,
    CmdAboutCancelAll,
    CmdAboutContext,
    CmdAboutAgents,
    CmdAboutTranscript,
    CmdAboutClear,
    CmdAboutSession,
    CmdAboutResume,
    CmdAboutEffort,
    CmdAboutUndo,
    CmdAboutRewind,
    CmdAboutModel,
    CmdAboutAutonomy,
    CmdAboutRename,
    CmdAboutDiff,
    CmdAboutMode,
    CmdAboutCd,
    CmdAboutPlan,
    CmdAboutBuild,
    CmdAboutAuto,
    CmdAboutStatus,
    CmdAboutCost,
    CmdAboutUsage,
    CmdAboutMcp,
    CmdAboutLanguage,
    CmdAboutReload,
    CmdAboutLogout,
    CmdAboutLogin,
    CmdAboutWhoami,
    CmdAboutThink,
    CmdAboutLook,
    CmdAboutHelp,

    // ── what a command takes, as it is shown after the name (`commands.rs`) ──
    CmdTakesPath,
    CmdTakesFilename,
    CmdTakesFile,
    CmdTakesSessionId,
    CmdTakesSessionIdRequired,
    CmdTakesTurn,
    CmdTakesTurnScope,
    CmdTakesModelId,
    CmdTakesName,
    CmdTakesDirectory,
    CmdTakesMcp,
    CmdTakesLanguage,

    // ── when the host refuses (`commands.rs`) ──
    HostBusy {
        reason: &'a str,
    },
    HostNotFound,
    HostSessionInUse {
        id: &'a str,
    },
    HostUnavailable,
    /// An undo or rewind based on less than the log now holds: a message or a
    /// turn arrived after what the screen had when it asked (`HostError::Stale`).
    HostStale,
    HostNoProvider {
        reason: &'a str,
    },
    HostSaidSomethingElse {
        reply: &'a str,
    },

    // ── the screen's own commands (`commands.rs`) ──
    NoClipboard,
    NoAgent,
    NoHost,
    /// One file's state in a `/diff` listing.
    DiffAdded,
    /// One file's state in a `/diff` listing.
    DiffAddedStaged,
    /// One file's state in a `/diff` listing.
    DiffModified,
    /// One file's state in a `/diff` listing.
    DiffModifiedStaged,
    /// One file's state in a `/diff` listing.
    DiffDeleted,
    /// One file's state in a `/diff` listing.
    DiffDeletedStaged,
    /// One file's state in a `/diff` listing.
    DiffRenamed,
    /// One file's state in a `/diff` listing.
    DiffUntracked,
    /// One file's state in a `/diff` listing.
    DiffConflicted,
    /// `/paste` found neither a picture nor text there.
    ClipboardHasNothing,
    /// A slash command was sent with pictures attached. No command takes
    /// them, so they went nowhere — and that has to be said.
    CommandCarriesNoPictures {
        count: usize,
    },
    /// `/team` with a word it does not know (`/todo` has `TodoUsage`).
    FoldUsage {
        name: &'a str,
        other: &'a str,
    },
    FileIsEmpty {
        path: &'a str,
    },
    FileUnreadable {
        path: &'a str,
        error: &'a str,
    },
    KeysHelp,
    ToolOutputUnknown {
        what: &'a str,
    },
    InjectionUnknown {
        what: &'a str,
        names: &'a str,
    },
    CopyNoSuchBlock {
        count: usize,
        asked: &'a str,
    },
    CopyNoBlocks,
    /// Nothing has been said to copy.
    CopyNothingYet,
    /// A form `/copy` does not take.
    CopyUsage,
    CopiedReply {
        lines: usize,
        chars: usize,
    },
    CopiedBlock {
        n: usize,
        lines: usize,
        chars: usize,
    },
    CopiedBlocks {
        count: usize,
        lines: usize,
        chars: usize,
    },
    SaveNothingYet,
    SavedTo {
        path: &'a str,
    },
    /// Refused: the target is a file this command did not write.
    SaveWouldOverwrite {
        path: &'a str,
    },
    SaveFailed {
        error: &'a str,
    },
    /// The status row, once an allowance window is close to spent.
    AllowanceNear {
        label: &'a str,
        percent: u8,
    },
    AllowanceNearWithReset {
        label: &'a str,
        percent: u8,
        resets_at: &'a str,
    },
    /// The same reading, for a host that reports a countdown and no clock.
    AllowanceNearWithCountdown {
        label: &'a str,
        percent: u8,
        duration: &'a str,
    },
    /// Said once, when a window is actually spent: where model access can come
    /// from instead, and the command that gets it.
    AllowanceExhaustedOpenRouter {
        label: &'a str,
    },
    ViewNotText {
        path: &'a str,
    },
    /// Said in the viewer's title, where it stays visible however far the
    /// reader scrolls — a notice on the last line is one a person meets only
    /// if they reach the end, and the whole point is that they cannot.
    ViewTooBig {
        mb: u64,
    },
    ViewOnlyFirstLines {
        lines: usize,
    },
    ViewLongLinesCut {
        lines: usize,
    },

    // ── the conversation's own commands (`commands.rs`) ──
    LookWhichSession,
    CancelledTurn,
    CancelledTurnAndMembers {
        members: usize,
    },
    NoCompaction,
    ContextCounts {
        turn: u64,
        messages: usize,
        facts: usize,
    },
    NothingSaidYet,
    NoRoster,
    AgentsLead,
    AgentsStopped {
        name: &'a str,
    },
    AgentsNoneYet,
    AgentsPickerHint,
    SessionNeedsNewerVersion {
        id: &'a str,
    },
    SessionTurnsWhenWhere {
        turns: u32,
        when: &'a str,
        dir: &'a str,
    },
    SessionTurnsWhen {
        turns: u32,
        when: &'a str,
    },
    ResumeNoOthers,
    ResumePickerHint,

    // ── background sessions (`bg.rs`, `modules/bg.rs`, the host's `background`) ──
    CmdAboutBg,
    CmdTakesBg,

    // ── `/review`: the current changes reviewed, in a session of their own ──
    CmdAboutReview,
    CmdTakesReview,
    /// What `/review` is about to do, for the line that says its session started
    /// (`审查未提交的改动`). The scope, in a person's words — the same parse that
    /// wrote the prompt, said out loud.
    ReviewWhatUncommitted,
    ReviewWhatStaged,
    ReviewWhatRange {
        base: &'a str,
    },
    /// `/review` 起来时的那一句:在做什么、有几个文件在变。`files` 是宿主量出来的;
    /// 量不出来(不在 git 仓库里之类)就只说范围。
    ///
    /// 不在这里许诺「结果回来我会逐条核实」:投递只交内容,要不要接着核实由那段对话自己定
    /// (见 [`Msg::BackgroundResult`])。
    ReviewStarted {
        what: &'a str,
        files: Option<usize>,
    },
    BgUsage,
    BgNoSuchSlot {
        slot: usize,
        count: usize,
    },
    BgMoved {
        slot: u32,
    },
    BgStarted {
        slot: u32,
    },
    /// The same line, saying what it is about to do — for the callers that know
    /// (`/review` knows the scope it just turned into a prompt).
    BgStartedWhat {
        slot: u32,
        what: &'a str,
    },
    BgDropped {
        slot: usize,
    },
    BgTold {
        title: &'a str,
    },
    BgNoPanel,
    BgPanelMoved,
    BgPanelLooking,
    BgGroupNeedsInput,
    BgGroupWorking,
    BgGroupCompleted,
    BgPanelEmpty,
    BgPlaceholder,
    BgReplyTo {
        title: &'a str,
    },
    BgLegend,
    /// The row armed for dropping, waiting for the second ctrl+d.
    BgDropArmed,
    BgKeys,
    BgNothingSaid,
    BgRefusedWhileSharing,
    BgRefusedWhileAsking,
    BgRefusedWhileReconfiguring,
    BgSlotsFull {
        most: usize,
    },
    BgQuitQuestion {
        count: usize,
    },
    BgQuitConfirm,
    BgReplyWaiting,
    BgQuitStay,
    BgWaitingTip {
        slot: usize,
        title: &'a str,
    },
    /// 那个后台会话挂着的已经不是这个请求了:答案不能安在别的问题上。
    BgAnswerStale,
    /// 前台答了一个后台会话的问询,送的时候它已经不在后台了(被带回前台、丢掉、结束):
    /// 答复没有送出。
    BgAnswerGone,
    /// 前台答了一个后台会话的问询,送的时候那个会话已经停了(命令通道关了):答复没有送出。
    BgAnswerUndelivered,
    /// 提上来的那个问询是谁在问：第几个后台会话、它叫什么。
    BgAsker {
        slot: usize,
        title: &'a str,
    },
    /// 一个后台问询这个屏幕画不出来：已按拒绝答复，好让那个会话不永远挂着。
    BgQuestionUnanswerable,
    /// A background session stopped without finishing: nothing comes home, so
    /// the conversation that may be waiting for it is told here.
    BgFailedTip {
        slot: usize,
        title: &'a str,
    },
    /// 一个后台会话的成果,投回发起它的那段对话(`background.rs`):就是内容本身。
    /// 不是一句"去 /bg 读" —— 那句把读它这件事留给了一个人;也不再附一句指令 ——
    /// 要不要接着核实,是那段对话看了上下文自己决定的事。
    BackgroundResult {
        title: &'a str,
        answer: &'a str,
    },
    /// 停下那行的账前头那两个字:让钟点有个落处(`停下 09:37`)。没有它,`✻ 09:37`
    /// 是一个光秃秃的时刻 —— 干净收尾那行是「词 钟点 · 数字」,这边也得是同一个语序。
    StopAt {
        at: &'a str,
    },

    // ── reasoning effort, undo and rewind (`commands.rs`) ──
    EffortAbout,
    EffortDefaultAbout,
    EffortPickerTitle {
        level: &'a str,
    },
    EffortPickerTitleDefault,
    EffortUnknown {
        wanted: &'a str,
        levels: &'a str,
    },
    EffortSet {
        wanted: &'a str,
    },
    EffortCurrent {
        now: &'a str,
        levels: &'a str,
    },
    EffortPickAfterModel {
        model: &'a str,
    },
    UndoLeadOnly,
    NotATurnNumber {
        what: &'a str,
    },
    RewindScopeUnknown {
        what: &'a str,
    },
    RewindRestored {
        files: usize,
    },

    // ── model, mode and working directory (`commands.rs`) ──
    ModelOnlyCurrent {
        current: &'a str,
    },
    ModelNoneConfigured,
    ModelPickerHint,
    ModelSet {
        wanted: &'a str,
    },
    ModeWhatEachDoes,
    ModeUnknown {
        what: &'a str,
    },
    ModeSet {
        mode: &'a str,
    },
    CdUpOneLevel,
    CdStepInto,
    CdStayHere,
    CdPickerHint {
        here: &'a str,
    },
    CdMovedNewSession {
        directory: &'a str,
        session: &'a str,
    },
    CdMoved {
        directory: &'a str,
    },

    // ── what changed, the language, and what is left on the account (`commands.rs`) ──
    DiffNoChangeIn {
        what: &'a str,
    },
    DiffNothingChanged,
    DiffBinary,
    NoLanguageSetting,
    LanguageNow {
        value: &'a str,
        accepts: &'a str,
    },
    LanguageSet {
        wanted: &'a str,
        applies: &'a str,
    },
    UsageNotCounted,
    UsageCallLimit {
        n: i64,
    },

    // ── running on its own, where the session stands, and who is signed in (`commands.rs`) ──
    AutonomyIdle,
    AutonomyGoal {
        what: &'a str,
    },
    AutonomyLoop {
        what: &'a str,
    },
    AutonomyRoundOf {
        round: u32,
        of: u32,
    },
    AutonomyRound {
        round: u32,
    },
    AutonomyLine {
        what: &'a str,
        rounds: &'a str,
        took: &'a str,
    },
    AutonomyHeld {
        line: &'a str,
        why: &'a str,
    },
    StatusNoModel,
    StatusEffortDefault,
    StatusSessionLine {
        session: &'a str,
    },
    StatusModelLine {
        model: &'a str,
        effort: &'a str,
    },
    StatusWhereLine {
        where_: &'a str,
    },
    StatusAutonomyLine {
        what: &'a str,
        round: u32,
        took: &'a str,
    },
    VisionFailedBecause {
        reason: &'a str,
    },
    CompactionInterrupted,
    ShellTimedOut {
        secs: u64,
    },
    ShellFailed {
        code: &'a str,
    },
    ShellSaidNothing,
    CostNothingYet,
    CostTokens {
        prompt: u64,
        completion: u64,
        cached: u64,
        rate: u64,
        total: u64,
    },
    CostUnattributed {
        tokens: u64,
    },
    RefusedStaleQuestion,
    RefusedNotRunning,
    RefusedUnavailable,
    RefusedUnsupported,
    ModelNotKept {
        error: &'a str,
    },
    RuntimeStopped {
        how: &'a str,
    },
    GoalMet {
        condition: &'a str,
    },
    GoalGaveUp {
        condition: &'a str,
    },
    WhoAmIUnnamed,
    WhoAmIStoredAt {
        path: &'a str,
    },
    WhoAmINobody,
    ThinkingNow {
        value: &'a str,
    },
    NoThinkingSwitch,
    NotOnOrOff {
        what: &'a str,
    },
    ThinkingSet {
        value: &'a str,
    },
    RenameNeedsName,
    RenamedTo {
        title: &'a str,
    },

    // ── MCP servers, reloading and signing in (`commands.rs`) ──
    McpNoneConfigured,
    McpUntrusted,
    McpNeedsAuthentication,
    McpFailed {
        message: &'a str,
    },
    McpDisabled,
    McpUnknownState,
    McpWithdrawn,
    McpNeedsServerName,
    /// `/mcp tools <server>` found none: with the server's state, so "connected
    /// and offers nothing" reads apart from "failed" or "still connecting".
    McpServerHasNoTools {
        server: &'a str,
        state: &'a str,
    },
    McpUnknownSubcommand {
        what: &'a str,
    },
    /// `/mcp help`: every subcommand, one line each.
    McpHelp,
    /// Over the per-server lines `/mcp reload` ends with.
    McpServersHeader,
    /// Servers held back because the project is not trusted, and the way out.
    McpBlockedTrustHint {
        count: usize,
    },
    /// Over the list `/mcp tools <server>` answers with.
    McpToolsHeader {
        server: &'a str,
    },
    /// `/mcp tools <server>` named no configured server.
    McpUnknownServer {
        name: &'a str,
        available: &'a str,
    },
    McpProjectTrusted,
    McpProjectUntrusted,
    McpProjectNotTrusted,
    McpLoginUsage,
    McpLogoutUsage,
    McpLoggedOut {
        server: &'a str,
    },
    Reloaded,
    SignedOut,
    SignedIn,

    // ── the MCP panel (`mcp.rs`, `modules/mcp.rs`) ──
    McpPanelTitle,
    McpPanelServers {
        n: usize,
    },
    McpPanelEmpty,
    /// The directory has servers, but the filter matched none. Distinct from
    /// [`Msg::McpPanelEmpty`] on purpose: telling a person "nothing is
    /// configured" when they are looking at a search box that matched nothing
    /// is a false statement about their own file.
    McpPanelNoMatch,
    /// `/mcp` with no argument, when this tree mounted no MCP panel at all. The
    /// row can be left out of a build's layout, and saying so beats a screen
    /// that looks like it did something.
    McpPanelUnavailable,
    /// The detail page, drawn the moment `Enter` is pressed and filled when the
    /// round trip comes back.
    McpDetailPending,
    /// The three values the host uses for where a server came from: `"global"`,
    /// `"project"`, `"driver"`. A value it does not know is drawn as it is.
    McpGroupGlobal,
    McpGroupProject,
    McpGroupDriver,
    /// The detail page's table: four labels, and how many tools are mounted.
    McpLabelState,
    McpLabelAuth,
    McpLabelEndpoint,
    McpLabelSource,
    McpLabelTools {
        n: usize,
    },
    /// What the detail page says about credentials.
    McpAuthNone,
    McpAuthAuthenticated,
    McpAuthNotAuthenticated,
    /// The six things an action can be.
    McpActionTrust,
    McpActionUntrust,
    McpActionLogin,
    McpActionLogout,
    McpActionEnable,
    McpActionDisable,
    /// The key legend: on the list, on the detail page, and while a round trip
    /// is out.
    McpLegendList,
    McpLegendDetail,
    McpLegendBusy,
    /// Something is running that cannot be stopped midway; Esc only hides it.
    McpLegendBusyHide,
    /// A cancel was asked for; waiting for it to stop.
    McpLegendCancelling,
    /// A panel sign-in was cancelled before the browser came back.
    McpSignInCancelled,

    // ── the toolbox and the plugins (`commands.rs`) ──
    CmdTakesToolbox,
    CmdAboutToolbox,
    NoToolCatalog,
    ToolboxUnknownVerb {
        what: &'a str,
    },
    ToolboxNeedsPattern {
        verb: &'a str,
    },
    ToolboxNothingMoved {
        pattern: &'a str,
    },
    ToolboxPutBack {
        names: &'a str,
    },
    ToolboxTurnedOff {
        names: &'a str,
    },
    ToolboxNameJoiner,
    CmdTakesPlugin,
    CmdAboutPlugin,
    PluginNoSuch {
        typed: &'a str,
    },
    PluginAmbiguous {
        name: &'a str,
        lines: &'a str,
    },
    NoPluginPort,
    /// The MCP panel has no port: this build mounted the screen without the
    /// launcher's `tui-mcp` row. The sibling of [`Msg::NoToolCatalog`].
    NoMcpPort,
    /// `takes` for `/setup`: what a person may type after it.
    CmdTakesSetup,
    /// `/setup`'s line in `/help` and in the slash menu.
    CmdAboutSetup,
    /// 这个屏幕没有接种子安装:启动器没有提供 `tui-setup`。`/setup` 在没装过种子的
    /// 机器上要靠它把种子放上磁盘,所以这条只有第一次用得着,但那时非它不可。
    NoSetupPort,
    /// 装种子的活没能派出去(线程起不来)。
    SetupJobDidNotStart {
        error: &'a str,
    },
    /// 装种子失败了。
    SetupFailed {
        error: &'a str,
    },
    /// 「正在装种子文件…」——装之前说,因为解压要一两秒,而一条从不出声的命令
    /// 看起来像没跑。
    SetupInstalling,
    /// 种子齐了、转发给 agent 之后说。说的是「这条命令接住了」,不是「模型答完了」
    /// ——后者是那个回合的事,由它在屏上自己出现。
    SetupRunningSkill,
    PluginNothingInstalled,
    PluginInstalledList {
        lines: &'a str,
    },
    PluginInstallWhich,
    PluginAlreadyInstalled {
        id: &'a str,
    },
    PluginInstalling {
        plugin: &'a str,
        market: &'a str,
    },
    PluginUninstallWhich,
    PluginNotInstalled {
        typed: &'a str,
    },
    PluginUninstalling {
        id: &'a str,
    },
    PluginUpdateWhich,
    PluginUpdating {
        id: &'a str,
    },
    MarketNoneYet,
    MarketRow {
        name: &'a str,
        source: &'a str,
        plugins: usize,
        installed: usize,
    },
    MarketList {
        lines: &'a str,
    },
    MarketAddWhich,
    MarketFetching {
        what: &'a str,
    },
    MarketRemoveWhich,
    MarketRemoving {
        what: &'a str,
    },
    MarketUpdateWhich,
    MarketUpdating {
        what: &'a str,
    },
    MarketUnknownAction {
        what: &'a str,
    },
    PluginUnknownAction {
        what: &'a str,
    },
    ReloadFailedAfter {
        said: &'a str,
        why: &'a str,
    },
    MarkdownUser,
    MarkdownAssistant,

    // ── the settings panel and what it shows about usage (`modules/settings.rs`) ──
    SettingsNoMatch,
    SettingsAbove {
        above: usize,
    },
    SettingsBelow {
        below: usize,
        arrow: &'a str,
    },
    SettingsTitle,
    AskingHost,
    UsageThisSession,
    UsageContextBar {
        percent: &'a str,
        used: &'a str,
        window: &'a str,
    },
    UsageModelNote {
        model: &'a str,
    },
    UsageAllowanceHead,
    UsageCallsUsedOfLimit {
        used: i64,
        limit: i64,
    },
    UsageSpentPercent {
        percent: u8,
        counted: &'a str,
    },
    UsageWindowNotReported,
    UsageSpent,
    UsageResetsAtNote {
        at: &'a str,
    },
    UsagePlanHead {
        plan: &'a str,
        state: &'a str,
    },
    UsagePlanTermPercent {
        percent: &'a str,
    },

    // ── what this build is running as (`modules/settings.rs`) ──
    StatusRowVersion,
    StatusRowSession,
    StatusRowSessionId,
    StatusRowDirectory,
    StatusRowSignedIn,
    StatusRowPlan,
    StatusRowUsage,
    StatusNoAccount,
    StatusWhoDetail {
        who: &'a str,
        detail: &'a str,
    },
    StatusModelEffort {
        model: &'a str,
        effort: &'a str,
    },
    StatusPlanExpires {
        at: &'a str,
    },
    StatusPlanDaysLeft {
        remaining: i32,
        total: i32,
    },
    StatusWindowSpent {
        percent: u8,
    },
    StatusWindowNotReported,
    StatusWindowResetsIn {
        duration: &'a str,
    },
    McpTallyFailed {
        n: usize,
    },
    McpTallyUntrusted {
        n: usize,
    },
    McpTallyNeedsAuthentication {
        n: usize,
    },
    McpTallyConnecting {
        n: usize,
    },
    McpTallyConnected {
        n: usize,
    },
    McpTallyOff {
        n: usize,
    },
    McpTallyDisabled {
        n: usize,
    },
    StatsNotKept,
    /// The figures were asked for and did not come back. **Not** the same
    /// sentence as [`Msg::StatsNotKept`], which is a host that keeps none.
    StatsUnknown {
        why: &'a str,
    },

    // ── the account's figures (`modules/settings.rs`) ──
    StatsNoDaily,
    StatsDailyHead,
    StatsNoModels,
    StatsRange {
        from: &'a str,
        to: &'a str,
    },
    StatsDays {
        n: usize,
    },
    StatsNoneInPeriod,
    StatsColTokens,
    StatsColRequests,
    StatsColShare,
    SettingUnset,
    SettingHintToggle,
    SettingHintEdit,
    SettingsNotFound,
    /// A configuration file listed a second time under another name — the
    /// project's memory when the project is the home directory.
    SourceSameFileAs {
        label: &'a str,
    },
    LegendSave,
    LegendCancel,
    LegendChangePage,
    LegendPagesHere,
    LegendPageKeys,
    LegendScroll,
    LegendClose,
    LegendPressAgainToReset,
    LegendAnyOtherKey,
    LegendSelect,
    LegendEdit,
    LegendRestoreDefault,
    LegendClearSearch,

    // ── the host loop: what it says while it works (`plugin.rs`) ──
    SwitchedToSession {
        session: &'a str,
    },
    TurnNotStored {
        message: &'a str,
    },
    /// A panel login's authorization URL, for when the browser did not open.
    McpLoginUrl {
        server: &'a str,
        url: &'a str,
    },
    /// What a running sign-in is waiting on. Shown on the panel's own row while
    /// it runs, which without it reads the same from the first second to the
    /// last — the difference between working and wedged.
    McpSignInAsking {
        host: &'a str,
    },
    /// …and the other half of the same wait: the browser is open.
    McpSignInWaiting,
    /// Signing in to a server the config does not define.
    McpServerNotConfigured {
        server: &'a str,
    },
    /// The sign-in's thread ended without answering.
    McpSignInLost,
    /// Signed in, but a turn is running, so the reconnect has to wait.
    McpSignedInReloadLater {
        server: &'a str,
    },
    MouseTakenBackAuto,
    ScreenNotConnectedProviders,
    NoProviderPort,
    ProviderEdited {
        id: &'a str,
    },
    /// The tip row, when a saved account answered as it should.
    ProviderProbePassed,
    /// The tip row, when it did not: the reason, with the address to use, is in
    /// the conversation — the tip row holds one line and fades.
    ProviderProbeFailed,
    ProviderAddedAddModel {
        id: &'a str,
    },
    ProviderAdded {
        id: &'a str,
    },
    ProviderDeletedWithModels {
        id: &'a str,
    },
    ProviderDeleted {
        id: &'a str,
    },
    ConfigWrittenReloadFailed {
        error: &'a str,
    },
    ToolCatalogUnreadable {
        why: &'a str,
    },
    ScreenNotConnectedTools,
    SwitchFailed {
        why: &'a str,
    },
    ScreenNotConnectedPlugins,
    PluginJobCancelled,
    PluginInstallingAt {
        plugin: &'a str,
        marketplace: &'a str,
    },
    PluginUpdatingAt {
        plugin: &'a str,
        marketplace: &'a str,
    },
    PluginUninstallingAt {
        plugin: &'a str,
        marketplace: &'a str,
    },
    ReloadFailedAfterPlugin {
        why: &'a str,
    },
    ScreenNotConnectedSettings,
    NoSettingsPort,
    SettingWrittenReloadFailed {
        why: &'a str,
    },
    NowViewing {
        name: &'a str,
    },
    PolicyBlockedNoWayOut,
    PolicyQuestion,
    PolicyAsker,
    NotDelivered {
        error: &'a str,
    },
    Compacted,
    NothingWorthCompacting,
    CompactFailed {
        error: &'a str,
    },
    ClipboardHasNoImage,
    /// A link in the conversation was clicked and is being handed to the
    /// browser — said on the tip row, since the browser may come up behind.
    OpeningLink {
        url: &'a str,
    },
    /// The desktop declined to open a clicked link (no display, over SSH, …).
    OpenLinkFailed {
        reason: &'a str,
    },
    /// Opening an attached image in the desktop viewer did not work.
    ImagePreviewFailed {
        reason: &'a str,
    },
    /// This front end cannot show a file — no desktop opener is wired.
    NoOpener,
    /// The clicked image marker no longer resolves to any bytes.
    ImageGone,
    /// The attached image's stored bytes could not be decoded.
    ImageCorrupt,
    MouseTaken,
    /// Most important first — how to take the mouse back — because the tip
    /// row is one right-aligned row that cuts what does not fit at the end.
    MouseHandedBack,
    /// Printed above the conversation `/raw` puts on the terminal's own screen.
    RawTop,
    /// …and under it: what this screen is for and the way back.
    RawBottom,
    /// Said once at start when `[ui] mouse = false` hands the pointer to the
    /// terminal from the first frame.
    MouseHandedBackAtStart,
    /// The terminal reports no mouse (HarmonyOS): what works instead, at start.
    MouseUnreportedAtStart,
    /// ctrl-g on a terminal that reports no mouse.
    MouseUnreported,
    /// Said once, after a turn whose reasoning is off the screen: the key that
    /// brings it back, since nothing on screen says there is any.
    ReasoningHiddenHint,
    NoProviderPanel,
    NoPluginPanel,
    NoToolPanel,
    CopiedSelection,
    MenuCopySelection,
    MenuCopySelectionAbout,
    /// The conversation menu's copy with nothing selected.
    MenuCopy,
    MenuCopyNothingAbout,
    MenuCopyAll,
    MenuCopyAllAbout,
    MenuPaste,
    MenuPasteAbout,
    MenuClear,
    MenuClearAbout,
    MenuSend,
    MenuSendAbout,
    SelectionHasNoText,
    NothingToCopy,
    ClipboardHasNoTextShort,
    ModelCannotSeeImages {
        model: &'a str,
    },
    ModelUnknownForImages,

    // ── installing a plugin: where it goes and what each action does (`plugins.rs`) ──
    ScopeUserName,
    ScopeProjectName,
    ScopeLocalName,
    ScopeUserAbout,
    ScopeProjectAbout,
    ScopeLocalAbout,
    ScopeUserShort,
    ScopeLocalShort,
    PluginTabAll,
    PluginTabInstalled,
    PluginTabMarkets,
    PluginActionUpdateAbout,
    PluginActionUninstallAbout,
    MarketActionBrowse,
    MarketActionRemove,
    MarketActionBrowseAbout,
    MarketActionUpdateAbout,
    MarketActionRemoveAbout,

    // ── how the conversation reads (`content.rs`) ──
    ThoughtLines {
        gutter: &'a str,
        n: usize,
    },
    ToolsRun {
        count: usize,
    },
    ToolsFailed {
        failed: usize,
    },
    VerbSkill,
    VerbMemory,
    /// A question the model put to the person, as its row in the conversation.
    VerbAsk,
    /// Pictures the person attached to an answer, after the answer.
    AskAttachedImages {
        count: usize,
    },
    OutcomeInterrupted,
    OutcomeFailedWith {
        first: &'a str,
    },
    OutcomeLines {
        lines: usize,
    },
    RewindScopeConversation,
    RewindScopeCode,
    RewindScopeBoth,
    RewoundToTurn {
        what: &'a str,
        turn: u64,
    },
    RewoundEarlier {
        what: &'a str,
    },
    AskRefuseChoice,
    TurnRounds {
        steps: u32,
    },
    TurnTools {
        tools: u32,
    },
    /// A turn's cache hit, on its closing line — the last request of this turn
    /// only, so it is said apart from the session's figure on the status row.
    TurnCached {
        pct: u8,
    },
    /// The session's cache hit, on the status row.
    SessionCached {
        pct: u64,
    },
    StopCancelled,
    StopMaxRounds,
    StopByPolicy,
    StopRunawayFuse,
    StopToolLoop,
    StopPromptRejected,
    StopPolicyDenied,
    StopRateLimited,
    StopTimeout,
    StopInvariantViolated,
    StopMaxContinuations,
    /// A turn the model ended on its own while the task list it kept still had
    /// open items: not a failure, and not a finish either.
    StopWithOpenItems {
        count: usize,
    },

    // ── the live strip and the folded-lines notes (`modules/live.rs`, `host.rs`) ──
    LiveStopping,
    /// Shown while the runtime's VL helper is turning a pasted picture into text
    /// for a non-vision model — before the turn's first fact, so the screen is
    /// not blank during the seconds that recognition takes.
    LiveRecognizingImage,
    /// Shown while a compaction's summary is being written and the request waits
    /// on it — the slow tier only: a cheap fold of tool output is instant and
    /// says nothing.
    LiveCompacting,
    /// The turn is over and work this conversation started is still running
    /// out of view; its results will come back here.
    LiveWaitingForBackground {
        n: usize,
    },
    LiveWaiting,
    LiveThinking,
    LiveWriting,
    /// How long the turn in flight has had nothing new, once that is long
    /// enough to be worth saying. A slow model and a stalled one look identical
    /// on a row that only counts the turn's own age.
    LiveSilentFor {
        secs: u64,
    },
    LiveRunningTools {
        n: u32,
    },
    LiveElapsed {
        took: &'a str,
    },
    LiveIn {
        tokens: &'a str,
    },
    LiveOut {
        tokens: &'a str,
    },
    LiveCached {
        hit: &'a str,
    },
    LiveStep {
        step: u32,
    },
    FoldedLines {
        hidden: usize,
    },
    MoreBelow {
        arrow: &'a str,
        lines: usize,
    },

    // ── the plugin panel (`modules/plugins.rs`) ──
    AddMarketNotesHead,
    AddMarketNoteHttps,
    AddMarketNoteSsh,
    AddMarketNoteLocal,
    PluginsNoMarketsYet,
    PluginsNoMatch,
    PluginsNothingInstalled,
    PluginsNoMatchInstalled,
    PluginsNoMatchMarkets,
    PluginFormScopeTitle {
        plugin: &'a str,
        marketplace: &'a str,
    },
    PluginFormAddMarketTitle,
    MarketPluginCount {
        n: usize,
    },
    MarketInstalledCount {
        n: usize,
    },
    ArmedRemoveMarket,
    ArmedRemoveMarketWithPlugins {
        n: usize,
    },
    ArmedUninstall,
    FieldAddress,
    LegendStopWaiting,
    LegendAdd,
    LegendThisOne,
    LegendBack,
    LegendPressAgain,
    LegendInstallOrOpen,
    LegendUpdateOrRemove,
    LegendOpen,
    LegendAddMarket,
    LegendTakeAway,

    // ── the provider panel (`modules/providers.rs`) ──
    ProvidersNoMatch,
    ProviderFormEdit {
        id: &'a str,
    },
    ProviderFormAddAccount,
    ProviderFormAddModelTo {
        account: &'a str,
    },
    ProviderFormAddModel,
    ProviderModelCount {
        n: usize,
    },
    ProviderHasKey,
    ProviderNoKey,
    ProviderManaged,
    ProviderUnconfigured,
    ProviderVision,
    ArmedDelete,
    FieldName,
    FieldProtocol,
    FieldKey,
    FieldVision,
    FieldEffort,
    FieldLevels,
    FieldWindow,
    FieldUseAfterSaving,
    VisionYes,
    VisionNo,
    EffortUnsupported,
    YesWord,
    NoWord,
    KeyLeaveBlankToKeep,
    LegendNextField,
    LegendChangeValue,
    /// Space on the levels row: turn the level under the brackets on or off.
    LegendToggleLevel,
    /// The arrows on the levels row: move between the levels.
    LegendPickLevel,
    /// A model form's context window left to the protocol's own default.
    WindowAutomatic,
    /// The last stop on a model form's window row: a window typed by hand.
    WindowCustom,
    /// What the custom window field takes.
    LegendTypeWindow,
    /// The arrows on the custom window field: leave it for the presets.
    LegendBackToPresets,
    LegendPressAgainToDelete,
    LegendSeeItsModels,
    LegendSwitchToIt,
    LegendChange,

    // ── the host side of the screen (`atomcode-cli`) ──
    HostConfigNotEditable,
    HostHeld,
    SourceConfigFile,
    SourceSettings,
    SourceInstructionFiles,
    SourceMemoryFiles,
    HostNoEditableConfig,
    HostNoModelCatalog,
    NameCannotBeEmpty,
    SettingThinking,
    RuntimeAlreadyStopped,
    NoProviderConfigured,
    LoginExpired,
    ProviderUnsupportedByBuild,
    ScreenNotConnectedAgent,
    HostHasNoControl,
    HostNotFoundShort,
    NoModelSelected,
    RetryCountFor {
        selection: &'a str,
    },
    NoSuchSetting {
        id: &'a str,
    },

    // ── the provider forms' refusals (`atomcode-cli`) ──
    ProviderNameRules,
    ProtocolNeedsEndpoint,
    ManagedCannotEditUseLogin {
        id: &'a str,
    },
    ManagedCannotDeleteUseLogout {
        id: &'a str,
    },
    NotInConfig {
        id: &'a str,
    },
    IdAlreadyInConfig {
        id: &'a str,
    },
    ModelAlreadyUnderAccount {
        model: &'a str,
    },
    LegacyHasNoDisplayName {
        id: &'a str,
    },
    AccountModelsManaged {
        account: &'a str,
    },
    ModelNameCannotBeEmpty,
    ManagedCannotEdit {
        id: &'a str,
    },
    ManagedCannotDelete {
        id: &'a str,
    },

    // ── installing plugins, from the launcher's side (`atomcode-cli`) ──
    SeedMarketFetched {
        name: &'a str,
        plugins: usize,
    },
    SeedPluginInstalled {
        plugin: &'a str,
        marketplace: &'a str,
    },
    UpdatedToday,
    UpdatedYesterday,
    UpdatedDaysAgo {
        days: u64,
    },
    PluginInstalledVerb,
    PluginUpdatedVerb,
    UninstallFailed {
        error: &'a str,
    },
    Uninstalled {
        id: &'a str,
    },
    MarketAddFailed {
        error: &'a str,
    },
    CancelledNothingLeft {
        what: &'a str,
    },
    MarketCarriesNothing,
    MarketCarriesSome {
        n: usize,
        names: &'a str,
    },
    MarketCarriesAll {
        n: usize,
        names: &'a str,
    },
    MarketAdded {
        name: &'a str,
        source: &'a str,
        carries: &'a str,
    },
    MarketUpdateFailed {
        error: &'a str,
    },
    MarketUpdated {
        name: &'a str,
        commit: &'a str,
        plugins: usize,
    },
    MarketRemoveFailed {
        error: &'a str,
    },
    MarketRemoved {
        name: &'a str,
    },
    MarketRemovedWithLeftovers {
        name: &'a str,
        failed: &'a str,
    },
    PluginInstallFailed {
        error: &'a str,
    },
    TallySkills {
        n: usize,
    },
    TallyCommands {
        n: usize,
    },
    TallyHooks,
    TallyBrought {
        what: &'a str,
    },
    JobDidNotStart {
        error: &'a str,
    },
    ListJoiner,

    // ── signing in, from the screen (`atomcode-cli`) ──
    CmdAboutTuiLogin,
    SetupThreadDied,
    ConfigWrittenReloadFailedCli {
        error: &'a str,
    },
    SignedInSettingUpProvider,
    AlreadySignedInRefreshing,
    TokenRejectedSigningInAgain,
    SignedInAgainRerunningSetup,
    ConfigNotWritten {
        error: &'a str,
    },
    LoginCouldNotStart {
        error: &'a str,
    },
    LoginFailed {
        error: &'a str,
    },
    LoginNoAnswer,
    TokenExchangeFailed {
        error: &'a str,
    },
    SignedInCredentialsNotSaved {
        error: &'a str,
    },
    ScanOrOpen,
    InternalError {
        error: &'a str,
    },

    // ── the first-run wizard (`atomcode-cli`) ──
    OnboardIntroTitle,
    OnboardIntroLine1,
    OnboardIntroLine2,
    OnboardIntroLine3,
    OnboardLanguageTitle,
    OnboardLanguageChinese,
    OnboardLanguageFollowSystem,
    OnboardLanguageFollowSystemAbout,
    OnboardSetupTitle,
    OnboardSetupCodingPlan,
    OnboardSetupCodingPlanAbout,
    OnboardSetupManual,
    OnboardSetupManualAbout,
    OnboardSetupSkip,
    OnboardSetupSkipAbout,
    OnboardSetupByHand,
    OnboardLoginTitle,
    OnboardFetchingLoginUrl,
    OnboardConfirmTitle,
    OnboardWouldClearTitle,
    OnboardWouldClearLine1,
    OnboardWouldClearLine2,
    OnboardModalTitle,
    OnboardAlreadyFinished,
    OnboardLanguageSet {
        language: &'a str,
    },
    OnboardLanguageNotWritten {
        error: &'a str,
    },
    OnboardLoginSkipped,
    OnboardSkipHint,
    OnboardSignedInConfigNotWritten {
        error: &'a str,
    },
    CmdAboutOnboarding,
    CmdAboutOnboardingFinished,
    /// A command the classic screen has and this one does not have yet — typed
    /// here, it says where it still lives rather than "no such command".
    ClassicOnlyForNow {
        command: &'a str,
    },
    CmdAboutClassicOnly,
    /// A command this screen does not have because it belongs to the command
    /// line — `/upgrade` replaces the binary, which a screen inside it cannot.
    LivesInTheCli {
        command: &'a str,
        run: &'a str,
    },
    CmdAboutInTheCli,
    /// The confirmation before a rollback (titled by the product's
    /// `UpgradeOptRollback`).
    RollbackConfirmSwitch,
    RollbackConfirmNoUpdates,
    RollbackConfirmKeys,
    /// No `.bak` next to the binary: nothing to roll back to.
    RollbackNothingToRollBackTo {
        path: &'a str,
    },
    /// Sharing a session needs a model to be configured first.
    ShareNoModel,
    CmdAboutWebui,
    WebuiTakes,
    CmdAboutSync,
    SyncTakes,
    CmdAboutDesktop,
    CmdAboutApp,
    AppTakes,
    AppRelayDisabled,
    AppRelayNotStarted {
        error: &'a str,
        path: &'a str,
    },
    AppStopped,
    AppWasNotOn,
    AppPairTitle,
    AppPairScan,
    AppPairType,
    /// 配对屏关掉了:码已经给出去,手机扫到就进这个会话。
    AppPaired,
    /// 配对屏关掉时派发的那行命令——隐藏,没人手打。
    CmdAboutAppPaired,
    /// 取中继客户端要先登录(它在受保护的 release 里)。
    RelayNeedsLogin,
    RelayUnsupportedPlatform {
        os: &'a str,
        arch: &'a str,
        dir: &'a str,
    },
    RelayDownloadOff {
        dir: &'a str,
    },
    RelayDownloadFailed {
        error: &'a str,
        dir: &'a str,
        releases: &'a str,
        install: &'a str,
    },
    ShareStarted,
    ShareStopped,
    ShareWasNotOn,
    DesktopOpening {
        name: &'a str,
        path: &'a str,
    },
    DesktopLaunchFailed {
        path: &'a str,
        error: &'a str,
    },
    DesktopNotInstalled {
        url: &'a str,
    },
    CmdAboutSchedule,
    CmdAboutOpenRouter,
    OpenRouterTakes,
    OpenRouterConnecting,
    /// A bare `/openrouter` went on with the key saved last time.
    OpenRouterUsingSavedKey,
    /// The saved key was refused; the browser is next.
    OpenRouterSavedKeyRejected,
    /// The saved key could not be checked (offline, a proxy); used anyway.
    OpenRouterSavedKeyUnchecked,
    OpenRouterAuthorise {
        url: &'a str,
    },
    OpenRouterNoAnswer,
    OpenRouterNoFreeModels,
    /// `/openrouter` finished: how many free models came in, how many of the
    /// ones an earlier run added went (no longer free, or no longer among the
    /// current ones), and the default model now.
    OpenRouterConnected {
        added: usize,
        removed: usize,
        default: &'a str,
    },
    /// The default was one of the free models `/openrouter` just removed, so it
    /// was moved to the first current one.
    OpenRouterDefaultReplaced {
        from: &'a str,
        to: &'a str,
    },
    /// The default was a model OpenRouter no longer offers; it is now `to`.
    OpenRouterDefaultRetired {
        from: &'a str,
        to: &'a str,
    },
    /// Models removed because OpenRouter no longer offers them at all, whoever
    /// added them.
    OpenRouterRetired {
        names: &'a str,
    },
    OpenRouterNotReloaded {
        error: &'a str,
    },
    OpenRouterFailed {
        error: &'a str,
    },
    ScheduleNone,
    ScheduleTaskLine {
        id: &'a str,
        title: &'a str,
        next: &'a str,
        last: &'a str,
        state: &'a str,
    },
    ScheduleOn,
    ScheduleOff,
    ScheduleEditInTheCli,
    CmdAboutProxy,
    ProxyTakes,
    ProxyPickerTitle {
        current: &'a str,
    },
    ProxyFollowSystemAbout,
    ProxyDefaultProxyAbout {
        captured: &'a str,
    },
    ProxyNoProxyAbout,
    ProxySet {
        summary: &'a str,
    },
    ProxySetNotReconnected {
        summary: &'a str,
        error: &'a str,
    },
    ProxyUnknown {
        wanted: &'a str,
    },
    ProxySaveFailed {
        error: &'a str,
    },
    /// Header above the type-ahead queue: lines typed while a turn runs, folded
    /// in at the next tool-call boundary (or flushed immediately with Esc).
    SteeringQueued,

    // ── a command the phone or the browser asked this screen to run (`remote.rs`) ──
    /// Said on the terminal too, so the person at the keyboard knows the far
    /// end did something.
    RemoteRan {
        command: &'a str,
    },
    /// Sent back to the far end when it asked for something it may not have.
    RemoteDesktopOnly,
    /// The meter was asked and did not answer. **Not** the same sentence as
    /// [`Msg::UsageNotCounted`], which is a host that counts nothing.
    UsageUnknown {
        why: &'a str,
    },
    /// Tacked onto a "copied" line when nothing could confirm it: the text
    /// went out as OSC 52 and the terminal does not answer.
    CopyHandedOver,
    /// `/paste <file>` on something too big to put in the composer. **Not**
    /// [`Msg::FileUnreadable`]: it reads fine, it just must not go there.
    FileTooBigToPaste {
        path: &'a str,
        size: &'a str,
        cap: &'a str,
    },
    /// `/context prompt` against a host that assembles no system prompt.
    ContextNoPrompt,
    /// The row under an empty composer offering what might be said next. It
    /// names the key, because a dim line with no key on it reads as a label.
    ComposerSuggested {
        text: &'a str,
    },
    /// A command that replaces or rewinds the conversation, typed while a turn
    /// is running: it has to wait for the turn, and Esc stops the turn now.
    WaitsForTheTurn {
        command: &'a str,
    },
    /// `/compact` typed while a turn is running: queued behind it.
    CompactAfterTurn,
    /// The runtime refused because it is busy (a turn, a compaction or a
    /// rebuild under way) — what the host says in place of its own words.
    RuntimeBusy,
    /// `/effort <level>` with a level this model does not take; `levels` are
    /// the ones it does.
    EffortNotForThisModel {
        wanted: &'a str,
        levels: &'a str,
    },
    /// The clipboard cannot be read on this platform (HarmonyOS) — text or
    /// picture — so a Ctrl+V reaches here with nothing: what to do instead.
    ClipboardImageUnsupportedHere,
    /// `/changelog` in the command menu and the welcome tips.
    CmdAboutChangelog,
    /// The header of `/changelog`'s list of releases.
    ChangelogPickerTitle,
    /// The tag on a release that is news to this person.
    ChangelogNewTag,
    /// A release's first tab: what it is about, and what changed.
    ChangelogOverviewTab,
    /// A release's issues tab.
    ChangelogIssuesTab {
        count: usize,
    },
    /// After the date of the release this build is.
    ChangelogThisBuild,
    /// `/changelog <version>` for a release with no notes.
    ChangelogNoSuchRelease {
        asked: &'a str,
    },
    /// This build ships no notes at all.
    ChangelogEmpty,
    /// The one line on the first launch after an upgrade. `from` is the release
    /// last told about, when known; `releases` how many with notes lie between;
    /// `highlights` the points to name; `more` when there are more than named.
    WhatsNewNotice {
        from: Option<&'a str>,
        to: &'a str,
        releases: usize,
        highlights: &'a [String],
        more: bool,
    },
}
