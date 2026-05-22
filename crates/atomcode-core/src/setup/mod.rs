//! Setup wizard — install seed files (skills/commands/hooks/MCP) to `$ATOMCODE_HOME/`.
//!
//! Simplified pipeline: lock → scan → install all seeds → setup-state → report.

pub mod error;
pub mod fs_atomic;
pub mod install;
pub mod lock;
pub mod scan;
pub mod seeds;
pub mod state;
pub mod types;

pub use error::{SetupError, SetupResult};
pub use types::*;

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub project_root: PathBuf,
    pub force: bool,
}

impl RunOptions {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root,
            force: false,
        }
    }
}

use crate::setup::install::{InstalledSummary, ReloadDirective};
use crate::setup::seeds::ensure_seeds_extracted;

/// Simplified pipeline: lock → scan → install all embedded seeds → setup-state → report.
///
/// This is a synchronous function — all operations (lock, scan, install, state write)
/// are blocking. If called from an async context, use `tokio::task::spawn_blocking`.
pub fn run(opts: RunOptions) -> SetupResult<SetupReport> {
    let started = std::time::Instant::now();

    // 1. Lock — RAII; released on function exit / panic.
    let _lock = lock::SetupLock::acquire(&opts.project_root, opts.force).map_err(|e| match e {
        lock::LockError::Held {
            pid,
            start_time,
            host,
        } => SetupError::LockHeld {
            pid,
            start_time,
            host,
        },
        lock::LockError::Io(io) => SetupError::LockIo(io),
    })?;

    // 2. Scan project for signals_hash (used by setup-state).
    let signals = scan::scan(&opts.project_root);

    // 3. Install all embedded seeds (skills + commands + hooks + mcp).
    let seeds_cache_root = crate::config::Config::config_dir();
    let cache_dir = ensure_seeds_extracted(&seeds_cache_root).map_err(SetupError::Other)?;

    let mut txn =
        install::InstalledTxn::new(opts.project_root.clone()).map_err(SetupError::Io)?;
    let mut summary = InstalledSummary::default();

    // Install directory-style skills (e.g., atomcode-automation-recommender/).
    install_directory_skills_from_seeds(&cache_dir, &mut summary, opts.force);

    // Append .gitignore marker for .atomcode/local/.
    if let Err(e) = txn.append_gitignore(&opts.project_root) {
        tracing::warn!("failed to append .gitignore: {e}");
    }

    let _written = txn.commit();

    // 4. Write setup-state.json.
    let state_data = state::SetupState {
        schema_version: state::CURRENT_SCHEMA_VERSION,
        signals_hash: signals.signals_hash.clone(),
        completed_at: chrono::Utc::now(),
        atomcode_version: env!("CARGO_PKG_VERSION").to_string(),
        accepted: summary
            .installed
            .iter()
            .map(|(id, _)| state::RecIdRef {
                kind: format!("{:?}", id.kind).to_lowercase(),
                slug: id.slug.clone(),
            })
            .collect(),
    };
    if let Err(e) = state::save_setup_state(&opts.project_root, &state_data) {
        tracing::warn!("failed to save setup-state.json: {e}");
    }

    Ok(SetupReport {
        summary,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Copy directory-style skills from seeds-cache to $ATOMCODE_HOME/skills/.
/// E.g., `atomcode-automation-recommender/SKILL.md` + `references/`.
///
/// When `force` is true, skills are reinstalled even if the content hash matches
/// (i.e., `--force` forces a clean reinstall, not just a lock bypass).
fn install_directory_skills_from_seeds(
    cache_dir: &std::path::Path,
    summary: &mut InstalledSummary,
    force: bool,
) {
    let seeds_skills = cache_dir.join("skills");
    // Target path must match SkillRegistry::reload's scan path: a single
    // unified config dir (Config::config_dir()) that resolves to
    // ATOMCODE_HOME when set, else $HOME/.atomcode.
    let target_skills = crate::config::Config::config_dir().join("skills");

    let entries = match std::fs::read_dir(&seeds_skills) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").exists() {
            let name = match path.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };
            let dest = target_skills.join(&name);
            let src_hash = compute_dir_hash(&path);

            if dest.exists() {
                // Version check: compare content hash.
                let installed_hash = read_seed_hash(&dest);
                if !force && installed_hash.as_deref() == Some(src_hash.as_str()) {
                    summary.skipped.push((
                        RecId::new(RecKind::Skill, &name),
                        install::SkipReason::AlreadyInstalled,
                    ));
                    continue;
                }
                // Hash differs or --force → remove old and reinstall.
                if force {
                    tracing::info!(skill = %name, "forced reinstall of seed skill");
                } else {
                    tracing::info!(skill = %name, "seed skill updated — reinstalling");
                }
                let _ = std::fs::remove_dir_all(&dest);
            }

            match copy_dir_recursive(&path, &dest) {
                Ok(()) => {
                    write_seed_hash(&dest, &src_hash);
                    summary.installed.push((RecId::new(RecKind::Skill, &name), dest));
                    summary.reload_directives.insert(ReloadDirective::Skill);
                }
                Err(e) => {
                    tracing::warn!("failed to install directory skill {name}: {e}");
                    summary.failed.push((RecId::new(RecKind::Skill, &name), e.to_string()));
                }
            }
        }
    }
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

const SEED_HASH_FILE: &str = ".seed-hash";

/// Compute a content hash of all files in a directory (recursive, sorted).
fn compute_dir_hash(dir: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut paths = Vec::new();
    collect_file_paths(dir, &mut paths);
    paths.sort();
    for p in &paths {
        if let Ok(content) = std::fs::read(p) {
            h.update(p.strip_prefix(dir).unwrap_or(p).to_string_lossy().as_bytes());
            h.update(b"\0");
            h.update(&content);
            h.update(b"\0");
        }
    }
    format!("{:x}", h.finalize())
}

fn collect_file_paths(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip .seed-hash from hash computation to avoid self-reference.
                collect_file_paths(&path, out);
            } else if path.file_name().and_then(|n| n.to_str()) != Some(SEED_HASH_FILE) {
                out.push(path);
            }
        }
    }
}

