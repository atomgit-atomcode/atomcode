//! stdio transport for MCP servers.
//!
//! Communicates with MCP servers via subprocess stdin/stdout using JSON-RPC.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use super::client::McpClient;
use super::types::{
    initialize_params, CallToolResult, InitializeResult, ListToolsResult, ServerStatus,
};

/// Default timeout for MCP operations (30 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Maximum non-protocol lines to skip before giving up.
/// Protects against servers that spam stdout with logs.
const MAX_SKIP_LINES: usize = 100;

/// stdio-based MCP client.
pub struct StdioClient {
    server_name: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    timeout_ms: u64,
    status: Arc<Mutex<ServerStatus>>,
    next_id: Arc<AtomicU64>,
    process: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    reader: Arc<Mutex<Option<BufReader<ChildStdout>>>>,
    /// First response line peeked during startup drain (NDJSON or `Content-Length:`), not yet consumed.
    preread_line: Arc<Mutex<Option<String>>>,
    /// Serialize request/response round-trips.
    ///
    /// MCP over stdio is a single ordered byte stream. Allowing concurrent
    /// in-flight requests can lead to response mix-ups or one caller
    /// consuming the other's response, causing timeouts.
    request_lock: Arc<Mutex<()>>,
    /// Keeps an operation's request, recovery decision, and optional retry in
    /// one critical section. Without this, a request from the failed generation
    /// can overlap the replacement process's initialize handshake.
    operation_lock: Arc<Mutex<()>>,
    /// Serializes teardown + respawn. Concurrent callers that observe the same
    /// dead pipe share one reconnect instead of spawning duplicate servers.
    reconnect_lock: Arc<Mutex<()>>,
    /// Wakes operations that arrived while an uncertain tool call's transport
    /// was being rebuilt in the background.
    recovery_notify: Arc<Notify>,
    /// Remains true for the complete detached-recovery lifecycle. This cannot
    /// be inferred from `status`: reconnect deliberately transitions through
    /// `Failed` while tearing down the old generation.
    recovery_in_progress: Arc<AtomicBool>,
    /// Cancels detached recovery when the registry drops the owning client.
    recovery_cancel: CancellationToken,
    /// Only the registry-owned instance tears down the shared subprocess.
    /// Internal recovery clones must leave it alive when their task completes.
    owns_transport_lifetime: bool,
    /// Advances after every successful initialize handshake. A waiter compares
    /// its failed generation after taking `reconnect_lock` to detect that another
    /// caller already repaired the connection.
    connection_generation: Arc<AtomicU64>,
}

