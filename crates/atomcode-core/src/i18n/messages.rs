pub enum Msg<'a> {
    // WelcomeWizard
    WelcomeBannerLine1,
    WelcomeBannerLine2,
    WelcomeOptionCodingPlan,
    WelcomeOptionCodingPlanHint,
    WelcomeOptionConfigureManually,
    WelcomeOptionConfigureManuallyHint,
    WelcomeOptionSkip,
    WelcomeOptionSkipHint,

    // ── /codingplan ──
    CodingPlanSetupFailed { error: &'a str },
    // SetupReport renderer (core/coding_plan/setup.rs)
    CpSetupHeader,
    CpLoggedIn { who: &'a str, username: &'a str, email: &'a str },
    CpStepSkipped { reason: &'a str },
    CpLoginFailed { error: &'a str },
    CpClaimed { message: &'a str, plan_type: &'a str },
    CpClaimSuccessFallback,
    CpAlreadyClaimed { reason: &'a str },
    CpClaimFailed { error: &'a str },
    CpAddedProviders { count: usize, plural_s: &'a str },
    CpLockedJediterm { name: &'a str },
    CpLockedAnsi { name: &'a str },
    CpProviderRow { provider: &'a str, model: &'a str, default_suffix: &'a str },
    CpDefaultSuffix,
    CpVisionAuto { kind: &'a str },
    CpVisionUserSupplied { kind: &'a str },
    CpVisionCleared,
    CpModelsSkipped { reason: &'a str },
    CpModelsFailed { error: &'a str },
    CpStatusHeader,
    CpPlanPending { plan: &'a str },
    CpPlanActive {
        plan: &'a str,
        expires_at: &'a str,
        remaining_days: i32,
        total_days: i32,
    },
    CpUsageLine { usage: &'a str, reset_at: &'a str, duration: &'a str },
    CpWindowQuotaExhausted,
    CpWindowQuotaHint { hint: &'a str },
    CpStatusFetchSkipped { reason: &'a str },
    CpStatusFetchFailed { error: &'a str },

    // i18n self-errors
    ErrUnsupportedLocale { input: &'a str },

    // ── Status bar (build_status) ──
    StatusNoProvider,
    StatusUpgradeHint { version: &'a str },
    StatusModelNotConfigured,
    StatusClipboardImageHint,

    // ── /status command body ──
    StatusBody { model: &'a str, dir: &'a str, config: &'a str, tokens: usize },
    StatusCpNotSignedIn,
    StatusCpFetchFailed { error: &'a str },
    StatusCpNoActive,
    StatusCpLine {
        plan: &'a str,
        expires_at: &'a str,
        remaining_days: i32,
        total_days: i32,
    },
    StatusCpUsage { usage: &'a str, reset_at: &'a str, seconds: i64 },
    StatusCpWindowExhausted,
    StatusCpWindowHint { hint: &'a str },
    StatusInstructionFilesHeader,
    StatusInstructionPresent { path: &'a str, label: &'a str },
    StatusInstructionMissing { label: &'a str },

    // ── /login completion ──
    LoginSignedInWithCpHint { name: &'a str, username: &'a str },

    // ── Help / commands ──
    HelpAvailableCommands,

    // ── Provider wizard ──
    ProviderWizardHeader,
    ProviderWizardCancelled,
    ProviderMenuAdd,
    ProviderMenuAddDesc,
    ProviderMenuEdit,
    ProviderMenuEditDesc,
    ProviderMenuDelete,
    ProviderMenuDeleteDesc,
    ProviderMenuSetDefault,
    ProviderMenuSetDefaultDesc,
    ProviderNoProviders,
    ProviderDeleteConfirm { name: &'a str },
    ProviderDeleted { name: &'a str },
    ProviderDeleteKept,
    ProviderDefaultSet { name: &'a str },
    ProviderAdded { name: &'a str, model: &'a str },
    ProviderUpdated { name: &'a str },
    ProviderStepName,
    ProviderStepType,
    ProviderStepTypeWithHint { current: &'a str },
    ProviderStepBaseUrl,
    ProviderStepBaseUrlWithHint { current: &'a str },
    ProviderDefaultHint,
    ProviderStepApiKey,
    ProviderStepApiKeyWithHint { hint: &'a str },
    ProviderStepApiKeySet,
    ProviderStepApiKeyUnset,
    ProviderStepModel,
    ProviderStepModelWithHint { current: &'a str },
    ProviderNameEmpty,
    ProviderUnknownType,
    ProviderUnknownTypeEdit,
    ProviderModelEmpty,
    ProviderEditKeep,

    // ── Model picker ──
    ModelSwitched { provider: &'a str, model: &'a str },

    // ── Session picker ──
    SessionLoadFailed { error: &'a str },
    SessionResumedLabel { name: &'a str },
    SessionTimeJustNow,
    SessionTimeMinAgo { n: u64 },
    SessionTimeHourAgo { n: u64 },
    SessionTimeDayAgo { n: u64 },
    SessionMsgCount { count: usize },
    SessionNameEmpty,
    SessionNameTooLong { max: usize },
    SessionNameControlChars,
    SessionListFailed { error: &'a str },
    SessionRenamed { old: &'a str, new: &'a str },
    SessionNoneSelected,
    SessionRenameEditing { buffer: &'a str },

    // ── Dir picker ──
    DirCurrent,
    DirNotExists { path: &'a str },
    DirChanged { path: &'a str },
    DirNotADirectory { path: &'a str },

    // ── Issue wizard ──
    IssueCancelled,
    IssueNewOn { owner: &'a str, repo: &'a str },
    IssueStep1,
    IssueStep2,
    IssueTitleConfirmed { title: &'a str },
    IssueRequiredField { field: &'a str },
    IssueCreated { number: u64, title: &'a str, url: &'a str },
    IssueCreateFailed { error: &'a str },

    // ── Language ──
    /// Confirmation rendered to scrollback after the user picks a
    /// locale via `/language` (modal or arg). Already includes the
    /// leading "  " indent and trailing "\n" so the call site is just
    /// `renderer.render(UiLine::CommandOutput(t(Msg::LanguageSwitched
    /// { ... }).into_owned()))`.
    LanguageSwitched { label: &'a str, locale: &'a str },

    // ── Idle / onboarding hints ──
    /// "type something, or press " (text before the slash)
    IdleHintPrefix,
    /// "/" (the slash shortcut itself — kept separate for accent styling)
    IdleHintSlash,
    /// " to browse commands" (text after the slash)
    IdleHintSuffix,
    /// Complete plain-text version: "type something, or press / to browse commands"
    IdleHintFull,
    /// "/provider" command label
    IdleHintProvider,
    /// "to add a custom model" (text after /provider)
    IdleHintProviderSuffix,
    /// Complete plain-text version: "/provider  to add a custom model"
    IdleHintProviderFull,

    // ── Slash-command high-frequency messages ──
    CmdSwitchedPlanMode,
    CmdSwitchedBuildMode,
    CmdNewSession,
    CmdNoProviders,
    CmdNoSessions,
    CmdUnknownCommand { name: &'a str },
    CmdLoginFailed { error: &'a str },
    CmdLogoutDone,
    CmdLogoutFailed { error: &'a str },
    CmdWhoamiNotSignedIn,
    CmdReloadDone { provider: &'a str, model: &'a str },
    CmdReloadFailed { error: &'a str },
    CmdUndoNotSupported,
    CmdNoChanges,
    CmdCheckingUpdate,
    CmdNoActiveProvider,

    // ── Approval prompt ──
    ApprovalPromptAlt { tool: &'a str, detail: &'a str },
    ApprovalWaitingLabel,
    ApprovalAllow,
    ApprovalAlways,
    ApprovalDeny,

    // ── Cancelled / Error prefix ──
    Cancelled,
    ErrorPrefix { msg: &'a str },

    // ── Upgrade messages ──
    UpgradeSuccess { from: &'a str, to: &'a str },
    UpgradeManifestFetched { version: &'a str },
    UpgradeDownloading { pct: i32, bytes: u64, total: u64 },
    UpgradeVerifying,
    UpgradeReplacing,
    UpgradeDone { version: &'a str, backup: &'a str },
    UpgradeAlreadyLatest { detail: &'a str },
    UpgradeFailed { error: &'a str },
    UpgradeRolledBack { exe: &'a str, backup: &'a str },

    // ── Terminal keyboard hints ──
    KbdHintMacos,
    KbdHintOther,

    // ── JediTerm / conhost fallback ──
    JediTermFallback,
    LegacyConhostFallback,

    // ── Session replay ──
    SessionReplayHint,

    // ── Background task ──
    BackgroundComplete { turns: usize },
    BackgroundFailed { turns: usize },
    BackgroundFilesEdited,

    // ── /config command ──
    ConfigProviderLabel { provider: &'a str, path: &'a str },

    // ── /cost command ──
    CostReport {
        prompt: usize,
        completion: usize,
        cached: usize,
        cache_rate: usize,
        total: usize,
        cost: &'a str,
    },

    // ── /think command ──
    ThinkStatus { status: &'a str, budget: u32, provider: &'a str },
    ThinkEnabled { budget: u32 },
    ThinkDisabled,
    ThinkBudgetSet { n: u32 },
    ThinkBudgetTooSmall { n: u32 },
    ThinkBudgetUsage,
    ThinkUsage,

    // ── /remember, /forget ──
    RememberUsage,
    ForgetUsage,

    // ── /background ──
    BackgroundUsage,

    // ── /init ──
    InitAlreadyExists { path: &'a str },
    InitWrote { path: &'a str, bytes: usize },
    InitFailed { error: &'a str },

    // ── /cd ──
    CdWorkingDir { cwd: &'a str },

    // ── /diff ──
    DiffFailed { error: &'a str },

    // ── /upgrade ──
    UpgradeUnknownArg { arg: &'a str },

    // ── /skills ──
    SkillsNone,
    SkillsAvailable,
    SkillUnknown { name: &'a str },

    // ── /mcp ──
    McpReloading { count: usize },
    McpConnecting,
    McpConnectingServer { name: &'a str },
    McpNoServersConfigured,
    McpClearedReconnecting { removed: usize },
    McpClearedNoServers { removed: usize },
    McpToolsUsage,
    McpToolsListing { server: &'a str },
    McpNoRegistry,
    McpServersHeader,
    McpReloadFailed { error: &'a str },
    // /mcp login / logout
    McpOAuthLoginUsage,
    McpOAuthLogoutUsage,
    McpOAuthLoadConfigFailed { error: &'a str },
    McpOAuthServerNotFound { server: &'a str },
    McpOAuthStarting { server: &'a str },
    McpOAuthSaved { provider: &'a str, server: &'a str },
    McpOAuthFailed { error: &'a str },
    McpOAuthTokenRemoved { server: &'a str },
    McpOAuthNoToken { server: &'a str },
    McpOAuthLogoutFailed { error: &'a str },
    // MCP / LSP server connect feedback (event handler output)
    McpServerConnected { name: &'a str },
    McpServerFailed { name: &'a str, error: &'a str },
    LspServerStarted { name: &'a str, ext: &'a str },
    LspServerFailed { name: &'a str, ext: &'a str, error: &'a str },

    // ── /worktree ──
    WorktreeUsage,
    WorktreeCreateUsage,
    WorktreeCreated { branch: &'a str, base: &'a str, path: &'a str },
    WorktreeCreateFailed { error: &'a str },
    WorktreeNoActive,
    WorktreeListFailed { error: &'a str },
    WorktreeActiveHeader,
    WorktreeHasChanges,
    WorktreeClean,
    WorktreeCurrent,
    WorktreeDoneBack { path: &'a str },
    WorktreeDoneMergeHint { branch: &'a str },
    WorktreeNoSession,
    WorktreeCleanupUsage,
    WorktreeCleaned { branch: &'a str },
    WorktreeCleanedSwitched { path: &'a str },
    WorktreeCleanupUncommitted { branch: &'a str },
    WorktreeCleanupFailed { error: &'a str },

    // ── /help commands (custom commands subcommand) ──
    HelpCustomCommandsHeader,
    HelpCustomNone,
    HelpCustomCreateHint,
    HelpSourceGlobal,
    HelpSourceProject,

    // ── /plugin ──
    PluginUsage,
    PluginMarketplaceUsage,
    PluginInstallUsage,
    PluginUninstallUsage,
    PluginNoMarketplaces,
    PluginMarketplacesHeader,
    PluginNoInstalled,
    PluginInstalledHeader,
    PluginMarketplaceCloning { url: &'a str },
    PluginMarketplaceRemoved { name: &'a str },
    PluginMarketplaceRemoveFailed { error: &'a str },
    PluginMarketplaceUpdating { name: &'a str },
    PluginMarketplaceListFailed { error: &'a str },
    PluginInstalling { plugin: &'a str, marketplace: &'a str },
    PluginUninstalled { plugin: &'a str, marketplace: &'a str },
    PluginUninstallFailed { error: &'a str },
    PluginListFailed { error: &'a str },

    // ── Command descriptions (for help_text dynamic lookup) ──
    CmdDescCodingplan,
    CmdDescResume,
    CmdDescRename,
    CmdDescLogin,
    CmdDescLogout,
    CmdDescWhoami,
    CmdDescModel,
    CmdDescProvider,
    CmdDescStatus,
    CmdDescConfig,
    CmdDescReload,
    CmdDescCd,
    CmdDescInit,
    CmdDescBackground,
    CmdDescDiff,
    CmdDescClear,
    CmdDescSession,
    CmdDescCost,
    CmdDescContext,
    CmdDescCompact,
    CmdDescRemember,
    CmdDescForget,
    CmdDescMemory,
    CmdDescMcp,
    CmdDescUndo,
    CmdDescWorktree,
    CmdDescUpgrade,
    CmdDescIssue,
    CmdDescPlan,
    CmdDescBuild,
    CmdDescThink,
    CmdDescHelp,
    CmdDescLanguage,
    CmdDescQuit,
    CmdDescSkills,
    CmdDescPlugin,

    // ── config save failed ──
    ConfigSaveFailed { error: &'a str },

    // ── OnboardingWizard (multi-step first-run + `/welcome`). Spec:
    //    docs/superpowers/specs/2026-05-11-welcome-wizard-redesign-design.md
    OnboardingStepHeaderWelcome,
    OnboardingStepHeaderLanguage,
    OnboardingStepHeaderSetup,
    OnboardingPanelTitle,
    OnboardingIntroVersionLine { v: &'a str },
    OnboardingIntroBullet1,
    OnboardingIntroBullet2,
    OnboardingIntroBullet3,
    OnboardingIntroPressEnter,
    OnboardingIntroCtrlC,
    OnboardingIntroCompactTagline,
    OnboardingLanguageTitleBilingual,
    OnboardingLanguagePrompt,
    OnboardingLanguageOptionAuto,
    OnboardingLanguageOptionEn,
    OnboardingLanguageOptionZhCn,
    OnboardingSetupTitle,
    OnboardingNavHint,
    OnboardingConfirmClear,
    CmdWelcomeDescription,

    /// TurnComplete separator summary, e.g.
    ///   `✓ Shipped · 3 rounds · 2 tools · 6.8s · 285 tokens`
    /// `done` is a playful English verb from `DONE_LABELS` — kept
    /// English in every locale because translated cute verbs read
    /// awkward; the structural words (`rounds`/`tools`/`tokens`)
    /// localise. `duration` is a pre-formatted human string (e.g.
    /// "6.8s").
    TurnSummary {
        done: &'a str,
        turn_count: usize,
        tool_call_count: usize,
        duration: &'a str,
        total_tokens: usize,
    },
}
