//! WebUI catalog preferences, separate from session storage and runtime ownership.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preference {
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub hidden: bool,
    pub working_dir: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Patch {
    pub pinned: Option<bool>,
    pub hidden: Option<bool>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    projects: BTreeMap<String, Preference>,
}

pub(crate) fn read(root: &Path) -> io::Result<BTreeMap<String, Preference>> {
    let bytes = match std::fs::read(root.join("webui-workspaces.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    };
    let file: File = serde_json::from_slice(&bytes)?;
    if file.version != 1 {
        return Err(io::Error::other(
            "unsupported workspace preferences version",
        ));
    }
    Ok(file.projects)
}

pub(crate) fn update(
    root: &Path,
    hash: &str,
    patch: Patch,
    dir: Option<PathBuf>,
) -> io::Result<Preference> {
    if !super::valid_project_bucket(hash) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid workspace identifier",
        ));
    }
    std::fs::create_dir_all(root)?;
    // A separate lock file survives atomic replacement and serializes all daemons.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("webui-workspaces.lock"))?;
    lock.lock()?;
    let mut projects = read(root)?;
    let pref = projects.entry(hash.to_string()).or_default();
    if let Some(value) = patch.pinned {
        pref.pinned = value;
    }
    if let Some(value) = patch.hidden {
        pref.hidden = value;
    }
    if dir.is_some() {
        pref.working_dir = dir;
    }
    let result = pref.clone();
    let mut staged = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer_pretty(
        &mut staged,
        &File {
            version: 1,
            projects,
        },
    )?;
    staged.write_all(b"\n")?;
    staged.as_file().sync_all()?;
    staged
        .persist(root.join("webui-workspaces.json"))
        .map_err(|e| e.error)?;
    Ok(result)
}

/// Include remembered empty groups without affecting the session catalog itself.
pub(crate) fn decorate(
    root: &Path,
    catalog: Vec<super::ProjectInfo>,
) -> io::Result<Vec<serde_json::Value>> {
    let mut projects: Vec<serde_json::Value> = catalog
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()?;
    for (hash, pref) in read(root)? {
        let index = match projects.iter().position(|p| p["hash"] == hash) {
            Some(index) => index,
            None => {
                let Some(dir) = &pref.working_dir else {
                    continue;
                };
                projects.push(serde_json::json!({
                    "hash": hash, "name": dir.file_name().unwrap_or_default().to_string_lossy(),
                    "working_dir": dir, "session_count": 0, "created_at": 0, "last_updated": 0
                }));
                projects.len() - 1
            }
        };
        projects[index]["pinned"] = pref.pinned.into();
        projects[index]["hidden"] = pref.hidden.into();
    }
    Ok(projects)
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH: &str = "0123456789abcdef";

    #[test]
    fn hiding_and_pinning_keep_sessions_and_the_project_directory() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("code");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("source.rs"), "keep").unwrap();
        let sessions = root.path().join("sessions").join(HASH);
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("example.events"), "keep").unwrap();
        assert!(read(root.path()).unwrap().is_empty());
        update(
            root.path(),
            HASH,
            Patch {
                pinned: Some(true),
                hidden: None,
            },
            Some(project.clone()),
        )
        .unwrap();
        let pref = update(
            root.path(),
            HASH,
            Patch {
                hidden: Some(true),
                pinned: None,
            },
            None,
        )
        .unwrap();
        assert!(pref.hidden && pref.pinned);
        let empty_group = decorate(root.path(), vec![]).unwrap();
        assert_eq!(empty_group.len(), 1);
        assert_eq!(empty_group[0]["hash"], HASH);
        assert_eq!(empty_group[0]["hidden"], true);
        assert_eq!(empty_group[0]["session_count"], 0);
        assert_eq!(
            read(root.path()).unwrap()[HASH].working_dir,
            Some(project.clone())
        );
        assert_eq!(
            std::fs::read_to_string(project.join("source.rs")).unwrap(),
            "keep"
        );
        assert_eq!(
            std::fs::read_to_string(sessions.join("example.events")).unwrap(),
            "keep"
        );
        assert!(
            !update(
                root.path(),
                HASH,
                Patch {
                    hidden: Some(false),
                    pinned: None
                },
                None
            )
            .unwrap()
            .hidden
        );
    }

    #[test]
    fn invalid_input_or_corrupt_preferences_fail_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        assert!(update(root.path(), "../outside", Patch::default(), None).is_err());
        std::fs::write(root.path().join("webui-workspaces.json"), "broken").unwrap();
        assert!(update(root.path(), HASH, Patch::default(), None).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("webui-workspaces.json")).unwrap(),
            "broken"
        );
    }

    #[test]
    fn concurrent_updates_preserve_both_changes() {
        let root = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                update(
                    root.path(),
                    HASH,
                    Patch {
                        pinned: Some(true),
                        hidden: None,
                    },
                    None,
                )
                .unwrap()
            });
            scope.spawn(|| {
                update(
                    root.path(),
                    HASH,
                    Patch {
                        hidden: Some(true),
                        pinned: None,
                    },
                    None,
                )
                .unwrap()
            });
        });
        let pref = &read(root.path()).unwrap()[HASH];
        assert!(pref.pinned && pref.hidden);
    }
}
