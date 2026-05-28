//! First-startup install + post-upgrade refresh hooks for the plugin
//! marketplace layer.
//!
//! Two distinct user journeys land here:
//!
//! 1. **Fresh install** — atomcode runs for the first time on a host
//!    that has never run it. The marker file
//!    `$ATOMCODE_HOME/.plugin_bootstrap_v1` does not exist. We `git clone`
//!    the default `atomcode-skills` marketplace and touch the marker.
//!    Failure (no network, no git on PATH, upstream down) is logged
//!    and swallowed — startup proceeds without skills.
//!
//! 2. **Existing user, just upgraded** — `apply_pending_upgrade`
//!    re-exec'd into the new binary and set `ATOMCODE_UPGRADED_FROM`.
//!    We `git pull --ff-only` every installed marketplace so the
//!    skills track the new binary. Same swallowed-failure semantics.
//!
//! The marker file makes (1) a one-time event. If the user later runs
//! `/plugin uninstall atomcode-skills`, the marker stays and we
//! respect their intent — no re-install on subsequent startups. To
//! force a re-bootstrap, the user can `rm $ATOMCODE_HOME/.plugin_bootstrap_v1`.
//!
//! Both functions are best-effort and never propagate errors —
//! atomcode must remain usable on offline machines, in air-gapped
//! corporate environments, on systems without git, etc.

use crate::config::Config;

use super::marketplace::{add_marketplace, list_marketplaces, update_marketplace};
use super::PluginJobEvent;

/// Public git URL for the default skills marketplace. The plugin
/// installer dispatches on the SOURCE field (the URL we cloned from),
/// so this string is the identity of the "default skills" entry.
pub const DEFAULT_SKILLS_URL: &str = "https://atomgit.com/atomgit_atomcode/atomcode-skills.git";

/// Versioned bootstrap marker. Bump the `_v1` suffix when introducing
/// a new bootstrap step (e.g. a second default marketplace, or a
/// post-install migration) so existing users opt into the new run.
const BOOTSTRAP_MARKER_FILENAME: &str = ".plugin_bootstrap_v1";

/// Env var that the upgrade path (`self_update::apply_pending_upgrade`
/// → `re_exec_self`) sets on the new binary so the new session knows
/// it was launched as the result of a version upgrade. We read it
/// non-destructively here — the TUIX event loop owns the eventual
/// `remove_var` so the welcome-screen confirmation still fires.
const UPGRADED_FROM_ENV: &str = "ATOMCODE_UPGRADED_FROM";

/// Entry point for both Plan A (auto-install default skills) and
/// Plan B (auto-update marketplaces after upgrade). Call once at
/// startup AFTER `Config::load` and AFTER any pending self-upgrade has
/// re-exec'd. Synchronous — runs `git` subprocesses inline; budget
/// roughly 1-3 s on a warm path, longer on first install.
///
/// Returns the list of `PluginJobEvent`s the caller should forward to
/// the TUI event loop so the user sees a toast (e.g. "marketplace
/// `atomcode-skills` added at abc1234 (12 plugins)"). The same lines
/// are still `eprintln!`'d for `stderr.log` posterity. No-op cases
/// (marker already present, no marketplaces to refresh, nothing
/// changed under HEAD) return an empty vec.
pub fn run_startup_hooks(config: &Config) -> Vec<PluginJobEvent> {
    let mut events = Vec::new();
    if config.plugin.auto_install_default_skills {
        if let Some(ev) = maybe_install_default_skills() {
            events.push(ev);
        }
    }
    let upgraded = std::env::var(UPGRADED_FROM_ENV).is_ok();
    if upgraded && config.plugin.auto_update_marketplaces {
        events.extend(refresh_installed_marketplaces());
    }
    events
}

fn bootstrap_marker_path() -> std::path::PathBuf {
    // Lives directly under `~/.atomcode/` (the canonical config dir),
    // not nested under `plugins/` — it's a per-user run-state flag,
    // not a plugin asset. Same neighbourhood as
    // `.telemetry_notice_shown`.
    Config::config_dir().join(BOOTSTRAP_MARKER_FILENAME)
}

fn marker_exists() -> bool {
    bootstrap_marker_path().exists()
}

fn touch_marker() {
    let path = bootstrap_marker_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, b"");
}

