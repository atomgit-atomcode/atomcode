//! Start and stop the agent's HTTP front end inside the app's own process.
//!
//! # Why a listener we bind ourselves
//!
//! [`ServerOpts::prebound_listener`] exists for the TUI's in-process `/webui`,
//! and it is what makes a phone build possible: binding port `0` gives us the
//! real port *before* the server boots, so Kotlin can load the URL immediately
//! and the page's own loading state covers bootstrap. Scanning for a free fixed
//! port would race with whatever else the phone is running.
//!
//! # Why `ATOMCODE_HOME` is set here
//!
//! The product tree is the *host's* decision (see the `ProductDirs` note in
//! `AGENTS.md`): the libraries read `Config::config_dir()`, and that resolves
//! `ATOMCODE_HOME` verbatim when it is set. On Android there is no `~`, so an
//! unset variable would land the tree in the process's fallback cwd — which is
//! `/` for an app process and is not writable. The app passes its private files
//! dir in, and this is the one place that turns it into the product tree.
//!
//! # Lifecycle
//!
//! One [`AgentServer`] per process. `start` is idempotent at the JNI layer (a
//! second call returns the running URL). `stop` asks the server to close its
//! connections through [`ServerOpts::owner_shutdown`] and then drops the
//! runtime; it never touches the session log, which is already durable on disk.

use std::path::PathBuf;

use atomcode_daemon::auth_token::WebuiTokenStore;
use atomcode_daemon::{run_server, ServerOpts};

/// 会话目录覆盖：外部 daemon 与 App 指同一处，就能读到同一份历史。
const SESSIONS_ENV: &str = "ATOMCODE_SESSIONS_DIR";

/// Presence of this file in the product tree keeps sessions app-private.
///
/// The choice has to be read here (Rust) while the switch lives in the UI
/// (Kotlin), so the two are joined by a file rather than by a new JNI parameter.
/// Shared storage is readable by every app with storage access, and a session log
/// is the whole conversation — this is how a person keeps it out of there.
const SESSIONS_PRIVATE_MARKER: &str = "sessions-private";
use atomcode_telemetry::{CliOverride, SessionMode};

/// Loopback only. The phone's page is loaded by the app's own WebView; nothing
/// off-device should reach this port, and the token guards what does.
const LOOPBACK: &str = "127.0.0.1";

/// What Android hands the host: where the product tree lives, where the agent
/// should start working, and where the bundled tools are installed.
#[derive(Debug, Clone)]
pub struct HostOptions {
    /// App-private directory that replaces `~/.atomcode`
    /// (`context.filesDir` from Kotlin).
    pub data_dir: PathBuf,
    /// Initial working directory for the agent — the project the person opened.
    /// Until a real directory picker lands, this can be the same app-private
    /// tree (Kotlin passes `filesDir/projects/default`).
    pub workspace: PathBuf,
    /// The APK's `nativeLibraryDir` (`applicationInfo.nativeLibraryDir`). The
    /// bundled `git` lives there because Android 10+ only allows executing
    /// binaries from that directory. `None` on a desktop build, where the
    /// platform `git` is used.
    pub native_lib_dir: Option<PathBuf>,
    /// `nativeLibraryDir`s of the provider APKs whose tools this host links in.
    ///
    /// Those APKs carry tools this one cannot: `make` and a C compiler, or `zig` —
    /// all of them *process-model* programs that spawn children, which the plugin
    /// system cannot host, because `execve` on our own data directory is forbidden
    /// (measured: exit 126 from the real app process, `docs/android-port.md` §P5.7).
    /// Android does allow executing *another* package's library directory, so a
    /// provider is only a file holder (no service, no IPC). Empty when none is
    /// installed; more than one is normal (`tools` for make/tcc, `tools.zig` for zig).
    pub helper_lib_dirs: Vec<PathBuf>,
}

impl HostOptions {
    pub fn new(data_dir: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            workspace: workspace.into(),
            native_lib_dir: None,
            helper_lib_dirs: Vec::new(),
        }
    }

    /// Point the host at the app's `nativeLibraryDir`, where the bundled git
    /// was installed (see `scripts/build-android-git.sh`).
    pub fn with_native_lib_dir(mut self, native_lib_dir: impl Into<PathBuf>) -> Self {
        self.native_lib_dir = Some(native_lib_dir.into());
        self
    }

    /// Point the host at provider library directories, whose tools are linked
    /// into `<data>/bin` (see [`crate::shims::HELPER_TOOLS`] and
    /// [`crate::shims::PROVIDER_TOOLS`]). Order matters only for reporting.
    pub fn with_helper_lib_dirs(
        mut self,
        helper_lib_dirs: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        self.helper_lib_dirs = helper_lib_dirs.into_iter().collect();
        self
    }
}

/// A running front end. Dropping it stops the server.
pub struct AgentServer {
    port: u16,
    token: String,
    shutdown: tokio::sync::watch::Sender<bool>,
    runtime: Option<tokio::runtime::Runtime>,
}

