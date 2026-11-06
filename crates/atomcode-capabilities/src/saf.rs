//! A [`FileSystem`] over Android's Storage Access Framework.
//!
//! Why this exists: the app normally asks for all-files access
//! (`MANAGE_EXTERNAL_STORAGE`) and works on real paths, which every tool —
//! `bash`, `git`, `grep` — understands. Google Play does not allow that
//! permission for a general-purpose file manager, so a distributable build has to
//! ask the person for one folder at a time through the system picker and work on
//! the `content://` documents it hands back. Those documents have no paths at
//! all, so the world the tools see has to be built on this side of the seam.
//!
//! Three things follow from "no paths", and they shape everything below:
//!
//! - **Paths here are synthetic.** They are the granted tree's fake root plus a
//!   relative name; [`SafBackend`] turns the relative half into whatever the
//!   platform needs. Nothing outside this module ever sees a URI.
//! - **There are no symlinks to resolve.** SAF documents cannot point at one
//!   another, so containment is decided lexically (`..` and `.` are folded
//!   before anything is asked of the platform). That makes
//!   [`FileSystem::canonicalize`] exact rather than best-effort — there is no
//!   race between checking a path and using it.
//! - **This world cannot run commands.** `Shell` still starts processes, and a
//!   `content://` document cannot be `exec`ed or handed to a compiler. A build
//!   that only has SAF access runs its shell in the app's private directory; the
//!   files in a granted tree are readable and writable, not runnable.
//!
//! The backend is a trait rather than direct JNI so the whole world — containment,
//! error mapping, directory creation — is exercised by tests on the build machine;
//! the Kotlin side supplies the real one (`SafBackend` over `DocumentFile`).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::world::{DirEntry, FileInfo, FileSystem, FsError, FsErrorKind};
use async_trait::async_trait;

/// One child of a directory, as the platform reports it. Name only: the world
/// composes the path, because only it knows its synthetic root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafEntry {
    pub name: String,
    pub is_dir: bool,
}

/// What the platform knows about one document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafStat {
    pub is_dir: bool,
    pub len: u64,
}

/// Why a backend call failed, in the seam's vocabulary rather than the platform's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SafError {
    /// The document is not there.
    NotFound,
    /// The platform refused: permission revoked, or outside the granted tree.
    /// Carries what the platform said, since "which grant went away" is the one
    /// thing a person can act on.
    Denied(String),
    /// Anything else the platform said.
    Io(String),
}

/// The platform side: everything that needs SAF, behind one trait.
///
/// Paths arrive already containment-checked and relative to the granted tree
/// (empty string is the tree itself), so an implementation never has to reason
/// about `..`. It answers with names, not paths, for the reason in [`SafEntry`].
#[async_trait]
pub trait SafBackend: Send + Sync {
    async fn list(&self, path: &str) -> Result<Vec<SafEntry>, SafError>;
    async fn read(&self, path: &str) -> Result<Vec<u8>, SafError>;
    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), SafError>;
    async fn create_dir(&self, path: &str) -> Result<(), SafError>;
    /// `None` when the document does not exist, rather than an error: "absent" is
    /// an answer the caller asks for by name in [`FileSystem::info`].
    async fn stat(&self, path: &str) -> Result<Option<SafStat>, SafError>;

    /// Identity of the tree this backend is bound to — for Android, the granted
    /// tree URI.
    ///
    /// [SafFs] captures it on first use and refuses further work once it changes.
    /// The grant is process-wide, so a person picking a *different* folder while a
    /// turn is running would otherwise redirect that session's writes into the new
    /// folder, without the runtime ever seeing a directory switch.
    ///
    /// Empty means "this backend has no identity" and disables the check, which is
    /// what the in-repo test backends return.
    async fn identity(&self) -> Result<String, SafError> {
        Ok(String::new())
    }
}

/// The platform backend, installed once by the host at startup.
///
/// A registry rather than a constructor argument because the row that builds this
/// world (`fs-saf`) lives in the harness, which must not name Android: the host
/// installs what it has, and the row asks for it. Absent means "this build has no
/// SAF access" — the row then refuses to mount instead of silently handing out a
/// world that reads nothing.
static BACKEND: std::sync::OnceLock<Arc<dyn SafBackend>> = std::sync::OnceLock::new();

