//! Explicitly bound, read-only IDE transport. Connection credentials never leave this module.
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_CONNECTION: u64 = 64 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connection {
    protocol_version: u32,
    window_id: String,
    endpoint: String,
    token: String,
    workspace_roots: Vec<PathBuf>,
    #[serde(rename = "ide")]
    _ide: String,
}

fn load(path: &Path) -> Result<Connection, &'static str> {
    if !path.is_absolute() {
        return Err("IDE connection file must be an absolute path.");
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        "IDE connection file is unavailable. Enable the bridge in the selected IDE window."
    })?;
    if !metadata.is_file() || metadata.len() > MAX_CONNECTION {
        return Err("Invalid IDE connection file.");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err("IDE connection file must be private (mode 0600).");
        }
    }
    let file = std::fs::File::open(path).map_err(|_| "Cannot open IDE connection file.")?;
    let opened = file
        .metadata()
        .map_err(|_| "Cannot inspect IDE connection file.")?;
    if !opened.is_file() {
        return Err("Invalid IDE connection file.");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.ino() != opened.ino()
            || metadata.dev() != opened.dev()
            || opened.mode() & 0o077 != 0
        {
            return Err("IDE connection file changed while opening it.");
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONNECTION + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read IDE connection file.")?;
    if bytes.len() as u64 > MAX_CONNECTION {
        return Err("IDE connection file exceeds its size limit.");
    }
    let connection: Connection =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid IDE connection file format.")?;
    if connection.protocol_version != 1 {
        return Err("Unsupported IDE bridge protocol version.");
    }
    if connection.window_id.is_empty()
        || connection.window_id.len() > 128
        || connection.token.len() != 64
        || !connection
            .token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || connection.workspace_roots.is_empty()
        || connection.workspace_roots.len() > 64
    {
        return Err("Invalid IDE bridge binding.");
    }
    let endpoint =
        url::Url::parse(&connection.endpoint).map_err(|_| "Invalid IDE bridge endpoint.")?;
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.port().is_none()
        || endpoint.port() == Some(0)
        || endpoint.path() != "/query"
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err("IDE bridge endpoint must be a private IPv4 loopback query endpoint.");
    }
    Ok(connection)
}

fn failed(status: &str, message: &str) -> Value {
    json!({"source":"ide", "status":status, "message":message,
        "fallback":"Do not retry blindly. Select another backend explicitly; text search is not semantic references."})
}

pub(super) async fn query(
    binding: &Path,
    root: &Path,
    requested: &Path,
    args: Value,
    timeout_secs: u64,
    cancel: &CancellationToken,
) -> Value {
    tokio::select! {
        _ = cancel.cancelled() => failed("cancelled", "IDE query cancelled."),
        result = tokio::time::timeout(Duration::from_secs(timeout_secs), query_inner(binding, root, requested, args, timeout_secs, cancel)) =>
            result.unwrap_or_else(|_| failed("timeout", "IDE query timed out; the provider may still be indexing.")),
    }
}