#[derive(Debug)]
struct StdioConnectionClosed(&'static str);

impl std::fmt::Display for StdioConnectionClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for StdioConnectionClosed {}

#[derive(Debug)]
struct RequestAttemptError {
    error: anyhow::Error,
    generation: u64,
    request_may_have_been_sent: bool,
}

impl StdioClient {
    /// Create a new stdio client.
    pub fn new(
        server_name: String,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
    ) -> Self {
        Self {
            server_name,
            command,
            args,
            env,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            status: Arc::new(Mutex::new(ServerStatus::Disconnected)),
            next_id: Arc::new(AtomicU64::new(1)),
            process: Arc::new(Mutex::new(None)),
            stdin: Arc::new(Mutex::new(None)),
            reader: Arc::new(Mutex::new(None)),
            preread_line: Arc::new(Mutex::new(None)),
            request_lock: Arc::new(Mutex::new(())),
            operation_lock: Arc::new(Mutex::new(())),
            reconnect_lock: Arc::new(Mutex::new(())),
            recovery_notify: Arc::new(Notify::new()),
            recovery_in_progress: Arc::new(AtomicBool::new(false)),
            recovery_cancel: CancellationToken::new(),
            owns_transport_lifetime: true,
            connection_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Start the subprocess and set up communication.
    async fn start(&self) -> Result<()> {
        // On Windows, commands like `npx`, `npm` are .cmd/.bat scripts
        // that cannot be spawned directly via Command::new(). Wrap them
        // through `cmd.exe /C` so the OS can locate and execute them.
        #[cfg(target_os = "windows")]
        let (command, args) = windows_wrap_command(&self.command, &self.args);

        #[cfg(not(target_os = "windows"))]
        let (command, args) = (self.command.clone(), self.args.clone());

        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        crate::mcp::util::suppress_console_window(&mut cmd);

        let mut child = cmd.spawn().with_context(|| {
            #[cfg(target_os = "windows")]
            {
                let msg = format!(
                    "Failed to spawn MCP server: {}. \
                     On Windows, commands like 'npx' are .cmd scripts and must \
                     be executed through 'cmd /C'. AtomCode wraps known commands \
                     automatically; if this is a custom .cmd/.bat, set command to \
                     'cmd' and add '/C' before the script name in args.",
                    self.command
                );
                msg
            }
            #[cfg(not(target_os = "windows"))]
            {
                format!("Failed to spawn MCP server: {}", self.command)
            }
        })?;

        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = child.stdout.take().context("Failed to get stdout")?;
        let reader = BufReader::new(stdout);

        *self.process.lock().await = Some(child);
        *self.stdin.lock().await = Some(stdin);
        *self.reader.lock().await = Some(reader);

        Ok(())
    }

    /// Send a request and wait for response.
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> std::result::Result<serde_json::Value, RequestAttemptError> {
        let _req_guard = self.request_lock.lock().await;
        // Record the generation only after entering the serialized request
        // section. A caller may have waited here while another caller replaced
        // the process.
        let generation = self.connection_generation.load(Ordering::SeqCst);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // IMPORTANT: omit `params` when it's None.
        //
        // The official JS MCP SDK's stdio transport can hang when it receives
        // `"params": null` for methods that expect params to be absent.
        let mut request = serde_json::Map::new();
        request.insert(
            "jsonrpc".to_string(),
            serde_json::Value::String("2.0".to_string()),
        );
        request.insert("id".to_string(), serde_json::Value::Number(id.into()));
        request.insert(
            "method".to_string(),
            serde_json::Value::String(method.to_string()),
        );
        if let Some(p) = params {
            request.insert("params".to_string(), p);
        }
        let request = serde_json::Value::Object(request);

        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.timeout_ms);

        // Write request (NDJSON).
        {
            let mut stdin = tokio::time::timeout_at(deadline, self.stdin.lock())
                .await
                .with_context(|| {
                    format!(
                        "MCP request {method} timed out after {}ms waiting for stdin",
                        self.timeout_ms
                    )
                })
                .map_err(|error| RequestAttemptError {
                    error,
                    generation,
                    request_may_have_been_sent: false,
                })?;
            let stdin = stdin.as_mut().ok_or_else(|| RequestAttemptError {
                error: anyhow::Error::new(StdioConnectionClosed("MCP stdin closed")),
                generation,
                request_may_have_been_sent: false,
            })?;

            let mut body = serde_json::to_vec(&request).map_err(|error| RequestAttemptError {
                error: error.into(),
                generation,
                request_may_have_been_sent: false,
            })?;
            body.push(b'\n');
            // Once write_all starts, a failure can still mean that a partial or
            // complete request reached the server. Treat the outcome as unknown
            // so side-effecting tools are never replayed automatically.
            tokio::time::timeout_at(deadline, stdin.write_all(&body))
                .await
                .with_context(|| {
                    format!(
                        "MCP request {method} timed out after {}ms while writing",
                        self.timeout_ms
                    )
                })
                .map_err(|error| RequestAttemptError {
                    error,
                    generation,
                    request_may_have_been_sent: true,
                })?
                .map_err(|error| RequestAttemptError {
                    error: error.into(),
                    generation,
                    request_may_have_been_sent: true,
                })?;
            tokio::time::timeout_at(deadline, stdin.flush())
                .await
                .with_context(|| {
                    format!(
                        "MCP request {method} timed out after {}ms while flushing",
                        self.timeout_ms
                    )
                })
                .map_err(|error| RequestAttemptError {
                    error,
                    generation,
                    request_may_have_been_sent: true,
                })?
                .map_err(|error| RequestAttemptError {
                    error: error.into(),
                    generation,
                    request_may_have_been_sent: true,
                })?;
        }

        // The write, flush, and read phases use one operation deadline.
        let result = tokio::time::timeout_at(deadline, self.recv_jsonrpc_response())
            .await
            .with_context(|| {
                format!(
                    "MCP request {} timed out after {}ms",
                    method, self.timeout_ms
                )
            })
            .map_err(|error| RequestAttemptError {
                error,
                generation,
                request_may_have_been_sent: true,
            })?
            .map_err(|error| RequestAttemptError {
                error,
                generation,
                request_may_have_been_sent: true,
            })?;

        if let Some(error) = result.error {
            return Err(RequestAttemptError {
                error: anyhow::anyhow!("MCP error {} (code {})", error.message, error.code),
                generation,
                request_may_have_been_sent: true,
            });
        }

        result.result.ok_or_else(|| RequestAttemptError {
            error: anyhow::anyhow!("MCP response missing result"),
            generation,
            request_may_have_been_sent: true,
        })
    }

    async fn initialize_connection(&self) -> Result<InitializeResult> {
        *self.status.lock().await = ServerStatus::Connecting;
        self.start().await?;
        self.drain_startup_messages().await?;

        let result: InitializeResult = serde_json::from_value(
            self.send_request("initialize", Some(initialize_params()))
                .await
                .map_err(|attempt| attempt.error)?,
        )
        .context("Failed to parse initialize result")?;

        {
            let mut stdin = self.stdin.lock().await;
            let stdin = stdin.as_mut().ok_or_else(|| {
                anyhow::Error::new(StdioConnectionClosed(
                    "MCP stdin closed before initialized notification",
                ))
            })?;
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            let mut body = serde_json::to_vec(&notification)?;
            body.push(b'\n');
            stdin.write_all(&body).await?;
            stdin.flush().await?;
        }

        *self.status.lock().await = ServerStatus::Connected;
        self.connection_generation.fetch_add(1, Ordering::SeqCst);
        Ok(result)
    }

    async fn clear_transport(&self) {
        self.stdin.lock().await.take();
        self.reader.lock().await.take();
        self.preread_line.lock().await.take();
        if let Some(mut child) = self.process.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    async fn mark_failed(&self, message: String) {
        *self.status.lock().await = ServerStatus::Failed(message);
        self.clear_transport().await;
    }

    async fn reconnect_after_failure(
        &self,
        failed_generation: u64,
        first_error: &anyhow::Error,
    ) -> Result<()> {
        let _reconnect = self.reconnect_lock.lock().await;
        if self.connection_generation.load(Ordering::SeqCst) != failed_generation
            && matches!(&*self.status.lock().await, ServerStatus::Connected)
        {
            return Ok(());
        }

        self.mark_failed(format!("stdio connection lost: {first_error:#}"))
            .await;
        if let Err(error) = self.initialize_connection().await {
            self.mark_failed(format!("stdio reconnect failed: {error:#}"))
                .await;
            return Err(error).context("MCP stdio reconnect failed");
        }
        Ok(())
    }

    async fn mark_failed_if_generation_current(&self, failed_generation: u64, message: String) {
        let _reconnect = self.reconnect_lock.lock().await;
        if self.connection_generation.load(Ordering::SeqCst) == failed_generation {
            self.mark_failed(message).await;
        }
    }

    async fn wait_for_background_recovery(&self) -> Result<()> {
        loop {
            let notified = self.recovery_notify.notified();
            if !self.recovery_in_progress.load(Ordering::Acquire) {
                return Ok(());
            }
            tokio::time::timeout(Duration::from_millis(self.timeout_ms), notified)
                .await
                .context("MCP stdio connection recovery timed out")?;
        }
    }

    async fn reconnect_unknown_tool_in_background(
        &self,
        failed_generation: u64,
        first_error: String,
    ) {
        // Publish the barrier before spawning: a queued operation must never
        // race ahead and write another request into the uncertain generation.
        self.recovery_in_progress.store(true, Ordering::Release);
        *self.status.lock().await = ServerStatus::Connecting;
        let client = self.clone_for_recovery();
        tokio::spawn(async move {
            let error = anyhow::anyhow!(first_error);
            tokio::select! {
                _ = client.recovery_cancel.cancelled() => {
                    // The registry/runtime owner went away while recovery was
                    // in flight. Finish teardown here in case the owner's
                    // best-effort Drop could not acquire the process lock.
                    client.clear_transport().await;
                }
                _ = client.reconnect_after_failure(failed_generation, &error) => {}
            }
            client.recovery_in_progress.store(false, Ordering::Release);
            client.recovery_notify.notify_waiters();
        });
    }

    /// Clone the shared transport state for one detached recovery task.
    ///
    /// This is intentionally not a public `Clone` implementation: only the
    /// registry-owned instance controls the subprocess lifetime.
    fn clone_for_recovery(&self) -> Self {
        Self {
            server_name: self.server_name.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            timeout_ms: self.timeout_ms,
            status: self.status.clone(),
            next_id: self.next_id.clone(),
            process: self.process.clone(),
            stdin: self.stdin.clone(),
            reader: self.reader.clone(),
            preread_line: self.preread_line.clone(),
            request_lock: self.request_lock.clone(),
            operation_lock: self.operation_lock.clone(),
            reconnect_lock: self.reconnect_lock.clone(),
            recovery_notify: self.recovery_notify.clone(),
            recovery_in_progress: self.recovery_in_progress.clone(),
            recovery_cancel: self.recovery_cancel.clone(),
            owns_transport_lifetime: false,
            connection_generation: self.connection_generation.clone(),
        }
    }

    async fn send_request_with_reconnect(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        retry_after_send: bool,
    ) -> Result<serde_json::Value> {
        let _operation = self.operation_lock.lock().await;
        self.wait_for_background_recovery().await?;
        match self.send_request(method, params.clone()).await {
            Ok(value) => Ok(value),
            Err(attempt) if is_reconnectable_stdio_error(&attempt.error) => {
                if attempt.request_may_have_been_sent && !retry_after_send {
                    let first_error = format!("{:#}", attempt.error);
                    self.reconnect_unknown_tool_in_background(attempt.generation, first_error)
                        .await;
                    return Err(attempt.error).context(
                        "MCP tool execution result is unknown; request was not replayed to avoid \
                         duplicate side effects, and stdio recovery continues in the background",
                    );
                }
                if let Err(reconnect_error) = self
                    .reconnect_after_failure(attempt.generation, &attempt.error)
                    .await
                {
                    return Err(reconnect_error);
                }
                match self.send_request(method, params).await {
                    Ok(value) => Ok(value),
                    Err(retry) => {
                        if is_reconnectable_stdio_error(&retry.error) {
                            self.mark_failed_if_generation_current(
                                retry.generation,
                                format!("stdio connection lost after reconnect: {:#}", retry.error),
                            )
                            .await;
                        }
                        Err(retry.error).context("MCP request failed after one stdio reconnect")
                    }
                }
            }
            Err(attempt) => Err(attempt.error),
        }
    }
}

#[async_trait]
impl McpClient for StdioClient {
    async fn initialize(&mut self) -> Result<InitializeResult> {
        let _reconnect = self.reconnect_lock.lock().await;
        match self.initialize_connection().await {
            Ok(result) => Ok(result),
            Err(error) => {
                self.mark_failed(format!("stdio initialize failed: {error:#}"))
                    .await;
                Err(error)
            }
        }
    }

    async fn list_tools(&self) -> Result<ListToolsResult> {
        let result = self
            .send_request_with_reconnect("tools/list", None, true)
            .await?;
        serde_json::from_value(result).context("Failed to parse tools/list result")
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });

        let result = self
            .send_request_with_reconnect("tools/call", Some(params), false)
            .await?;
        serde_json::from_value(result).context("Failed to parse tools/call result")
    }

    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn status(&self) -> ServerStatus {
        if let Ok(mut process) = self.process.try_lock() {
            if let Some(child) = process.as_mut() {
                match child.try_wait() {
                    Ok(Some(exit)) => {
                        return ServerStatus::Failed(format!(
                            "stdio process exited unexpectedly: {exit}"
                        ));
                    }
                    Err(error) => {
                        return ServerStatus::Failed(format!(
                            "failed to inspect stdio process: {error}"
                        ));
                    }
                    Ok(None) => {}
                }
            }
        }
        self.status
            .try_lock()
            .map(|s| s.clone())
            .unwrap_or(ServerStatus::Disconnected)
    }
}

impl StdioClient {
    /// Read one JSON-RPC response (NDJSON per MCP stdio spec, or legacy `Content-Length` framing).
    async fn recv_jsonrpc_response(&self) -> Result<super::types::JsonRpcResponse> {
        let mut reader = self.reader.lock().await;
        let reader = reader
            .as_mut()
            .ok_or_else(|| anyhow::Error::new(StdioConnectionClosed("MCP stdout closed")))?;

        let mut skipped_lines = 0;
        loop {
            let line = if let Some(s) = self.preread_line.lock().await.take() {
                s
            } else {
                let mut buf = String::new();
                loop {
                    buf.clear();
                    let n = reader.read_line(&mut buf).await?;
                    if n == 0 {
                        return Err(anyhow::Error::new(StdioConnectionClosed(
                            "MCP server closed stdout",
                        )));
                    }
                    if !buf.trim().is_empty() {
                        break;
                    }
                }
                buf
            };

            let body = line.trim_end_matches(['\r', '\n']).trim_start();
            if body.starts_with('{') || body.starts_with('[') {
                return serde_json::from_str(body)
                    .context("Failed to parse NDJSON MCP message as JSON-RPC");
            }
            if strip_prefix_ci(body, "content-length:").is_some() {
                return read_content_length_message(reader, line).await;
            }

            // Some third-party MCP servers incorrectly print status logs to stdout
            // after initialization. MCP requires stdout to contain only protocol
            // messages, but skipping plain-text lines keeps otherwise usable tools
            // available while still failing on malformed JSON-RPC frames above.
            skipped_lines += 1;
            if skipped_lines > MAX_SKIP_LINES {
                bail!(
                    "MCP stdio: too many non-protocol lines (>{MAX_SKIP_LINES}), last line: {}",
                    body.chars().take(80).collect::<String>()
                );
            }
        }
    }

    /// Drain non-protocol lines the server may print to stdout before the first MCP message.
    ///
    /// Lines that look like NDJSON or `Content-Length` are **not** consumed; they are moved to
    /// [`Self::preread_line`] for [`Self::recv_jsonrpc_response`].
    async fn drain_startup_messages(&self) -> Result<()> {
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let mut line = String::new();
                let mut reader = self.reader.lock().await;
                let Some(r) = reader.as_mut() else {
                    return;
                };
                let read_res =
                    tokio::time::timeout(Duration::from_millis(80), r.read_line(&mut line)).await;
                drop(reader);

                match read_res {
                    Err(_) | Ok(Err(_)) | Ok(Ok(0)) => return,
                    Ok(Ok(_)) => {
                        let t = line.trim();
                        if t.is_empty() {
                            continue;
                        }
                        let js = t.trim_start();
                        if js.starts_with('{')
                            || js.starts_with('[')
                            || strip_prefix_ci(js, "content-length:").is_some()
                        {
                            *self.preread_line.lock().await = Some(line);
                            return;
                        }
                    }
                }
            }
        })
        .await;

        Ok(())
    }
}

