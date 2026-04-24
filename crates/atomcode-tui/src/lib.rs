pub mod app;
pub mod command;
pub mod event;
pub mod file_attach;
pub mod project_context;
pub mod provider_manager;
// turn_log removed 2026-04-13: atomcode-core owns datalog (DatalogWriter) —
// having two per-turn loggers wrote 2 files per turn (one per crate).
pub mod ui;
pub mod wecom_login;

use std::io::Write;

use anyhow::{Context, Result};
use crossterm::{
    execute,
    event::{
        DisableMouseCapture,
        KeyboardEnhancementFlags, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    },
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        SetTitle, Clear, ClearType,
    },
};
#[cfg(not(target_os = "windows"))]
use crossterm::event::{EnableBracketedPaste, DisableBracketedPaste};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Clear the main screen and scrollback buffer before entering alternate screen.
///
/// This prevents macOS Terminal.app from scrolling "through" the alternate screen
/// to reveal the main screen's scrollback history on mouse wheel scroll-up when
/// mouse capture is temporarily disabled.
fn clear_scrollback(w: &mut impl Write) -> std::io::Result<()> {
    // \x1b[2J  — clear entire screen (so saved main screen is blank)
    // \x1b[3J  — clear scrollback buffer
    // \x1b[H   — move cursor to top-left
    w.write_all(b"\x1b[2J\x1b[3J\x1b[H")?;
    w.flush()
}

/// Flush the terminal input buffer (discard any pending input)
/// This is critical when switching between TUI and external editor
#[cfg(unix)]
fn flush_stdin() {
    unsafe {
        // TCIFLUSH = 0, flush input queue
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

#[cfg(not(unix))]
fn flush_stdin() {
    // Previously tried to drain via stdin.read() in a loop, but Windows
    // console stdin never returns n==0 when empty — it blocks waiting for
    // input. That hung the entire TUI the moment the user ran `/config`:
    // the alternate screen was left, flush_stdin blocked forever, the user
    // saw a blank terminal and assumed the app "exited".
    //
    // Any buffered keystrokes will just be consumed by the child editor,
    // which is harmless (worst case: a few extra keypresses in vim/notepad).
    // If we ever need a real flush on Windows, use FlushConsoleInputBuffer
    // from windows-sys — not a blocking read loop.
}

/// Look up an executable on PATH, honouring Windows' PATHEXT suffixes.
fn which(cmd: &str) -> Option<std::path::PathBuf> {
    if cmd.contains(std::path::MAIN_SEPARATOR) || cmd.contains('/') {
        // Already a path — trust it as-is.
        let p = std::path::PathBuf::from(cmd);
        return if p.is_file() { Some(p) } else { None };
    }
    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.BAT;.CMD;.COM".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{}{}", cmd, ext));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve which editor to launch for `/config`. Honours `$EDITOR` / `$VISUAL`
/// first, then falls back to a platform candidate list — picking the first one
/// that actually exists on PATH so we don't try to spawn a binary the user
/// doesn't have (e.g. `vim` on minimal macOS/Linux installs).
fn resolve_editor() -> Result<String, String> {
    for var in ["EDITOR", "VISUAL"] {
        if let Ok(v) = std::env::var(var) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["notepad", "code"]
    } else if cfg!(target_os = "macos") {
        &["vim", "vi", "nano", "pico", "open"]
    } else {
        &["vim", "vi", "nano", "pico"]
    };
    for c in candidates {
        if which(c).is_some() {
            return Ok(c.to_string());
        }
    }
    Err(format!(
        "Could not find a default editor. Tried: {}. \
         Set the EDITOR env var to a working editor (e.g. `code --wait`, `notepad`, `vim`).",
        candidates.join(", ")
    ))
}

use atomcode_core::agent::AgentHandle;
use atomcode_core::config::Config;
use atomcode_core::config::provider::ProviderConfig;
use atomcode_core::tool::ToolContext;

use app::App;
use event::{AppEvent, EventLoop};

/// OAuth login configuration
mod oauth {
    pub const PLATFORM_BROKER_URL: &str = "https://acs.atomgit.com";
    pub const PLATFORM_LOGIN_URL: &str = "https://acs.atomgit.com/auth/login";
    pub const PLATFORM_CHECK_URL: &str = "https://acs.atomgit.com/auth/check";
    pub const PLATFORM_TOKEN_URL: &str = "https://acs.atomgit.com/auth/token";
}

#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub access_token: String,
    pub user: UserInfo,
}

