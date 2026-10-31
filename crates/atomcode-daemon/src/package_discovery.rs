//! Discover runtime-package bundles on the local device.
//!
//! Discovery is read-only: it opens each `.tar.gz` just far enough to validate
//! the root `package.json`, and never unpacks or loads the package. Results are
//! streamed because a full shared-storage scan can take a while on a phone.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::packages::{validate_manifest, Manifest};

const MAX_RESULTS: usize = 200;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// How many archive entries to walk while looking for `package.json`.
///
/// A real bundle puts the manifest first (our own packer does), so this bound is
/// never reached in practice. It exists so a corrupt — or hostile, high-ratio —
/// `.tar.gz` sitting in shared storage cannot turn a "scan" into an unbounded
/// decompression: reaching the bound just means "not a package we can read".
const MAX_ENTRIES_INSPECTED: usize = 4096;
const PROGRESS_INTERVAL: u64 = 32;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocalPackage {
    pub path: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscoveryEvent {
    Progress {
        directories: u64,
        archives: u64,
        skipped: u64,
    },
    Found {
        package: LocalPackage,
    },
    Done {
        directories: u64,
        archives: u64,
        skipped: u64,
        truncated: bool,
    },
    Error {
        message: String,
    },
}

/// GET /packages/discover/stream — scan the device's storage and stream each
/// valid bundle as it is found. Closing the response cancels the directory walk.
pub(crate) async fn discover_packages_stream() -> Response {
    use futures::StreamExt;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<DiscoveryEvent>();
    tokio::task::spawn_blocking(move || {
        discover(&discovery_root(), &mut |event| tx.send(event).is_ok());
    });
    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|event| {
        let json = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().data(json))
    });
    axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

#[cfg(target_os = "android")]
fn discovery_root() -> PathBuf {
    PathBuf::from("/sdcard")
}

#[cfg(not(target_os = "android"))]
fn discovery_root() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn discover(root: &Path, emit: &mut dyn FnMut(DiscoveryEvent) -> bool) {
    discover_with_limit(root, MAX_RESULTS, emit);
}

fn discover_with_limit(root: &Path, limit: usize, emit: &mut dyn FnMut(DiscoveryEvent) -> bool) {
    if !root.is_dir() {
        let _ = emit(DiscoveryEvent::Error {
            message: format!("discovery root is not a directory: {}", root.display()),
        });
        return;
    }

    let mut stack = vec![root.to_path_buf()];
    let mut directories = 0u64;
    let mut archives = 0u64;
    let mut skipped = 0u64;
    let mut found = 0usize;
    let mut truncated = false;
    let mut cancelled = false;

    'scan: while let Some(dir) = stack.pop() {
        directories += 1;
        if directories % PROGRESS_INTERVAL == 1
            && !emit(DiscoveryEvent::Progress {
                directories,
                archives,
                skipped,
            })
        {
            cancelled = true;
            break;
        }

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            // Following links would let a loop turn a finite scan into an
            // endless one, and can visit the same tree more than once.
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || !is_bundle(&path) {
                continue;
            }

            archives += 1;
            match read_package(&path) {
                Some(package) => {
                    if !emit(DiscoveryEvent::Found { package }) {
                        cancelled = true;
                        break 'scan;
                    }
                    found += 1;
                    if found >= limit {
                        truncated = true;
                        break 'scan;
                    }
                }
                None => skipped += 1,
            }
            if archives % PROGRESS_INTERVAL == 0
                && !emit(DiscoveryEvent::Progress {
                    directories,
                    archives,
                    skipped,
                })
            {
                cancelled = true;
                break 'scan;
            }
        }
    }

    if !cancelled {
        let _ = emit(DiscoveryEvent::Done {
            directories,
            archives,
            skipped,
            truncated,
        });
    }
}

fn is_bundle(path: &Path) -> bool {
    path.file_name()
        .map(|name| {
            name.to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".tar.gz")
        })
        .unwrap_or(false)
}

fn is_root_manifest(path: &Path) -> bool {
    let mut parts = path
        .components()
        .filter(|part| !matches!(part, Component::CurDir));
    matches!(parts.next(), Some(Component::Normal(name)) if name == "package.json")
        && parts.next().is_none()
}

