use super::messages::Msg;
use std::borrow::Cow;

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

        // ── /login (full setup flow) ──
        Msg::CodingPlanSetupFailed { error } =>
            format!("/login setup failed: {error}").into(),
        Msg::CpReauthAfter401 =>
            "  ⚠ Stored login expired — re-authenticating...\n".into(),
        Msg::ChatAuthExpired =>
            "Authentication expired — please run /login to sign in again".into(),
        Msg::CpSetupHeader =>
            "  AtomCode CodingPlan setup:\n\n".into(),
        Msg::CpLoggedIn { who, username, email } =>
            format!("  ✓ Logged in as {} ({}, {})\n", who, username, email).into(),
        Msg::CpStepSkipped { reason } =>
            format!("  ✓ {}\n", reason).into(),
        Msg::CpLoginFailed { error } =>
            format!("  × Login failed — {}\n", error).into(),
        Msg::CpClaimed { message, plan_type } =>
            format!("  ✓ CodingPlan claimed — {} (CodingPlan {})\n", message, plan_type).into(),
        Msg::CpClaimSuccessFallback => "success".into(),
        Msg::CpAlreadyClaimed { reason } =>
            format!("  ✓ CodingPlan already claimed — {}\n", reason).into(),
        Msg::CpClaimFailed { error } =>
            format!("  × CodingPlan tier setup failed — {}\n", error).into(),
        Msg::CpClaimFailedBare =>
            "  × CodingPlan tier setup failed\n".into(),
        Msg::CpClaimTierSucceeded { tier } =>
            format!("  ✓ CodingPlan {} active\n", tier).into(),
        Msg::CpClaimTierAlreadyHeld { tier } =>
            format!("  ✓ CodingPlan {} active\n", tier).into(),
        Msg::CpClaimTierFailed { tier, reason } =>
            format!("  × CodingPlan {} tier setup failed — {}\n", tier, reason).into(),
        Msg::CpAddedProviders { count, plural_s } =>
            format!("  ✓ Added {} provider{}:\n", count, plural_s).into(),
        Msg::CpLocked { name } =>
            // SGR 31 = standard red foreground, SGR 39 = reset to
            // default fg. Standard (not bright) so the terminal's
            // theme palette decides the exact shade — Solarized,
            // Dracula, light-mode, etc. all map this onto their
            // own "red" rather than a hard-coded RGB the user can't
            // tune. The `× … (requires Pro plan or higher)` text inside is
            // a redundant signal so retained-mode terminals (which
            // strip SGR via the strict sanitizer path) still get
            // the meaning, just without the colour.
            format!("      \x1b[31m× {}  (requires Pro plan or higher)\x1b[39m\n", name).into(),
        Msg::CpProviderRow { provider, model, default_suffix } =>
            format!("      • {}  →  {}{}\n", provider, model, default_suffix).into(),
        Msg::CpDefaultSuffix => "  (default)".into(),
        Msg::CpVisionAuto { kind } =>
            format!("  ✓ Vision preprocessor → {}  (auto-detected)\n", kind).into(),
        Msg::CpVisionUserSupplied { kind } =>
            format!("  ✓ Vision preprocessor → {}  (user setting kept)\n", kind).into(),
        Msg::CpVisionCleared =>
            "  ⚠ Vision preprocessor cleared — no VL/OCR model in current list\n".into(),
        Msg::CpModelsSkipped { reason } =>
            format!("  ✓ Models step skipped — {}\n", reason).into(),
        Msg::CpModelsFailed { error } =>
            format!("  × Models step failed — {}\n", error).into(),
        Msg::CpStatusHeader =>
            "  ✓ CodingPlan status:\n".into(),
        Msg::CpPlanPending { plan } =>
            format!("      Plan: {}  ·  pending activation\n", plan).into(),
        Msg::CpPlanActive { plan, expires_at, remaining_days, total_days } =>
            format!(
                "      Plan: {}  ·  expires {} ({}d / {}d remaining)\n",
                plan, expires_at, remaining_days, total_days,
            ).into(),
        Msg::CpUsageLine { usage, reset_at, duration } =>
            format!("      Usage: {}  ·  resets {} (in {})\n", usage, reset_at, duration).into(),
        Msg::CpMonthlyQuotaExhausted { reset_at, duration } =>
            format!("      ⚠ Monthly quota exhausted  ·  resets at {} (in {})\n", reset_at, duration).into(),
        Msg::CpWindowQuotaExhausted =>
            "      ⚠ Current window quota exhausted\n".into(),
        Msg::CpWindowQuotaHint { hint } =>
            format!("      ⚠ {}\n", hint).into(),
        Msg::CpStatusFetchSkipped { reason } =>
            format!("  ⚠ Status fetch skipped — {}\n", reason).into(),
        Msg::CpStatusFetchFailed { error } =>
            format!("  ⚠ Status fetch failed (non-fatal) — {}\n", error).into(),
        Msg::CpOfficialBuildRequired => Cow::Borrowed(
            "This feature requires the official AtomCode build. Download it from \
             https://atomgit.com/atomgit_atomcode/atomcode/releases.",
        ),
        Msg::CpAuthRequired => Cow::Borrowed(
            "Not signed in to AtomCode CodingPlan. Run /login to sign in \
             before sending a request.",
        ),
        Msg::CpSignStaleClockSkew => Cow::Borrowed(
            "Request rejected: signed timestamp outside the accepted window. \
             Please check your system clock (NTP sync) and retry.",
        ),
        Msg::CpSignReplayPersisted => Cow::Borrowed(
            "Request was repeatedly flagged as a replay. Please try the command again.",
        ),
        Msg::CpSignVersionTooOld => Cow::Borrowed(
            "AtomCode is out of date and no longer compatible with CodingPlan. \
             Please upgrade AtomCode to continue.",
        ),
        Msg::CpUpgradeRequired => Cow::Borrowed(
            "An upgrade is required to continue using CodingPlan. \
             Please install the latest AtomCode from the official releases.",
        ),

        Msg::ErrUnsupportedLocale { input } =>
            format!("unsupported locale: {input}").into(),

        // ── Status bar ──
        Msg::StatusNoProvider =>
            "no provider · /provider to configure".into(),
        Msg::StatusOfficialBuildRequired =>
            "CodingPlan needs the official build".into(),
        Msg::StatusUpgradeHint { version } =>
            format!("↑ {version} available · /upgrade").into(),
        Msg::StatusModelNotConfigured =>
            "(not configured)".into(),
        Msg::StatusClipboardImageHint =>
            "Image in clipboard · ctrl+v to paste".into(),
        Msg::StatusClipboardImageHintSlash =>
            "Image in clipboard · /paste".into(),

        // ── /status command body ──
        Msg::StatusBody { model, dir, config, tokens } =>
            format!(
                "  Model:  {}\n  Dir:    {}\n  Config: {}\n  Tokens: {}\n",
                model, dir, config, tokens,
            ).into(),
        Msg::StatusCpNotSignedIn =>
            "  CodingPlan: (not signed in — run /login to set up)\n".into(),
        Msg::StatusCpFetchFailed { error } =>
            format!("  CodingPlan: (status fetch failed — {})\n", error).into(),
        Msg::StatusCpNoActive =>
            "  CodingPlan: (no active plan — run /login)\n".into(),
        Msg::StatusCpLine { plan, expires_at, remaining_days, total_days } =>
            format!(
                "  CodingPlan: {}  ·  expires {} ({}d/{}d)\n",
                plan, expires_at, remaining_days, total_days,
            ).into(),
        Msg::StatusCpUsage { usage, reset_at, seconds } =>
            format!("  Usage: {}  ·  resets {} (in {}s)\n", usage, reset_at, seconds).into(),
        Msg::StatusCpWindowExhausted =>
            "  ⚠ Current window quota exhausted\n".into(),
        Msg::StatusCpWindowHint { hint } =>
            format!("  ⚠ {}\n", hint).into(),
        Msg::StatusInstructionFilesHeader =>
            "  Instruction files:\n".into(),
        Msg::StatusInstructionPresent { path, label } =>
            format!("    ✓ {} ({})\n", path, label).into(),
        Msg::StatusInstructionMissing { label } =>
            format!("    × {} — not found\n", label).into(),

        // ── Help ──
        Msg::HelpAvailableCommands =>
            "  Available commands:\n".into(),
        Msg::KeybindingsHelp => r#"  Keyboard shortcuts

  ── Input ──
    Enter                            Send message
    Ctrl+J                           Insert newline (universal)
    \ then Enter                     Insert newline (atomcode fallback, universal)
    Alt+Enter                        Insert newline *
    Shift+Enter                      Insert newline **
    /                                Open slash command menu
    Tab                              Autocomplete
    Backspace / Ctrl+H               Delete previous char
    Delete / Ctrl+?                  Delete next char
    Ctrl+W                           Delete word backward
    Ctrl+U                           Clear current line
    Ctrl+K                           Delete to end of line
    Ctrl+A / Home                    Jump to line start
    Ctrl+E / End                     Jump to line end
    Left / Right                     Move cursor

  ── History ──
    Up                               Previous input
    Down                             Next input

  ── Scrollback ──
    Use the host terminal's native scrollback (cmd+↑/↓, mouse wheel,
    tmux copy-mode — whatever your terminal already provides).
    Drag + Ctrl+C                    Copy text (atomcode does not capture the mouse)

  ── Session ──
    Ctrl+C                           Cancel current turn / dismiss modal
    Ctrl+D                           Exit AtomCode
    Ctrl+L                           Clear screen
    Ctrl+O                           Toggle tool real-time output
    Ctrl+V                           Paste (text + image)

  ── Slash menu / modal navigation ──
    Up / Down                        Move selection
    Enter                            Confirm
    Esc                              Cancel / close modal
    Tab                              Insert highlighted command

  * Alt+Enter works in most terminals; macOS Apple Terminal users
    must enable "Use Option as Meta key" under Settings → Profiles
    → Keyboard for the keystroke to register as a newline.
  ** Shift+Enter requires a terminal that disambiguates the modifier.
     Known-supported: Kitty / WezTerm / iTerm2 (with Report Modifiers
     enabled) / Windows Terminal / Ghostty / Warp. Other terminals
     (macOS Apple Terminal, default xterm, GNOME Terminal, VS Code's
     integrated terminal) collapse Shift+Enter into plain Enter —
     use Ctrl+J or \ + Enter instead.

  Tip: run /help for the full slash command list.