/// `prefix_lower` must be ASCII lower case.
fn strip_prefix_ci<'a>(s: &'a str, prefix_lower: &'static str) -> Option<&'a str> {
    let b = s.as_bytes();
    let p = prefix_lower.as_bytes();
    if b.len() < p.len() {
        return None;
    }
    if !b[..p.len()].eq_ignore_ascii_case(p) {
        return None;
    }
    Some(&s[p.len()..])
}

fn is_reconnectable_stdio_error(error: &anyhow::Error) -> bool {
    use std::io::ErrorKind;

    error.chain().any(|cause| {
        if cause.downcast_ref::<StdioConnectionClosed>().is_some()
            || cause
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        {
            return true;
        }
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionReset
                    | ErrorKind::NotConnected
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::TimedOut
            )
        })
    })
}

async fn read_content_length_message(
    reader: &mut BufReader<ChildStdout>,
    mut line: String,
) -> Result<super::types::JsonRpcResponse> {
    let mut content_length: Option<usize> = None;
    loop {
        let t = line.trim_end_matches(['\r', '\n']).trim();
        if t.is_empty() {
            break;
        }
        if let Some(rest) = strip_prefix_ci(t, "content-length:") {
            content_length = Some(rest.trim().parse().context("Invalid Content-Length")?);
        }
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow::Error::new(StdioConnectionClosed(
                "MCP server closed stdout while reading headers",
            )));
        }
    }

    let length = content_length.context("Missing Content-Length header")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("Failed to parse JSON-RPC response")
}

