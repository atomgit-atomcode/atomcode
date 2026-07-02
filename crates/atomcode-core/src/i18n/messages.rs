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

    // ── /login (full setup flow) ──
    CodingPlanSetupFailed { error: &'a str },
    /// Emitted inline by `/login` and `atomcode login` when the stored
    /// OAuth token comes back 401 from the CodingPlan API mid-flow.
    /// We re-run the OAuth dance, save the fresh token, and retry the
    /// whole setup once — this line tells the user that's what's about
    /// to happen so the second "Open this URL in any browser…" block
    /// isn't a surprise.
    CpReauthAfter401,
    /// Emitted by the OpenAI provider when an AtomGit-gateway chat
    /// request returns 401 and our one automatic refresh_token attempt
    /// either failed or the retried request still came back 401. The
    /// raw server message ("Gitcode auth: token rejected") is not
    /// useful to end users — this replaces it with an actionable hint
    /// pointing at `/login`. Non-atomgit gateways still surface the
    /// verbatim server error so user-supplied API keys (sk-...) get
    /// the diagnostic detail.
    ChatAuthExpired,
    // SetupReport renderer (core/coding_plan/setup.rs)
    CpSetupHeader,
    CpLoggedIn { who: &'a str, username: &'a str, email: &'a str },
    CpStepSkipped { reason: &'a str },
    CpLoginFailed { error: &'a str },
    CpClaimed { message: &'a str, plan_type: &'a str },
    CpClaimSuccessFallback,
    CpAlreadyClaimed { reason: &'a str },
    CpClaimFailed { error: &'a str },
    /// Same as `CpClaimFailed` but with no trailing detail body.
    /// Used in the rare edge case where every tier returned success=
    /// false with an empty server message AND no transport error
    /// text — there's nothing to put after `— `, so the line stops
    /// at the prefix.
    CpClaimFailedBare,
    /// Per-tier cascade row — winning tier, fresh claim. `plan` is the
    /// full plan label already including the "CodingPlan " prefix (the
    /// server's `plan_name`, e.g. "CodingPlan Pro", or "CodingPlan
    /// {tier}" fallback). Example (zh-CN): `  ✓ CodingPlan Pro 生效`
    CpClaimTierSucceeded { plan: &'a str },
    /// Per-tier cascade row — winning tier, server reported the user
    /// already holds this tier or higher (`duplicate=true`). `plan` as
    /// above.
    CpClaimTierAlreadyHeld { plan: &'a str },
    /// Per-tier cascade row — tier was refused (2xx with success=
    /// false / 5xx / transport). `reason` is the server's human-
    /// readable message (e.g. `额度已满`, `暂无开放`) or a short
    /// rendering of the transport error.
    CpClaimTierFailed { tier: &'a str, reason: &'a str },
    CpAddedProviders { count: usize, plural_s: &'a str },
    /// Locked-model row. `name` is expected to be pre-decorated with
    /// U+0336 combining strikethrough by the caller (see
    /// `coding_plan::setup::strikethrough`), so the template itself
    /// stays a plain `format!` and survives every renderer's CSI
    /// scrubber without needing SGR escapes.
    CpLocked { name: &'a str },
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
    /// Open-source build attempted to use a CodingPlan provider. The
    /// signing capability is not present in this build, so the request
    /// can't reach the AtomGit LLM gateway. Surface a clear hint
    /// pointing to the official Releases page.
    CpOfficialBuildRequired,
    /// Official build, but no stored auth (or auth has empty
    /// `user.id` / `access_token`). The signing path needs these
    /// fields to derive a per-user key; without them the request
    /// can't be signed. Surface a "please run `/codingplan` to log
    /// in" hint instead of the misleading "official build required"
    /// message — the user IS on an official build.
    CpAuthRequired,
    /// Server returned `ATOMCODE_SIG_STALE` — the request's signed
    /// timestamp is outside the ±5min window the gateway accepts.
    /// Typically caused by an unsynced local clock.
    CpSignStaleClockSkew,
    /// Server returned `ATOMCODE_SIG_REPLAY` even after the client's
    /// one automatic retry with a fresh nonce. Surface a "please retry
    /// the command" hint — usually self-heals on the next attempt.
    CpSignReplayPersisted,
    /// Server returned `ATOMCODE_SIG_INVALID` AND the alg_version is
    /// no longer in the server's `accepted_versions` set — the client
    /// binary is too old. Force-upgrade hint.
    CpSignVersionTooOld,
    /// Server returned `426 Upgrade Required` — emergency rotation
    /// playbook in progress; this build cannot continue without
    /// upgrading.
    CpUpgradeRequired,

    // i18n self-errors
    ErrUnsupportedLocale { input: &'a str },

    // ── Status bar (build_status) ──
    StatusNoProvider,
    /// Open-source build with an AtomGit-gateway provider configured.
    /// Sending any chat will fail with `CpOfficialBuildRequired`; this
    /// hint surfaces the same diagnosis up-front so the user doesn't
    /// have to type a message to discover the dead-end.
    StatusOfficialBuildRequired,
    StatusUpgradeHint { version: &'a str },
    /// Right-aligned status-row hint, HarmonyBrew variant: a newer version
    /// exists, upgrade via the package manager rather than `/upgrade`.
    StatusUpgradeHintPm { version: &'a str },
    StatusModelNotConfigured,
    /// macOS / Linux variant: "Image in clipboard · ctrl+v to paste".
    /// Ctrl+V is intercepted by Windows Terminal / conhost before
    /// reaching atomcode, so Windows builds emit
    /// `StatusClipboardImageHintSlash` instead.
    StatusClipboardImageHint,
    /// Windows variant: "Image in clipboard · /paste". Tells the
    /// user to fall back on the `/paste` slash command, which works
    /// in every terminal regardless of host keybinds.
    StatusClipboardImageHintSlash,
    /// Lowest-priority status-row fallback: nudge the user toward the
    /// `/webui` command (browser UI) when no higher-priority hint
    /// (warnings / usage / upgrade) is competing for the slot.
    StatusWebuiHint,

    // ── /status command body ──
    StatusBody { model: &'a str, dir: &'a str, config: &'a str },
    /// `/status` login line — signed in, showing the account display name/username.
    StatusLoginLoggedIn { user: &'a str },
    /// `/status` login line — not signed in.
    StatusLoginNotSignedIn,
    StatusCpNotSignedIn,
    StatusCpFetchFailed { error: &'a str },
    /// `/status` CodingPlan line when the fetch failed specifically because auth
    /// expired (`is_auth_expired`) — a clear re-login prompt instead of the raw error.
    StatusCpAuthExpired,
    StatusCpNoActive,
    StatusCpLine {
        plan: &'a str,
        expires_at: &'a str,
        remaining_days: i32,
        total_days: i32,
    },
    StatusCpUsage { usage: &'a str, reset_at: &'a str, duration: &'a str },
    StatusCpWindowExhausted,
    StatusCpWindowHint { hint: &'a str },
    StatusInstructionFilesHeader,
    StatusInstructionPresent { path: &'a str, label: &'a str },
    StatusInstructionMissing { label: &'a str },

    // ── Help / commands ──
    HelpAvailableCommands,
    /// Full keyboard-shortcuts reference dumped to scrollback by the
    /// `/keys` slash command. Carries every line of the panel as a
    /// single multi-line string so translators can adjust column
    /// alignment per locale without rebuilding rows in Rust.
    KeybindingsHelp,

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
    ProviderImportPrompt,
    ProviderImportParsed { base_url: &'a str, type_name: &'a str, model: &'a str },
    ProviderImportFailed,
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
    ProviderBaseUrlEmpty,
    ProviderUnknownType,
    ProviderUnknownTypeEdit,
    ProviderModelEmpty,
    ProviderEditKeep,
    ProviderTypeInferred { type_name: &'a str },
    ProviderStepNameDefault { default: &'a str },
    ProviderStepProgress { current: usize, total: usize },

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
    SessionSaveFailed { error: &'a str },
    SessionDeleted { name: &'a str },
    SessionDeleteConfirm { name: &'a str },
    SessionDeleteFailed { error: &'a str },
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
    /// "/codingplan" command label
    IdleHintCodingplan,
    /// "to claim a free token quota" (text after /codingplan)
    IdleHintCodingplanSuffix,
    /// Complete plain-text version: "/codingplan  to claim a free token quota"
    IdleHintCodingplanFull,
    /// "/webui" command label
    IdleHintWebui,
    /// "open a synced session in the browser" (text after /webui)
    IdleHintWebuiSuffix,
    /// Complete plain-text version: "/webui  open a synced session in the browser"
    IdleHintWebuiFull,

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
    CmdUndoDone { target: usize, last: usize },
    CmdUndoDiskWarning,
    CmdUndoNoTurns,
    CmdUndoOutOfRange { requested: usize, available: usize },
    CmdUndoBusy,
    CmdUndoBadArg,
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
    UpgradeAlreadyLatest { current: &'a str, latest: &'a str },
    UpgradeFailed { error: &'a str },
    UpgradeRolledBack { exe: &'a str, backup: &'a str },

    // ── Terminal keyboard hints ──
    KbdHintMacos,
    KbdHintOther,

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
    /// Shown when `/upgrade` (or rollback) is invoked in a HarmonyBrew-managed
    /// build: self-update is disabled, point the user at `brew upgrade`.
    UpgradePackageManaged,
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

    // ── /setup ──
    /// Header line: "✅ Setup complete — 3 installed, 1 skipped, 0 failed · 120ms"
    SetupHeader { installed: usize, skipped: usize, failed: usize, duration_ms: u64 },
    /// "Installed:" section label in setup report.
    SetupInstalledLabel,
    /// "Skipped:" section label in setup report.
    SetupSkippedLabel,
    /// "Failed:" section label in setup report.
    SetupFailedLabel,
    /// Per-item installed row: "  ✓ skill:atomcode-automation-recommender → /path"
    SetupInstalledRow { kind: &'a str, slug: &'a str, path: &'a str },
    /// Per-item skipped row: "  - skill:xyz (hash match)"
    SetupSkippedRow { kind: &'a str, slug: &'a str, reason: &'a str },
    /// Per-item failed row: "  × mcp:xyz — error message"
    SetupFailedRow { kind: &'a str, slug: &'a str, error: &'a str },
    /// "💡 Tip: Run /setup …" — first-run hint shown above the prompt
    /// when the project has no setup state yet.
    CmdSetupTip,
    /// "Running atomcode setup..." — shown while setup is in progress.
    CmdSetupRunning,
    /// "Skills reloaded — N available" — after setup completes and skills are reloaded.
    CmdSetupSkillsReloaded { count: usize },
    /// "setup error: {e}" — when setup::run returns an error.
    CmdSetupError { error: &'a str },
    /// "Running setup skill..." — after seeds installed and skill is auto-invoked.
    CmdSetupRunningSkill,
    /// "Setup skill not found..." — when the setup skill cannot be resolved or expanded.
    CmdSetupSkillMissing,

    // ── /plugin ──
    PluginUsage,
    PluginMarketplaceUsage,
    PluginInstallUsage,
    PluginInstallNotFound { plugin: &'a str },
    PluginInstallAmbiguous { plugin: &'a str },
    PluginUninstallUsage,
    PluginUninstallNotFound { plugin: &'a str },
    PluginUninstallAmbiguous { plugin: &'a str },
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
    PluginInstallingByName { plugin: &'a str },
    PluginAlreadyInstalled { id: &'a str },
    // Interactive `/plugin` manager modal.
    PluginMgrBrowse,
    PluginMgrAdd,
    PluginMgrRemove,
    PluginMgrInstalled { count: usize },
    PluginMgrInstalledMark,
    PluginMgrHintNav,
    PluginMgrHintToggle,
    PluginMgrHintRemove,
    PluginMgrHintUninstall,
    PluginMgrHintUrl,
    PluginMgrHintPending,
    PluginMgrInstallingLabel,
    PluginMgrEmptyMarketplaces,
    PluginMgrEmptyPlugins,
    PluginMgrEmptyInstalled,
    PluginMgrCloning,
    PluginMgrInstalling { plugin: &'a str },
    PluginMgrEscToCancel,
    // Scope selection screen.
    PluginScopeUser,
    PluginScopeUserDesc,
    PluginScopeProject,
    PluginScopeProjectDesc,
    PluginScopeLocal,
    PluginScopeLocalDesc,
    PluginScopeHint,
    PluginUninstalled { plugin: &'a str, marketplace: &'a str },
    PluginUninstallFailed { error: &'a str },
    PluginListFailed { error: &'a str },
    PluginReloadDone { skills: usize, warnings: usize },
    /// Git not found on the system — marketplace auto-install and auto-update
    /// are disabled. Shown as a friendly hint (not an error) at startup.
    PluginGitNotFound,
    /// Marketplace `add` completion toast. Emitted by `handle_plugin_job_event`
    /// for both manual `/plugin marketplace add` and the detached
    /// startup-bootstrap auto-install. `count` is the number of plugins the
    /// marketplace exposes after cloning.
    PluginMarketplaceAdded { name: &'a str, commit: &'a str, count: usize, plugins: &'a str },
    /// Marketplace `update` completion toast — HEAD actually moved. No-op
    /// pulls (HEAD unchanged) emit no toast at all so a quiet `git pull`
    /// doesn't spam the body region.
    PluginMarketplaceUpdated { name: &'a str, commit: &'a str },
    /// Plugin `install` completion toast. `skipped` counts skills that the
    /// loader rejected (bad SKILL.md frontmatter, namespace collision, etc.);
    /// `show_details_hint` flips on the trailing "(Ctrl+O for details)"
    /// nudge when warnings exist and verbose mode is off.
    PluginInstallDone {
        plugin: &'a str,
        marketplace: &'a str,
        loaded: usize,
        skipped: usize,
        show_details_hint: bool,
    },
    SetupAutoReloaded { skills: usize, warnings: usize },

    // ── Command descriptions (for help_text dynamic lookup) ──
    CmdDescWebui,
    CmdDescSetup,
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
    CmdDescBg,
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
    CmdDescEffort,
    CmdDescHelp,
    CmdDescKeys,
    CmdDescLanguage,
    CmdDescQuit,
    CmdDescSkills,
    CmdDescPlugin,
    /// Description for the `/paste` slash command — pulls a clipboard
    /// image and attaches it as `[Image #N]`. Exists for Windows
    /// users whose Ctrl+V is swallowed by Windows Terminal / conhost
    /// before reaching the app, but works on every platform.
    CmdDescPaste,
    /// Description for the `/copy` slash command — copies a code block from the
    /// last reply to the clipboard.
    CmdDescCopy,
    /// `/copy`: confirmation after a code block lands on the clipboard.
    CopyOk { lines: usize, chars: usize },
    /// `/copy`: the last reply has no fenced code block to copy.
    CopyNoCodeBlock,
    /// `/copy N`: the requested index is out of range; `count` blocks exist.
    CopyBadIndex { count: usize },
    /// `/copy`: the clipboard write failed (no arboard backend — headless/SSH).
    CopyFailed,
    /// Hint shown after a code block is auto-copied to clipboard (issue #699).
    CodeBlockCopied,
    /// Description for the `/guide` slash command — asks atomcode-guide a question.
    CmdDescGuide,
    /// Description for the `/view` slash command — opens an overlay modal showing file content.
    CmdDescView,
    /// Description for the `/app` slash command — expose the session to the mobile App via relay.
    CmdDescApp,
    /// Description for the `/sync` slash command — attach to a live webui session.
    CmdDescSync,
    /// Description for the `/review` slash command — code review the current changes.
    CmdDescReview,
    /// Description for the `/goal` slash command — set an autonomous completion goal.
    CmdDescGoal,
    /// Description for the `/proxy` slash command — switch the outbound proxy mode.
    CmdDescProxy,
    /// Error shown when `/view` is used without a filepath argument.
    ViewUsage,
    /// `/guide` menu header: "📖 AtomCode Guide — type /guide <question>"
    GuideMenuHeader,
    /// `/guide` menu: "Common topics:" section label
    GuideMenuTopics,
    /// `/guide` menu topic: getting started
    GuideMenuGettingStarted,
    /// `/guide` menu topic: switching models
    GuideMenuSwitchModel,
    /// `/guide` menu topic: using MCP
    GuideMenuMcp,
    /// `/guide` menu topic: skills and plugins
    GuideMenuSkills,
    /// `/guide` menu topic: memory feature
    GuideMenuMemory,
    /// `/guide` menu topic: background tasks
    GuideMenuBackground,
    /// `/guide` menu topic: context management
    GuideMenuContext,
    /// `/guide` menu topic: keyboard shortcuts
    GuideMenuKeybindings,
    /// `/guide` menu topic: configuration
    GuideMenuConfig,
    /// /guide menu tip: hint for users to type a question
    GuideMenuTip,
    /// /guide menu: documentation URL
    GuideMenuDocUrl,
    /// `/guide`: ask skill install already in progress, please wait
    CmdGuideInstalling,
    /// `/guide`: ask skill not installed, triggering auto-install
    CmdGuideAutoInstall,
    /// `/guide`: auto-invoke completed, now answering
    CmdGuideAutoInvoke { topic: &'a str },
    /// `/guide`: install succeeded but ask skill still not found
    CmdGuideSkillNotFound,
    /// `/guide`: install failed, suggest manual install
    CmdGuideInstallFailed { error: &'a str },
    /// `/paste` failed because the clipboard holds no image. Shown
    /// in scrollback as an error line so the user isn't left
    /// wondering whether the command did anything.
    CmdPasteNoImage,

    // ── reasoning effort ──
    /// Rendered when the user tries to set reasoning_effort on a
    /// model that doesn't support it (only DeepSeek V4 / reasoner).
    ReasoningEffortNoEffect,

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

    /// Vision preprocessor success banner. Shown as a body line right
    /// after a VL turn finishes, in the form
    ///   `✓ VL recognised image, returned N chars`
    /// (English) /
    ///   `✓ VL 识别图片成功，返回 N chars`
    /// (zh-CN). The model key trails as a dim suffix in the renderer
    /// — kept out of this message so the wrapper styling stays
    /// renderer-side.
    VisionPreprocessSuccess { char_count: usize },

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
        /// Cache-hit ratio over the turn's input, if reported. `Some(n)` appends
        /// `· n% cached`; `None` appends nothing.
        cached_pct: Option<u8>,
    },

    /// Turn-end summary when the turn terminated in an error (the red
    /// error line is rendered separately, just above this). Same stats
    /// as `TurnSummary` but with a ✗ marker and a neutral "stopped"
    /// label instead of a celebratory verb — otherwise an errored turn
    /// reads as `✓ Nailed it` right under its own error message.
    TurnSummaryError {
        turn_count: usize,
        tool_call_count: usize,
        duration: &'a str,
        total_tokens: usize,
    },

    // ── OAuth login chrome (/login + /codingplan share these) ──
    /// Header above the QR block when scanning with WeChat is the
    /// expected flow. Includes the leading "  " indent and trailing
    /// "\n\n" paragraph break that the caller used to inline.
    LoginQrHeader,
    /// Separator + URL prelude shown below the QR block when both
    /// QR and URL fallback are available. Leading "\n\n  " and
    /// trailing "\n  " are part of the template.
    LoginUrlAfterQr,
    /// QR + URL both unavailable (Unicode-incapable terminal AND a
    /// platform where URL-based login doesn't work, e.g. OHOS).
    LoginNoQrNoUrl,
    /// URL-only header when QR can't render but URL login works.
    /// Leading "  " indent and trailing "\n  " before the URL.
    LoginUrlOnly,
    /// Footer line: "Press ESC to cancel" with surrounding
    /// blank-line padding.
    LoginCancelHint,

    // ── /context report ──
    CtxUsageHeader,
    CtxUsageNoTurns,
    CtxUsageWaiting,
    CtxProvider,
    CtxCtxName,
    CtxLabelSystemPrompt,
    CtxLabelToolDefs,
    CtxLabelColdZone,
    CtxLabelMessages,
    CtxLabelFree,
    CtxMessagesInWindow { n: usize },
    CtxSystemPromptHeader,
    CtxSystemPromptEmpty,
    /// Used in the "used/window tokens (pct)" line below the bar.
    CtxTokensSuffix,

    // ── /compact ──
    CompactNothingShort,
    CompactStarting,
    CompactNothingNoSavings { before: &'a str, after: &'a str },
    CompactDropped { messages: usize, before: &'a str, after: &'a str },
    /// Footer spinner label while a compaction's LLM summary runs (slow tier).
    Compacting,
    /// Spinner label variant when the compaction summary has stalled (>20s).
    CompactingSlow,
    /// Scrollback marker for a committed drain+summarize compaction (auto or
    /// manual). `messages` = exact count summarized; `before`/`after` = raw
    /// token-estimate strings (e.g. "48.2K") — the `~` marker is added by the
    /// i18n format string, not by the caller.
    CompactMarkDrain { messages: usize, before: &'a str, after: &'a str },
    /// Scrollback marker for a committed in-place stub fold (tool results
    /// collapsed, no messages dropped). `saved` = raw token-estimate string;
    /// the `~` marker is added by the i18n format string, not by the caller.
    CompactMarkStub { saved: &'a str },

    // ── /goal ──
    /// The full `/goal help` usage block (header + Usage + Notes).
    GoalHelp,
    /// `/goal` / `/goal status` while a goal is active. `condition` is the goal
    /// text; `round`/`mins`/`secs` are the live progress counters.
    GoalStatus { condition: &'a str, round: u32, mins: u64, secs: u64 },
    /// `/goal status` (or bare `/goal`) when no goal is active.
    GoalNoActive,
    /// Confirmation line after `/goal clear` (and its aliases).
    GoalCleared,

    /// Surfaced when the user pastes/attaches an image but the active
    /// model can't accept images AND no `vision_preprocessor_provider`
    /// is configured. `model` is the current model identifier.
    ModelNoImageSupport { model: &'a str },

    // ── --dangerously-skip-permissions / -y ──
    /// Scrollback warning banner when --dangerously-skip-permissions is active
    /// in TUI mode. Includes leading "⚠ " and trailing "\n".
    BypassWarningBanner,
    /// Headless-mode stderr warning when --dangerously-skip-permissions is active.
    BypassWarningHeadless,
    /// Status-bar badge text shown when --dangerously-skip-permissions is
    /// active. Typically "⚠ BYPASS" — kept short for the status row.
    BypassBadge,

    // ── admin / root privilege warning ──
    /// TUI scrollback warning when AtomCode is running as admin/root.
    /// Includes leading "⚠ " and trailing "\n".
    AdminWarningBanner,
    /// Headless-mode stderr warning when running as admin/root.
    AdminWarningHeadless,

    /// Confirmation hint after the first Ctrl+C on an empty buffer.
    /// "  (press Ctrl+C again to exit)\n" — leading indent + trailing
    /// newline are part of the template.
    CtrlCAgainToExit,

    /// Discovery hint after the first bare Esc on an empty idle buffer.
    /// A second Esc within the window rolls the conversation back a turn.
    /// "  (press Esc again to undo last turn)\n" — leading indent +
    /// trailing newline are part of the template.
    EscAgainToUndo,

    /// Startup hint shown on terminals where Kitty CSI-u keyboard
    /// disambiguation isn't available, telling the user the
    /// guaranteed-works `\<Enter>` multi-line trick. Multi-line
    /// payload with leading indent + trailing paragraph break.
    HintMultiLineInput,

    // ── /bg (background sessions) ──
    /// Help text for `/bg help`. Multi-line string with leading indent
    /// and trailing newlines baked in.
    BgHelp,
    /// Empty state for `/bg list`.
    BgListEmpty,
    /// Table header for `/bg list`. Trailing newline baked in.
    BgListHeader,
    /// Row format for `/bg list`. `state` is the localised state label,
    /// `age` is the humanised age string, `summary` is the session name.
    BgListRow { slot: usize, short_id: &'a str, state: &'a str, age: &'a str, summary: &'a str },
    /// Localised label for `RuntimeState::Running`.
    BgStateRunning,
    /// Localised label for `RuntimeState::Idle`.
    BgStateIdle,
    /// Localised label for `RuntimeState::Done`.
    BgStateDone,
    /// Localised label for `RuntimeState::Cancelled`.
    BgStateCancelled,
    /// Localised label for `RuntimeState::Error`.
    BgStateError,
    /// Age string: less than 60 seconds.
    BgAgeNow,
    /// Age string: minutes. `n` is the number of minutes.
    BgAgeMinutes { n: u64 },
    /// Age string: hours. `n` is the number of hours.
    BgAgeHours { n: u64 },
    /// Age string: days. `n` is the number of days.
    BgAgeDays { n: u64 },
    /// Error: too many background slots. `max` is the slot limit.
    BgSlotLimitReached { max: usize },
    /// Output after `/bg` sends the current session to background.
    /// `new_id` is the new foreground session short id,
    /// `slot` is the background slot number,
    /// `old_id` is the backgrounded session short id,
    /// `state` is the localised runtime state.
    BgBackgroundCurrent { new_id: &'a str, slot: usize, old_id: &'a str, state: &'a str },
    /// Error: invalid slot number. `slot` is the requested slot,
    /// `available` is the number of available slots.
    BgInvalidSlot { slot: usize, available: usize },
    /// Error: background slot has no runtime client.
    BgNoRuntimeClient,
    /// Output after `/bg <N>` resumes a background session.
    /// `slot` is the resumed slot, `short_id` is the session short id.
    BgResumed { slot: usize, short_id: &'a str },
    /// When resuming moves the previous foreground into a background slot.
    /// `slot` is the new background slot number.
    BgPreviousForegroundMoved { slot: usize },
    /// Output after `/bg drop <N>`. `slot` is the dropped slot,
    /// `short_id` is the session short id.
    BgDropped { slot: usize, short_id: &'a str },
    /// Output after `/background <task>` starts a one-shot task.
    /// `slot` is the background slot, `short_id` is the session short id.
    BgTaskStarted { slot: usize, short_id: &'a str },
    /// Background task timed out. `secs` is the timeout in seconds.
    BgTaskTimedOut { secs: u64 },
    /// Background task internal error. `error` is the error message.
    BgTaskError { error: &'a str },
    /// Background task was cancelled.
    BgTaskCancelled,
    /// Background task finished but produced no summary text.
    BgTaskNoSummary,

    // CLI atomcode --help i18n
    CliAbout,
    CliAboutLogin,
    CliAboutLogout,
    CliAboutStatus,
    CliAboutUpgrade,
    CliAboutRollback,
    CliAboutFixissue,
    CliAboutMcp,
    CliAboutDaemon,
    CliAboutWebui,
    CliAboutTelemetry,
    CliAboutPlugin,
    CliAboutUninstall,
    CliAboutSetup,
    CliAboutHooks,
    CliAboutHooksList,
    CliAboutHooksTest,
    CliAboutHooksPaths,
    CliAboutPluginMarketplace,
    CliAboutPluginInstall,
    CliAboutPluginUninstall,
    CliAboutPluginList,
    CliAboutMarketplaceAdd,
    CliAboutMarketplaceRemove,
    CliAboutMarketplaceUpdate,
    CliAboutMarketplaceList,
    CliAboutMcpAdd,
    CliAboutMcpAddGithubOauth,
    CliAboutMcpLogin,
    CliAboutMcpLogout,
    CliAboutTelemetryStatus,
    CliAboutTelemetryEnable,
    CliAboutTelemetryDisable,
    CliAboutTelemetryDump,
    CliAboutTelemetryClear,
    CliHelpContinue,
    CliHelpProvider,
    CliHelpModel,
    CliHelpLang,
    CliHelpConfig,
    CliHelpDir,
    CliHelpPrompt,
    CliHelpPromptFile,
    CliHelpVerbose,
    CliHelpMaxTurns,
    CliHelpDev,
    CliHelpDisableTools,
    CliHelpNoTelemetry,
    CliHelpDangerouslySkipPermissions,
    CliHelpForce,
    CliHelpPortDaemon,
    CliHelpClient,
    CliHelpIdleTimeout,
    CliHelpPortWebui,
    CliHelpHost,
    CliHelpFixissueUrl,
    CliHelpUninstallYes,
    CliHelpUninstallPurge,
    CliHelpUninstallKeepData,
    CliHelpUninstallDryRun,
    CliHelpMcpGlobal,
    CliHelpMcpDir,
    CliHelpMcpName,
    CliHelpHooksTestName,
    CliHelpPluginSpec,
    CliHelpMarketplaceUrl,
    CliHelpMarketplaceName,
    CliHelpMcpCommand,
    /// About for the built-in help subcommand.
    CliAboutHelp,

    // ── engine v2 provider init (atomcode-bridge) ──
    /// Frame for a provider/engine init failure surfaced to the driver.
    /// `detail` carries the underlying cause (often `GatewayAuthUnavailable`).
    ProviderInitFailed { detail: &'a str },
    /// Calm advisory (yellow) when a provider build fails purely because the
    /// user isn't logged in — the expected state right after `/logout` or on a
    /// fresh launch before `/login`. Replaces the alarming red init-failure line.
    ProviderInitNeedsLogin,
    /// The configured `base_url` is an AtomGit gateway that this (open-source)
    /// build can't sign requests for. Points the user at the official binary
    /// or a plain OpenAI-compatible endpoint.
    GatewayAuthUnavailable { base_url: &'a str },

    // ── streaming liveness (atomcode-tuix spinner) ──
    /// Spinner hint shown when a streaming response has gone silent past the stall
    /// threshold. A silent stretch is OFTEN legitimate (slow first-byte prefill or
    /// long high-effort reasoning at large context), so the text makes NO judgment
    /// about speed — labelling it "slow" reads as a malfunction ("is it stuck?")
    /// when it usually isn't. The elapsed timer already conveys duration; this adds
    /// only the one thing not otherwise surfaced mid-stream — that esc cancels.
    StreamStalled,

    // ── legacy Windows console (conhost) one-shot hint ──
    /// Shown once at startup ONLY on the classic Windows console host
    /// (`TerminalCaps::legacy_conhost`), never on Windows Terminal or any
    /// other terminal. Legacy conhost snaps the viewport back to the bottom
    /// on every write, so the live footer repaint during a running task
    /// makes scrolling up to read history impossible until the task ends.
    /// This is a conhost limitation we don't fix in-app — the hint tells the
    /// user that scrolling resumes when the task finishes, and that Windows
    /// Terminal has no such limitation.
    ConhostScrollHint,
}
