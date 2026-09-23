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
    OverlayFilterHint,
    OverlayNoMatch,

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

    // ── the input line (`modules/input.rs`) ──
    InputAnswerKeys,
    InputHistoryNth {
        nth: usize,
        total: usize,
    },
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
    },
    TeamHeader {
        count: usize,
    },
    /// The lead's own row, first in the list.
    TeamLead,
    /// Marks the member whose stream the screen is showing.
    TeamViewing,
    TeamWorkingRound {
        round: u64,
    },

    // ── the todo fold (`modules/todo.rs`) ──
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
    TranscriptDropped {
        through: u64,
    },
    TranscriptRateLimited {
        until: &'a str,
    },
    TranscriptMemberEnded,

    // ── what each command is for (`commands.rs`) ──
    CmdAboutQuit,
    CmdAboutReasoning,
    CmdAboutTools,
    CmdAboutShowInject,
    CmdAboutMouse,
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
    CmdTakesPathRequired,
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
    ClipboardHasNoText,
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
    CopyWhichBlock {
        count: usize,
    },
    CopyNoSuchBlock {
        count: usize,
        asked: &'a str,
    },
    CopyNoBlocks,
    CopiedLines {
        lines: usize,
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
        resets_in: &'a str,
    },
    ViewWhichFile,
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
    DiffPickerHint {
        count: usize,
        added: u64,
        removed: u64,
    },
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
    UsageResetsIn {
        duration: &'a str,
    },
    UsageResetsAt {
        at: &'a str,
    },
    UsageExhausted {
        label: &'a str,
        when: &'a str,
        cap: &'a str,
    },
    UsageLeft {
        label: &'a str,
        cap: &'a str,
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
    WhoAmIUnnamed,
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
    McpConnecting,
    McpConnected,
    McpUntrusted,
    McpNeedsAuthentication,
    McpFailed {
        message: &'a str,
    },
    McpDisconnected,
    McpDisabled,
    McpUnknownState,
    McpWithdrawn,
    McpNeedsServerName,
    McpServerHasNoTools {
        server: &'a str,
    },
    McpUnknownSubcommand {
        what: &'a str,
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
    MouseHandedBack,
    NoProviderPanel,
    NoPluginPanel,
    NoToolPanel,
    CopiedSelection,
    MenuCopySelection,
    MenuCopySelectionAbout,
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

    // ── the live strip and the folded-lines notes (`modules/live.rs`, `host.rs`) ──
    LiveStopping,
    /// Shown while the runtime's VL helper is turning a pasted picture into text
    /// for a non-vision model — before the turn's first fact, so the screen is
    /// not blank during the seconds that recognition takes.
    LiveRecognizingImage,
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
    OnboardLoginTitle,
    OnboardFetchingLoginUrl,
    OnboardConfirmTitle,
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
    OpenRouterAuthorise {
        url: &'a str,
    },
    OpenRouterNoAnswer,
    OpenRouterNoFreeModels,
    OpenRouterConnected {
        added: usize,
        default: &'a str,
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
}
