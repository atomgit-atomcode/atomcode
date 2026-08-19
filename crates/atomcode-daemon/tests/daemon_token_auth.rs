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
