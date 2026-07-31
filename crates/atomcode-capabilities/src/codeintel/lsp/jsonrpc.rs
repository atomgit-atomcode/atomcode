//! Minimal LSP JSON-RPC framing: `Content-Length` headers over a byte stream. Ported
//! from production `lsp/jsonrpc.rs`.

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

/// Frame a JSON body with its `Content-Length` header.
pub fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Read one framed message body: parse the header block (until the blank line), then
/// read exactly `Content-Length` bytes. Errors on EOF / missing header.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Vec<u8>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "LSP stream closed",
            ));
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok();
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
}
