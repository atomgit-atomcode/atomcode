use atomcode_capabilities::session::{DisplayAnchor, SessionManager};
use atomcode_daemon::legacy_convert::{converge_session, ImportDiagnostic, ImportStatus};

fn legacy_fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/session/legacy_full.json")).unwrap()
}

fn write_legacy(legacy: &serde_json::Value) -> (tempfile::TempDir, SessionManager, String) {
    let dir = tempfile::tempdir().unwrap();
    let manager = SessionManager::with_root(dir.path());
    let id = legacy["id"].as_str().unwrap().to_string();
    std::fs::write(
        manager.legacy_path(&id).unwrap(),
        serde_json::to_vec(legacy).unwrap(),
    )
    .unwrap();
    (dir, manager, id)
}

#[test]
fn damaged_legacy_turn_boundaries_are_repaired_during_cutover() {
    let mut legacy = legacy_fixture();
    let stats = legacy["turn_stats"].as_array_mut().unwrap();
    let valid = stats[0].clone();
    let mut zero = valid.clone();
    zero["after_message"] = 0.into();
    let mut duplicate = stats[1].clone();
    duplicate["after_message"] = valid["after_message"].clone();
    let mut decreasing = stats[1].clone();
    decreasing["after_message"] = 3.into();
    *stats = vec![zero, valid, duplicate, decreasing];

    let (_dir, manager, id) = write_legacy(&legacy);
    let lease = manager.acquire_lease(&id).unwrap();

    let outcome = converge_session(&manager, &lease).unwrap();

    assert_eq!(outcome.status, ImportStatus::ImportedFull);
    assert_eq!(
        outcome.diagnostic,
        Some(ImportDiagnostic::RepairedLegacyTurnBoundaries {
            dropped_turn_stats: 3,
        })
    );
    assert_eq!(outcome.meta.turn_count, 1);
    assert_eq!(outcome.meta.turn_stats.len(), 1);
    assert_eq!(outcome.meta.turn_stats[0].after_message, 4);
    assert_eq!(outcome.meta.turn_stats[0].turn_id, 1);
    assert_eq!(outcome.snapshot.turn_counter, 1);
    assert_eq!(
        outcome.presentation.entries[1].anchor,
        DisplayAnchor::AfterTurn { turn_id: 1 }
    );
    assert_eq!(manager.read_meta(&id).unwrap(), outcome.meta);
}

#[test]
fn presentation_anchors_at_start_when_all_legacy_boundaries_are_invalid() {
    let mut legacy = legacy_fixture();
    let stats = legacy["turn_stats"].as_array_mut().unwrap();
    stats.truncate(1);
    stats[0]["after_message"] = 0.into();

    let (_dir, manager, id) = write_legacy(&legacy);
    let lease = manager.acquire_lease(&id).unwrap();

    let outcome = converge_session(&manager, &lease).unwrap();

    assert_eq!(
        outcome.diagnostic,
        Some(ImportDiagnostic::RepairedLegacyTurnBoundaries {
            dropped_turn_stats: 1,
        })
    );
    assert_eq!(outcome.meta.turn_count, 0);
    assert!(outcome.meta.turn_stats.is_empty());
    assert_eq!(outcome.snapshot.turn_counter, 0);
    assert!(outcome
        .presentation
        .entries
        .iter()
        .all(|entry| entry.anchor == DisplayAnchor::AtStart));
}

#[test]
fn out_of_range_legacy_turn_boundary_is_repaired_during_cutover() {
    let mut legacy = legacy_fixture();
    let message_count = legacy["messages"].as_array().unwrap().len();
    legacy["turn_stats"][0]["after_message"] = (message_count + 1).into();

    let (_dir, manager, id) = write_legacy(&legacy);
    let lease = manager.acquire_lease(&id).unwrap();

    let outcome = converge_session(&manager, &lease).unwrap();

    assert_eq!(outcome.status, ImportStatus::ImportedFull);
    assert_eq!(
        outcome.diagnostic,
        Some(ImportDiagnostic::RepairedLegacyTurnBoundaries {
            dropped_turn_stats: 1,
        })
    );
    assert_eq!(outcome.meta, manager.read_meta(&id).unwrap());
}

#[test]
fn invalid_presentation_position_without_boundary_repair_remains_an_error() {
    let mut legacy = legacy_fixture();
    legacy["display_messages"][1]["after_message"] = 8.into();

    let (_dir, manager, id) = write_legacy(&legacy);
    let lease = manager.acquire_lease(&id).unwrap();

    let error = converge_session(&manager, &lease).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("legacy presentation position is outside the message history"),
        "{error:#}"
    );
    assert!(manager.read_meta(&id).is_err());
}

#[test]
fn invalid_presentation_position_with_boundary_repair_remains_an_error() {
    let mut legacy = legacy_fixture();
    legacy["turn_stats"][1]["after_message"] = 3.into();
    legacy["display_messages"][1]["after_message"] = 8.into();

    let (_dir, manager, id) = write_legacy(&legacy);
    let lease = manager.acquire_lease(&id).unwrap();

    let error = converge_session(&manager, &lease).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("legacy presentation position is outside the message history"),
        "{error:#}"
    );
    assert!(manager.read_meta(&id).is_err());
}