"#.into(),

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
        Msg::SessionNameEmpty =>
            "Session name cannot be empty".into(),
        Msg::SessionNameTooLong { max } =>
            format!("Session name too long (max {max} characters)").into(),
        Msg::SessionNameControlChars =>
            "Session name cannot contain control characters".into(),
        Msg::SessionListFailed { error } =>
            format!("list sessions failed: {error}").into(),
        Msg::SessionRenamed { old, new } =>
            format!("  Renamed: '{old}' -> '{new}'").into(),
        Msg::SessionSaveFailed { error } =>
            format!("Failed to save session: {error}. The name was not persisted.").into(),
        Msg::SessionNoneSelected =>
            "No session selected".into(),
        Msg::SessionRenameEditing { buffer } =>
            format!("> {buffer}_  [Enter: confirm, Esc: cancel]").into(),

        // ── Dir picker ──
        Msg::DirCurrent => "current".into(),
        Msg::DirNotExists { path } =>
            format!("directory no longer exists: {path}").into(),
        Msg::DirChanged { path } =>
            format!("  Changed to: {path}\n").into(),
        Msg::DirNotADirectory { path } =>
            format!("Not a directory: {path}").into(),

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
        Msg::IssueCreated { number, title, url } =>
            format!("  [issue] ✓ created #{number}: {title}\n  {url}\n").into(),
        Msg::IssueCreateFailed { error } =>
            format!("  [issue] × create failed: {error}\n").into(),
        Msg::IssueRequiredField { field } =>
            format!("(required — type a {field} or Esc to cancel)").into(),

        // ── Language ──
        Msg::LanguageSwitched { label, locale } =>
            format!("  ✓ Language switched to {label} ({locale}).\n").into(),

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
        Msg::IdleHintCodingplan => "/login".into(),
        Msg::IdleHintCodingplanSuffix =>
            "to claim a free token quota".into(),
        Msg::IdleHintCodingplanFull =>
            "/login  to claim a free token quota".into(),

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

        // ── Approval prompt ──
        Msg::ApprovalPromptAlt { tool, detail } =>
            format!("Allow {}({})? [Y]es / [N]o / [A]lways", tool, detail).into(),
        Msg::ApprovalWaitingLabel =>
            "▶ Waiting for approval: ".into(),
        Msg::ApprovalAllow => " Allow  ".into(),
        Msg::ApprovalAlways => " Always  ".into(),
        Msg::ApprovalDeny => " Deny".into(),

        // ── Cancelled / Error prefix ──
        Msg::Cancelled => "(cancelled)".into(),
        Msg::ErrorPrefix { msg } =>
            format!("[Error: {msg}]").into(),

        // ── Upgrade ──
        Msg::UpgradeSuccess { from, to } =>
            format!("  ✓ Upgraded {} → {}\n", from, to).into(),
        Msg::UpgradeManifestFetched { version } =>
            format!("  Latest version: {}\n", version).into(),
        Msg::UpgradeDownloading { pct, bytes, total } =>
            format!("  Downloading {}% ({} / {} bytes)\n", pct, bytes, total).into(),
        Msg::UpgradeVerifying =>
            "  Verifying SHA256\n".into(),
        Msg::UpgradeReplacing =>
            "  Replacing binary\n".into(),
        Msg::UpgradeDone { version, backup } =>
            format!("\n✓ Upgraded to {} (previous version kept at {})\n  Restarting new version...\n", version, backup).into(),
        Msg::UpgradeAlreadyLatest { current, latest } =>
            format!(
                "  ✓ Already on the latest version. already on {} (latest is {}). Pass --force to reinstall.\n",
                current, latest
            ).into(),
        Msg::UpgradeFailed { error } =>
            format!("Upgrade failed: {}", error).into(),
        Msg::UpgradeRolledBack { exe, backup } =>
            format!("\n✓ Rolled back. Current binary: {}; other version saved at {}\n  Restarting rolled-back version...\n", exe, backup).into(),

        // ── Terminal keyboard hints ──
        Msg::KbdHintMacos =>
            "  ⚠ Terminal does not support enhanced keyboard protocol.\n    Use Ctrl+Enter for newline (Shift+Enter won't work).\n\n".into(),
        Msg::KbdHintOther =>
            "  ⚠ Terminal does not support enhanced keyboard protocol.\n    Use Alt+Enter or Ctrl+Enter for newline (Shift+Enter won't work).\n\n".into(),

        // ── Background task ──
        Msg::BackgroundComplete { turns } =>
            format!("  Background task complete ({} turn{}):\n",
                    turns, if turns == 1 { "" } else { "s" }).into(),
        Msg::BackgroundFailed { turns } =>
            format!("  Background task failed after {} turn{}:\n",
                    turns, if turns == 1 { "" } else { "s" }).into(),
        Msg::BackgroundFilesEdited =>
            "  Files edited:\n".into(),

        // ── /config ──
        Msg::ConfigProviderLabel { provider, path } =>
            format!("  Provider: {}\n  Config: {}\n\n", provider, path).into(),

        // ── /cost ──
        Msg::CostReport { prompt, completion, cached, cache_rate, total, cost } =>
            format!(
                "  Prompt tokens:     {}\n  Completion tokens: {}\n  Cached tokens:     {} ({}% hit rate)\n  Total tokens:      {}\n  Estimated cost:    {}\n",
                prompt, completion, cached, cache_rate, total, cost
            ).into(),

        // ── /think ──
        Msg::ThinkStatus { status, budget, provider } =>
            format!(
                "  Extended thinking: {}\n  Budget: {} tokens\n  Provider: {}\n\n  Usage: /think on | off | budget <N>\n",
                status, budget, provider
            ).into(),
        Msg::ThinkEnabled { budget } =>
            format!("  Extended thinking enabled (budget: {} tokens).\n", budget).into(),
        Msg::ThinkDisabled =>
            "  Extended thinking disabled.\n".into(),
        Msg::ThinkBudgetSet { n } =>
            format!("  Thinking budget set to {} tokens.\n", n).into(),
        Msg::ThinkBudgetTooSmall { n } =>
            format!("Budget must be >= 1024 (got {})", n).into(),
        Msg::ThinkBudgetUsage =>
            "Usage: /think budget <number>".into(),
        Msg::ThinkUsage =>
            "  Usage: /think [on | off | budget <N>]\n".into(),

        // ── /remember, /forget ──
        Msg::RememberUsage =>
            "Usage: /remember <fact to remember>  (--global for global scope)".into(),
        Msg::ForgetUsage =>
            "Usage: /forget <keyword>".into(),

        // ── /background ──
        Msg::BackgroundUsage =>
            "  Usage: /background <task description>\n".into(),

        // ── /init ──
        Msg::InitAlreadyExists { path } =>
            format!("  {} already exists. Use `/init --force` to overwrite.\n", path).into(),
        Msg::InitWrote { path, bytes } =>
            format!("  Wrote {} ({} bytes). Edit to customise; takes effect on next message.\n", path, bytes).into(),
        Msg::InitFailed { error } =>
            format!("  /init failed: {}\n", error).into(),

        // ── /cd ──
        Msg::CdWorkingDir { cwd } =>
            format!("  Working directory: {}\n  No recent projects. Use `/cd <path>` to switch.\n", cwd).into(),

        // ── /diff ──
        Msg::DiffFailed { error } =>
            format!("git diff failed: {}", error).into(),

        // ── /upgrade ──
        Msg::UpgradeUnknownArg { arg } =>
            format!("unknown /upgrade argument: {}\n  usage: /upgrade [rollback|--force]", arg).into(),

        // ── /skills ──
        Msg::SkillsNone =>
            "  No user-invocable skills loaded.\n".into(),
        Msg::SkillsAvailable =>
            "  Available skills:\n".into(),
        Msg::SkillUnknown { name } =>
            format!("Unknown skill: {} (try /skills to list)", name).into(),

        // ── /mcp ──
        Msg::McpReloading { count } =>
            format!("  Reloading MCP servers... ({} configured)\n", count).into(),
        Msg::McpConnecting =>
            "  Connecting:\n".into(),
        Msg::McpConnectingServer { name } =>
            format!("    - {}  connecting...\n", name).into(),
        Msg::McpNoServersConfigured =>
            "  No MCP servers configured.\n".into(),
        Msg::McpClearedReconnecting { removed } =>
            format!("  ✓ Cleared {} MCP tools. Reconnecting in background...\n", removed).into(),
        Msg::McpClearedNoServers { removed } =>
            format!("  ✓ Cleared {} MCP tools. No servers to connect.\n", removed).into(),
        Msg::McpToolsUsage =>
            "  Usage: /mcp tools <server>\n  Example: /mcp tools filesystem\n".into(),
        Msg::McpToolsListing { server } =>
            format!("  Listing MCP tools for '{}'...\n", server).into(),
        Msg::McpNoRegistry =>
            "  No MCP registry loaded. Run /mcp reload first.\n".into(),
        Msg::McpServersHeader =>
            "  MCP Servers:\n".into(),
        Msg::McpReloadFailed { error } =>
            format!("mcp reload failed: failed to load .mcp.json / $ATOMCODE_HOME/mcp.json: {:#}", error).into(),
        // /mcp login / logout
        Msg::McpOAuthLoginUsage =>
            "  Usage: /mcp login <server>\n  Example: /mcp login github\n".into(),
        Msg::McpOAuthLogoutUsage =>
            "  Usage: /mcp logout <server>\n  Example: /mcp logout github\n".into(),
        Msg::McpOAuthLoadConfigFailed { error } =>
            format!("  MCP OAuth login failed to load config: {error}\n").into(),
        Msg::McpOAuthServerNotFound { server } =>
            format!("  MCP OAuth login failed: server '{server}' not found in config.\n").into(),
        Msg::McpOAuthStarting { server } =>
            format!("  Starting MCP OAuth for '{server}' in your browser...\n").into(),
        Msg::McpOAuthSaved { provider, server } =>
            format!("  Saved {provider} OAuth token for MCP server '{server}'. Run /mcp reload to connect.\n").into(),
        Msg::McpOAuthFailed { error } =>
            format!("  MCP OAuth failed: {error}\n").into(),
        Msg::McpOAuthTokenRemoved { server } =>
            format!("  Removed saved OAuth token for MCP server '{server}'.\n").into(),
        Msg::McpOAuthNoToken { server } =>
            format!("  No saved OAuth token found for MCP server '{server}'.\n").into(),
        Msg::McpOAuthLogoutFailed { error } =>
            format!("  MCP OAuth logout failed: {error}\n").into(),
        // MCP / LSP server connect feedback
        Msg::McpServerConnected { name } =>
            format!("✓ MCP server '{name}' connected").into(),
        Msg::McpServerFailed { name, error } =>
            format!("× MCP server '{name}' failed: {error}").into(),
        Msg::LspServerStarted { name, ext } =>
            format!("✓ LSP server '{name}' started for .{ext}").into(),
        Msg::LspServerFailed { name, ext, error } =>
            format!("× LSP server '{name}' for .{ext} failed: {error}").into(),

        // ── /worktree ──
        Msg::WorktreeUsage =>
            "  Usage:\n    /worktree create <branch> [base]  Create worktree and switch\n    /worktree list                     List all worktrees\n    /worktree done                     Switch back to original directory\n    /worktree cleanup <branch>         Clean up worktree\n".into(),
        Msg::WorktreeCreateUsage =>
            "  Usage: /worktree create <branch> [base]\n  Example: /worktree create fix-bug main\n".into(),
        Msg::WorktreeCreated { branch, base, path } =>
            format!("  ✓ Worktree created\n    Branch: {} (based on {})\n    Path: {}\n    Working directory switched\n", branch, base, path).into(),
        Msg::WorktreeCreateFailed { error } =>
            format!("worktree create failed: {}", error).into(),
        Msg::WorktreeNoActive =>
            "  No active worktrees.\n".into(),
        Msg::WorktreeListFailed { error } =>
            format!("worktree list failed: {}", error).into(),
        Msg::WorktreeActiveHeader =>
            "  Active worktrees:\n".into(),
        Msg::WorktreeHasChanges => "(has changes)".into(),
        Msg::WorktreeClean => "(clean)".into(),
        Msg::WorktreeCurrent => " ← current".into(),
        Msg::WorktreeDoneBack { path } =>
            format!("  ✓ Switched back to: {}\n", path).into(),
        Msg::WorktreeDoneMergeHint { branch } =>
            format!("  Hint: use 'git merge {}' or create a PR to merge into main branch\n", branch).into(),
        Msg::WorktreeNoSession =>
            "  No active worktree session. Use /worktree create first.\n".into(),
        Msg::WorktreeCleanupUsage =>
            "  Usage: /worktree cleanup <branch> [--force]\n".into(),
        Msg::WorktreeCleaned { branch } =>
            format!("  ✓ Worktree '{}' cleaned up\n", branch).into(),
        Msg::WorktreeCleanedSwitched { path } =>
            format!("  Switched back to: {}\n", path).into(),
        Msg::WorktreeCleanupUncommitted { branch } =>
            format!("  ⚠ Worktree '{}' has uncommitted changes.\n  Use /worktree cleanup {} --force to force cleanup\n", branch, branch).into(),
        Msg::WorktreeCleanupFailed { error } =>
            format!("worktree cleanup failed: {}", error).into(),

        // ── /help commands (custom) ──
        Msg::HelpCustomCommandsHeader =>
            "  Custom commands:\n".into(),
        Msg::HelpCustomNone =>
            "    (none)\n\n".into(),
        Msg::HelpCustomCreateHint =>
            "  Create: ~/.atomcode/commands/<name>.md or .atomcode/commands/<name>.md\n".into(),
        Msg::HelpSourceGlobal => "global".into(),
        Msg::HelpSourceProject => "project".into(),

        // ── /setup ──
        Msg::SetupHeader { installed, skipped, failed, duration_ms } =>
            format!("\n✅ Setup complete — {} installed, {} skipped, {} failed  · {}ms\n\n", installed, skipped, failed, duration_ms).into(),
        Msg::SetupInstalledLabel =>
            "Installed:\n".into(),
        Msg::SetupSkippedLabel =>
            "\nSkipped:\n".into(),
        Msg::SetupFailedLabel =>
            "\nFailed:\n".into(),
        Msg::SetupInstalledRow { kind, slug, path } =>
            format!("  ✓ {}:{} → {}\n", kind, slug, path).into(),
        Msg::SetupSkippedRow { kind, slug, reason } =>
            format!("  - {}:{} ({:?})\n", kind, slug, reason).into(),
        Msg::SetupFailedRow { kind, slug, error } =>
            format!("  × {}:{} — {}\n", kind, slug, error).into(),
        Msg::CmdSetupTip =>
            "\u{1f4a1} Tip: Run \x1b[1;96m/setup\x1b[0m to auto-configure hooks, skills, and MCP for this project.".into(),
        Msg::CmdSetupRunning =>
            "Running atomcode setup...".into(),
        Msg::CmdSetupSkillsReloaded { count } =>
            format!("  🔄 Skills reloaded — {} available", count).into(),
        Msg::CmdSetupError { error } =>
            format!("setup error: {error}").into(),
        Msg::CmdSetupRunningSkill =>
            "  🚀 Running setup skill — analyzing project and generating recommendations...".into(),
        Msg::CmdSetupSkillMissing =>
            "setup skill not found — try running /setup again to reinstall".into(),

        // ── /plugin ──
        Msg::PluginUsage =>
            "usage: /plugin [marketplace add|remove|update|list | install <p>@<m> | uninstall <p>@<m> | reload | list]".into(),
        Msg::PluginMarketplaceUsage =>
            "usage: /plugin marketplace [add|remove|update|list] <args>".into(),
        Msg::PluginInstallUsage =>
            "usage: /plugin install <plugin>@<marketplace>".into(),
        Msg::PluginUninstallUsage =>
            "usage: /plugin uninstall <plugin>@<marketplace>".into(),
        Msg::PluginNoMarketplaces =>
            "no marketplaces registered".into(),
        Msg::PluginMarketplacesHeader =>
            "registered marketplaces:".into(),
        Msg::PluginNoInstalled =>
            "no installed plugins".into(),
        Msg::PluginInstalledHeader =>
            "installed plugins:".into(),
        Msg::PluginMarketplaceCloning { url } =>
            format!("cloning marketplace from {url}…").into(),
        Msg::PluginMarketplaceRemoved { name } =>
            format!("marketplace `{name}` removed").into(),
        Msg::PluginMarketplaceRemoveFailed { error } =>
            format!("remove marketplace: {error}").into(),
        Msg::PluginMarketplaceUpdating { name } =>
            format!("updating marketplace `{name}`…").into(),
        Msg::PluginMarketplaceListFailed { error } =>
            format!("list marketplaces: {error}").into(),
        Msg::PluginInstalling { plugin, marketplace } =>
            format!("installing `{plugin}@{marketplace}`…").into(),
        Msg::PluginUninstalled { plugin, marketplace } =>
            format!("uninstalled `{plugin}@{marketplace}`").into(),
        Msg::PluginUninstallFailed { error } =>
            format!("uninstall: {error}").into(),
        Msg::PluginListFailed { error } =>
            format!("list plugins: {error}").into(),
        Msg::PluginReloadDone { skills, warnings } =>
            format!("Plugins reloaded: {skills} skill(s), {warnings} warning(s)").into(),
        Msg::PluginMarketplaceAdded { name, commit, count } =>
            format!("✓ marketplace `{name}` added at {commit} ({count} plugins)").into(),
        Msg::PluginMarketplaceUpdated { name, commit } =>
            format!("✓ marketplace `{name}` updated to {commit}").into(),
        Msg::PluginInstallDone { plugin, marketplace, loaded, skipped, show_details_hint } => {
            let hint = if show_details_hint { "  (Ctrl+O for details)" } else { "" };
            format!("✓ installed `{plugin}@{marketplace}` — {loaded} skills loaded, {skipped} skipped{hint}").into()
        }
        Msg::SetupAutoReloaded { skills, warnings } =>
            format!("✓ Setup complete, auto-reloaded: {skills} skill(s), {warnings} warning(s)").into(),

        // ── Command descriptions ──