#[derive(Debug, Clone)]
pub struct UserInfo {
    pub id: String,
    pub username: String,
    pub name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct TokenResponse {
    #[serde(alias = "access_token", alias = "accessToken")]
    access_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Build an OAuth ProviderConfig for AtomGit.
/// api_key is NOT included — it is loaded from auth.toml at runtime.
/// The provider is non-ephemeral so it gets persisted to config.toml.
fn build_oauth_provider() -> ProviderConfig {
    ProviderConfig {
        provider_type: "openai".to_string(),
        api_key: None, // injected from auth.toml at runtime
        model: "MiniMax-M2.7".to_string(),
        base_url: Some("https://api-ai.gitcode.com/v1".to_string()),
        system_prompt: None,
        user_agent: None,
        context_window: 64000,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        ephemeral: false,
    }
}

/// Build an in-memory ProviderConfig from broker-minted credentials.
/// Broker supplies model + api_base per user, so we don't hardcode them.
fn build_wecom_provider(creds: &wecom_login::BrokerCreds) -> ProviderConfig {
    ProviderConfig {
        provider_type: "openai".to_string(),
        api_key: Some(creds.api_key.clone()),
        model: creds.model.clone(),
        base_url: Some(creds.api_base.clone()),
        system_prompt: None,
        user_agent: None,
        context_window: 64000,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        ephemeral: true,
    }
}

/// Parse query parameters from a callback URL (handles both full URL and path-only).
fn parse_callback_params(url: &str) -> anyhow::Result<std::collections::HashMap<String, String>> {
    // Find the query string — works for both "/callback?code=..." and "http://...?code=..."
    let query_start = url.find('?').ok_or_else(|| anyhow::anyhow!("No query parameters in URL"))?;
    let query = &url[query_start + 1..];

    Ok(query.split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().unwrap_or_default();
            // Basic URL decode
            let decoded = value.replace('+', " ")
                .replace("%3A", ":").replace("%2F", "/")
                .replace("%3D", "=").replace("%26", "&");
            Some((key.to_string(), decoded))
        })
        .collect())
}

/// Run OAuth login flow (blocking)
fn run_oauth_login() -> anyhow::Result<AuthInfo> {
    let client = reqwest::blocking::Client::new();

    // Step 1: Call Platform /auth/login to get authorization URL
    #[derive(serde::Deserialize)]
    struct PlatformLoginResponse {
        login_url: String,
        state: String,
    }

    let login_resp: PlatformLoginResponse = client
        .post(oauth::PLATFORM_LOGIN_URL)
        .json(&serde_json::json!({ "provider": "atomgit" }))
        .send()
        .context("Failed to call /auth/login")?
        .json()
        .context("Failed to parse /auth/login response")?;

    println!("  Opening browser for authorization...");

    // Try to open browser
    let browser_opened = {
        #[cfg(target_os = "macos")]
        { std::process::Command::new("open").arg(&login_resp.login_url).spawn().is_ok() }
        #[cfg(target_os = "linux")]
        { std::process::Command::new("xdg-open").arg(&login_resp.login_url).spawn().is_ok() }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            std::process::Command::new("cmd")
                .raw_arg(format!("/C start \"\" \"{}\"", login_resp.login_url))
                .spawn().is_ok()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        { false }
    };

    if !browser_opened {
        println!("  Browser did not open. Visit:\n");
        println!("  {}\n", login_resp.login_url);
    }

    println!("  Waiting for authorization...\n");

    // Step 2: Poll /auth/check until login is complete
    #[derive(serde::Deserialize)]
    struct PlatformCheckResponse {
        valid: bool,
    }

    let checked_state = loop {
        std::thread::sleep(std::time::Duration::from_secs(2));

        let resp = client
            .get(oauth::PLATFORM_CHECK_URL)
            .query(&[("state", &login_resp.state)])
            .send();

        match resp {
            Ok(r) if r.status().is_success() => {
                if let Ok(check) = r.json::<PlatformCheckResponse>() {
                    if check.valid {
                        break login_resp.state;
                    }
                }
            }
            _ => {}
        }
        println!("  Waiting for browser authorization...");
    };

    println!("  Authorization received, fetching token...\n");

    // Step 3: Get token from Platform
    #[derive(serde::Deserialize)]
    struct PlatformTokenResponse {
        access_token: String,
        token_type: String,
        expires_in: Option<i64>,
        refresh_token: Option<String>,
        user: PlatformUser,
    }

    #[derive(serde::Deserialize)]
    struct PlatformUser {
        id: String,
        username: String,
        name: Option<String>,
    }

    let token_resp: PlatformTokenResponse = client
        .get(oauth::PLATFORM_TOKEN_URL)
        .query(&[("state", &checked_state)])
        .send()
        .context("Failed to call /auth/token")?
        .json()
        .context("Failed to parse /auth/token response")?;

    let access_token = token_resp.access_token;

    let auth_info = AuthInfo {
        access_token: access_token.clone(),
        user: UserInfo {
            id: token_resp.user.id,
            username: token_resp.user.username,
            name: token_resp.user.name,
        },
    };

    // Save auth.toml (credentials only, not written to config.toml)
    let auth_path = atomcode_core::config::Config::config_dir().join("auth.toml");
    if let Some(parent) = auth_path.parent() {
        let _ = std::fs::create_dir_all(parent);
        // Set directory permissions to 0o700 (owner only) on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut auth_content = format!("access_token = \"{}\"\ncreated_at = {}\n",
        access_token, now);
    if let Some(ref rt) = token_resp.refresh_token {
        auth_content.push_str(&format!("refresh_token = \"{}\"\n", rt));
    }
    if let Some(exp) = token_resp.expires_in {
        auth_content.push_str(&format!("expires_in = {}\n", exp));
    }
    auth_content.push_str(&format!(
        "\n[user]\nid = \"{}\"\nusername = \"{}\"\n",
        auth_info.user.id, auth_info.user.username
    ));
    if let Some(ref name) = auth_info.user.name {
        auth_content.push_str(&format!("name = \"{}\"\n", name));
    }

    match atomcode_core::auth::write_auth_file_secure(&auth_path, &auth_content) {
        Ok(_) => {
            // Set file permissions to 0o600 (owner read/write only) on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600));
            }
            println!("  Auth saved to: {}\n", auth_path.display());
        },
        Err(e) => println!("  Warning: failed to save auth.toml: {}\n", e),
    }

    Ok(auth_info)
}

