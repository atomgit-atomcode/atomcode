use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

pub fn encode(body: &[u8]) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut msg = header.into_bytes();
    msg.extend_from_slice(body);
    msg
}

pub async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> anyhow::Result<Value> {
    let mut content_length: usize = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("EOF while reading LSP header");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(len_str) = trimmed.strip_prefix("Content-Length:") {
            content_length = len_str.trim().parse()?;
        }
    }
    if content_length == 0 {
        anyhow::bail!("No Content-Length in LSP message");
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_format() {
        let body = b"{\"jsonrpc\":\"2.0\"}";
        let encoded = encode(body);
        let expected = format!(
            "Content-Length: {}\r\n\r\n{}",
            body.len(),
            "{\"jsonrpc\":\"2.0\"}"
        );
        assert_eq!(encoded, expected.as_bytes());
    }

    #[tokio::test]
    async fn read_message_parses() {
        let body = b"{\"jsonrpc\":\"2.0\",\"id\":1}";
        let raw = format!(
            "Content-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let cursor = std::io::Cursor::new(raw.into_bytes());
        let mut reader = BufReader::new(cursor);
        let msg = read_message(&mut reader).await.unwrap();
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["id"], 1);
    }

    #[tokio::test]
    async fn read_message_eof_error() {
        let cursor = std::io::Cursor::new(Vec::<u8>::new());
        let mut reader = BufReader::new(cursor);
        let result = read_message(&mut reader).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("EOF"));
    }
}
