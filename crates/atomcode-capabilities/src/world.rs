//! The execution world: where a tool's reads, writes and commands actually happen.
//!
//! # Why this lives beside the tools rather than above them
//!
//! A tool that calls `std::fs::read` directly has hard-coded its world. Every
//! layer above it can then only *ask* it to behave — an approval policy, a path
//! allowlist, a prompt — and every one of those is a rule the tool cooperates
//! with rather than a boundary it cannot cross. Swap the world instead and the
//! guarantee holds even when the policy above is misconfigured or absent, which
//! is the difference between "read-only" as a promise and read-only as a fact.
//!
//! The seam therefore belongs next to the implementations that go through it,
//! not in the layer that assembles them. An assembly that wanted the seam
//! somewhere else would have to re-implement every tool to reach it — which is
//! exactly what happened before this module existed.
//!
//! # Bytes are the primitive; text is derived
//!
//! [`FileSystem`] requires five methods. Text reading, text writing and
//! single-occurrence editing are default implementations over them, so a new
//! world — a container, a remote sandbox, an in-memory fixture — implements the
//! five and inherits the rest, and cannot accidentally make `read_text` and
//! `read_bytes` disagree about the same file.
//!
//! # What deliberately does not route through here
//!
//! Not every tool has a world. Three categories, and only the first belongs
//! here:
//!
//! * **Operates on the world** — read, write, edit, list, glob, grep, bash.
//!   These route.
//! * **Pure session state** — `todo`, `report_finding`, `request_user_input`.
//!   Their behaviour is identical in every world, so routing them would be
//!   ceremony.
//! * **Belongs to a *different* world** — `open_file` launches the viewer's
//!   desktop application (`open` / `xdg-open` / `explorer.exe`). That is the
//!   machine the *person* is at, not the machine the agent runs on. A remote
//!   world should not mount it at all rather than route it somewhere wrong.
//!
//! # Routing the I/O is only half of it
//!
//! A tool that performs its I/O through a world but *decides* from the host has
//! a split brain: the command runs in a Linux container while "which shell, and
//! how are paths written" was answered by the Windows machine that launched it.
//! So the rule is stronger than "call the seam":
//!
//! > **Every fact a tool branches on must come from the same world its I/O
//! > goes to.**
//!
//! Two kinds of branch, and only one is a problem:
//!
//! * **Derived from content** — `edit` and `search_replace` pick a line ending
//!   with `content.contains("\r\n")`, i.e. from the bytes the world just handed
//!   back. That is already correct: change the world and the answer changes with
//!   it. No interface needed.
//! * **Probed from the host** — `where bash`, `reg query`, `/proc/version`.
//!   These are the split brain. They belong *inside* the local implementation of
//!   a seam, as its private business, which is why [`Shell::run`] takes a command
//!   string rather than an argv: which shell exists and where is a property of
//!   the world, and a remote world knows its own answer without probing anything.
//!
//! The filesystem tools currently branch on nothing from the host, so
//! [`FileSystem`] deliberately exposes no world *properties* (os, separator,
//! case-sensitivity) yet. When a world needs them it should add them here rather
//! than let a tool ask the host — but adding them before a caller exists would be
//! guessing at the shape of a world nobody has built.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---- errors and value types ---------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsErrorKind {
    NotFound,
    NotAFile,
    NotADirectory,
    /// Outside the world's containment root, or the world is read-only.
    Denied,
    /// Not valid UTF-8, or otherwise not text.
    NotText,
    Io,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsError {
    pub kind: FsErrorKind,
    pub message: String,
}

impl FsError {
    pub fn denied(message: impl Into<String>) -> Self {
        Self {
            kind: FsErrorKind::Denied,
            message: message.into(),
        }
    }
    pub fn not_found(path: &Path) -> Self {
        Self {
            kind: FsErrorKind::NotFound,
            message: format!("{} does not exist", path.display()),
        }
    }
    pub fn not_a_file(path: &Path) -> Self {
        Self {
            kind: FsErrorKind::NotAFile,
            message: format!("{} is a directory", path.display()),
        }
    }
    pub fn io(error: impl std::fmt::Display) -> Self {
        Self {
            kind: FsErrorKind::Io,
            message: error.to_string(),
        }
    }
    /// True when the world refused rather than failed — the distinction a
    /// caller needs to tell "you may not" from "it broke".
    pub fn is_denied(&self) -> bool {
        self.kind == FsErrorKind::Denied
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FsError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileInfo {
    pub exists: bool,
    pub is_dir: bool,
    pub len: u64,
}

// ---- the filesystem seam -------------------------------------------------

/// One execution world's view of files.
///
/// Paths arriving here are model-supplied. **Containment is the world's job**,
/// not the caller's: a consumer must not have to know whether it is talking to
/// a local disk or a sandbox, so it also must not be the one enforcing the
/// boundary. A world that resolves a path outside its root returns
/// [`FsErrorKind::Denied`].
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Human-readable identity, for diagnostics and for telling the model which
    /// world it is operating in.
    fn describe(&self) -> String;

    /// The root that relative paths resolve against.
    fn root(&self) -> PathBuf;

    async fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, FsError>;

    async fn write_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), FsError>;