fn read_seed_hash(dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(dir.join(SEED_HASH_FILE)).ok()
}

fn write_seed_hash(dir: &std::path::Path, hash: &str) {
    let _ = std::fs::write(dir.join(SEED_HASH_FILE), hash);
}

// ── SetupReport ────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SetupReport {
    pub summary: InstalledSummary,
    pub duration_ms: u64,
}

impl SetupReport {
    pub fn render_cli(&self) -> String {
        use crate::i18n::{t, Msg};

        let kind_str = |k: &RecKind| format!("{:?}", k).to_lowercase();

        let mut out = String::new();
        out.push_str(&t(Msg::SetupHeader {
            installed: self.summary.installed.len(),
            skipped: self.summary.skipped.len(),
            failed: self.summary.failed.len(),
            duration_ms: self.duration_ms,
        }));

        if !self.summary.installed.is_empty() {
            out.push_str(&t(Msg::SetupInstalledLabel));
            for (id, path) in &self.summary.installed {
                out.push_str(&t(Msg::SetupInstalledRow {
                    kind: &kind_str(&id.kind),
                    slug: &id.slug,
                    path: &path.display().to_string(),
                }));
            }
        }
        if !self.summary.skipped.is_empty() {
            out.push_str(&t(Msg::SetupSkippedLabel));
            for (id, reason) in &self.summary.skipped {
                out.push_str(&t(Msg::SetupSkippedRow {
                    kind: &kind_str(&id.kind),
                    slug: &id.slug,
                    reason: &format!("{:?}", reason),
                }));
            }
        }
        if !self.summary.failed.is_empty() {
            out.push_str(&t(Msg::SetupFailedLabel));
            for (id, err) in &self.summary.failed {
                out.push_str(&t(Msg::SetupFailedRow {
                    kind: &kind_str(&id.kind),
                    slug: &id.slug,
                    error: err,
                }));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_installed_count() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::locale::Locale::ZhCn);

        let mut sum = InstalledSummary::default();
        sum.installed
            .push((RecId::new(RecKind::Skill, "x"), PathBuf::from("/p/x.md")));
        let report = SetupReport {
            summary: sum,
            duration_ms: 123,
        };
        let rendered = report.render_cli();
        assert!(rendered.contains("1"));
        assert!(rendered.contains("/p/x.md"));
        assert!(rendered.contains("123ms"));
    }

}
