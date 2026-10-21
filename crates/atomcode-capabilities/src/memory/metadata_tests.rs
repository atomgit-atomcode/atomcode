use super::context::{MemoryContext, CHAR_BUDGET};
use super::entry::{MemoryPatch, MemoryStatus};
use super::MemoryStore;
use std::fs;

#[test]
fn exact_legacy_update_preserves_surroundings_and_keeps_creation_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.md");
    let original = "# Notes\r\n\r\n- untouched\r\n- old fact\r\n";
    fs::write(&path, original).unwrap();
    let store = MemoryStore::new(path.clone());
    let old = store.entries().unwrap().remove(1);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        original,
        "reading must not migrate"
    );
    let updated = store
        .update(
            &old.metadata.id,
            &MemoryPatch {
                content: Some("corrected fact".into()),
                topic: Some("architecture".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(updated.metadata.id, old.metadata.id);
    assert!(updated.metadata.created_at.is_none());
    assert!(updated.metadata.updated_at.is_some());
    let raw = fs::read_to_string(path).unwrap();
    assert!(raw.starts_with("# Notes\r\n\r\n- untouched\r\n- corrected fact "));
    assert!(raw.ends_with(" -->\r\n"));
    assert_eq!(store.entries().unwrap()[1], updated);
}
#[test]
fn deprecated_and_outdated_entries_are_retained_and_can_be_reactivated_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(dir.path().join("memory.md"));
    let (entry, _) = store
        .remember("lasting fact", &MemoryPatch::default())
        .unwrap();
    for state in [MemoryStatus::Outdated, MemoryStatus::Deprecated] {
        let changed = store
            .update(
                &entry.metadata.id,
                &MemoryPatch {
                    status: Some(state),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(store.load().is_empty());
        assert_eq!(store.entries().unwrap()[0].content, "lasting fact");
        assert!(
            store.full_text().unwrap().starts_with("> - "),
            "old bullet-only readers must not inject inactive entries"
        );
        assert_eq!(changed.metadata.created_at, entry.metadata.created_at);
    }
    store
        .update(
            &entry.metadata.id,
            &MemoryPatch {
                status: Some(MemoryStatus::Active),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(store.load(), ["lasting fact"]);
    assert_eq!(store.entries().unwrap()[0].metadata.id, entry.metadata.id);
}
#[test]
fn keyword_forget_never_matches_topic_source_or_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(dir.path().join("memory.md"));
    let (entry, _) = store
        .remember(
            "keep this",
            &MemoryPatch {
                topic: Some("delete".into()),
                source: Some("delete-source".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let before = store.full_text().unwrap();
    assert!(store.remove_matching("delete").unwrap().is_empty());
    assert!(store
        .remove_matching(&entry.metadata.id)
        .unwrap()
        .is_empty());
    assert_eq!(store.full_text().unwrap(), before);
    assert_eq!(store.remove_matching("keep").unwrap(), ["keep this"]);
}
#[test]
fn invalid_metadata_unknown_ids_and_ambiguous_legacy_ids_do_not_rewrite_source() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(dir.path().join("memory.md"));
    let patch = MemoryPatch {
        pinned: Some(true),
        ..Default::default()
    };
    let duplicate = "- same fact\n- same fact\n";
    fs::write(store.path(), duplicate).unwrap();
    let id = store.entries().unwrap()[0].metadata.id.clone();
    assert!(store
        .update(&id, &patch)
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
    assert_eq!(store.full_text().unwrap(), duplicate);
    assert!(store.update("missing", &patch).is_err());
    let future = "- fact <!-- atomcode-memory:v9 {} -->\n";
    fs::write(store.path(), future).unwrap();
    assert!(store.remember("new fact", &Default::default()).is_err());
    assert!(store.remove_matching("fact").is_err());
    assert_eq!(store.full_text().unwrap(), future);
    fs::write(store.path(), [0xff, 0xfe]).unwrap();
    assert!(store.append("no overwrite").is_err());
    assert_eq!(fs::read(store.path()).unwrap(), [0xff, 0xfe]);
}
#[test]
fn concurrent_managed_writers_do_not_lose_entries_or_duplicate_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.md");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
    let threads: Vec<_> = (0..12)
        .map(|n| {
            let barrier = barrier.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                barrier.wait();
                MemoryStore::new(path)
                    .append(&format!("unique entry {n}"))
                    .unwrap();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let entries = MemoryStore::new(path).entries().unwrap();
    assert_eq!(entries.len(), 12);
    let ids: std::collections::HashSet<_> = entries.iter().map(|e| &e.metadata.id).collect();
    assert_eq!(ids.len(), 12);
}
#[test]
fn pinned_global_entry_survives_local_pressure_and_metadata_never_enters_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let g = MemoryStore::new(dir.path().join("g.md"));
    let p = MemoryStore::new(dir.path().join("p.md"));
    let l = MemoryStore::new(dir.path().join("l.md"));
    let (pinned, _) = g
        .remember(
            "PINNED_GLOBAL",
            &MemoryPatch {
                pinned: Some(true),
                source: Some("provenance-only".into()),
                ..Default::default()
            },
        )
        .unwrap();
    for n in 0..40 {
        l.append(&format!("local {n} {}", "x".repeat(100))).unwrap();
    }
    let inactive = p.remember("NOT_ACTIVE", &Default::default()).unwrap().0;
    p.update(
        &inactive.metadata.id,
        &MemoryPatch {
            status: Some(MemoryStatus::Deprecated),
            ..Default::default()
        },
    )
    .unwrap();
    let context = MemoryContext::load(&g, &p, &l, "project", &dir.path().join("missing-index"));
    assert!(context.prompt.contains("PINNED_GLOBAL"));
    assert!(!context.prompt.contains("NOT_ACTIVE"));
    assert!(!context.prompt.contains("atomcode-memory"));
    assert!(!context.prompt.contains(&pinned.metadata.id));
    assert!(!context.prompt.contains("provenance-only"));
    assert!(context.prompt.chars().count() <= CHAR_BUDGET);
}
#[test]
fn pinned_entries_before_the_ordinary_tail_window_are_still_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let g = MemoryStore::new(dir.path().join("g.md"));
    let empty = MemoryStore::new(dir.path().join("empty.md"));
    g.remember(
        "EARLY_PIN",
        &MemoryPatch {
            pinned: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(g.path()).unwrap();
    file.write_all("- ordinary entry\n".repeat(6000).as_bytes())
        .unwrap();
    drop(file);
    let context = MemoryContext::load(&g, &empty, &empty, "p", &dir.path().join("no-index"));
    assert!(context.scopes[0].limited);
    assert!(context.prompt.contains("EARLY_PIN"));
}
#[cfg(unix)]
#[test]
fn managed_replacement_preserves_permissions_and_symlink_targets() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real.md");
    fs::write(&target, "- old\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    let link = dir.path().join("memory.md");
    symlink(&target, &link).unwrap();
    MemoryStore::new(link.clone()).append("new").unwrap();
    assert!(link.is_symlink());
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(MemoryStore::new(target).load(), ["old", "new"]);
}

#[test]
fn an_external_edit_detected_before_commit_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.md");
    fs::write(&path, "- original\n").unwrap();
    let result = super::document::edit(&path, |document| {
        fs::write(&path, "- external edit\n")?;
        document.push(super::entry::MemoryEntry::new(
            "managed append",
            &Default::default(),
        )?)
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(fs::read_to_string(path).unwrap(), "- external edit\n");
}

#[test]
fn an_oversized_managed_line_is_rejected_before_it_can_poison_selection() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(dir.path().join("memory.md"));
    store.append("KEEP").unwrap();
    let before = store.full_text().unwrap();
    assert!(store.append(&"x".repeat(64 * 1024)).is_err());
    assert_eq!(store.full_text().unwrap(), before);
}

#[cfg(unix)]
#[test]
fn directory_open_failure_after_replacement_reports_uncertain_durability() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(dir.path().join("memory.md"));
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o300)).unwrap();
    let readable = fs::File::open(dir.path()).is_ok();
    let result = store.append("COMMITTED");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    if readable {
        result.unwrap(); // Privileged processes may bypass the directory mode.
    } else {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("uncertain durability"));
    }
    assert_eq!(store.load(), ["COMMITTED"]);
}

#[test]
fn expired_pins_are_retained_but_do_not_consume_budget_and_can_be_renewed() {
    let dir = tempfile::tempdir().unwrap();
    let global = MemoryStore::new(dir.path().join("g.md"));
    let project = MemoryStore::new(dir.path().join("p.md"));
    let local = MemoryStore::new(dir.path().join("l.md"));
    let (entry, _) = project
        .remember(
            "EXPIRED_PIN_FACT",
            &MemoryPatch {
                pinned: Some(true),
                expires_at: Some(Some("2000-01-01T00:00:00Z".into())),
                ..Default::default()
            },
        )
        .unwrap();
    let original = project.full_text().unwrap();
    let preview = MemoryContext::load(
        &global,
        &project,
        &local,
        "test",
        &dir.path().join("INDEX.md"),
    );
    assert!(!preview.prompt.contains("EXPIRED_PIN_FACT"));
    assert!(preview.warning.is_none(), "expiry is not budget omission");
    assert!(preview.scopes[1].entries[0].injected.is_none());
    assert!(project.load().is_empty());
    assert_eq!(
        project.full_text().unwrap(),
        original,
        "reading never cleans up"
    );
    assert_eq!(project.entries().unwrap().len(), 1);
    let changed = project
        .update(
            &entry.metadata.id,
            &MemoryPatch {
                expires_at: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(changed.metadata.id, entry.metadata.id);
    assert!(MemoryContext::load(
        &global,
        &project,
        &local,
        "test",
        &dir.path().join("INDEX.md")
    )
    .prompt
    .contains("EXPIRED_PIN_FACT"));
    assert!(
        project
            .remember(
                "EXPIRED_PIN_FACT",
                &MemoryPatch {
                    expires_at: Some(Some("2000-01-01T00:00:00Z".into())),
                    ..Default::default()
                }
            )
            .is_err(),
        "dedup must not silently ignore a conflicting expiry"
    );
}

#[test]
fn malformed_expiry_is_not_treated_as_permanent_and_cannot_be_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(dir.path().join("memory.md"));
    let (entry, _) = store
        .remember(
            "fact",
            &MemoryPatch {
                expires_at: Some(Some("2026-10-21T00:00:00Z".into())),
                ..Default::default()
            },
        )
        .unwrap();
    let broken = store
        .full_text()
        .unwrap()
        .replace("2026-10-21T00:00:00Z", "not-a-timestamp");
    fs::write(store.path(), &broken).unwrap();
    assert!(store.entries().is_err());
    assert!(store
        .update(
            &entry.metadata.id,
            &MemoryPatch {
                expires_at: Some(None),
                ..Default::default()
            }
        )
        .is_err());
    assert_eq!(store.full_text().unwrap(), broken);
}