/// On Windows, commands like `npx`, `npm`, `yarn`, `pnpm` are actually
/// `.cmd`/`.bat` scripts that cannot be spawned directly via
/// `Command::new()`. The OS `CreateProcess` API only launches `.exe`
/// files directly. This function detects such commands and wraps them
/// through `cmd.exe /C` so the OS can locate and execute the script.
///
/// If the user has already wrapped the command themselves (e.g.
/// `command: "cmd"`, `args: ["/C", "npx", ...]`), this function is a
/// no-op — `cmd` / `cmd.exe` are not in the wrap list.
///
/// The core logic is platform-independent (and testable on all platforms);
/// the `shell` parameter is `"cmd.exe"` on Windows.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn wrap_cmd_script(command: &str, args: &[String], shell: &str) -> (String, Vec<String>) {
    /// Commands that are known to be `.cmd`/`.bat` scripts on Windows.
    /// Checked case-insensitively.
    const CMD_SCRIPTS: &[&str] = &[
        "npx", "npm", "npx.cmd", "npm.cmd", "yarn", "yarn.cmd", "pnpm", "pnpm.cmd",
    ];

    let lower = command.to_ascii_lowercase();
    let needs_wrap = CMD_SCRIPTS.iter().any(|&s| lower == s)
        || lower.ends_with(".cmd")
        || lower.ends_with(".bat");

    if needs_wrap {
        let mut wrapped_args = vec!["/C".to_string(), command.to_string()];
        wrapped_args.extend(args.iter().cloned());
        (shell.to_string(), wrapped_args)
    } else {
        (command.to_string(), args.to_vec())
    }
}

