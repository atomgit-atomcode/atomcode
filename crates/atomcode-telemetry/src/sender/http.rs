//! HTTP POST of gzipped NDJSON segment files.

use flate2::write::GzEncoder;
use flate2::Compression;
use reqwest::{Client, StatusCode};
use std::env;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

fn apply_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if env::var("ATOMCODE_PROXY_MODE").ok().as_deref() == Some("no_proxy") {
        builder.no_proxy()
    } else {
        builder
    }
}

#[derive(Debug, Error)]
pub enum SendError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("bad request")]
    BadRequest,
    #[error("unauthorized")]
    Unauthorized,
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("rate limited (retry after {0:?})")]
    RateLimited(Option<Duration>),
    #[error("server error: {0}")]
    Server(u16),
    #[error("network / other")]
    Other,
}

pub struct HttpSender {
    client: Client,
    endpoint: String,
}

impl HttpSender {
    pub fn new(endpoint: String, app_version: String) -> Self {
        let client = apply_proxy_policy(Client::builder())
            .timeout(Duration::from_secs(10))
            .user_agent(format!("atomcode-telemetry/{}", app_version))
            .build()
            .expect("reqwest client build");
        Self { client, endpoint }
    }

    /// Send one segment file. `dropped` is cumulative since process start.
    pub async fn send_segment(&self, path: &Path, dropped: u64) -> Result<(), SendError> {
        let body = std::fs::read(path)?;
        let gz = gzip(&body)?;
        let resp = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/x-ndjson")
            .header("content-encoding", "gzip")
            .header("x-atomcode-dropped", dropped.to_string())
            .header("x-atomcode-schema", crate::SCHEMA_VERSION.to_string())
            .body(gz)
            .send()
            .await?;
        map_status(resp.status(), resp.headers())
    }
}

fn gzip(buf: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::with_capacity(buf.len() / 2), Compression::fast());
    enc.write_all(buf)?;
    enc.finish()
}

fn map_status(s: StatusCode, h: &reqwest::header::HeaderMap) -> Result<(), SendError> {
    match s.as_u16() {
        200 | 202 => Ok(()),
        400 => Err(SendError::BadRequest),
        401 | 403 => Err(SendError::Unauthorized),
        413 => Err(SendError::PayloadTooLarge),
        429 => {
            let ra = h
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            Err(SendError::RateLimited(ra))
        }
        c if (500..600).contains(&c) => Err(SendError::Server(c)),
        _ => Err(SendError::Other),
    }
}
