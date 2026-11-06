//! Bounded stdio DAP transport. One owner; no global debugger process pool.
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    task::JoinHandle,
};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_HEADER: usize = 8192;

pub(super) async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Value, String> {
    let mut header = Vec::new();
    loop {
        let byte = reader
            .read_u8()
            .await
            .map_err(|e| format!("DAP stream closed: {e}"))?;
        header.push(byte);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() >= MAX_HEADER {
            return Err("DAP header too large".into());
        }
    }
    let header = std::str::from_utf8(&header).map_err(|_| "invalid DAP header")?;
    let mut length = None;
    for line in header.split("\r\n").filter(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').ok_or("invalid DAP header")?;
        if key.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err("duplicate Content-Length".into());
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "invalid Content-Length")?,
            );
        }
    }
    let length = length
        .filter(|n| *n > 0 && *n <= MAX_FRAME)
        .ok_or("missing or oversized Content-Length")?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| format!("invalid DAP JSON: {e}"))
}

pub(super) struct Client {
    _child: Child,
    pid: u32,
    #[cfg(windows)]
    job: Option<crate::process_utils::JobHandle>,
    input: ChildStdin,
    reader: JoinHandle<()>,
    stderr: JoinHandle<()>,
    messages: mpsc::Receiver<Result<Value, String>>,
    seq: u64,
    responses: HashMap<u64, Value>,
    pub id: u64,
    pub initialized: bool,
    pub state: &'static str,
    pub stop_id: u64,
    pub thread: Option<i64>,
    pub reason: Option<String>,
    pub capabilities: Value,
    output: String,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr.abort();
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        crate::process_utils::kill_windows_tree(&self.job, Some(self.pid));
        // Child's kill_on_drop also terminates/reaps the direct adapter.
    }
}

impl Client {
    pub fn spawn(command: &str, args: &[String], cwd: &Path, id: u64) -> Result<Self, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::process_utils::suppress_console_window(&mut cmd);
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start DAP adapter: {e}"))?;
        let pid = child.id().ok_or("adapter exited before initialization")?;
        #[cfg(windows)]
        let job = crate::process_utils::assign_child_to_kill_on_close_job(&child);
        let input = child.stdin.take().ok_or("missing adapter stdin")?;
        let stdout = child.stdout.take().ok_or("missing adapter stdout")?;
        let mut stderr = child.stderr.take().ok_or("missing adapter stderr")?;
        let (tx, messages) = mpsc::channel(64);
        let reader = tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            loop {
                let message = read_frame(&mut stdout).await;
                let failed = message.is_err();
                // Debuggee output may be unbounded. Never let it hold up protocol
                // responses. The tool exposes a bounded best-effort output tail.
                if message
                    .as_ref()
                    .is_ok_and(|m| m["type"] == "event" && m["event"] == "output")
                {
                    let _ = tx.try_send(message);
                } else if tx.send(message).await.is_err() {
                    break;
                }
                if failed {
                    break;
                }
            }
        });
        let stderr = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            while matches!(stderr.read(&mut buffer).await, Ok(n) if n > 0) {}
        });
        Ok(Self {
            _child: child,
            pid,
            #[cfg(windows)]
            job,
            input,
            reader,
            stderr,
            messages,
            seq: 0,
            responses: HashMap::new(),
            id,
            initialized: false,
            state: "starting",
            stop_id: 0,
            thread: None,
            reason: None,
            capabilities: Value::Null,
            output: String::new(),
        })
    }

    async fn write(&mut self, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FRAME {
            return Err("DAP request too large".into());
        }
        self.input
            .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.input
            .write_all(&bytes)
            .await
            .map_err(|e| e.to_string())?;
        self.input.flush().await.map_err(|e| e.to_string())
    }
    pub async fn send(&mut self, command: &str, arguments: Value) -> Result<u64, String> {
        self.seq += 1;
        let seq = self.seq;
        self.write(&json!({"seq":seq,"type":"request","command":command,"arguments":arguments}))
            .await?;
        Ok(seq)
    }
    pub async fn request(&mut self, command: &str, arguments: Value) -> Result<Value, String> {
        let seq = self.send(command, arguments).await?;
        self.response(seq).await
    }
    pub fn check_request_failure(&self, seq: u64) -> Result<(), String> {
        if let Some(response) = self.responses.get(&seq) {
            if response["success"] != true {
                return Err(format!(
                    "DAP {} failed: {}",
                    response["command"], response["message"]
                ));
            }
        }
        Ok(())
    }
    pub async fn response(&mut self, seq: u64) -> Result<Value, String> {
        loop {
            if let Some(response) = self.responses.remove(&seq) {
                if response["success"] != true {
                    return Err(format!(
                        "DAP {} failed: {}",
                        response["command"], response["message"]
                    ));
                }
                return Ok(response.get("body").cloned().unwrap_or(json!({})));
            }
            self.pump().await?;
        }
    }
    pub async fn pump(&mut self) -> Result<(), String> {
        let message = self
            .messages
            .recv()
            .await
            .ok_or("DAP adapter disconnected")??;
        self.ingest(message).await
    }
    pub async fn drain(&mut self) -> Result<(), String> {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                Ok(message) => self.ingest(message).await?,
                Err(_) if self.state == "terminated" => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    async fn ingest(&mut self, message: Value) -> Result<(), String> {
        match message["type"].as_str() {
            Some("response") => {
                let seq = message["request_seq"]
                    .as_u64()
                    .ok_or("invalid DAP response seq")?;
                if self.responses.len() >= 16 {
                    return Err("too many unsolicited DAP responses".into());
                }
                self.responses.insert(seq, message);
            }
            Some("event") => match message["event"].as_str() {
                Some("initialized") => self.initialized = true,
                Some("stopped") => {
                    self.state = "stopped";
                    self.stop_id += 1;
                    self.thread = message["body"]["threadId"].as_i64();
                    self.reason = message["body"]["reason"]
                        .as_str()
                        .map(|reason| reason.chars().take(128).collect());
                }
                Some("continued") => {
                    self.state = "running";
                    self.thread = None;
                }
                Some("terminated" | "exited") => {
                    self.state = "terminated";
                    self.thread = None;
                }
                Some("output") => {
                    if let Some(text) = message["body"]["output"].as_str() {
                        self.output.push_str(text);
                        if self.output.len() > 8192 {
                            let mut start = self.output.len() - 8192;
                            while !self.output.is_char_boundary(start) {
                                start += 1;
                            }
                            self.output.drain(..start);
                        }
                    }
                }
                _ => {}
            },
            Some("request") => {
                self.seq += 1;
                self.write(&json!({"seq":self.seq,"type":"response","request_seq":message["seq"],
                    "command":message["command"],"success":false,
                    "message":"Reverse requests (runInTerminal/startDebugging) are unsupported; use an internal console and disable subprocess debugging."})).await?;
                return Err("DAP adapter requires an unsupported reverse request".into());
            }
            _ => return Err("invalid DAP message type".into()),
        }
        Ok(())
    }
    pub fn snapshot(&mut self) -> Value {
        json!({"session_id":self.id,"state":self.state,"stop_id":self.stop_id,
            "thread_id":self.thread,"reason":self.reason,"output_tail":std::mem::take(&mut self.output)})
    }
}
