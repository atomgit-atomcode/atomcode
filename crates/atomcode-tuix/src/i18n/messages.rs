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

    // i18n self-errors
    ErrUnsupportedLocale { input: &'a str },

    // ── Status bar (build_status) ──
    StatusNoProvider,
    StatusUpgradeHint { version: &'a str },
    StatusModelNotConfigured,
    StatusClipboardImageHint,

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
    LanguageSetTo { locale: &'a str },

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
}
