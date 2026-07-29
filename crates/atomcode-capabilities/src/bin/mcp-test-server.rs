use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Write};

fn main() -> io::Result<()> {
    if let Some(path) = std::env::var_os("MCP_TEST_SPAWN_COUNTER") {
        let mut counter = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(counter, "spawn")?;
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    loop {
        let request = match read_frame(&mut reader) {
            Ok(value) => value,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };

        let Some(method) = request.get("method").and_then(|value| value.as_str()) else {
            continue;
        };

        let id = request.get("id").cloned();
        let response = match method {
            "initialize" => id.map(|id| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-11-05",
                        "capabilities": {
                            "tools": {}
                        },
                        "serverInfo": {
                            "name": "mcp-test-server",
                            "version": "0.1.0"
                        }
                    }
                })
            }),
            "tools/list" => id.map(|id| {
                if std::env::var_os("MCP_TEST_EXIT_ON_EVERY_TOOLS_LIST").is_some() {
                    std::process::exit(0);
                }
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "echo",
                                "description": "Echoes the provided message",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "message": {
                                            "type": "string"
                                        }
                                    },
                                    "required": ["message"]
                                }
                            }
                        ]
                    }
                })
            }),
            "tools/call" => {
                if let Some(path) = std::env::var_os("MCP_TEST_EXIT_TOOL_CALLS_COUNTER") {
                    let path = std::path::PathBuf::from(path);
                    let remaining = std::fs::read_to_string(&path)?
                        .trim()
                        .parse::<usize>()
                        .map_err(io::Error::other)?;
                    if remaining > 0 {
                        std::fs::write(path, (remaining - 1).to_string())?;
                        return Ok(());
                    }
                }
                if let Some(marker) = std::env::var_os("MCP_TEST_EXIT_ONCE_MARKER") {
                    let marker = std::path::PathBuf::from(marker);
                    if !marker.exists() {
                        std::fs::write(marker, b"exited")?;
                        return Ok(());
                    }
                }
                if std::env::var_os("MCP_TEST_EXIT_ON_EVERY_TOOL_CALL").is_some() {
                    return Ok(());
                }
                let message = request
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .and_then(|arguments| arguments.get("message"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                id.map(|id| {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!("echo:{message}")
                                }
                            ]
                        }
                    })
                })
            }
            "notifications/initialized" => {
                if std::env::var_os("MCP_TEST_EXIT_AFTER_INITIALIZED").is_some() {
                    return Ok(());
                }
                if std::env::var_os("MCP_TEST_STDOUT_NOISE_AFTER_INITIALIZED").is_some() {
                    writeln!(writer, "✅ MCP server initialized and ready (stdio).")?;
                    writer.flush()?;
                }
                None
            }
            _ => id.map(|id| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Unknown method: {method}")
                    }
                })
            }),
        };

        if let Some(response) = response {
            write_frame(&mut writer, &response)?;
        }
    }
}

/// MCP stdio: newline-delimited JSON (NDJSON).
fn write_frame<W: Write>(writer: &mut W, payload: &serde_json::Value) -> io::Result<()> {
    writeln!(
        writer,
        "{}",
        serde_json::to_string(payload).map_err(io::Error::other)?
    )?;
    writer.flush()?;
    Ok(())
}

fn read_frame<R: BufRead + Read>(reader: &mut R) -> io::Result<serde_json::Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stdin closed"));
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with('{') || t.starts_with('[') {
            return serde_json::from_str(t).map_err(io::Error::other);
        }
        if strip_prefix_ci(t, "content-length:").is_some() {
            return read_content_length_body(reader, &line);
        }
        return Err(io::Error::other(format!(
            "expected NDJSON or Content-Length, got: {}",
            t.chars().take(80).collect::<String>()
        )));
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix_lower: &'static str) -> Option<&'a str> {
    let b = s.as_bytes();
    let p = prefix_lower.as_bytes();
    if b.len() < p.len() || !b[..p.len()].eq_ignore_ascii_case(p) {
        return None;
    }
    Some(&s[p.len()..])
}

fn read_content_length_body<R: BufRead + Read>(
    reader: &mut R,
    first: &str,
) -> io::Result<serde_json::Value> {
    let mut line = first.to_string();
    let mut content_length: Option<usize> = None;
    loop {
        let t = line.trim_end_matches(['\r', '\n']).trim();
        if t.is_empty() {
            break;
        }
        if let Some(rest) = strip_prefix_ci(t, "content-length:") {
            content_length = Some(rest.trim().parse().map_err(io::Error::other)?);
        }
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdin closed in headers",
            ));
        }
    }
    let length = content_length.ok_or_else(|| io::Error::other("missing Content-Length"))?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}
