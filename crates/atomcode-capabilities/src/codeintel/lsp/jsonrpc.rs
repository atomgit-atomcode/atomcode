//! Minimal LSP JSON-RPC framing: `Content-Length` headers over a byte stream. Ported
//! from production `lsp/jsonrpc.rs`.

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

/// Count wire bytes, including CR/LF. Check before retaining bytes, even when
/// the peer never sends a newline. BufReader itself retains only its fixed buffer.
async fn read_header_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    remaining: usize,
) -> std::io::Result<Vec<u8>> {
    let limit = remaining.min(MAX_HEADER_LINE_BYTES);
    let mut line = Vec::new();
    loop {
        if line.len() == limit {
            return Err(invalid("LSP message header exceeds line/block byte limit"));
        }
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "LSP stream closed in headers",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if count > limit - line.len() {
            return Err(invalid("LSP message header exceeds line/block byte limit"));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(line);
        }
    }
}

/// Frame a JSON body with its `Content-Length` header.
pub fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Read one framed message body: parse the header block (until the blank line), then
/// read exactly `Content-Length` bytes. Bounds apply before buffering headers/body.
/// A framing error is fatal: callers must close this reader, not attempt resynchronization.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Vec<u8>> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0;
    loop {
        let line = read_header_line(reader, MAX_HEADER_BYTES - header_bytes).await?;
        header_bytes += line.len();
        let line =
            std::str::from_utf8(&line).map_err(|_| invalid("LSP header is not valid UTF-8"))?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                if content_length.is_some() {
                    return Err(invalid("LSP message has duplicate Content-Length"));
                }
                let value = value.trim();
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(invalid("LSP message has invalid Content-Length"));
                }
                let len = value
                    .parse::<usize>()
                    .map_err(|_| invalid("LSP Content-Length overflows usize"))?;
                if len > MAX_MESSAGE_BYTES {
                    return Err(invalid(format!(
                        "LSP message exceeds {MAX_MESSAGE_BYTES} bytes"
                    )));
                }
                content_length = Some(len);
            }
        }
    }
    let len = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LSP message: no Content-Length",
        )
    })?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_has_header_and_body() {
        let framed = encode(b"{\"jsonrpc\":\"2.0\"}");
        let s = String::from_utf8(framed).unwrap();
        assert_eq!(s, "Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}");
    }

    #[tokio::test]
    async fn round_trips_two_messages() {
        let a = encode(b"{\"id\":1}");
        let b = encode(b"{\"id\":2}");
        let mut buf = a.clone();
        buf.extend_from_slice(&b);
        let mut reader = BufReader::new(&buf[..]);
        assert_eq!(read_message(&mut reader).await.unwrap(), b"{\"id\":1}");
        assert_eq!(read_message(&mut reader).await.unwrap(), b"{\"id\":2}");
    }

    #[tokio::test]
    async fn eof_is_error() {
        let mut reader = BufReader::new(&b""[..]);
        assert!(read_message(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn oversized_message_is_rejected_before_allocation() {
        let input = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1);
        let mut reader = BufReader::new(input.as_bytes());
        assert_eq!(
            read_message(&mut reader).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn header_line_limit_counts_newlines_and_checks_before_append() {
        for (size, accepted) in [(8192, true), (8193, false)] {
            let input = format!(
                "Content-Length: 2\r\nX:{}\r\n\r\n{{}}",
                "x".repeat(size - 4)
            );
            let mut reader = BufReader::with_capacity(1, input.as_bytes());
            let result = read_message(&mut reader).await;
            if accepted {
                assert_eq!(result.unwrap(), b"{}");
            } else {
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
            }
        }
    }

    fn header_block(total: usize) -> Vec<u8> {
        let mut input = String::from("Content-Length: 2\r\n");
        while input.len() < total - 2 {
            let line = (total - 2 - input.len()).min(8192);
            assert!(line >= 4);
            input.push_str(&format!("X:{}\r\n", "x".repeat(line - 4)));
        }
        input.push_str("\r\n{}");
        input.into_bytes()
    }

    #[tokio::test]
    async fn complete_header_block_has_its_own_limit_and_resets_per_message() {
        let mut valid = header_block(65536);
        valid.extend_from_slice(&encode("你好🌍".as_bytes()));
        let mut reader = BufReader::with_capacity(7, valid.as_slice());
        assert_eq!(read_message(&mut reader).await.unwrap(), b"{}");
        assert_eq!(
            read_message(&mut reader).await.unwrap(),
            "你好🌍".as_bytes()
        );
        let invalid = header_block(65537);
        let mut reader = BufReader::new(invalid.as_slice());
        assert_eq!(
            read_message(&mut reader).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn nonterminating_header_is_rejected_while_peer_stays_open() {
        use tokio::io::AsyncWriteExt;
        let (mut peer, stream) = tokio::io::duplex(16384);
        peer.write_all(&vec![b'x'; 8192]).await.unwrap();
        let mut reader = BufReader::new(stream);
        let error =
            tokio::time::timeout(std::time::Duration::from_secs(1), read_message(&mut reader))
                .await
                .expect("must reject without waiting for newline or EOF")
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn length_headers_are_case_insensitive_unique_and_decimal() {
        let mut reader = BufReader::new(
            &b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{}"[..],
        );
        assert_eq!(read_message(&mut reader).await.unwrap(), b"{}");
        for input in [
            "Content-Length: 2\r\ncontent-length: 2\r\n\r\n{}",
            "Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}x",
            "Content-Length: +2\r\n\r\n{}",
            "Content-Length: -1\r\n\r\n",
            "Content-Length: 1.5\r\n\r\n{}",
            "Content-Length: 99999999999999999999999999\r\n\r\n",
            "Content-Length: 16777217\r\n",
        ] {
            let mut reader = BufReader::new(input.as_bytes());
            assert_eq!(
                read_message(&mut reader).await.unwrap_err().kind(),
                std::io::ErrorKind::InvalidData,
                "{input:?}"
            );
        }
    }

    #[tokio::test]
    async fn incomplete_header_or_body_is_never_a_complete_message() {
        for input in [
            "Content-Length: 2",
            "Content-Length: 2\r\n",
            "Content-Length: 2\r\n\r\n{",
        ] {
            let mut reader = BufReader::new(input.as_bytes());
            assert_eq!(
                read_message(&mut reader).await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        }
    }
}
