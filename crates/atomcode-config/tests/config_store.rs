use std::sync::{Arc, Barrier};

use atomcode_config::{Config, ConfigStore, ProviderConfig};

fn provider(model: &str) -> ProviderConfig {
    ProviderConfig {
        provider_type: "openai".into(),
        api_key: None,
        model: model.into(),
        base_url: Some("https://example.test/v1".into()),
        system_prompt: None,
        user_agent: None,
        context_window: 128_000,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        reasoning_effort: None,
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: false,
        capable_model: None,
    }
}

fn seeded_config() -> Config {
    let mut config = Config::with_default_provider("a");
    config.providers.insert("a".into(), provider("model-a"));
    config.providers.insert("b".into(), provider("model-b"));
    config
}

#[test]
fn transaction_returns_revision_for_the_exact_persisted_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    seeded_config().save(&path).unwrap();
    let store = ConfigStore::new(&path);

    let before = store.read().unwrap();
    let commit = store
        .update(|config| {
            config.default_provider = "b".into();
            Ok(())
        })
        .unwrap();

    assert_ne!(before.revision, commit.snapshot.revision);
    assert_eq!(commit.snapshot.config.default_provider, "b");
    let reread = store.read().unwrap();
    assert_eq!(reread.revision, commit.snapshot.revision);
    assert_eq!(reread.config.default_provider, "b");
}

#[test]
fn concurrent_incremental_updates_do_not_overwrite_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    seeded_config().save(&path).unwrap();
    let store = Arc::new(ConfigStore::new(&path));
    let barrier = Arc::new(Barrier::new(3));

    let first = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            store
                .update(|config| {
                    config.default_provider = "b".into();
                    Ok(())
                })
                .unwrap();
        })
    };
    let second = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            store
                .update(|config| {
                    config.providers.insert("c".into(), provider("model-c"));
                    Ok(())
                })
                .unwrap();
        })
    };

    barrier.wait();
    first.join().unwrap();
    second.join().unwrap();

    let final_snapshot = store.read().unwrap();
    assert_eq!(final_snapshot.config.default_provider, "b");
    assert_eq!(final_snapshot.config.providers["c"].model, "model-c");
}

#[test]
fn conditional_rollback_does_not_overwrite_a_newer_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    seeded_config().save(&path).unwrap();
    let store = ConfigStore::new(&path);

    let switch = store
        .update(|config| {
            config.default_provider = "b".into();
            Ok(())
        })
        .unwrap();
    store
        .update(|config| {
            config.default_provider = "a".into();
            config.providers.insert("c".into(), provider("model-c"));
            Ok(())
        })
        .unwrap();

    let rolled_back = store
        .update_if_revision(&switch.snapshot.revision, |config| {
            config.default_provider = "a".into();
            Ok(())
        })
        .unwrap();

    assert!(rolled_back.is_none());
    let final_snapshot = store.read().unwrap();
    assert_eq!(final_snapshot.config.default_provider, "a");
    assert_eq!(final_snapshot.config.providers["c"].model, "model-c");
}

#[test]
fn failed_transaction_leaves_the_previous_snapshot_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    seeded_config().save(&path).unwrap();
    let store = ConfigStore::new(&path);
    let before = store.read().unwrap();

    let error = store
        .update(|config| {
            config.default_provider = "b".into();
            anyhow::bail!("reject mutation")
        })
        .unwrap_err();

    assert!(error.to_string().contains("reject mutation"));
    let after = store.read().unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.config.default_provider, "a");
}

#[test]
fn incremental_update_never_overwrites_a_corrupt_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let corrupt = "default_provider = [not valid toml";
    std::fs::write(&path, corrupt).unwrap();
    let store = ConfigStore::new(&path);

    let error = store
        .update(|config| {
            config.default_provider = "replacement".into();
            Ok(())
        })
        .unwrap_err();

    assert!(error.to_string().contains("Failed to parse config"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
}

#[test]
fn readers_never_observe_a_partially_written_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    seeded_config().save(&path).unwrap();
    let store = Arc::new(ConfigStore::new(&path));

    let writer = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            for i in 0..100 {
                store
                    .update(|config| {
                        config.providers.get_mut("a").unwrap().system_prompt =
                            Some(format!("revision-{i}-{}", "x".repeat(64 * 1024)));
                        Ok(())
                    })
                    .unwrap();
            }
        })
    };

    while !writer.is_finished() {
        Config::load(&path).expect("atomic replacement must expose a complete TOML snapshot");
    }
    writer.join().unwrap();
}