/// Install the platform half. The first call wins: a second grant replaces the
/// folder, not the backend, so there is nothing here to swap.
pub fn install_backend(backend: Arc<dyn SafBackend>) {
    let _ = BACKEND.set(backend);
}

/// The installed backend, or `None` in a build without SAF access.
pub fn installed_backend() -> Option<Arc<dyn SafBackend>> {
    BACKEND.get().cloned()
}

/// The path the granted tree is presented under, recorded when its row mounts.
///
/// The daemon serves `/fs/*` over real paths. In a SAF build the session's
/// working directory is this synthetic root — nothing is on disk behind it — so
/// the routes that browse or search have to know which path means "the granted
/// tree". The `fs-saf` row is the only thing that knows, hence the registry.
static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn install_root(root: PathBuf) {
    let _ = ROOT.set(root);
}

pub fn installed_root() -> Option<PathBuf> {
    ROOT.get().cloned()
}

/// How deep the name search descends, and how many matches it returns.
const SEARCH_MAX_DEPTH: usize = 8;
const SEARCH_MAX_MATCHES: usize = 200;

/// Directory names the name search never descends into.
fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | "build" | "dist" | ".gradle" | ".venv" | "__pycache__"
    )
}

/// A bounded, name-only search over a world that has no paths.
///
/// This is the same `@`-mention contract the real-path search serves, expressed
/// over the [`FileSystem`] seam: `scope` is the directory to start from (relative
/// to `fs.root()`), `filter` is the name fragment. Matches come back relative to
/// `fs.root()`, forward-slashed, with whether each is a directory.
///
/// Why not reuse `file_index`: it walks a real directory and reads `.gitignore`,
/// while SAF hands out `content://` documents with neither. The bounds exist for
/// the same reason the file picker has them — a grant can be a whole SD card.
pub async fn search_names(fs: &dyn FileSystem, scope: &str, filter: &str) -> Vec<(String, bool)> {
    let needle = filter.trim().to_lowercase();
    let root = fs.root();
    let start = if scope.trim().is_empty() {
        root.clone()
    } else {
        root.join(scope)
    };
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        std::collections::VecDeque::from([(start, 0usize)]);

    while let Some((dir, depth)) = queue.pop_front() {
        if out.len() >= SEARCH_MAX_MATCHES {
            break;
        }
        // An unreadable branch (revoked grant, deleted document) is skipped rather
        // than ending the search: one folder must not hide the rest of the tree.
        let Ok(entries) = fs.list(&dir).await else {
            continue;
        };
        for entry in entries {
            if out.len() >= SEARCH_MAX_MATCHES {
                break;
            }
            let name = entry
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            // An empty filter is "show me what is here", which is the immediate
            // children — the same thing typing `@` with nothing after it means.
            let matched = if needle.is_empty() {
                depth == 0
            } else {
                name.to_lowercase().contains(&needle)
            };
            if matched {
                let relative = entry.path.strip_prefix(&root).unwrap_or(&entry.path);
                out.push((relative.to_string_lossy().replace('\\', "/"), entry.is_dir));
            }
            if entry.is_dir && depth + 1 <= SEARCH_MAX_DEPTH && !is_noise_dir(name) {
                queue.push_back((entry.path.clone(), depth + 1));
            }
        }
    }
    out
}

/// A [`FileSystem`] whose root is a folder the person granted through the picker.
pub struct SafFs {
    root: PathBuf,
    backend: Arc<dyn SafBackend>,
    read_only: bool,
    /// The tree identity this world was opened against, captured on first use.
    bound: std::sync::OnceLock<String>,
}

impl SafFs {
    pub fn new(root: PathBuf, backend: Arc<dyn SafBackend>) -> Self {
        Self {
            root,
            backend,
            read_only: false,
            bound: std::sync::OnceLock::new(),
        }
    }