    /// Create `path` and every missing ancestor.
    ///
    /// Separate from [`write_bytes`](Self::write_bytes) (which creates parents on
    /// its own) because a caller that wants to report *which* step failed — "could
    /// not create the parent directory" rather than "could not write" — has to be
    /// able to do the two in sequence.
    async fn create_dir_all(&self, path: &Path) -> Result<(), FsError>;

    /// Direct children of `path`, one level, sorted by name.
    ///
    /// Deliberately **unfiltered**: a world reports what is there. Which entries
    /// are worth showing, which subtrees are too noisy to descend into, and how
    /// deep to go are the caller's policy — and a world that quietly hid
    /// `node_modules` would be answering a presentation question on behalf of a
    /// tool that has its own opinion about it, leaving two skip-lists to disagree.
    async fn list(&self, path: &Path) -> Result<Vec<DirEntry>, FsError>;

    async fn info(&self, path: &Path) -> Result<FileInfo, FsError>;

    /// The real path, with symlinks resolved and containment applied.
    ///
    /// Required because a *gate* — a policy that decides whether a write may
    /// happen — has to compare real paths, and a gate that canonicalizes
    /// through `std::fs` while the tool it guards writes through a remote world
    /// is checking one machine and permitting another.
    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, FsError>;

    // ---- derived, overridable -------------------------------------------

    async fn read_text(&self, path: &Path) -> Result<String, FsError> {
        let bytes = self.read_bytes(path).await?;
        String::from_utf8(bytes).map_err(|_| FsError {
            kind: FsErrorKind::NotText,
            message: format!("{} is not UTF-8 text", path.display()),
        })
    }

    async fn write_text(&self, path: &Path, content: &str) -> Result<(), FsError> {
        self.write_bytes(path, content.as_bytes()).await
    }

    /// Replace the single occurrence of `old` with `new`.
    ///
    /// Default-implemented over read + write, but overridable: a world with real
    /// atomicity should do the match and the rewrite in one critical section
    /// rather than leaving a read-modify-write race in the open.
    async fn edit_text(&self, path: &Path, old: &str, new: &str) -> Result<(), FsError> {
        let content = self.read_text(path).await?;
        let Some(index) = content.find(old) else {
            return Err(FsError {
                kind: FsErrorKind::NotFound,
                message: "the text to replace was not found".into(),
            });
        };
        if content[index + old.len()..].contains(old) {
            return Err(FsError::denied(
                "the text to replace is not unique; include more context",
            ));
        }
        let mut updated = String::with_capacity(content.len() - old.len() + new.len());
        updated.push_str(&content[..index]);
        updated.push_str(new);
        updated.push_str(&content[index + old.len()..]);
        self.write_text(path, &updated).await
    }
}

// ---- the process seam ----------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub timeout: Option<std::time::Duration>,
    /// Bytes of combined output to keep. `0` means unbounded.
    pub max_output_bytes: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// True when output was cut to `max_output_bytes`.
    pub truncated: bool,
}

/// Shell execution for one world.
///
/// Deliberately *not* "spawn this argv": which shell exists, where it lives and
/// how it is invoked is a property of the world, and discovering it is the local
/// implementation's private business rather than an interface every remote world
/// has to reimplement.
#[async_trait]
pub trait Shell: Send + Sync {
    fn describe(&self) -> String;
    async fn run(&self, command: &str, options: &SpawnOptions) -> Result<Output, String>;
}

// ---- the local world -----------------------------------------------------

/// The local disk, fenced to a root, optionally read-only.
///
/// Containment lives here rather than in each tool: a consumer cannot forget to
/// call it, and a different world enforces its own boundary its own way.
pub struct LocalFs {
    /// `None` means **no containment**: the path is used exactly as given.
    ///
    /// That is the default a tool gets when nobody hands it a world, and it
    /// reproduces byte for byte what these tools did before this seam existed —
    /// a direct `std::fs` call on a path the tool already resolved. Adopting the
    /// seam therefore changes no behaviour anywhere until an assembly actually
    /// chooses a different world.
    root: Option<PathBuf>,
    read_only: bool,
}

impl LocalFs {
    /// The transparent default: plain local I/O, no fence, no read-only.
    ///
    /// Path resolution stays where it already is — in the tool, against its
    /// `ToolContext::working_dir`. This world only performs the I/O, which is
    /// what makes adopting it a no-op for every existing caller.
    pub fn unfenced() -> Self {
        Self {
            root: None,
            read_only: false,
        }
    }