pub async fn run(
    config: Config,
    model_name: String,
    agent_handle: AgentHandle,
    tool_context: ToolContext,
    working_dir: std::path::PathBuf,
    session_to_continue: Option<atomcode_core::session::Session>,
) -> Result<()> {
    // Initialize theme (detect terminal background color)
    ui::theme::Theme::init();
    
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Clear main screen + scrollback BEFORE entering alternate screen so Terminal.app
    // has nothing to scroll back to (prevents scrollback bleed-through on mouse wheel).
    clear_scrollback(&mut stdout)?;
    execute!(
        stdout,
        EnterAlternateScreen,
        SetTitle("AtomCode"),
        Clear(ClearType::All),
    )?;
    // Bracketed paste: reliable on macOS/Linux only. On Windows conhost it can
    // mis-interpret Enter/typing as paste events, which caused "second Enter
    // doesn't submit" (the key arrived as AppEvent::Paste containing "\n" and
    // got inserted into the input buffer instead of triggering send_message).
    // Ctrl+V on Windows reads the clipboard directly (see app.rs::read_clipboard).
    #[cfg(not(target_os = "windows"))]
    execute!(stdout, EnableBracketedPaste)?;

    // Enable Kitty keyboard protocol so the terminal can distinguish
    // Shift+Enter from plain Enter (needed for multi-line input).
    // Silently ignored by terminals that don't support it.
    let _ = execute!(stdout, PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
    ));

    // Wrap stdout in BufWriter: crossterm emits many small writes per frame
    // (cursor moves, style changes, cells). On Windows each write is a
    // WriteConsole syscall, which is orders of magnitude slower than Unix tty
    // writes — unbuffered stdout caused visible per-keystroke lag. ratatui
    // flushes the backend at the end of every draw, so latency is unaffected.
    // 512KB capacity: full-repaint mode (underline_color toggle) writes every
    // cell on every frame — a 200×50 terminal produces ~200KB of ANSI output.
    // The buffer must hold the entire frame to avoid mid-frame flushes that
    // expose intermediate cursor positions as visible flicker.
    let backend = CrosstermBackend::new(std::io::BufWriter::with_capacity(512 * 1024, stdout));
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut app = App::new(model_name, config, agent_handle, tool_context, working_dir, session_to_continue);
    let mut event_loop = EventLoop::new();
    let event_tx = event_loop.sender();
    event_loop.start();

    loop {
        // Force a full redraw when requested (e.g. after bash tool completion)
        // to overwrite any artifacts left by child processes that wrote
        // directly to the terminal via /dev/tty.
        //
        // terminal.clear() only resets the BACK buffer.  The front buffer
        // retains stale cell state, so the diff engine skips cells whose new
        // content happens to match the old front — leaving artifacts visible.
        //
        // Fix: clear() + draw-nothing.  The empty draw diffs a blank back
        // buffer against the stale front, flushing blanks everywhere the
        // front had content.  After the swap the front is blank, so the REAL
        // draw (immediately after) sees every content cell as "changed" and
        // writes it — guaranteed full repaint.
        // Full-repaint trick: toggle an invisible style attribute (underline_color)
        // on every cell after rendering, alternating each frame.  ratatui's diff
        // compares Cell fields including underline_color, so every cell appears
        // "changed" and is written to the terminal — guaranteeing no stale
        // artifacts from external processes (SSH master, git hooks) survive.
        // underline_color is invisible without the UNDERLINED modifier, so the
        // toggle has zero visual effect.  Single draw() = single flush = no flicker.
        let repaint_parity = app.tick_count;
        terminal.draw(|frame| {
            ui::render(frame, &mut app);
            // Toggle underline_color across the entire buffer
            let uc = if repaint_parity % 2 == 0 {
                ratatui::style::Color::Reset
            } else {
                ratatui::style::Color::Indexed(0)
            };
            let buf = frame.buffer_mut();
            let area = buf.area;
            // Skip row 0 (status bar) and last 3 rows (input box) — they
            // redraw fully on every frame anyway, and the SGR toggle causes
            // visible flicker on the ◉ logo character in some terminals.
            let y_start = area.y + 1;
            let y_end = area.y + area.height.saturating_sub(3);
            for y in y_start..y_end {
                for x in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(x, y)) {
                        cell.set_style(cell.style().underline_color(uc));
                    }
                }
            }
        })?;

        if let Some(file_path) = app.pending_editor.take() {
            // LeaveAlternateScreen before disable_raw_mode — on Windows
            // crossterm clears saved console mode on disable_raw_mode().
            let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
            let _ = execute!(
                terminal.backend_mut(),
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            #[cfg(not(target_os = "windows"))]
            { let _ = execute!(terminal.backend_mut(), DisableBracketedPaste); }
            disable_raw_mode()?;
            terminal.show_cursor()?;

            event_loop.stop();
            let _ = std::io::stdout().flush();
            flush_stdin();

            let mut spawn_error: Option<String> = None;
            match resolve_editor() {
                Err(msg) => {
                    spawn_error = Some(msg);
                }
                Ok(editor) => {
                    let status = std::process::Command::new(&editor)
                        .arg(&file_path)
                        .stdin(std::process::Stdio::inherit())
                        .stdout(std::process::Stdio::inherit())
                        .stderr(std::process::Stdio::inherit())
                        .status();

                    match status {
                        Ok(exit_status) if exit_status.success() => {
                            let config_path = Config::default_path();
                            if std::path::Path::new(&file_path) == config_path {
                                if let Ok(mut new_config) = Config::load(&config_path) {
                                    // Preserve ephemeral providers (e.g. WeCom SSO) —
                                    // they only exist in memory, not on disk.
                                    for (name, provider) in &app.config.providers {
                                        if provider.ephemeral {
                                            new_config.providers.insert(name.clone(), provider.clone());
                                        }
                                    }
                                    // If the previous default was ephemeral and still exists, keep it.
                                    if app.config.providers.get(&app.config.default_provider)
                                        .map(|p| p.ephemeral).unwrap_or(false)
                                        && new_config.providers.contains_key(&app.config.default_provider)
                                    {
                                        new_config.default_provider = app.config.default_provider.clone();
                                    }
                                    app.config = new_config;
                                    app.rebuild_provider();
                                    app.sync_config_to_agent();
                                    let default_name = app.config.default_provider.clone();
                                    if let Ok(provider) = app.config.active_provider(None) {
                                        app.model_name = format!("{} / {}", default_name, provider.model);
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            spawn_error = Some(format!(
                                "Failed to launch editor `{}`: {}\nSet the EDITOR env var to a working editor and retry.",
                                editor, e
                            ));
                        }
                    }
                }
            }

            flush_stdin();
            enable_raw_mode()?;
            clear_scrollback(terminal.backend_mut())?;
            execute!(
                terminal.backend_mut(),
                EnterAlternateScreen,
                SetTitle("AtomCode"),
                Clear(ClearType::All),
                // Mouse mode is re-enabled by EventLoop::start (scroll-only). See startup comment.
            )?;
            #[cfg(not(target_os = "windows"))]
            execute!(terminal.backend_mut(), EnableBracketedPaste)?;
            let _ = execute!(terminal.backend_mut(), PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            ));
            terminal.clear()?;
            event_loop.start();
            if let Some(msg) = spawn_error {
                app.conversation.push_delta(&msg);
                app.conversation.finalize_stream();
            }
            continue;
        }

        // Handle pending OAuth login
        if app.pending_login {
            app.pending_login = false;


            // Stop the input-reading thread before releasing the terminal.
            // On Windows, if the background thread keeps calling
            // crossterm::event::read() while we toggle raw mode / leave the
            // alt screen / spawn the browser via `cmd /C start`, the shared
            // console state goes inconsistent and the process exits.
            event_loop.stop();

            // On Windows, crossterm clears the saved console mode when
            // disable_raw_mode() is called, so LeaveAlternateScreen must
            // happen BEFORE disable_raw_mode() — otherwise it fails with
            // "Initial console modes not set".
            let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
            let _ = execute!(
                terminal.backend_mut(),
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            #[cfg(not(target_os = "windows"))]
            { let _ = execute!(terminal.backend_mut(), DisableBracketedPaste); }
            disable_raw_mode()?;
            terminal.show_cursor()?;
            let _ = std::io::stdout().flush();

            println!("\n  AtomCode AtomGit Login");
            println!("  =========================\n");

            // reqwest::blocking owns a tokio runtime; run on a dedicated
            // blocking thread so its Drop doesn't panic from async context.
// (Same pattern as /login-with-sso below.)
            let login_result = tokio::task::spawn_blocking(run_oauth_login)
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("login task panicked: {}", e)));

            match login_result {
                Ok(auth) => {
                    println!("\n  Login successful! Logged in as: {}", auth.user.username);
                    let oauth_name = app.pending_oauth_name.take().unwrap_or_else(|| "AtomGit".to_string());
                    // Save provider config to config.toml (without api_key).
                    // api_key comes from auth.toml at runtime.
                    let oauth_provider = build_oauth_provider();
                    app.config.providers.insert(oauth_name.clone(), oauth_provider);
                    app.config.default_provider = oauth_name.clone();
                    let _ = app.config.save(&Config::default_path());
                    // api_key stays None in config — create_provider() reads
                    // it from auth.toml automatically.
                    app.rebuild_provider();
                    app.sync_config_to_agent();
                    let model_display = app.provider.model_name();
                    app.conversation.add_user_message("/login");
                    app.conversation.push_delta(&format!(
                        "Login successful! Logged in as: **{}** (ID: {})\n\nProvider `{}` saved to config.\nModel: `{}`",
                        auth.user.username, auth.user.id, oauth_name, model_display
                    ));
                    app.conversation.finalize_stream();
                }
                Err(e) => {
                    println!("\n  Login failed: {}", e);
                    app.conversation.add_user_message("/login");
                    app.conversation.push_delta(&format!("Login failed: {}", e));
                    app.conversation.finalize_stream();
                }
            }

            enable_raw_mode()?;
            clear_scrollback(terminal.backend_mut())?;
            execute!(
                terminal.backend_mut(),
                EnterAlternateScreen,
                SetTitle("AtomCode"),
                Clear(ClearType::All),
                // Mouse mode is re-enabled by EventLoop::start (scroll-only). See startup comment.
            )?;
            #[cfg(not(target_os = "windows"))]
            execute!(terminal.backend_mut(), EnableBracketedPaste)?;
            let _ = execute!(terminal.backend_mut(), PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            ));
            terminal.clear()?;
            event_loop.start();
            continue;
        }

        // Handle pending WeCom login
        if app.pending_wecom_login {
            app.pending_wecom_login = false;

            // See /login above — stop input thread before terminal surgery.
            event_loop.stop();

            // LeaveAlternateScreen before disable_raw_mode — see /login comment.
            let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
            let _ = execute!(
                terminal.backend_mut(),
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            #[cfg(not(target_os = "windows"))]
            { let _ = execute!(terminal.backend_mut(), DisableBracketedPaste); }
            disable_raw_mode()?;
            terminal.show_cursor()?;
            let _ = std::io::stdout().flush();

            // reqwest::blocking owns a tokio runtime; run on a dedicated
            // blocking thread so its Drop doesn't panic from async context.
            let login_result = tokio::task::spawn_blocking(wecom_login::login)
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("login task panicked: {}", e)));

            match login_result {
                Ok((identity, Some(creds))) => {
                    let display_name = identity.name.clone().unwrap_or_else(|| identity.userid.clone());
                    println!("\n  Login successful! WeCom user: {} ({})", display_name, identity.userid);
                    let provider_name = wecom_login::INTERNAL_PROVIDER_NAME.to_string();
                    let provider = build_wecom_provider(&creds);
                    app.config.providers.insert(provider_name.clone(), provider);
                    app.config.default_provider = provider_name.clone();
                    app.rebuild_provider();
                    app.sync_config_to_agent();
                    let model_display = app.provider.model_name();
app.conversation.add_user_message("/login-with-sso");
                    app.conversation.push_delta(&format!(
                        "Login successful! SSO user: **{}** ({}).\n\nProvider `{}` active (in-memory, not saved to config).\nModel: `{}`",
                        display_name, identity.userid, provider_name, model_display
                    ));
                    app.conversation.finalize_stream();
                }
                Ok((_identity, None)) => {
                    println!("\n  Broker returned no credentials; provider not registered.");
app.conversation.add_user_message("/login-with-sso");
                    app.conversation.push_delta("Login completed, but broker returned no credentials. Contact the GitCode platform team.");
                    app.conversation.finalize_stream();
                }
                Err(e) => {
                    println!("\n  WeCom login failed: {}", e);
app.conversation.add_user_message("/login-with-sso");
                    app.conversation.push_delta(&format!("Login failed: {}", e));
                    app.conversation.finalize_stream();
                }
            }

            enable_raw_mode()?;
            clear_scrollback(terminal.backend_mut())?;
            execute!(
                terminal.backend_mut(),
                EnterAlternateScreen,
                SetTitle("AtomCode"),
                Clear(ClearType::All),
            )?;
            #[cfg(not(target_os = "windows"))]
            execute!(terminal.backend_mut(), EnableBracketedPaste)?;
            let _ = execute!(terminal.backend_mut(), PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            ));
            terminal.clear()?;
            event_loop.start();
            continue;
        }

        // Poll both TUI keyboard/tick events and AgentLoop events concurrently.
        // We use two separate futures so neither blocks the other.
        enum Wake {
            Tui(AppEvent),
            Agent(atomcode_core::agent::AgentEvent),
        }

        let tui_fut = event_loop.next();
        let agent_fut = app.agent_handle.event_rx.recv();

        let wake = tokio::select! {
            Some(e) = tui_fut => Wake::Tui(e),
            Some(e) = agent_fut => Wake::Agent(e),
        };

        match wake {
            Wake::Tui(event) => {
                app.handle_event(event, &event_tx);
                // Drain any remaining buffered TUI events without blocking.
                loop {
                    match event_loop.try_next() {
                        Some(e) => app.handle_event(e, &event_tx),
                        None => break,
                    }
                }
            }
            Wake::Agent(agent_event) => {
                app.handle_agent_event(agent_event);
                // Drain any additional agent events that are already queued.
                loop {
                    match app.agent_handle.event_rx.try_recv() {
                        Ok(e) => app.handle_agent_event(e),
                        Err(_) => break,
                    }
                }
                // Redraw immediately after processing agent events.
                // Without this, the terminal won't update until the next user
                // input event, causing the UI to appear "frozen".
                let repaint_parity = app.tick_count;
                terminal.draw(|frame| {
                    ui::render(frame, &mut app);
                    let uc = if repaint_parity % 2 == 0 {
                        ratatui::style::Color::Reset
                    } else {
                        ratatui::style::Color::Indexed(0)
                    };
                    let buf = frame.buffer_mut();
                    let area = buf.area;
                    let y_start = area.y + 1;
                    let y_end = area.y + area.height.saturating_sub(3);
                    for y in y_start..y_end {
                        for x in area.x..area.x + area.width {
                            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(x, y)) {
                                cell.set_style(cell.style().underline_color(uc));
                            }
                        }
                    }
                })?;
            }
        }

        if app.mode.is_exiting() {
            // Clean up empty session - don't save sessions with no messages
            if app.current_session.messages.is_empty() {
                // Delete the session file if it exists
                let _ = app.session_manager.delete(&app.current_session.id);
            }
            break;
        }
    }

    disable_raw_mode()?;
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
    )?;
    #[cfg(not(target_os = "windows"))]
    execute!(terminal.backend_mut(), DisableBracketedPaste)?;
    terminal.show_cursor()?;

    Ok(())
}