fn read_package(path: &Path) -> Option<LocalPackage> {
    let file = std::fs::File::open(path).ok()?;
    let size_bytes = file.metadata().ok()?.len();
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut inspected = 0usize;
    for entry in archive.entries().ok()? {
        let entry = entry.ok()?;
        inspected += 1;
        if inspected > MAX_ENTRIES_INSPECTED {
            return None;
        }
        let entry_path = entry.path().ok()?.into_owned();
        if !is_root_manifest(&entry_path) {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return None;
        }
        let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
        validate_manifest(&manifest).ok()?;
        return Some(LocalPackage {
            path: path.to_string_lossy().replace('\\', "/"),
            name: manifest.name,
            version: manifest.version,
            description: manifest.description,
            size_bytes,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{discover, discover_with_limit, DiscoveryEvent};
    use std::path::Path;

    fn write_bundle(path: &Path, manifest: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "package.json", manifest.as_bytes())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }

    fn collect(root: &Path) -> Vec<DiscoveryEvent> {
        let mut events = Vec::new();
        discover(root, &mut |event| {
            events.push(event);
            true
        });
        events
    }

    #[test]
    fn finds_only_bundles_with_valid_root_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_bundle(
            &root.join("nested/valid.tar.gz"),
            r#"{"name":"cpython","version":"3.12","description":"Python","entry":"lib/python.so"}"#,
        );
        write_bundle(&root.join("invalid.tar.gz"), r#"{"name":"broken"}"#);
        write_bundle(&root.join("not-a-package.tar.gz"), "not json");
        std::fs::write(root.join("corrupt.tar.gz"), b"not gzip").unwrap();
        std::fs::write(root.join("notes.txt"), b"ignored").unwrap();

        let events = collect(root);
        let found: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                DiscoveryEvent::Found { package } => Some(package),
                _ => None,
            })
            .collect();
        assert_eq!(found.len(), 1, "only the valid bundle may be listed");
        assert_eq!(found[0].name, "cpython");
        assert_eq!(found[0].version, "3.12");
        assert!(found[0].path.ends_with("nested/valid.tar.gz"));
        assert!(matches!(
            events.last(),
            Some(DiscoveryEvent::Done {
                archives: 4,
                truncated: false,
                ..
            })
        ));
    }

    #[test]
    fn stops_when_the_consumer_cancels() {
        let temp = tempfile::tempdir().unwrap();
        write_bundle(
            &temp.path().join("one.tar.gz"),
            r#"{"name":"one","version":"1","entry":"lib/one.so"}"#,
        );
        write_bundle(
            &temp.path().join("two.tar.gz"),
            r#"{"name":"two","version":"1","entry":"lib/two.so"}"#,
        );

        let mut found = 0;
        let mut finished = false;
        discover(temp.path(), &mut |event| {
            match event {
                DiscoveryEvent::Found { .. } => {
                    found += 1;
                    return false;
                }
                DiscoveryEvent::Done { .. } => finished = true,
                _ => {}
            }
            true
        });
        assert_eq!(found, 1);
        assert!(!finished, "a disconnected consumer ends the scan");
    }

    #[test]
    fn the_result_limit_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["one", "two", "three"] {
            write_bundle(
                &temp.path().join(format!("{name}.tar.gz")),
                &format!(r#"{{"name":"{name}","version":"1","entry":"lib/{name}.so"}}"#),
            );
        }

        let mut events = Vec::new();
        discover_with_limit(temp.path(), 2, &mut |event| {
            events.push(event);
            true
        });
        let found = events
            .iter()
            .filter(|event| matches!(event, DiscoveryEvent::Found { .. }))
            .count();
        assert_eq!(found, 2);
        assert!(matches!(
            events.last(),
            Some(DiscoveryEvent::Done {
                truncated: true,
                ..
            })
        ));
    }

    #[test]
    fn an_endless_archive_is_not_walked_forever() {
        // A corrupt or hostile archive can hold an unbounded number of entries.
        // Scanning must give up instead of decompressing it from end to end.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("endless.tar.gz");
        let file = std::fs::File::create(&path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for index in 0..(super::MAX_ENTRIES_INSPECTED + 8) {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("filler{index}.txt"), &b"x"[..])
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        let events = collect(temp.path());
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, DiscoveryEvent::Found { .. })),
            "an archive that never reaches a manifest must not be listed"
        );
    }

    #[test]
    fn an_oversized_manifest_is_not_listed() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = format!(
            r#"{{"name":"huge","version":"1","entry":"lib/huge.so","description":"{}"}}"#,
            "x".repeat(70 * 1024)
        );
        write_bundle(&temp.path().join("huge.tar.gz"), &manifest);

        let events = collect(temp.path());
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, DiscoveryEvent::Found { .. })),
            "an unbounded manifest must not be read into the result list"
        );
    }
}
