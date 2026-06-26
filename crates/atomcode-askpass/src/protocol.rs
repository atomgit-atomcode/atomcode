use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, BufRead, Write};

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    pub prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub password: Option<String>,
}

/// One JSON object per line. Newline-delimited so the reader knows where a
/// frame ends without a length prefix.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let mut s = serde_json::to_vec(v)?;
    s.push(b'\n');
    w.write_all(&s)?;
    w.flush()
}

pub fn read_frame<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no frame"));
    }
    serde_json::from_str(line.trim_end()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_roundtrips_request_and_response() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Request { token: "t".into(), prompt: "[sudo] password:".into() }).unwrap();
        let mut rdr = std::io::BufReader::new(&buf[..]);
        let got: Request = read_frame(&mut rdr).unwrap();
        assert_eq!(got.token, "t");
        assert_eq!(got.prompt, "[sudo] password:");

        let mut b2: Vec<u8> = Vec::new();
        write_frame(&mut b2, &Response { password: Some("pw".into()) }).unwrap();
        let mut r2 = std::io::BufReader::new(&b2[..]);
        let resp: Response = read_frame(&mut r2).unwrap();
        assert_eq!(resp.password.as_deref(), Some("pw"));
    }
}