async fn query_inner(
    binding: &Path,
    root: &Path,
    requested: &Path,
    mut args: Value,
    timeout_secs: u64,
    cancel: &CancellationToken,
) -> Value {
    if cancel.is_cancelled() {
        return failed("cancelled", "IDE query cancelled.");
    }
    let binding = binding.to_owned();
    let connection = match tokio::task::spawn_blocking(move || load(&binding)).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(message)) => return failed("unavailable", message),
        Err(_) => return failed("unavailable", "Cannot load IDE connection binding."),
    };
    let root = match tokio::fs::canonicalize(root).await {
        Ok(root) => root,
        Err(_) => return failed("wrong_workspace", "Working directory cannot be resolved."),
    };
    let file = match tokio::fs::canonicalize(requested).await {
        Ok(file) if file.starts_with(&root) => file,
        _ => {
            return failed(
                "wrong_workspace",
                "File must exist inside the current workspace.",
            )
        }
    };
    let mut matches = false;
    for workspace in &connection.workspace_roots {
        if workspace.is_absolute() {
            if let Ok(workspace) = tokio::fs::canonicalize(workspace).await {
                matches |= root.starts_with(workspace);
            }
        }
    }
    if !matches {
        return failed(
            "wrong_workspace",
            "This IDE window does not contain the current workspace.",
        );
    }
    args["file_path"] = json!(crate::pathnorm::strip_verbatim_path(&file));
    args["workspace_root"] = json!(crate::pathnorm::strip_verbatim_path(&root));
    args["timeout_secs"] = json!(timeout_secs);
    let client = match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(3))
        .build()
    {
        Ok(client) => client,
        Err(_) => return failed("unavailable", "Cannot initialize the IDE bridge client."),
    };
    let request = async {
        let mut response = client
            .post(&connection.endpoint)
            .bearer_auth(&connection.token)
            .json(&args)
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(failed(
                "unavailable",
                "The selected IDE bridge rejected the query.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE as u64)
        {
            return Ok(failed(
                "response_too_large",
                "IDE response exceeds its limit.",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Ok(failed(
                    "response_too_large",
                    "IDE response exceeds its limit.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        // Defensive redaction also covers a misbehaving local provider echoing the secret.
        let text = String::from_utf8_lossy(&bytes).replace(&connection.token, "[redacted]");
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                return Ok(failed(
                    "invalid_response",
                    "IDE bridge returned invalid JSON.",
                ))
            }
        };
        if value["protocol_version"] != 1
            || value["source"] != "ide"
            || value["window_id"] != connection.window_id
            || !value["status"].is_string()
        {
            return Ok(failed(
                "invalid_response",
                "IDE bridge response does not match the selected window or protocol.",
            ));
        }
        if value["status"] == "ok" {
            let expected = if args["operation"] == "inspect" {
                vec!["hover", "definition", "references"]
            } else {
                vec![args["operation"].as_str().unwrap_or_default()]
            };
            let valid_document = value["document"]["uri"].is_string()
                && value["document"]["language_id"].is_string()
                && value["document"]["version"].is_u64()
                && value["document"]["dirty"].is_boolean()
                && value["document"]["position_encoding"] == "utf-16";
            let valid_results = value["results"].as_array().is_some_and(|results| {
                results.len() == expected.len()
                    && results.iter().zip(&expected).all(|(result, operation)| {
                        result["operation"] == *operation
                            && matches!(
                                result["status"].as_str(),
                                Some("ok" | "empty" | "provider_error")
                            )
                            && result["items"]
                                .as_array()
                                .is_some_and(|items| items.len() <= 200)
                    })
            });
            if !valid_document || !valid_results {
                return Ok(failed(
                    "invalid_response",
                    "IDE bridge returned an incomplete or mismatched semantic result.",
                ));
            }
        }
        Ok::<_, reqwest::Error>(value)
    };
    tokio::select! {
        _ = cancel.cancelled() => failed("cancelled", "IDE query cancelled."),
        result = request => match result {
            Ok(value) => value,
            Err(error) if error.is_timeout() => failed("timeout", "IDE query timed out; the provider may still be indexing."),
            Err(_) => failed("unavailable", "Cannot reach the selected IDE window. Enable its bridge and copy a fresh binding."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn binding(root: &Path, endpoint: &str) -> PathBuf {
        let file = root.join("connection.json");
        std::fs::write(
            &file,
            json!({"protocol_version":1,"window_id":"selected-window","endpoint":endpoint,
            "token":"a".repeat(64),"workspace_roots":[root],"ide":"fixture IDE"})
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        file
    }

    #[test]
    fn binding_rejects_remote_redirectable_or_credential_bearing_endpoints() {
        let root = tempfile::tempdir().unwrap();
        for endpoint in [
            "http://example.com:123/query",
            "http://localhost:123/query",
            "http://127.0.0.1:123/other",
            "http://user:password@127.0.0.1:123/query",
            "http://127.0.0.1:123/query?token=secret",
            "https://127.0.0.1:123/query",
        ] {
            let file = binding(root.path(), endpoint);
            assert!(load(&file).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn invalid_connection_contents_are_not_echoed() {
        let root = tempfile::tempdir().unwrap();
        let file = binding(root.path(), "http://127.0.0.1:123/query");
        std::fs::write(&file, "private-credential-invalid-json").unwrap();
        let Err(error) = load(&file) else {
            panic!("invalid binding accepted")
        };
        assert!(!error.contains("private-credential"));
    }

    #[cfg(unix)]
    #[test]
    fn connection_rejects_public_files_and_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let file = binding(root.path(), "http://127.0.0.1:123/query");
        let alias = root.path().join("alias.json");
        symlink(&file, &alias).unwrap();
        assert!(load(&alias).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&file).is_err());
    }

    // Actual loopback HTTP exercises auth and the wire contract, not a clock-only mock.
    #[tokio::test(flavor = "current_thread")]
    async fn native_client_sends_bound_workspace_and_redacts_echoed_credentials() {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let source = canonical.join("main.cj");
        std::fs::write(&source, "not sent to the IDE").unwrap();
        let file = binding(&canonical, &format!("{}/query", server.uri()));
        Mock::given(method("POST")).and(path("/query")).and(header("authorization", format!("Bearer {}", "a".repeat(64))))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"protocol_version":1, "source":"ide", "window_id":"selected-window", "status":"ok", "message":"a".repeat(64),
                "document":{"uri":"file:///main.cj","language_id":"cangjie","version":1,"dirty":true,"position_encoding":"utf-16"},
                "results":[{"operation":"hover","status":"ok","items":[]},{"operation":"definition","status":"empty","items":[]},{"operation":"references","status":"empty","items":[]}]
})))
            .expect(1).mount(&server).await;
        let value = query(
            &file,
            &canonical,
            &source,
            json!({"operation":"inspect", "line":1,"symbol":"文档"}),
            20,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(value["status"], "ok");
        assert_eq!(value["message"], "[redacted]");
        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["workspace_root"], canonical.to_str().unwrap());
        assert_eq!(body["file_path"], source.to_str().unwrap());
        assert!(!body.to_string().contains("not sent to the IDE"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_drops_an_inflight_http_request() {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a.cj");
        std::fs::write(&source, "content").unwrap();
        let file = binding(root.path(), &format!("{}/query", server.uri()));
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal = entered.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                signal.notify_one();
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
            })
            .mount(&server)
            .await;
        let cancel = CancellationToken::new();
        let pending = query(&file, root.path(), &source, json!({}), 20, &cancel);
        let (result, ()) = tokio::join!(pending, async {
            entered.notified().await;
            cancel.cancel();
        });
        assert_eq!(result["status"], "cancelled");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn incomplete_success_is_not_reported_as_semantic_success() {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a.cj");
        std::fs::write(&source, "content").unwrap();
        let file = binding(root.path(), &format!("{}/query", server.uri()));
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "protocol_version":1,"source":"ide","window_id":"selected-window","status":"ok"
            })))
            .mount(&server)
            .await;
        let result = query(
            &file,
            root.path(),
            &source,
            json!({"operation":"hover"}),
            20,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result["status"], "invalid_response");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_response_never_reaches_the_model() {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a.cj");
        std::fs::write(&source, "content").unwrap();
        let file = binding(root.path(), &format!("{}/query", server.uri()));
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(MAX_RESPONSE + 1)))
            .mount(&server)
            .await;
        let result = query(
            &file,
            root.path(),
            &source,
            json!({}),
            20,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result["status"], "response_too_large");
        assert!(result.to_string().len() < 500);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrong_workspace_is_rejected_without_contacting_the_ide() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = binding(root.path(), "http://127.0.0.1:1/query");
        let source = other.path().join("a.cj");
        std::fs::write(&source, "content").unwrap();
        let result = query(
            &file,
            other.path(),
            &source,
            json!({}),
            20,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result["status"], "wrong_workspace");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancelled_query_never_opens_the_connection_file() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = query(
            Path::new("/missing/private"),
            Path::new("/missing"),
            Path::new("/missing/file"),
            json!({}),
            20,
            &cancel,
        )
        .await;
        assert_eq!(result["status"], "cancelled");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mismatched_window_and_redirects_are_rejected() {
        for redirect in [false, true] {
            let server = MockServer::start().await;
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("a.cj");
            std::fs::write(&source, "content").unwrap();
            let file = binding(root.path(), &format!("{}/query", server.uri()));
            let response = if redirect {
                ResponseTemplate::new(302).insert_header("location", "http://example.com/query")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"protocol_version":1,"source":"ide","window_id":"another-window","status":"ok"}))
            };
            Mock::given(method("POST"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let result = query(
                &file,
                root.path(),
                &source,
                json!({}),
                20,
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(
                result["status"],
                if redirect {
                    "unavailable"
                } else {
                    "invalid_response"
                }
            );
        }
    }
}