    /// The same tree with every mutation refused.
    pub fn read_only(root: PathBuf, backend: Arc<dyn SafBackend>) -> Self {
        Self {
            root,
            backend,
            read_only: true,
            bound: std::sync::OnceLock::new(),
        }
    }

    /// Refuse to keep working on a tree that was replaced under this world.
    ///
    /// The parked grant is process-wide, so picking a *different* folder while a
    /// turn runs would otherwise redirect every later write of that session into the
    /// new folder: the session never asked for it, and the runtime never saw a
    /// directory switch. Binding the instance to the identity it first saw turns
    /// that silent redirect into an explicit refusal the runtime can act on
    /// (re-mount is what makes the new folder usable).
    ///
    /// An empty identity means the backend cannot tell (test backends), and the
    /// check is skipped rather than guessed.
    async fn verify_binding(&self) -> Result<(), FsError> {
        let identity = self
            .backend
            .identity()
            .await
            .map_err(|error| self.map(&self.root, error))?;
        if identity.is_empty() {
            return Ok(());
        }
        let bound = self.bound.get_or_init(|| identity.clone());
        if bound != &identity {
            return Err(FsError::denied(format!(
                "the granted folder changed while this session was open (it now points at \
                 {}, which this session never asked for); reopen the session to use it",
                identity
            )));
        }
        Ok(())
    }

    /// The backend-facing relative path, or [`FsErrorKind::Denied`].
    ///
    /// Containment happens here and only here. Folding `.` and `..` lexically is
    /// sound for this world because SAF documents have no links to follow (see
    /// the module docs) — there is no second, real path that could differ.
    fn relative(&self, path: &Path) -> Result<String, FsError> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let mut resolved: Vec<Component<'_>> = Vec::new();
        let mut depth = 0usize;
        for component in joined.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => resolved.push(component),
                Component::CurDir => {}
                Component::ParentDir => {
                    // Popping past the tree root is the escape we exist to refuse.
                    if depth == 0 {
                        return Err(FsError::denied(format!(
                            "{} is outside this folder",
                            path.display()
                        )));
                    }
                    resolved.pop();
                    depth -= 1;
                }
                Component::Normal(_) => {
                    resolved.push(component);
                    depth += 1;
                }
            }
        }
        let rebuilt: PathBuf = resolved.iter().collect();
        let rel = rebuilt
            .strip_prefix(&self.root)
            .map_err(|_| FsError::denied(format!("{} is outside this folder", path.display())))?;
        // SAF addresses by relative name; the separator is always `/` regardless of
        // what the host's `Path` used while parsing.
        let mut text = String::new();
        for component in rel.components() {
            if let Component::Normal(name) = component {
                if !text.is_empty() {
                    text.push('/');
                }
                text.push_str(&name.to_string_lossy());
            }
        }
        Ok(text)
    }

    /// Create `rel` (relative, `/`-separated) and every missing ancestor.
    ///
    /// `stat` first, create only what is absent: SAF's own create is *nearly*
    /// idempotent across providers, and a world should not depend on how nearly.
    async fn ensure_dir(&self, rel: &str) -> Result<(), FsError> {
        if rel.is_empty() {
            return Ok(());
        }
        let mut prefix = String::new();
        for segment in rel.split('/') {
            if segment.is_empty() {
                continue;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            match self.backend.stat(&prefix).await {
                Ok(Some(found)) if found.is_dir => continue,
                Ok(Some(_)) => {
                    return Err(FsError {
                        kind: FsErrorKind::NotADirectory,
                        message: format!("{prefix} is not a directory"),
                    })
                }
                Ok(None) => self
                    .backend
                    .create_dir(&prefix)
                    .await
                    .map_err(|e| self.map(Path::new(&prefix), e))?,
                Err(e) => return Err(self.map(Path::new(&prefix), e)),
            }
        }
        Ok(())
    }

    fn map(&self, path: &Path, error: SafError) -> FsError {
        match error {
            SafError::NotFound => FsError::not_found(&self.root.join(path)),
            SafError::Denied(reason) => FsError::denied(format!(
                "the platform refused access to {}: {reason}",
                self.root.join(path).display()
            )),
            SafError::Io(message) => FsError {
                kind: FsErrorKind::Io,
                message,
            },
        }
    }
}