/// Plan A: clone the default skills marketplace into
/// `$ATOMCODE_HOME/plugins/marketplaces/atomcode-skills/` if (a) the
/// bootstrap marker isn't there yet AND (b) the marketplace isn't
/// already installed. After this attempt — successful or not — the
/// marker is written so the next startup doesn't try again.
///
/// Returns `Some(PluginJobEvent)` when an `add_marketplace` actually
/// fired (succeeded or failed). `None` for the no-op paths (marker
/// exists, or the marketplace was already installed manually).
fn maybe_install_default_skills() -> Option<PluginJobEvent> {
    if marker_exists() {
        return None;
    }

    // Marketplace already present from a prior manual `/plugin install`?
    // Honour it. Just write the marker so we don't try to clone over
    // it next startup.
    let already_installed = list_marketplaces()
        .map(|list| {
            list.iter()
                .any(|m| m.source.eq_ignore_ascii_case(DEFAULT_SKILLS_URL))
        })
        .unwrap_or(false);
    if already_installed {
        touch_marker();
        return None;
    }

    let event = match add_marketplace(DEFAULT_SKILLS_URL) {
        Ok(info) => {
            eprintln!(
                "✓ Auto-installed default skills marketplace `{}` (commit {}).",
                info.name,
                short_commit(&info.git_commit)
            );
            Some(PluginJobEvent::MarketplaceAdded(info))
        }
        Err(e) => {
            let msg = format!(
                "auto-install of default skills marketplace failed: {e}. \
                 Run `/plugin install {DEFAULT_SKILLS_URL}` manually when ready."
            );
            eprintln!("⚠ {msg}");
            Some(PluginJobEvent::Failed {
                op: "auto-install".into(),
                msg,
            })
        }
    };

    // Mark the bootstrap as attempted. Even on failure we don't want
    // to retry on every launch — that turns into a flapping network
    // probe. The user can delete the marker to force a retry.
    touch_marker();
    event
}

/// Plan B: best-effort `git pull --ff-only` on every installed
/// marketplace. Only runs when called from a session that was launched
/// by `apply_pending_upgrade` (caller gates on `ATOMCODE_UPGRADED_FROM`).
///
/// Returns one `PluginJobEvent` per marketplace whose HEAD actually
/// moved, plus one `Failed` event per pull error. No-op pulls (HEAD
/// unchanged) produce no event — keeps the toast lane quiet when
/// there's nothing the user needs to know about.
fn refresh_installed_marketplaces() -> Vec<PluginJobEvent> {
    let mut events = Vec::new();
    let list = match list_marketplaces() {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("could not enumerate marketplaces for auto-update: {e}");
            eprintln!("⚠ {msg}");
            events.push(PluginJobEvent::Failed {
                op: "auto-update".into(),
                msg,
            });
            return events;
        }
    };
    if list.is_empty() {
        return events;
    }
    for entry in list {
        match update_marketplace(&entry.name) {
            Ok(info) => {
                if info.git_commit != entry.git_commit {
                    eprintln!(
                        "✓ Updated marketplace `{}` ({} → {}).",
                        entry.name,
                        short_commit(&entry.git_commit),
                        short_commit(&info.git_commit)
                    );
                    events.push(PluginJobEvent::MarketplaceUpdated(info));
                }
            }
            Err(e) => {
                let msg = format!("auto-update of marketplace `{}` failed: {e}", entry.name);
                eprintln!("⚠ {msg}");
                events.push(PluginJobEvent::Failed {
                    op: "auto-update".into(),
                    msg,
                });
            }
        }
    }
    events
}

fn short_commit(sha: &str) -> &str {
    if sha.len() >= 7 {
        &sha[..7]
    } else {
        sha
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_commit_truncates_long_shas() {
        assert_eq!(short_commit("0123456789abcdef"), "0123456");
    }

    #[test]
    fn short_commit_passes_through_short_input() {
        assert_eq!(short_commit("abc"), "abc");
        assert_eq!(short_commit(""), "");
    }

    #[test]
    fn marker_path_uses_versioned_filename() {
        // We don't assert the exact `~/.atomcode/...` location (depends
        // on $HOME / env), but the suffix should be the versioned
        // marker name so future bootstrap-v2 introductions don't
        // accidentally overwrite v1's marker.
        let p = bootstrap_marker_path();
        assert!(
            p.to_string_lossy().ends_with(BOOTSTRAP_MARKER_FILENAME),
            "marker path must end with versioned filename, got {:?}",
            p
        );
    }
}
