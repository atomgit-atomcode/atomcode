// crates/atomcode-tuix/src/input/history.rs

use std::fs;
use std::io;
use std::path::PathBuf;

pub const HISTORY_MAX: usize = 1000;

pub struct History {
    path: PathBuf,
    entries: Vec<String>,
}

impl History {
    pub fn load<P: Into<PathBuf>>(path: P) -> Self {
        let path = path.into();
        let entries = fs::read_to_string(&path)
            .ok()
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.to_string())
                    .collect()
            })
            .unwrap_or_default();
        Self { path, entries }
    }

    /// Default history path: `~/.atomcode/history` on Unix,
    /// `%USERPROFILE%\.atomcode\history` on Windows (or a tempdir
    /// fallback if home is unknown).
    pub fn default_path() -> Option<PathBuf> {
        Some(crate::platform::history_path())
    }

    pub fn entries(&self) -> &Vec<String> {
        &self.entries
    }

    pub fn push(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        if self.entries.last() == Some(&line) {
            return;
        }
        self.entries.push(line);
        if self.entries.len() > HISTORY_MAX {
            let drop = self.entries.len() - HISTORY_MAX;
            self.entries.drain(..drop);
        }
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = self.entries.join("\n");
        fs::write(&self.path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_nonexistent_returns_empty() {
        let dir = tempdir().unwrap();
        let h = History::load(dir.path().join("hist"));
        assert_eq!(h.entries(), &Vec::<String>::new());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        let mut h = History::load(&path);
        h.push("one".into());
        h.push("two".into());
        h.save().unwrap();

        let h2 = History::load(&path);
        assert_eq!(h2.entries(), &vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn duplicate_consecutive_collapsed() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push("x".into());
        h.push("x".into());
        h.push("y".into());
        assert_eq!(h.entries(), &vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn capped_at_max_entries() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        for i in 0..2000 {
            h.push(format!("cmd{}", i));
        }
        assert!(h.entries().len() <= HISTORY_MAX);
        assert!(!h.entries().iter().any(|s| s == "cmd0"));
    }

    #[test]
    fn empty_entries_ignored() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push("".into());
        h.push("  ".into());
        h.push("real".into());
        assert_eq!(h.entries(), &vec!["real".to_string()]);
    }
}
