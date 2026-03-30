pub mod app;
pub mod command;
pub mod event;
pub mod file_attach;
pub mod project_context;
pub mod provider_manager;
pub mod turn_log;
pub mod ui;

use std::io::Write;

use anyhow::Result;
use crossterm::{
    execute,
    event::{EnableBracketedPaste, DisableBracketedPaste},
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        SetTitle, Clear, ClearType,
    },
};
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
    // On non-Unix systems, we can't flush stdin directly
    // Just drain any available input
    use std::io::Read;
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];
    while let Ok(n) = stdin.read(&mut buf) {
        if n == 0 { break; }
    }
}

use atomcode_core::agent::AgentHandle;
use atomcode_core::config::Config;
use atomcode_core::config::provider::ProviderConfig;
use atomcode_core::tool::ToolContext;

use app::App;
use event::{AppEvent, EventLoop};

/// OAuth login configuration
mod oauth {
    pub const CLIENT_ID: &str = "b9956e5327e544578128af8979ba3ccb";
    pub const CLIENT_SECRET: &str = "756ef00061884c7aa1ac64bd4eae3be7";
    pub const REDIRECT_PORT: u16 = 8765;
    pub const REDIRECT_URI: &str = "http://127.0.0.1:8765/callback";
    pub const AUTHORIZE_URL: &str = "https://atomgit.com/oauth/authorize";
    pub const TOKEN_URL: &str = "https://atomgit.com/oauth/token";
    pub const USER_URL: &str = "https://atomgit.com/api/v5/user";
    pub const SCOPES: &str = "user_info projects";
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

#[derive(Debug, serde::Deserialize)]
struct UserResponse {
    #[serde(alias = "id", alias = "user_id")]
    id: Option<String>,
    #[serde(alias = "login", alias = "username")]
    login: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Add AtomGit provider to config and set as default
fn add_atomgit_provider(access_token: &str, provider_name: &str) -> anyhow::Result<()> {
    let config_path = Config::default_path();
    
    // Load existing config or create new one
    let mut config = if config_path.exists() {
        Config::load(&config_path).unwrap_or_else(|_| Config {
            default_provider: "AtomGit".to_string(),
            default_workdir: None,
            providers: std::collections::HashMap::new(),
        })
    } else {
        Config {
            default_provider: "AtomGit".to_string(),
            default_workdir: None,
            providers: std::collections::HashMap::new(),
        }
    };
    
    // Add or update AtomGit provider
    let atomgit_provider = ProviderConfig {
        provider_type: "openai".to_string(),
        api_key: Some(access_token.to_string()),
        model: "Qwen/Qwen3.5-35B-A3B".to_string(),
        base_url: Some("https://api-ai.gitcode.com/v1".to_string()),
        system_prompt: None,
        user_agent: None,
        context_window: 32000,
    };
    
    config.providers.insert(provider_name.to_string(), atomgit_provider);
    config.default_provider = provider_name.to_string();

    // Save config
    config.save(&config_path)?;

    println!("  {} provider added to config: {}\n", provider_name, config_path.display());
    
    Ok(())
}

/// Run OAuth login flow (blocking)
fn run_oauth_login() -> anyhow::Result<AuthInfo> {
    use std::io::{BufRead, Write};
    use std::net::TcpListener;
    use std::collections::HashMap;

    // Generate state for CSRF protection
    let state = format!("atomcode_{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos());

    // Build authorization URL
    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&state={}&scope={}",
        oauth::AUTHORIZE_URL,
        url::form_urlencoded::byte_serialize(oauth::CLIENT_ID.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(oauth::REDIRECT_URI.as_bytes()).collect::<String>(),
        state,
        url::form_urlencoded::byte_serialize(oauth::SCOPES.as_bytes()).collect::<String>(),
    );

    println!("  Opening browser for authorization...");
    println!("  If browser doesn't open, visit:\n");
    println!("  {}\n", auth_url);

    // Open browser
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&auth_url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&auth_url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", &auth_url]).spawn();

    println!("  Waiting for authorization callback on port {}...\n", oauth::REDIRECT_PORT);

    // Start local server
    let listener = TcpListener::bind(("127.0.0.1", oauth::REDIRECT_PORT))?;
    let (mut stream, _) = listener.accept()?;

    // Read HTTP request
    let mut reader = std::io::BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let url: String = request_line.split_whitespace().nth(1)
        .ok_or_else(|| anyhow::anyhow!("Invalid HTTP request"))?
        .to_string();

    let query_start = url.find('?').ok_or_else(|| anyhow::anyhow!("No query in callback"))?;
    let query = &url[query_start + 1..];

    let params: HashMap<String, String> = query.split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().map(|v| {
                v.replace('+', " ")
                    .chars()
                    .fold(String::new(), |mut s, c| {
                        if c == '%' {
                            // Simple URL decode
                            s
                        } else {
                            s.push(c);
                            s
                        }
                    })
            }).unwrap_or_default();
            Some((key.to_string(), value))
        })
        .collect();

    if let Some(error) = params.get("error") {
        anyhow::bail!("OAuth error: {}", error);
    }

    let code = params.get("code").ok_or_else(|| anyhow::anyhow!("No code in callback"))?;
    let returned_state = params.get("state").cloned().unwrap_or_default();