#[async_trait]
impl FileSystem for SafFs {
    fn describe(&self) -> String {
        format!(
            "the folder you granted through the Android file picker (Storage Access Framework), rooted at {}",
            self.root.display()
        )
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    async fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        self.verify_binding().await?;
        let rel = self.relative(path)?;
        if rel.is_empty() {
            return Err(FsError::not_a_file(path));
        }
        self.backend.read(&rel).await.map_err(|e| self.map(path, e))
    }

    async fn write_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::denied(format!(
                "{} is in a read-only world",
                path.display()
            )));
        }
        self.verify_binding().await?;
        let rel = self.relative(path)?;
        if rel.is_empty() {
            return Err(FsError::not_a_file(path));
        }
        let parent = rel.rsplit_once('/').map(|(head, _)| head).unwrap_or("");
        self.ensure_dir(parent).await?;
        self.backend
            .write(&rel, bytes)
            .await
            .map_err(|e| self.map(path, e))
    }

    async fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::denied(format!(
                "{} is in a read-only world",
                path.display()
            )));
        }
        let rel = self.relative(path)?;
        self.verify_binding().await?;
        self.ensure_dir(&rel).await
    }

    async fn list(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
        self.verify_binding().await?;
        let rel = self.relative(path)?;
        let base = self.root.join(&rel);
        let mut entries: Vec<DirEntry> = self
            .backend
            .list(&rel)
            .await
            .map_err(|e| self.map(path, e))?
            .into_iter()
            .map(|entry| DirEntry {
                path: base.join(entry.name),
                is_dir: entry.is_dir,
            })
            .collect();
        // The seam promises name order; providers hand back whatever order they
        // store documents in.
        entries.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
        Ok(entries)
    }

    async fn info(&self, path: &Path) -> Result<FileInfo, FsError> {
        self.verify_binding().await?;
        let rel = self.relative(path)?;
        match self
            .backend
            .stat(&rel)
            .await
            .map_err(|e| self.map(path, e))?
        {
            Some(found) => Ok(FileInfo {
                exists: true,
                is_dir: found.is_dir,
                len: found.len,
            }),
            // Absent is a normal answer here, not an error: callers ask
            // `info().exists` precisely to find out.
            None => Ok(FileInfo {
                exists: false,
                is_dir: false,
                len: 0,
            }),
        }
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, FsError> {
        let rel = self.relative(path)?;
        Ok(if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        })
    }
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    /// A tree in memory, so the world's own logic is what is under test.
    #[derive(Default)]
    struct Memory {
        dirs: Mutex<BTreeSet<String>>,
        files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        /// What `identity()` reports. Empty by default, which disables the binding
        /// check — existing tests are about the world's logic, not the identity.
        identity: Mutex<String>,
    }

    impl Memory {
        /// Pretend the person re-granted a different folder mid-session.
        fn set_identity(&self, id: &str) {
            *self.identity.lock().unwrap() = id.to_string();
        }
    }

    impl Memory {
        fn new() -> Self {
            let memory = Self::default();
            memory.dirs.lock().unwrap().insert(String::new());
            memory
        }

        fn with_file(self, path: &str, bytes: &[u8]) -> Self {
            let parent = path.rsplit_once('/').map(|(head, _)| head).unwrap_or("");
            let mut dirs = self.dirs.lock().unwrap();
            let mut prefix = String::new();
            dirs.insert(prefix.clone());
            for segment in parent.split('/').filter(|s| !s.is_empty()) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(segment);
                dirs.insert(prefix.clone());
            }
            drop(dirs);
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), bytes.to_vec());
            self
        }
    }

    #[async_trait]
    impl SafBackend for Memory {
        async fn identity(&self) -> Result<String, SafError> {
            Ok(self.identity.lock().unwrap().clone())
        }

        async fn list(&self, path: &str) -> Result<Vec<SafEntry>, SafError> {
            if !self.dirs.lock().unwrap().contains(path) {
                return Err(SafError::NotFound);
            }
            let mut out = Vec::new();
            for dir in self.dirs.lock().unwrap().iter() {
                if let Some(rest) = dir.strip_prefix(path) {
                    let rest = rest.trim_start_matches('/');
                    if !rest.is_empty() && !rest.contains('/') {
                        out.push(SafEntry {
                            name: rest.to_string(),
                            is_dir: true,
                        });
                    }
                }
            }
            for file in self.files.lock().unwrap().keys() {
                if let Some(rest) = file.strip_prefix(path) {
                    let rest = rest.trim_start_matches('/');
                    if !rest.is_empty() && !rest.contains('/') {
                        out.push(SafEntry {
                            name: rest.to_string(),
                            is_dir: false,
                        });
                    }
                }
            }
            // Deliberately reversed: the world must sort, not the provider.
            out.reverse();
            Ok(out)
        }

        async fn read(&self, path: &str) -> Result<Vec<u8>, SafError> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or(SafError::NotFound)
        }

        async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), SafError> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), bytes.to_vec());
            Ok(())
        }

        async fn create_dir(&self, path: &str) -> Result<(), SafError> {
            self.dirs.lock().unwrap().insert(path.to_string());
            Ok(())
        }

        async fn stat(&self, path: &str) -> Result<Option<SafStat>, SafError> {
            if path.is_empty() || self.dirs.lock().unwrap().contains(path) {
                return Ok(Some(SafStat {
                    is_dir: true,
                    len: 0,
                }));
            }
            if let Some(bytes) = self.files.lock().unwrap().get(path) {
                return Ok(Some(SafStat {
                    is_dir: false,
                    len: bytes.len() as u64,
                }));
            }
            Ok(None)
        }
    }

    fn world(memory: Memory) -> SafFs {
        SafFs::new(PathBuf::from("/saf/tree"), Arc::new(memory))
    }

    /// A folder granted over another one must not silently redirect a running
    /// session: the grant is process-wide, so without this the session's next write
    /// would land in a folder it never asked for.
    #[tokio::test]
    async fn a_folder_swapped_mid_session_refuses_instead_of_redirecting() {
        let memory = Arc::new(Memory::new().with_file("notes.txt", b"first tree"));
        memory.set_identity("content://tree-a");
        let fs = SafFs::new(PathBuf::from("/saf/tree"), memory.clone());
        let path = PathBuf::from("/saf/tree/notes.txt");

        // First use binds the world to the tree it was opened against.
        assert_eq!(fs.read_bytes(&path).await.unwrap(), b"first tree");

        // The person re-grants a different folder.
        memory.set_identity("content://tree-b");

        let error = fs.read_bytes(&path).await.unwrap_err();
        let text = format!("{error:?}");
        assert!(text.contains("granted folder changed"), "{text}");
        // Writes are refused the same way — that is the one that would corrupt.
        assert!(fs.write_bytes(&path, b"into the wrong tree").await.is_err());
        assert!(fs.list(&PathBuf::from("/saf/tree")).await.is_err());
    }

    #[tokio::test]
    async fn writes_create_missing_ancestors_and_read_back() {
        let fs = world(Memory::new());
        fs.write_bytes(Path::new("notes/day/one.txt"), b"hello")
            .await
            .expect("write");
        assert_eq!(
            fs.read_bytes(Path::new("notes/day/one.txt")).await.unwrap(),
            b"hello"
        );
        let info = fs.info(Path::new("notes/day/one.txt")).await.unwrap();
        assert!(info.exists && !info.is_dir);
        assert_eq!(info.len, 5);
    }

    #[tokio::test]
    async fn reading_a_missing_document_is_not_found() {
        let fs = world(Memory::new());
        let error = fs.read_bytes(Path::new("nope.txt")).await.unwrap_err();
        assert_eq!(error.kind, FsErrorKind::NotFound);
    }

    #[tokio::test]
    async fn a_dot_dot_escape_is_denied() {
        let fs = world(Memory::new());
        let error = fs.read_bytes(Path::new("../secrets")).await.unwrap_err();
        assert_eq!(error.kind, FsErrorKind::Denied);
        // And an absolute path outside the tree is the same refusal.
        let error = fs
            .read_bytes(Path::new("/sdcard/other/secret"))
            .await
            .unwrap_err();
        assert_eq!(error.kind, FsErrorKind::Denied);
    }

    #[tokio::test]
    async fn canonicalize_folds_dots_and_refuses_the_escape() {
        let fs = world(Memory::new());
        assert_eq!(
            fs.canonicalize(Path::new("a/./b/../c")).await.unwrap(),
            PathBuf::from("/saf/tree/a/c")
        );
        assert_eq!(
            fs.canonicalize(Path::new("/saf/tree/x/y")).await.unwrap(),
            PathBuf::from("/saf/tree/x/y")
        );
        assert!(fs.canonicalize(Path::new("../../etc")).await.is_err());
    }

    #[tokio::test]
    async fn listing_is_sorted_by_name_whatever_the_provider_returns() {
        let fs = world(
            Memory::new()
                .with_file("z.txt", b"z")
                .with_file("a.txt", b"a")
                .with_file("m.txt", b"m"),
        );
        let names: Vec<String> = fs
            .list(Path::new(""))
            .await
            .unwrap()
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt", "m.txt", "z.txt"]);
    }

    #[tokio::test]
    async fn a_read_only_world_refuses_every_mutation() {
        let backend: Arc<dyn SafBackend> = Arc::new(Memory::new());
        let fs = SafFs::read_only(PathBuf::from("/saf/tree"), backend);
        for error in [
            fs.write_bytes(Path::new("a.txt"), b"x").await.unwrap_err(),
            fs.create_dir_all(Path::new("dir")).await.unwrap_err(),
        ] {
            assert_eq!(error.kind, FsErrorKind::Denied);
        }
        // Reading is still allowed.
        assert!(!fs.info(Path::new("a.txt")).await.unwrap().exists);
    }

    #[tokio::test]
    async fn a_missing_document_reports_absent_rather_than_failing() {
        let fs = world(Memory::new());
        let info = fs.info(Path::new("not/there")).await.unwrap();
        assert!(!info.exists && !info.is_dir && info.len == 0);
    }

    #[tokio::test]
    async fn the_description_names_the_picker_so_the_model_knows_its_world() {
        let fs = world(Memory::new());
        let text = fs.describe();
        assert!(text.contains("picker"), "{text}");
        assert!(text.contains("/saf/tree"), "{text}");
    }

    /// The `@`-mention picker has to keep working where there are no paths. It
    /// used to be empty in a SAF build, because the search walked a real
    /// directory that does not exist here.
    #[tokio::test]
    async fn a_name_search_reaches_into_subdirectories_and_skips_noise() {
        let fs = world(
            Memory::new()
                .with_file("src/main/java/cn/ApplyStockController.java", b"x")
                .with_file("src/apply_helper.rs", b"x")
                .with_file("target/ApplyStockController.class", b"x")
                .with_file("README.md", b"x"),
        );

        let found = search_names(&fs, "", "apply").await;
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert!(
            paths.contains(&"src/main/java/cn/ApplyStockController.java"),
            "{paths:?}"
        );
        assert!(paths.contains(&"src/apply_helper.rs"), "{paths:?}");
        // Build output must not crowd out source matches.
        assert!(
            !paths.iter().any(|path| path.starts_with("target/")),
            "{paths:?}"
        );

        // An empty filter is "what is here" — the immediate children, the same
        // thing typing `@` with nothing after it asks for.
        let top = search_names(&fs, "", "").await;
        let tops: Vec<&str> = top.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(tops, vec!["README.md", "src", "target"]);
    }

    /// The scope narrows where the walk starts, exactly like the real-path
    /// search: `src/` plus filter `apply` must not return a match outside `src`.
    #[tokio::test]
    async fn a_scoped_search_stays_inside_the_scope() {
        let fs = world(
            Memory::new()
                .with_file("src/apply.rs", b"x")
                .with_file("docs/apply.md", b"x"),
        );
        let found = search_names(&fs, "src", "apply").await;
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, vec!["src/apply.rs"]);
    }
}