Msg::CmdDescSetup =>
"Scan project, install seeds, and run setup skill [hooks|mcp|skills|all]".into(),
        Msg::CmdDescResume => "Resume a previous session".into(),
        Msg::CmdDescRename => "Rename current session".into(),
        Msg::CmdDescLogin => "Sign in with AtomGit OAuth and claim CodingPlan models".into(),
        Msg::CmdDescLogout => "Sign out of AtomGit".into(),
        Msg::CmdDescWhoami => "Show current logged-in user".into(),
        Msg::CmdDescModel => "Switch provider / model".into(),
        Msg::CmdDescProvider => "Manage providers (add / edit / delete)".into(),
        Msg::CmdDescStatus => "Show session status".into(),
        Msg::CmdDescConfig => "Show config path".into(),
        Msg::CmdDescReload => "Reload $ATOMCODE_HOME/config.toml from disk".into(),
        Msg::CmdDescCd => "Change working directory".into(),
Msg::CmdDescInit => "Generate .atomcode.md project instructions from the working directory".into(),
Msg::CmdDescBg => "Background sessions: /bg, /bg list, /bg <N>, /bg drop <N>".into(),
Msg::CmdDescBackground => "Run a one-shot task in an isolated background context (read-only-ish tool subset)".into(),
        Msg::CmdDescDiff => "Show git diff".into(),
        Msg::CmdDescClear => "Clear screen".into(),
        Msg::CmdDescSession => "Start a new session (clears conversation)".into(),
        Msg::CmdDescCost => "Show token cost".into(),
        Msg::CmdDescContext => "Show context budget breakdown".into(),
        Msg::CmdDescCompact => "Compact conversation history".into(),
        Msg::CmdDescRemember => "Save a fact to memory (/remember --global for global)".into(),
        Msg::CmdDescForget => "Remove matching memories".into(),
        Msg::CmdDescMemory => "Show all saved memories".into(),
        Msg::CmdDescMcp => "Show MCP server status (subcommand: reload)".into(),
        Msg::CmdDescUndo => "Undo last change (not yet supported)".into(),
        Msg::CmdDescWorktree => "Git worktree isolation (create/list/done/cleanup)".into(),
        Msg::CmdDescUpgrade => "Upgrade atomcode to latest (subcommand: rollback)".into(),
        Msg::CmdDescIssue => "Report a bug / request a feature for AtomCode itself (interactive wizard)".into(),
        Msg::CmdDescPlan => "Switch to Plan mode (read-only exploration)".into(),
        Msg::CmdDescBuild => "Switch to Build mode (full execution)".into(),
        Msg::CmdDescThink => "Extended thinking control (on/off/budget N)".into(),
        Msg::CmdDescEffort => "DeepSeek reasoning effort control (high / max / off)".into(),
        Msg::CmdDescHelp => "Show this help".into(),
        Msg::CmdDescKeys => "Show keyboard shortcuts".into(),
        Msg::CmdDescLanguage => "Switch display language".into(),
        Msg::CmdDescQuit => "Exit AtomCode".into(),
        Msg::CmdDescSkills => "Browse loaded skills".into(),
        Msg::CmdDescPlugin => "Plugin marketplace (subcommands: marketplace, install, uninstall, reload, list)".into(),
        Msg::CmdDescPaste => "Attach an image from the clipboard (Windows fallback for Ctrl+V)".into(),
        Msg::CmdPasteNoImage => "No image in clipboard.".into(),
        Msg::ReasoningEffortNoEffect => "reasoning_effort has no effect on the current model (only DeepSeek V4 / reasoner)".into(),

        // ── config save failed ──
        Msg::ConfigSaveFailed { error } =>
            format!("config save failed: {}", error).into(),

        // ── OnboardingWizard ──
        Msg::OnboardingStepHeaderWelcome => "Step 1/3 · Welcome".into(),
        Msg::OnboardingStepHeaderLanguage => "Step 2/3 · Language".into(),
        Msg::OnboardingStepHeaderSetup => "Step 3/3 · Setup".into(),
        Msg::OnboardingPanelTitle => "AtomCode".into(),
        Msg::OnboardingIntroVersionLine { v } =>
            format!("Version {v}  ·  AI coding agent in your terminal").into(),
        Msg::OnboardingIntroBullet1 =>
            "• Multi-step agent loop · built-in code-graph tools".into(),
        Msg::OnboardingIntroBullet2 =>
            "• Connects to any OpenAI-compatible API".into(),
        Msg::OnboardingIntroBullet3 =>
            "• Free tokens via CodingPlan".into(),
        Msg::OnboardingIntroPressEnter => "Press Enter to continue.".into(),
        Msg::OnboardingIntroCtrlC => "Ctrl+C exits at any point.".into(),
        Msg::OnboardingIntroCompactTagline =>
            "AI coding agent that lives in your terminal.".into(),
        Msg::OnboardingLanguageTitleBilingual =>
            "Choose your language / 选择语言".into(),
        Msg::OnboardingLanguagePrompt =>
            "Pick the UI language. You can change it any time with `/language`.".into(),
        Msg::OnboardingLanguageOptionAuto =>
            "Auto-detect (LC_ALL / LANG)".into(),
        Msg::OnboardingLanguageOptionEn => "English".into(),
        Msg::OnboardingLanguageOptionZhCn => "简体中文 (Simplified Chinese)".into(),
        Msg::OnboardingSetupTitle => "How would you like to set up?".into(),
        Msg::OnboardingNavHint =>
            "1-3 select · Enter confirm · ← back · Esc skip".into(),
        Msg::OnboardingConfirmClear =>
            "/welcome will clear the screen. Continue? [y/N]".into(),
        Msg::CmdWelcomeDescription => "Re-run the onboarding wizard".into(),
        Msg::VisionPreprocessSuccess { char_count } =>
            format!("✓ VL recognised image, returned {char_count} chars").into(),
        Msg::TurnSummary { done, turn_count, tool_call_count, duration, total_tokens } =>
            format!("✓ {done} · {turn_count} rounds · {tool_call_count} tools · {duration} · {total_tokens} tokens").into(),
        Msg::LoginQrHeader =>
            "  Sign in to AtomGit — scan the QR code with your WeChat:\n\n".into(),
        Msg::LoginUrlAfterQr =>
            "\n\n  OR open the URL below in a browser:\n  ".into(),
        Msg::LoginNoQrNoUrl =>
            "  Cannot render a QR code in this terminal,\n  \
             and URL-based login is unavailable on this platform.\n  \
             Try a Unicode-capable terminal to display the QR.".into(),
        Msg::LoginUrlOnly =>
            "  Open this URL in any browser to sign in to AtomGit:\n  ".into(),
        Msg::LoginCancelHint => "\n\n  Press ESC to cancel\n".into(),
        Msg::CtxUsageHeader => "Context Usage".into(),
        Msg::CtxUsageNoTurns => "(run at least one turn first — stats are captured per turn)".into(),
        Msg::CtxUsageWaiting => "(waiting for first complete turn — partial stats only)".into(),
        Msg::CtxProvider => "Provider".into(),
        Msg::CtxCtxName => "ctx".into(),
        Msg::CtxLabelSystemPrompt => "System prompt".into(),
        Msg::CtxLabelToolDefs => "Tool defs".into(),
        Msg::CtxLabelColdZone => "Cold zone".into(),
        Msg::CtxLabelMessages => "Messages".into(),
        Msg::CtxLabelFree => "Free".into(),
        Msg::CtxMessagesInWindow { n } => format!("Messages in window: {n}").into(),
        Msg::CtxSystemPromptHeader => "=== SYSTEM PROMPT ===".into(),
        Msg::CtxSystemPromptEmpty => "(empty — wait for one complete turn to capture)".into(),
        Msg::CtxTokensSuffix => "tokens".into(),
        Msg::CompactNothingShort => "(nothing to compact — conversation is short)\n".into(),
        Msg::CompactStarting => "(compacting with LLM summary...)\n".into(),
        Msg::CompactNothingNoSavings { before, after } =>
            format!("(nothing to compact — would not save tokens: {} → {})\n", before, after).into(),
        Msg::CompactDropped { messages, before, after } => {
            let plural = if messages == 1 { "" } else { "s" };
            format!("(compacted — dropped {} message{}, {} → {} tokens)\n", messages, plural, before, after).into()
        }
        Msg::ModelNoImageSupport { model } => format!(
            "Current model \"{}\" does not support image input and no \
             vision_preprocessor_provider is configured. Use /model to \
             switch to a vision-capable model, or set \
             vision_preprocessor_provider in config.",
            model
        )
        .into(),
        Msg::CtrlCAgainToExit => "  (press Ctrl+C again to exit)\n".into(),
        Msg::HintMultiLineInput =>
            "  \u{24d8} Multi-line input: end the line with `\\` then press Enter.\n    \
            Works in every terminal. (Shift / Alt / Ctrl + Enter may also work\n    \
            depending on the terminal's keyboard protocol — try them out.)\n\n"
                .into(),

        // ── /bg (background sessions) ──
        Msg::BgHelp =>
            "  /bg                 Send current session to background and open a new foreground\n  /bg list            List background sessions\n  /bg <N>             Resume background slot N\n  /bg drop <N>        Drop background slot N\n  /bg help            Show this help\n".into(),
        Msg::BgListEmpty => "  No background sessions.\n".into(),
        Msg::BgListHeader => "  #   ID        State      Created   Summary\n".into(),
        Msg::BgListRow { slot, short_id, state, age, summary } =>
            format!("  {:<3} {:<8}  {:<9}  {:<8}  {}\n", slot, short_id, state, age, summary).into(),
        Msg::BgStateRunning => "running".into(),
        Msg::BgStateIdle => "idle".into(),
        Msg::BgStateDone => "done".into(),
        Msg::BgStateCancelled => "cancelled".into(),
        Msg::BgStateError => "error".into(),
        Msg::BgAgeNow => "now".into(),
        Msg::BgAgeMinutes { n } => format!("{n}m").into(),
        Msg::BgAgeHours { n } => format!("{n}h").into(),
        Msg::BgAgeDays { n } => format!("{n}d").into(),
        Msg::BgSlotLimitReached { max } =>
            format!("background slot limit reached ({max})").into(),
        Msg::BgBackgroundCurrent { new_id, slot, old_id, state } =>
            format!("  New foreground session [{new_id}]\n  Background: [#{slot}] {old_id} (state: {state})\n").into(),
        Msg::BgInvalidSlot { slot, available } =>
            format!("invalid background slot {slot} (available: {available})").into(),
        Msg::BgNoRuntimeClient => "background slot has no runtime client".into(),
        Msg::BgResumed { slot, short_id } =>
            format!("  Resumed background [#{slot}] {short_id}\n").into(),
        Msg::BgPreviousForegroundMoved { slot } =>
            format!("  Previous foreground moved to [#{slot}]\n").into(),
        Msg::BgDropped { slot, short_id } =>
            format!("  Dropped background [#{slot}] {short_id}\n").into(),
        Msg::BgTaskStarted { slot, short_id } =>
            format!("  Background: [#{slot}] {short_id} (state: running)\n").into(),
        Msg::BgTaskTimedOut { secs } =>
            format!("Background task timed out after {secs}s.").into(),
        Msg::BgTaskError { error } =>
            format!("Error: {error}").into(),
        Msg::BgTaskCancelled => "Cancelled.".into(),
        Msg::BgTaskNoSummary => "Task completed (no summary text).".into(),
    }
}

#[cfg(test)]
mod codingplan_crypto_tests {
    use super::*;
    use crate::i18n::Msg;

    #[test]
    fn en_official_build_required_mentions_releases() {
        let s = en(Msg::CpOfficialBuildRequired);
        assert!(s.contains("official"));
        assert!(s.contains("releases"));
    }

    #[test]
    fn en_stale_clock_mentions_system_time() {
        let s = en(Msg::CpSignStaleClockSkew);
        assert!(s.to_lowercase().contains("clock") || s.to_lowercase().contains("time"));
    }

    #[test]
    fn en_replay_persisted_is_non_empty() {
        let s = en(Msg::CpSignReplayPersisted);
        assert!(!s.is_empty());
    }

    #[test]
    fn en_version_too_old_mentions_upgrade() {
        let s = en(Msg::CpSignVersionTooOld);
        assert!(s.to_lowercase().contains("upgrade") || s.to_lowercase().contains("update"));
    }

    #[test]
    fn en_upgrade_required_is_non_empty() {
        let s = en(Msg::CpUpgradeRequired);
        assert!(!s.is_empty());
    }
}