/// Windows-specific entry point that passes `"cmd.exe"` as the shell.
#[cfg(target_os = "windows")]
fn windows_wrap_command(command: &str, args: &[String]) -> (String, Vec<String>) {
    wrap_cmd_script(command, args, "cmd.exe")
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        if !self.owns_transport_lifetime {
            return;
        }
        self.recovery_cancel.cancel();
        // Try to kill the subprocess gracefully
        if let Ok(mut process) = self.process.try_lock() {
            if let Some(mut child) = process.take() {
                let _ = child.start_kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Platform-independent tests for wrap_cmd_script logic ---
    // These run on ALL platforms (macOS, Linux, Windows) so we can
    // verify the wrapping logic locally without a Windows machine.

    #[test]
    fn wrap_npx() {
        let (cmd, args) = wrap_cmd_script("npx", &["-y".into(), "@pkg/server".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "npx", "-y", "@pkg/server"]);
    }

    #[test]
    fn wrap_npx_cmd_suffix() {
        let (cmd, args) =
            wrap_cmd_script("npx.cmd", &["-y".into(), "@pkg/server".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "npx.cmd", "-y", "@pkg/server"]);
    }

    #[test]
    fn wrap_npm() {
        let (cmd, args) = wrap_cmd_script("npm", &["install".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "npm", "install"]);
    }

    #[test]
    fn wrap_yarn() {
        let (cmd, args) = wrap_cmd_script("yarn", &["add".into(), "lodash".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "yarn", "add", "lodash"]);
    }

    #[test]
    fn wrap_pnpm() {
        let (cmd, args) = wrap_cmd_script("pnpm", &["install".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "pnpm", "install"]);
    }

    #[test]
    fn wrap_custom_bat() {
        let (cmd, args) = wrap_cmd_script("my-script.bat", &["--flag".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "my-script.bat", "--flag"]);
    }

    #[test]
    fn wrap_custom_cmd_suffix() {
        let (cmd, args) = wrap_cmd_script("build.cmd", &[], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "build.cmd"]);
    }

    #[test]
    fn no_wrap_exe() {
        let (cmd, args) = wrap_cmd_script("node", &["server.js".into()], "cmd.exe");
        assert_eq!(cmd, "node");
        assert_eq!(args, vec!["server.js"]);
    }

    #[test]
    fn no_wrap_already_wrapped() {
        // If user already set command to "cmd", don't double-wrap
        let (cmd, args) =
            wrap_cmd_script("cmd", &["/C".into(), "npx".into(), "-y".into()], "cmd.exe");
        assert_eq!(cmd, "cmd");
        assert_eq!(args, vec!["/C", "npx", "-y"]);
    }

    #[test]
    fn wrap_case_insensitive() {
        let (cmd, args) = wrap_cmd_script("NPX", &["-y".into(), "@pkg/server".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args, vec!["/C", "NPX", "-y", "@pkg/server"]);
    }

    #[test]
    fn wrap_preserves_original_command_in_args() {
        // The original command (with original casing) should appear in args
        let (cmd, args) = wrap_cmd_script("Npx", &["-y".into()], "cmd.exe");
        assert_eq!(cmd, "cmd.exe");
        assert_eq!(args[1], "Npx"); // original casing preserved
    }

    #[test]
    fn no_wrap_python() {
        let (cmd, args) = wrap_cmd_script("python", &["-m".into(), "server".into()], "cmd.exe");
        assert_eq!(cmd, "python");
        assert_eq!(args, vec!["-m", "server"]);
    }

    #[tokio::test]
    async fn background_recovery_barrier_survives_transient_failed_status() {
        let client = StdioClient::new(
            "test".to_string(),
            "unused".to_string(),
            Vec::new(),
            BTreeMap::new(),
            Some(1_000),
        );
        client.recovery_in_progress.store(true, Ordering::Release);
        *client.status.lock().await = ServerStatus::Failed("tearing down".to_string());

        let waiter_client = client.clone_for_recovery();
        let mut waiter =
            tokio::spawn(async move { waiter_client.wait_for_background_recovery().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err(),
            "a transient Failed status must not let requests bypass recovery"
        );

        client.recovery_in_progress.store(false, Ordering::Release);
        client.recovery_notify.notify_waiters();
        waiter
            .await
            .expect("waiter task should complete")
            .expect("recovery barrier should open");
    }
}