    if returned_state != state {
        anyhow::bail!("OAuth state mismatch");
    }

    // Send success response
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <html><head><style>body{font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#1a1a2e;color:#eee}\
        .container{text-align:center}h1{color:#7c3aed}.success{color:#22c55e;font-size:4rem}</style></head>\
        <body><div class=\"container\"><div class=\"success\">✓</div><h1>Authorization Successful</h1>\
        <p>You can close this window and return to AtomCode.</p></div></body></html>";
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    println!("  Authorization received, exchanging token...\n");

    // Exchange code for token
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(oauth::TOKEN_URL)
        .form(&[
            ("client_id", oauth::CLIENT_ID),
            ("client_secret", oauth::CLIENT_SECRET),
            ("code", code),
            ("redirect_uri", oauth::REDIRECT_URI),
            ("grant_type", "authorization_code"),
        ])
        .send()?;

    let response_text = response.text()?;
    println!("  Token response: {}\n", response_text);
    
    let token_resp: TokenResponse = serde_json::from_str(&response_text)
        .map_err(|e| anyhow::anyhow!("Failed to parse token response: {} - body: {}", e, response_text))?;
    
    let access_token = token_resp.access_token
        .ok_or_else(|| anyhow::anyhow!("No access_token in response"))?;

    // Get user info
    let user_response = client
        .get(oauth::USER_URL)
        .bearer_auth(&access_token)
        .send()?;
    
    let user_text = user_response.text()?;
    println!("  User response: {}\n", user_text);
    
    let user_resp: UserResponse = serde_json::from_str(&user_text)
        .map_err(|e| anyhow::anyhow!("Failed to parse user response: {} - body: {}", e, user_text))?;

    let auth_info = AuthInfo {
        access_token: access_token.clone(),
        user: UserInfo {
            id: user_resp.id.unwrap_or_default(),
            username: user_resp.login.clone().unwrap_or_else(|| "unknown".to_string()),
            name: user_resp.name,
        },
    };

    // Save to file
    let auth_path = atomcode_core::config::Config::config_dir().join("auth.toml");

    if let Some(parent) = auth_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let auth_content = format!(
        "access_token = \"{}\"\n[user]\nid = {}\nusername = \"{}\"\n",
        auth_info.access_token,
        auth_info.user.id,
        auth_info.user.username
    );
    std::fs::write(&auth_path, auth_content)?;

    println!("  Auth saved to: {}\n", auth_path.display());

    Ok(auth_info)
}

pub async fn run(
    config: Config,
    model_name: String,
    agent_handle: AgentHandle,
    tool_context: ToolContext,
    working_dir: std::path::PathBuf,
) -> Result<()> {
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
        EnableBracketedPaste,
    )?;
    // Bracketed paste enabled. Scroll events handled by terminal's alternate scroll mode.

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(model_name, config, agent_handle, tool_context, working_dir);
    let mut event_loop = EventLoop::new();
    let event_tx = event_loop.sender();
    event_loop.start();

    loop {
        terminal.draw(|frame| ui::render(frame, &mut app))?;

        if let Some(file_path) = app.pending_editor.take() {
            // First disable raw mode to release terminal control completely
            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                DisableBracketedPaste,
                LeaveAlternateScreen
            )?;
            terminal.show_cursor()?;

            event_loop.stop();
            let _ = std::io::stdout().flush();
            flush_stdin();

            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
            let status = std::process::Command::new(&editor)
                .arg(&file_path)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();

            if let Ok(exit_status) = status {
                if exit_status.success() {
                    let config_path = Config::default_path();
                    if std::path::Path::new(&file_path) == config_path {
                        if let Ok(new_config) = Config::load(&config_path) {
                            app.config = new_config;
                            let default_name = app.config.default_provider.clone();
                            if let Ok(provider) = app.config.active_provider(None) {
                                app.model_name = format!("{} / {}", default_name, provider.model);
                            }
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
                EnableBracketedPaste,
            )?;
            terminal.clear()?;
            event_loop.start();
            continue;
        }

        // Handle pending OAuth login
        if app.pending_login {
            app.pending_login = false;

            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                DisableBracketedPaste,
                LeaveAlternateScreen
            )?;
            terminal.show_cursor()?;

            println!("\n  AtomCode AtomGit Login");
            println!("  =========================\n");

            match run_oauth_login() {
                Ok(auth) => {
                    println!("\n  Login successful! Logged in as: {}", auth.user.username);
                    let oauth_name = app.pending_oauth_name.take().unwrap_or_else(|| "AtomGit".to_string());
                    if let Err(e) = add_atomgit_provider(&auth.access_token, &oauth_name) {
                        println!("  Warning: Failed to add provider: {}", e);
                    }
                    let config_path = Config::default_path();
                    if let Ok(new_config) = Config::load(&config_path) {
                        app.config = new_config;
                        app.rebuild_provider();
                    }
                    let model_display = app.provider.model_name();
                    app.conversation.add_user_message("/login");
                    app.conversation.push_delta(&format!(
                        "Login successful! Logged in as: **{}** (ID: {})\n\nProvider `{}` added and set as default.\nModel: `{}`",
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
                EnableBracketedPaste,
            )?;
            terminal.clear()?;
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
            }
        }

        if app.mode.is_exiting() {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
    )?;
    terminal.show_cursor()?;

    Ok(())
}
