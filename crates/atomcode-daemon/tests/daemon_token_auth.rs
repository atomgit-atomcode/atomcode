use std::time::Duration;

#[tokio::test]
async fn chat_requires_token_health_is_public() {
    let tmp = std::env::temp_dir().join(format!("atomcode_it_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("ATOMCODE_HOME", &tmp);

    let port = 18099u16;
    let tmp_for_spawn = tmp.clone();
    let handle = tokio::spawn(async move {
        atomcode_daemon::run_server(atomcode_daemon::ServerOpts {
            host: "127.0.0.1".into(),
            port,
            cli_override: atomcode_telemetry::CliOverride { disabled: true },
            idle_timeout_secs: 0,
            startup_mode: atomcode_telemetry::SessionMode::Ide,
            webui_tokens: {
                let store = atomcode_daemon::auth_token::WebuiTokenStore::new();
                store.insert("it-token".to_string());
                Some(store)
            },
            working_dir_override: Some(tmp_for_spawn),
            quiet: true,
            prebound_listener: None,
            owner_shutdown: None,
            app_user_id: None,
            daemon_token_file: Some("it-token".to_string()),
        })
        .await
        .ok();
    });

    // wait for bind
    tokio::time::sleep(Duration::from_millis(800)).await;
    let base = format!("http://127.0.0.1:{port}");

    let health = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(health.status(), 200, "/health must be public");

    // GET /models without token must 401
    let no_tok = reqwest::Client::new()
        .get(format!("{base}/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        no_tok.status(),
        401,
        "protected route without token must 401"
    );

    let with_tok = reqwest::Client::new()
        .get(format!("{base}/models"))
        .header("Authorization", "Bearer it-token")
        .send()
        .await
        .unwrap();
    assert_ne!(with_tok.status(), 401, "valid token must not 401");

    // token file written with 0600
    let tf = tmp.join(format!("daemon-{port}.json"));
    assert!(tf.exists(), "daemon token file must exist");

    handle.abort();
    std::env::remove_var("ATOMCODE_HOME");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The webui URL carries the auth token, so the browser must not be told to
/// send it anywhere else.
///
/// A `Referer` header carries the full URL, token included, on every
/// sub-resource request the page makes — so without this the token travels to
/// whatever origin the page references.
#[tokio::test]
async fn every_response_says_not_to_send_the_url_anywhere_else() {
    let tmp = std::env::temp_dir().join(format!("atomcode_it_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("ATOMCODE_HOME", &tmp);

    let port = 18098u16;
    let tmp_for_spawn = tmp.clone();
    let handle = tokio::spawn(async move {
        atomcode_daemon::run_server(atomcode_daemon::ServerOpts {
            host: "127.0.0.1".into(),
            port,
            cli_override: atomcode_telemetry::CliOverride { disabled: true },
            idle_timeout_secs: 0,
            startup_mode: atomcode_telemetry::SessionMode::Ide,
            webui_tokens: {
                let store = atomcode_daemon::auth_token::WebuiTokenStore::new();
                store.insert("it-token".to_string());
                Some(store)
            },
            working_dir_override: Some(tmp_for_spawn),
            quiet: true,
            prebound_listener: None,
            owner_shutdown: None,
            app_user_id: None,
            daemon_token_file: Some("it-token".to_string()),
        })
        .await
        .ok();
    });

    tokio::time::sleep(Duration::from_millis(800)).await;
    let base = format!("http://127.0.0.1:{port}");

    for path in ["/health", "/", "/models"] {
        let response = reqwest::get(format!("{base}{path}")).await.unwrap();
        let policy = response
            .headers()
            .get("referrer-policy")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        assert_eq!(
            policy.as_deref(),
            Some("no-referrer"),
            "{path} must not tell the browser to send its URL elsewhere"
        );
    }

    handle.abort();
    std::env::remove_var("ATOMCODE_HOME");
    let _ = std::fs::remove_dir_all(&tmp);
}