/// Whether this build can sign requests for the AtomGit gateway.
///
/// The gateway answers `ATOMCODE_SIG_MISSING` (403) to any request that carries
/// only an OAuth bearer, so a build without the signing overlay cannot talk to
/// it at all. Reporting that at start-up turns "the first turn fails with a
/// provider error" into "this build has no gateway access" — a fact the person
/// can act on instead of a puzzle.
pub fn gateway_signer_available() -> bool {
    atomcode_capabilities::provider::signer_available()
}

impl AgentServer {
    /// Bind loopback, start the daemon on it, and return once the server task is
    /// spawned. The server may still be bootstrapping (config load, MCP init);
    /// the webui shows its own loading state until it answers.
    pub fn start(options: HostOptions) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&options.data_dir)?;
        std::fs::create_dir_all(&options.workspace)?;
        // Verbatim product tree — see the module docs. Set before anything reads
        // config, which is why this happens before the runtime exists: a
        // `CodingRuntime` built later must see the same tree.
        std::env::set_var(atomcode_config::distribution::HOME_ENV, &options.data_dir);

        // Examples, before the runtime scans `<product>/skills`. The desktop has a setup
        // wizard that seeds them; a phone install has no such moment, so `/skills` used to
        // be empty and nothing showed what a skill or an MCP server looks like. Skipped
        // when the directory already exists, so an edit or a deletion sticks.
        let seeded = atomcode_capabilities::example_skills::seed_missing(
            &atomcode_config::config::Config::config_dir().join("skills"),
        );
        if !seeded.written.is_empty() {
            tracing::info!(
                skills = %seeded
                    .written
                    .iter()
                    .filter_map(|path| path.file_name().map(|name| name.to_string_lossy().to_string()))
                    .collect::<Vec<_>>()
                    .join(","),
                "example skills installed"
            );
        }

        // A callable MCP server, for the same reason: `/mcp/status` used to be empty on
        // a fresh install. The entry names `python3`, so it is only written when that
        // binary is actually in this APK — a server that can never start is worse than
        // an empty list.
        let python_available = options
            .native_lib_dir
            .as_ref()
            .is_some_and(|dir| dir.join(crate::shims::PYTHON3_LIB).exists());
        let seeded_mcp = atomcode_capabilities::example_mcp::seed_missing(
            &atomcode_config::config::Config::config_dir(),
            python_available,
        );
        if seeded_mcp.config_written {
            tracing::info!("example MCP server configured (example-tools)");
        } else if let Some(reason) = &seeded_mcp.config_skipped {
            tracing::info!(reason = %reason, "example MCP server was not configured");
        }

        // Sessions live in the shared workspace by default: an externally started
        // daemon (`atomcode daemon` on the device) can be pointed at the same
        // directory with `ATOMCODE_SESSIONS_DIR`, and then the two read *one*
        // history. `config.toml` and the keys in it stay private — only the
        // session log moves.
        //
        // That default costs privacy: shared storage is readable by every app
        // holding storage access, and the log is the entire conversation. A marker
        // file in the product tree moves the log back into app-private storage.
        let sessions = std::env::var(SESSIONS_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                if options.data_dir.join(SESSIONS_PRIVATE_MARKER).exists() {
                    options.data_dir.join(".sessions")
                } else {
                    options.workspace.join(".sessions")
                }
            });
        if let Err(error) = std::fs::create_dir_all(&sessions) {
            tracing::warn!(dir = %sessions.display(), %error, "cannot create the sessions directory");
        }
        if atomcode_capabilities::session::set_sessions_root(&sessions) {
            // The directory itself is in the field: "shared workspace" would be a
            // lie once the private marker has moved it into the product tree.
            tracing::info!(dir = %sessions.display(), "sessions root pinned");
        } else {
            tracing::warn!(
                dir = %sessions.display(),
                "sessions root was already pinned; keeping the first one"
            );
        }

        // Keeping the CPU awake has to follow the runtime's turn lifecycle: a
        // poller cannot wake an already sleeping device to discover that a turn
        // began (measured — the turn stalled with the screen off). The daemon
        // fires this hook on the first turn and after the last one ends.
        #[cfg(target_os = "android")]
        atomcode_daemon::set_turn_hook(Box::new(|active| {
            crate::jni_api::notify_turn_active(active);
        }));

        // The bundled git, before anything can ask for it. Android forbids
        // executing binaries from the data dir, so the real executable is in
        // `nativeLibraryDir` and the wrappers that `PATH` finds are shims
        // written here (see [`crate::shims`]). On a desktop build there is no
        // bundle and this is a no-op.
        if let Some(native_lib_dir) = &options.native_lib_dir {
            match crate::shims::install(native_lib_dir, &options.data_dir, &options.helper_lib_dirs)
            {
                Some(installed) => {
                    match &installed.git_version {
                        Ok(version) => tracing::info!(
                            git = %version,
                            bin = %installed.bin_dir.display(),
                            "bundled git is available"
                        ),
                        Err(error) => tracing::warn!(
                            %error,
                            bin = %installed.bin_dir.display(),
                            "the bundled git is installed but cannot run: git-dependent features \
                             (agent `bash` git, session rewind) will fail"
                        ),
                    }
                    // The helper APK is optional; say which of its tools are on
                    // PATH so a missing `make` is explained rather than mysterious.
                    if installed.helper_tools.is_empty() {
                        tracing::info!(
                            "helper APK tools are unavailable (install com.atomcode.tools to get \
                             make and a C compiler)"
                        );
                    } else {
                        tracing::info!(
                            tools = %installed.helper_tools.join(","),
                            "helper APK tools are available"
                        );
                    }
                }
                None => tracing::warn!(
                    lib = %native_lib_dir.display(),
                    "no bundled git in the APK: build it with scripts/build-android-git.sh; \
                     git-dependent features will fail"
                ),
            }
        }

        // Say once, at start-up, whether the gateway is reachable at all. The
        // phone build without the official overlay is a valid build — it just
        // cannot use the hosted gateway, and the person should learn that here
        // rather than from a failed turn.
        if gateway_signer_available() {
            tracing::info!("gateway signing available");
        } else {
            tracing::warn!(
                "gateway request signing is unavailable in this build: the AtomGit gateway \
                 (llm-api.atomgit.com) will reject requests. Configure a provider with its own \
                 API key, or use a build that carries the official signing overlay."
            );
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Two workers is deliberate: a phone core is heterogeneous (big.LITTLE)
            // and the heavy work is one agent turn plus HTTP, not a server farm.
            .worker_threads(2)
            .thread_name("atomcode-android")
            .enable_all()
            .build()?;

        let (listener, port) = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind((LOOPBACK, 0u16)).await?;
            let port = listener.local_addr()?.port();
            Ok::<_, std::io::Error>((listener, port))
        })?;

        let tokens = WebuiTokenStore::new();
        let token = tokens.mint();
        let (shutdown, _) = tokio::sync::watch::channel(false);

        let server = ServerOpts {
            host: LOOPBACK.to_string(),
            port,
            // The app owns consent and update delivery; nothing here phones home.
            cli_override: CliOverride { disabled: true },
            // The screen may be off for a long turn; the idle watchdog would kill
            // the server (and the turn) mid-work.
            idle_timeout_secs: 0,
            startup_mode: SessionMode::Webui,
            // A token store means `enforce_token = true`: the URL carries the
            // token once, the index route swaps it for an HttpOnly cookie.
            webui_tokens: Some(tokens),
            working_dir_override: Some(options.workspace.clone()),
            // No startup banner: stdout is logcat, not a person.
            quiet: true,
            prebound_listener: Some(listener),
            owner_shutdown: Some(shutdown.clone()),
            // No AtomGit relay inside the phone build; the app pairs directly.
            app_user_id: None,
            // In-process launcher: never write a daemon token file.
            daemon_token_file: None,
        };

        runtime.spawn(async move {
            if let Err(error) = run_server(server).await {
                tracing::error!(%error, "atomcode android server exited");
            }
        });

        Ok(Self {
            port,
            token,
            shutdown,
            runtime: Some(runtime),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Is this server still listening?
    ///
    /// `nativeStart` is idempotent and hands back the cached URL on an Activity
    /// recreation. That cache can outlive the server: `POST /shutdown` (or an
    /// error inside `run_server`) stops the listener while the process keeps
    /// running, and the URL then points at a port nobody serves. The WebView
    /// keeps showing the page it already loaded, so this looks like "the app is
    /// fine but nothing works" until the process dies.
    ///
    /// A loopback connect is the cheapest check that answers the real question.
    pub fn is_alive(&self) -> bool {
        port_is_listening(self.port)
    }

    /// The URL the WebView loads. Carries the one-time token, which the index
    /// route turns into an HttpOnly cookie — the same handoff a browser gets.
    pub fn url(&self) -> String {
        format!("http://{LOOPBACK}:{}/?token={}", self.port, self.token)
    }

    /// Close HTTP connections and drop the runtime. The session log is already
    /// on disk; a turn still in flight is cancelled by losing its runtime.
    pub fn stop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Whether something accepts connections on `port` of the loopback interface.
///
/// Free-standing rather than a method so it can be asserted without standing up
/// a whole `AgentServer`.
pub fn port_is_listening(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).is_ok()
}

impl Drop for AgentServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_carries_the_token_the_index_route_expects() {
        // A property of the string we hand Kotlin: the parameter name is the one
        // `serve_webui_index` reads (`?token=`), and the address is loopback.
        let server = AgentServer {
            port: 43210,
            token: "abc123".into(),
            shutdown: tokio::sync::watch::channel(false).0,
            runtime: None,
        };
        assert_eq!(server.url(), "http://127.0.0.1:43210/?token=abc123");
        assert!(server.url().starts_with("http://127.0.0.1:"));
    }
}