    /// Local I/O fenced to `root`: anything resolving outside it is denied.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            read_only: false,
        }
    }

    /// A world that refuses every mutation at the boundary.
    ///
    /// Not a policy: there is no configuration of the layers above that can talk
    /// this into writing.
    pub fn read_only(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            read_only: true,
        }
    }

    /// Read-only with no fence — for an audit that may roam but may not write.
    pub fn read_only_unfenced() -> Self {
        Self {
            root: None,
            read_only: true,
        }
    }

    /// Resolve a model-supplied path inside the root, refusing anything that
    /// escapes it. Symlinks are resolved where the path exists, so a link out of
    /// the tree is caught rather than followed.
    ///
    /// With no root, the path is returned untouched: an unfenced world performs
    /// I/O and nothing else.
    fn resolve(&self, path: &Path) -> Result<PathBuf, FsError> {
        let Some(root_dir) = self.root.as_ref() else {
            return Ok(path.to_path_buf());
        };
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root_dir.join(path)
        };
        // Canonicalize the deepest existing ancestor, then re-attach the rest, so
        // a write to a not-yet-existing file still gets a real-path check.
        let mut existing = joined.as_path();
        let mut trailing = PathBuf::new();
        loop {
            if existing.exists() {
                break;
            }
            let Some(parent) = existing.parent() else {
                return Err(FsError::denied("path has no resolvable ancestor"));
            };
            let name = existing
                .file_name()
                .ok_or_else(|| FsError::denied("path has no file name"))?;
            trailing = if trailing.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                PathBuf::from(name).join(&trailing)
            };
            existing = parent;
        }
        let real = existing.canonicalize().map_err(FsError::io)?;
        let root = root_dir
            .canonicalize()
            .unwrap_or_else(|_| root_dir.clone());
        let full = if trailing.as_os_str().is_empty() {
            real
        } else {
            real.join(&trailing)
        };
        if !full.starts_with(&root) {
            return Err(FsError::denied(format!(
                "{} is outside the world's root {}",
                full.display(),
                root.display()
            )));
        }
        Ok(full)
    }

    fn deny_write(&self) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::denied("this filesystem world is read-only"));
        }
        Ok(())
    }
}

#[async_trait]
impl FileSystem for LocalFs {
    fn describe(&self) -> String {
        let access = if self.read_only {
            "local (read-only)"
        } else {
            "local"
        };
        match &self.root {
            Some(root) => format!("{access} at {}", root.display()),
            None => format!("{access}, unfenced"),
        }
    }

