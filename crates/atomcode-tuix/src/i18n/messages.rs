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

    // i18n self-errors
    ErrUnsupportedLocale { input: &'a str },

    // ── Status bar (build_status) ──
    StatusNoProvider,
    StatusUpgradeHint { version: &'a str },
    StatusModelNotConfigured,

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

    // ── Dir picker ──
    DirCurrent,
    DirNotExists { path: &'a str },
    DirChanged { path: &'a str },

    // ── Issue wizard ──
    IssueCancelled,
    IssueNewOn { owner: &'a str, repo: &'a str },
    IssueStep1,
    IssueStep2,
    IssueTitleConfirmed { title: &'a str },
    IssueRequiredField { field: &'a str },

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
}
