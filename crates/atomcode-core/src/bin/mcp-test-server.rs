use std::io::{self, BufRead, BufReader, Write};

fn main() -> io::Result<()> {
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
            "notifications/initialized" => None,
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

fn write_frame<W: Write>(writer: &mut W, payload: &serde_json::Value) -> io::Result<()> {
    let body = serde_json::to_vec(payload)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_frame<R: BufRead>(reader: &mut R) -> io::Result<serde_json::Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdin closed",
            ));
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse().map_err(io::Error::other)?);
        }
    }

    let length = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}
