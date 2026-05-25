//! UI state persisted between sessions. Currently: scrollbar visibility.
//! Stored at `$ATOMCODE_HOME/ui-state.toml`. Load/save are best-effort —
//! missing file or parse error returns default (everything false).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiState {
    #[serde(default)]
    pub ui: UiSection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiSection {
    #[serde(default)]
    pub show_scrollbar: bool,
}

fn ui_state_path() -> Option<PathBuf> {
    let home = std::env::var_os("ATOMCODE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".atomcode")))?;
    Some(home.join("ui-state.toml"))
}

pub fn load() -> UiState {
    let Some(path) = ui_state_path() else { return UiState::default(); };
    let Ok(text) = std::fs::read_to_string(&path) else { return UiState::default(); };
    toml::from_str(&text).unwrap_or_default()
}

pub fn save(state: &UiState) {
    let Some(path) = ui_state_path() else { return; };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(text) = toml::to_string(state) else {
        crate::tuix_trace!("UI", "ui-state serialize failed");
        return;
    };
    // Atomic write: write to `.tmp` sibling, then rename. `std::fs::
    // rename` is atomic on POSIX (same filesystem) and on Windows
    // (since 1.5) — readers either see the old file or the new file,
    // never a truncated partial. A crash mid-`write_all` to the .tmp
    // file leaves the original ui-state.toml intact. Same fix every
    // serious config-persistence path needs and that `serial_test`
    // hack in tests was papering over for the env-var race only.
    let tmp = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, text) {
        crate::tuix_trace!("UI", "ui-state tmp write failed: {}", e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        crate::tuix_trace!("UI", "ui-state rename failed: {}", e);
        // best-effort cleanup of orphan .tmp
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use tempfile::TempDir;
    use serial_test::serial;

    #[test]
    #[serial]
    fn ui_state_round_trip_via_atomcode_home() {
        let td = TempDir::new().unwrap();
        env::set_var("ATOMCODE_HOME", td.path());
        let mut s = UiState::default();
        s.ui.show_scrollbar = true;
        save(&s);
        let loaded = load();
        assert!(loaded.ui.show_scrollbar);
    }

    #[test]
    #[serial]
    fn ui_state_missing_file_returns_default() {
        let td = TempDir::new().unwrap();
        env::set_var("ATOMCODE_HOME", td.path());
        let loaded = load();
        assert!(!loaded.ui.show_scrollbar);
    }
}
