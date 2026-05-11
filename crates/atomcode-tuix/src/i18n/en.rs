use std::borrow::Cow;
use super::messages::Msg;

pub(super) fn en(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        Msg::WelcomeBannerLine1 =>
            "Welcome to AtomCode. Pick an option to get started:".into(),
        Msg::WelcomeBannerLine2 =>
            "(↑↓ to navigate, Enter to confirm, Esc to skip)".into(),
        Msg::WelcomeOptionCodingPlan => "Set up CodingPlan".into(),
        Msg::WelcomeOptionCodingPlanHint => "Free tokens · recommended".into(),
        Msg::WelcomeOptionConfigureManually => "Configure manually".into(),
        Msg::WelcomeOptionConfigureManuallyHint => "API key".into(),
        Msg::WelcomeOptionSkip => "Skip for now".into(),
        Msg::WelcomeOptionSkipHint => "explore first".into(),

        Msg::ErrUnsupportedLocale { input } =>
            format!("unsupported locale: {input}").into(),

        // ── Status bar ──
        Msg::StatusNoProvider =>
            "no provider · /provider to configure".into(),
        Msg::StatusUpgradeHint { version } =>
            format!("↑ {version} available · /upgrade").into(),
        Msg::StatusModelNotConfigured =>
            "(not configured)".into(),

        // ── Help ──
        Msg::HelpAvailableCommands =>
            "  Available commands:\n".into(),

        // ── Provider wizard ──
        Msg::ProviderWizardHeader =>
            "  Provider management — Add / Edit / Delete / Set default. Esc to cancel.\n".into(),
        Msg::ProviderWizardCancelled =>
            "(cancelled)".into(),
        Msg::ProviderMenuAdd => "add".into(),
        Msg::ProviderMenuAddDesc => "Add a new provider".into(),
        Msg::ProviderMenuEdit => "edit".into(),
        Msg::ProviderMenuEditDesc => "Edit an existing provider".into(),
        Msg::ProviderMenuDelete => "delete".into(),
        Msg::ProviderMenuDeleteDesc => "Remove a provider".into(),
        Msg::ProviderMenuSetDefault => "set-default".into(),
        Msg::ProviderMenuSetDefaultDesc => "Switch the default provider".into(),
        Msg::ProviderNoProviders =>
            "No providers configured yet.".into(),
        Msg::ProviderDeleteConfirm { name } =>
            format!("Delete \"{name}\"? [y/N]").into(),
        Msg::ProviderDeleted { name } =>
            format!("Removed \"{name}\".").into(),
        Msg::ProviderDeleteKept => "(kept)".into(),
        Msg::ProviderDefaultSet { name } =>
            format!("Default set to {name}.").into(),
        Msg::ProviderAdded { name, model } =>
            format!("Added provider \"{name}\" and switched to {name} · {model}.").into(),
        Msg::ProviderUpdated { name } =>
            format!("Updated \"{name}\".").into(),
        Msg::ProviderStepName => "Provider name?".into(),
        Msg::ProviderStepType => "Type? (openai / claude / ollama)".into(),
        Msg::ProviderStepTypeWithHint { current } =>
            format!("Type? [{current}] (openai / claude / ollama, blank to keep)").into(),
        Msg::ProviderStepBaseUrl =>
            "Base URL? (blank to use provider default)".into(),
        Msg::ProviderStepBaseUrlWithHint { current } =>
            format!("Base URL? [{current}] (blank to keep)").into(),
        Msg::ProviderDefaultHint => "provider default".into(),
        Msg::ProviderStepApiKey =>
            "API key? (blank to leave unset)".into(),
        Msg::ProviderStepApiKeyWithHint { hint } =>
            format!("API key? [{hint}]").into(),
        Msg::ProviderStepApiKeySet => "set — blank to keep".into(),
        Msg::ProviderStepApiKeyUnset => "unset".into(),
        Msg::ProviderStepModel => "Model?".into(),
        Msg::ProviderStepModelWithHint { current } =>
            format!("Model? [{current}] (blank to keep)").into(),
        Msg::ProviderNameEmpty => "Name cannot be empty.".into(),
        Msg::ProviderUnknownType =>
            "Unknown type. Choose openai / claude / ollama.".into(),
        Msg::ProviderUnknownTypeEdit =>
            "Unknown type. Choose openai / claude / ollama or leave blank.".into(),
        Msg::ProviderModelEmpty => "Model cannot be empty.".into(),
        Msg::ProviderEditKeep => "(keep)".into(),

        // ── Model picker ──
        Msg::ModelSwitched { provider, model } =>
            format!("  Switched to {provider} · {model}\n").into(),

        // ── Session picker ──
        Msg::SessionLoadFailed { error } =>
            format!("load session failed: {error}").into(),
        Msg::SessionResumedLabel { name } =>
            format!("resumed: {name}").into(),
        Msg::SessionTimeJustNow => "just now".into(),
        Msg::SessionTimeMinAgo { n } => format!("{n}m ago").into(),
        Msg::SessionTimeHourAgo { n } => format!("{n}h ago").into(),
        Msg::SessionTimeDayAgo { n } => format!("{n}d ago").into(),
        Msg::SessionMsgCount { count } =>
            format!("{count} msgs").into(),

        // ── Dir picker ──
        Msg::DirCurrent => "current".into(),
        Msg::DirNotExists { path } =>
            format!("directory no longer exists: {path}").into(),
        Msg::DirChanged { path } =>
            format!("  Changed to: {path}\n").into(),

        // ── Issue wizard ──
        Msg::IssueCancelled => "(cancelled)".into(),
        Msg::IssueNewOn { owner, repo } =>
            format!("New issue on atomgit.com/{owner}/{repo}").into(),
        Msg::IssueStep1 =>
            "Step 1/2 — enter title (required, Esc to cancel):".into(),
        Msg::IssueStep2 =>
            "Step 2/2 — enter description (Shift+Enter = newline, Enter to submit, Esc to cancel):".into(),
        Msg::IssueTitleConfirmed { title } =>
            format!("✓ title: {title}").into(),
        Msg::IssueRequiredField { field } =>
            format!("(required — type a {field} or Esc to cancel)").into(),

        // ── Language ──
        Msg::LanguageSetTo { locale } =>
            format!("Language set to: {locale}").into(),

        // ── Idle / onboarding hints ──
        Msg::IdleHintPrefix =>
            "type something, or press ".into(),
        Msg::IdleHintSlash => "/".into(),
        Msg::IdleHintSuffix =>
            " to browse commands".into(),
        Msg::IdleHintFull =>
            "type something, or press / to browse commands".into(),
        Msg::IdleHintProvider => "/provider".into(),
        Msg::IdleHintProviderSuffix =>
            "to add a custom model".into(),
        Msg::IdleHintProviderFull =>
            "/provider  to add a custom model".into(),

        // ── Slash commands ──
        Msg::CmdSwitchedPlanMode =>
            "  Switched to Plan mode (read-only exploration).\n".into(),
        Msg::CmdSwitchedBuildMode =>
            "  Switched to Build mode (full execution).\n".into(),
        Msg::CmdNewSession =>
            "  New session started.\n".into(),
        Msg::CmdNoProviders =>
            "  No providers configured.\n".into(),
        Msg::CmdNoSessions =>
            "  No previous sessions found. Start a conversation first.\n".into(),
        Msg::CmdUnknownCommand { name } =>
            format!("Unknown command: /{name}").into(),
        Msg::CmdLoginFailed { error } =>
            format!("login failed: {error}").into(),
        Msg::CmdLogoutDone =>
            "  Signed out of AtomGit. Permissions refreshed.\n".into(),
        Msg::CmdLogoutFailed { error } =>
            format!("logout failed: {error}").into(),
        Msg::CmdWhoamiNotSignedIn =>
            "  Not signed in. Use /login to authenticate.\n".into(),
        Msg::CmdReloadDone { provider, model } =>
            format!("  Config reloaded. Active: {provider} · {model}\n").into(),
        Msg::CmdReloadFailed { error } =>
            format!("reload failed: {error} (kept previous config)").into(),
        Msg::CmdUndoNotSupported =>
            "  Undo is not yet supported.\n".into(),
        Msg::CmdNoChanges =>
            "  (no changes)\n".into(),
        Msg::CmdCheckingUpdate =>
            "  Checking for updates...\n".into(),
        Msg::CmdNoActiveProvider =>
            "No active provider configured. Use /provider to add one.".into(),
    }
}