    fn root(&self) -> PathBuf {
        self.root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    async fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        let path = self.resolve(path)?;
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_dir() => return Err(FsError::not_a_file(&path)),
            Ok(_) => {}
            Err(_) => return Err(FsError::not_found(&path)),
        }
        tokio::fs::read(&path).await.map_err(FsError::io)
    }

    async fn write_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
        self.deny_write()?;
        let path = self.resolve(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(FsError::io)?;
        }
        tokio::fs::write(&path, bytes).await.map_err(FsError::io)
    }

    async fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
        self.deny_write()?;
        let path = self.resolve(path)?;
        tokio::fs::create_dir_all(&path).await.map_err(FsError::io)
    }

    async fn list(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
        let root = self.resolve(path)?;
        match tokio::fs::metadata(&root).await {
            Ok(meta) if !meta.is_dir() => {
                return Err(FsError {
                    kind: FsErrorKind::NotADirectory,
                    message: format!("{} is not a directory", root.display()),
                })
            }
            Ok(_) => {}
            Err(_) => return Err(FsError::not_found(&root)),
        }
        let mut reader = tokio::fs::read_dir(&root).await.map_err(FsError::io)?;
        let mut out = Vec::new();
        while let Ok(Some(entry)) = reader.next_entry().await {
            let is_dir = entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false);
            out.push(DirEntry {
                path: entry.path(),
                is_dir,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn info(&self, path: &Path) -> Result<FileInfo, FsError> {
        let path = self.resolve(path)?;
        Ok(match tokio::fs::metadata(&path).await {
            Ok(meta) => FileInfo {
                exists: true,
                is_dir: meta.is_dir(),
                len: meta.len(),
            },
            Err(_) => FileInfo::default(),
        })
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, FsError> {
        self.resolve(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cap-world-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.canonicalize().expect("canonical scratch")
    }

    #[tokio::test]
    async fn text_and_bytes_agree_because_one_is_built_on_the_other() {
        let dir = scratch("agree");
        let fs = LocalFs::new(&dir);
        fs.write_text(Path::new("a.txt"), "héllo").await.unwrap();
        assert_eq!(fs.read_text(Path::new("a.txt")).await.unwrap(), "héllo");
        assert_eq!(
            fs.read_bytes(Path::new("a.txt")).await.unwrap(),
            "héllo".as_bytes()
        );
    }

    #[tokio::test]
    async fn the_default_world_is_transparent() {
        // The property every existing caller depends on: adopting the seam
        // without choosing a world changes nothing. No fence, no extra
        // canonicalisation, no refusal — the path the tool resolved is the path
        // that gets read, exactly as a direct `std::fs` call would have.
        let dir = scratch("transparent");
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "reachable").unwrap();
        std::fs::create_dir_all(dir.join("inner")).unwrap();

        let fenced = LocalFs::new(dir.join("inner"));
        assert!(
            fenced.read_text(&outside).await.is_err(),
            "a fenced world refuses a path outside it"
        );

        let plain = LocalFs::unfenced();
        assert_eq!(
            plain.read_text(&outside).await.unwrap(),
            "reachable",
            "the default world reaches whatever the tool resolved, like std::fs did"
        );
        plain.write_text(&outside, "written").await.unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "written");
        assert_eq!(plain.canonicalize(&outside).await.unwrap(), outside);
    }

    #[tokio::test]
    async fn a_read_only_world_refuses_at_the_boundary() {
        let dir = scratch("ro");
        std::fs::write(dir.join("a.txt"), "before").unwrap();
        let fs = LocalFs::read_only(&dir);

        let err = fs.write_text(Path::new("a.txt"), "after").await.unwrap_err();
        assert!(err.is_denied(), "{err}");
        // The refusal is the world's, so the file is untouched — not "the write
        // was logged and skipped", actually untouched.
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "before");
        // And reading still works: read-only, not inert.
        assert_eq!(fs.read_text(Path::new("a.txt")).await.unwrap(), "before");
    }

    #[tokio::test]
    async fn a_path_escaping_the_root_is_denied_not_followed() {
        let dir = scratch("escape");
        std::fs::create_dir_all(dir.join("inside")).unwrap();
        let fs = LocalFs::new(dir.join("inside"));

        let err = fs.read_text(Path::new("../outside.txt")).await.unwrap_err();
        assert!(err.is_denied(), "{err}");
    }

    #[tokio::test]
    async fn canonicalize_applies_the_same_containment_a_write_would() {
        // The property a gate depends on: it cannot approve a path the world
        // would then refuse, and cannot refuse one the world would accept.
        let dir = scratch("canon");
        let fs = LocalFs::new(&dir);
        std::fs::write(dir.join("a.txt"), "x").unwrap();

        assert_eq!(
            fs.canonicalize(Path::new("a.txt")).await.unwrap(),
            dir.join("a.txt")
        );
        assert!(fs
            .canonicalize(Path::new("../elsewhere"))
            .await
            .unwrap_err()
            .is_denied());
    }

    #[tokio::test]
    async fn edit_refuses_an_ambiguous_match_rather_than_guessing() {
        let dir = scratch("edit");
        let fs = LocalFs::new(&dir);
        fs.write_text(Path::new("a.txt"), "x = 1\nx = 1\n")
            .await
            .unwrap();

        let err = fs
            .edit_text(Path::new("a.txt"), "x = 1", "x = 2")
            .await
            .unwrap_err();
        assert!(err.is_denied(), "{err}");
        assert_eq!(
            fs.read_text(Path::new("a.txt")).await.unwrap(),
            "x = 1\nx = 1\n",
            "an ambiguous edit must change nothing"
        );
    }

    #[tokio::test]
    async fn listing_reports_what_is_there_and_hides_nothing() {
        // The world does not own the skip-list. `list_directory` wants to print
        // `target/ (skipped)` — it cannot, if the world already dropped the entry.
        let dir = scratch("list");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("a.rs"), "").unwrap();
        let fs = LocalFs::new(&dir);

        let entries = fs.list(Path::new(".")).await.unwrap();
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"target".to_string()), "{names:?}");
        assert!(names.contains(&"src".to_string()));
        assert!(names.contains(&"a.rs".to_string()));
        assert!(entries.iter().find(|e| e.path.ends_with("src")).unwrap().is_dir);
        assert!(!entries.iter().find(|e| e.path.ends_with("a.rs")).unwrap().is_dir);
        // One level only — recursion and depth are the caller's policy.
        assert_eq!(names.len(), 3, "{names:?}");
    }

    #[tokio::test]
    async fn info_reports_absence_rather_than_failing() {
        let dir = scratch("info");
        let fs = LocalFs::new(&dir);
        let info = fs.info(Path::new("nope.txt")).await.unwrap();
        assert!(!info.exists);
    }
}
