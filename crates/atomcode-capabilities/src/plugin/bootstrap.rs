//! First-startup install + per-startup sync hooks for the plugin
//! marketplace layer.
//!
//! Two distinct user journeys land here:
//!
//! 1. **Fresh install** — atomcode runs for the first time on a host
//!    that has never run it. The marker file
//!    `$ATOMCODE_HOME/.plugin_bootstrap_v2` does not exist. We `git clone`
//!    the official `atomcode-plugins-official` marketplace and touch the
//!    marker.
//!    Failure (no network, no git on PATH, upstream down) is logged
//!    to `$ATOMCODE_HOME/stderr.log` and swallowed — startup proceeds
//!    without skills. Direct file writes avoid stderr-fd leakage into
//!    the TUI input box on Windows (raw-mode cursor sits at the prompt).
//!
//! 2. **Every startup** — we `git pull --ff-only` every installed
//!    marketplace so the plugins stay in sync with the remote.
//!    This runs on every launch (not just upgrades) so that new
//!    plugins published to official marketplaces are discovered
//!    promptly. Same swallowed-failure semantics.
//!
//! The marker file makes (1) a one-time event. If the user later runs
//! `/plugin marketplace remove atomcode`, the marker stays and we
//! respect their intent — no re-install on subsequent startups. To
//! force a re-bootstrap, the user can `rm $ATOMCODE_HOME/.plugin_bootstrap_v2`.
//!
//! Both functions are best-effort and never propagate errors —
//! atomcode must remain usable on offline machines, in air-gapped
//! corporate environments, on systems without git, etc.

use atomcode_config::config::Config;

use super::installer::{install, list_installed};
use super::marketplace::{add_marketplace, list_marketplaces, update_marketplace};
use super::PluginJobEvent;
use std::io::Write;

/// Append a diagnostic line to `$ATOMCODE_HOME/stderr.log`.
///
/// This replaces the previous `eprintln!` approach, which leaked into
/// the TUI input box on Windows when the terminal was already in raw
/// mode (the cursor sits at the prompt footer, so stderr-fd writes
/// rendered "inside" the input area). Direct file I/O is cross-platform
/// and avoids the fd-sharing problem entirely.
///
/// Best-effort: if the home dir can't be resolved or the file can't
/// be opened, silently drop the line — bootstrap failures are already
/// reported through `PluginJobEvent` channels.
fn log_to_file(msg: &str) {
    let home = Config::config_dir();
    if std::fs::create_dir_all(&home).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("stderr.log"))
    else {
        return;
    };
    let _ = std::io::Write::write_all(&mut file, msg.as_bytes());
    let _ = file.write_all(b"\n");
}

/// Public git URLs for the default marketplaces — the official AtomCode
/// plugin registry and the legacy AtomCode skills bag. The plugin
/// installer dispatches on the SOURCE field (the URL we cloned from),
/// so each URL here is the identity of a bootstrapped "default" entry.
/// To add another default marketplace in a future release, append its
/// URL to the list and bump BOOTSTRAP_MARKER_FILENAME so existing users
/// re-bootstrap.
pub const DEFAULT_SKILLS_URLS: &[&str] = &[
    "https://atomgit.com/atomgit_atomcode/atomcode-plugins-official.git",
    "https://atomgit.com/atomgit_atomcode/atomcode-skills.git",
];

/// Subset of [`DEFAULT_SKILLS_URLS`]: only plugins from marketplaces
/// listed here are auto-installed (both on fresh bootstrap and after
/// post-upgrade `git pull`). `atomcode-plugins-official` is purposely
/// excluded — it is registered for discoverability, not force-installed.
const DEFAULT_AUTO_INSTALL_URLS: &[&str] =
    &["https://atomgit.com/atomgit_atomcode/atomcode-skills.git"];

/// Versioned bootstrap marker. Bumped v1 → v2 when the default
/// marketplace was repointed from the legacy `atomcode-skills` bag to the
/// official `atomcode-plugins-official` registry, so existing users
/// re-bootstrap onto the new default exactly once. Bump again when
/// introducing a future bootstrap step (e.g. a second default marketplace).
const BOOTSTRAP_MARKER_FILENAME: &str = ".plugin_bootstrap_v2";

/// Entry point for both Plan A (auto-install default skills) and
/// Plan B (sync marketplaces on every startup). Call once at
/// startup AFTER `Config::load` and AFTER any pending self-upgrade has
/// re-exec'd. Synchronous — runs `git` subprocesses inline; budget
/// roughly 1-3 s on a warm path, longer on first install.
///
/// Returns the list of `PluginJobEvent`s the caller should forward to
/// the TUI event loop so the user sees a toast (e.g. "marketplace
/// `atomcode` added at abc1234 (3 plugins)"). Diagnostic lines are
/// written to `$ATOMCODE_HOME/stderr.log` for posterity. No-op cases
/// (marker already present, no marketplaces to refresh, nothing
/// changed under HEAD) return an empty vec.
pub fn run_startup_hooks(config: &Config) -> Vec<PluginJobEvent> {
    let mut events = Vec::new();

    // Early check: if git is not available, skip both auto-install and
    // auto-update entirely and emit a single friendly message instead of
    // per-marketplace spawn failures that confuse users on machines without
    // git on PATH (common on macOS GUI-launched processes that don't inherit
    // the user's shell profile).
    if super::marketplace::find_git().is_err() {
        log_to_file(
            "⚠ git is not installed or not on PATH. \
             Plugin marketplace auto-install and auto-update are disabled. \
             Install git (e.g. `xcode-select --install` on macOS, \
             `sudo apt install git` on Ubuntu) and restart AtomCode.",
        );
        events.push(PluginJobEvent::GitNotFound);
        return events;
    }

    if config.plugin.auto_install_default_skills {
        events.extend(maybe_install_default_skills());
    }
    if config.plugin.auto_update_marketplaces
        && !atomcode_config::config::offline::is_offline_active()
    {
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

fn should_auto_install(source_url: &str) -> bool {
    DEFAULT_AUTO_INSTALL_URLS
        .iter()
        .any(|u| u.eq_ignore_ascii_case(source_url))
}

/// Plan A: clone the default plugin marketplaces into
/// `$ATOMCODE_HOME/plugins/marketplaces/<name>/` and install every
/// plugin listed in their manifests. Iterates [`DEFAULT_SKILLS_URLS`].
/// After this attempt — successful or not — the marker is written so
/// the next startup doesn't try again.
///
/// If a marketplace is already installed (from a prior session or a
/// manual `/plugin marketplace add`) but its plugins haven't been
/// installed yet, this bootstraps those plugins too — the marketplace
/// clone is skipped but the install loop still runs.
///
/// Returns one `PluginJobEvent` per marketplace/plugin actually
/// installed, or one `Failed` event per failure.
fn maybe_install_default_skills() -> Vec<PluginJobEvent> {
    if marker_exists() {
        return vec![];
    }

    // Fresh clone vs. already-installed marketplace: the latter was
    // cloned by a prior bootstrap that didn't call `install()` (before
    // we added the auto-install loop), or by a manual
    // `/plugin marketplace add`. Either way, the marketplace entry
    // exists but its plugins may not — we still need to run the install
    // loop on the already-installed entry.
    let installed = list_marketplaces().unwrap_or_default();

    let mut events = Vec::new();

    for url in DEFAULT_SKILLS_URLS {
        // Marketplace already installed? Use the existing entry.
        let already = installed
            .iter()
            .find(|m| m.source.eq_ignore_ascii_case(url));

        if let Some(mp) = already {
            if should_auto_install(url) {
                install_plugins_from_marketplace(&mut events, &mp.name, &mp.plugins);
            }
            continue;
        }

        match add_marketplace(url) {
            Ok(info) => {
                let mp_name = info.name.clone();
                let plugins = info.plugins.clone();
                log_to_file(&format!(
                    "✓ Auto-installed plugin marketplace `{}` (commit {}).",
                    mp_name,
                    short_commit(&info.git_commit)
                ));
                events.push(PluginJobEvent::MarketplaceAdded(info));
                if should_auto_install(url) {
                    install_plugins_from_marketplace(&mut events, &mp_name, &plugins);
                }
            }
            Err(e) => {
                let msg = auto_install_failure_msg(url, &e);
                log_to_file(&format!("⚠ {msg}"));
                events.push(PluginJobEvent::Failed {
                    op: "auto-install".into(),
                    msg,
                });
            }
        }
    }

    // Mark the bootstrap as attempted. Even on failure we don't want
    // to retry on every launch — that turns into a flapping network
    // probe. The user can delete the marker to force a retry.
    touch_marker();
    events
}

/// Build the message for a failed marketplace auto-install. The actionable
/// recovery hint (`/plugin marketplace add …`) is placed FIRST, before the
/// `{e:#}` error chain — the driver renders this as a CALM one-line yellow
/// Warning via `msg.lines().next()`, and `{e:#}` is often multi-line (git
/// stderr), so anything after it would be dropped. Keeping the hint on line 1
/// guarantees the user still sees how to recover.
fn auto_install_failure_msg(url: &str, err: &anyhow::Error) -> String {
    format!(
        "marketplace `{url}` not installed yet — run \
         `/plugin marketplace add {url}` when ready. Cause: {err:#}"
    )
}

/// Install each plugin from a marketplace's manifest. Failures for
/// individual plugins are logged but swallowed so one bad plugin
/// doesn't block the rest.
fn install_plugins_from_marketplace(
    events: &mut Vec<PluginJobEvent>,
    mp_name: &str,
    plugins: &[String],
) {
    for plugin in plugins {
        match install(plugin, mp_name, super::state::InstallScope::User) {
            Ok(pi) => {
                log_to_file(&format!(
                    "  ✓ Installed plugin `{}@{}` from marketplace `{mp_name}`.",
                    pi.plugin, pi.marketplace
                ));
                events.push(PluginJobEvent::PluginInstalled(pi));
            }
            Err(e) => {
                // AlreadyInstalledError is benign — the plugin was
                // registered during an earlier bootstrap attempt (e.g.
                // re-run after deleting the marker).
                let msg = format!("auto-install of plugin `{plugin}@{mp_name}` failed: {e:#}");
                log_to_file(&format!("  ⚠ {msg}"));
                events.push(PluginJobEvent::Failed {
                    op: "auto-install-plugin".into(),
                    msg,
                });
            }
        }
    }
}

/// Plan B: best-effort `git pull --ff-only` on every installed
/// marketplace. Runs on **every startup** (not just upgrades) so that
/// new plugins published to official marketplaces are discovered
/// promptly.
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
            let msg = format!("could not enumerate marketplaces for auto-update: {e:#}");
            log_to_file(&format!("⚠ {msg}"));
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
                    let is_auto = should_auto_install(&entry.source);
                    let name = info.name.clone();
                    let plugins = info.plugins.clone();
                    log_to_file(&format!(
                        "✓ Updated marketplace `{}` ({} → {}).",
                        entry.name,
                        short_commit(&entry.git_commit),
                        short_commit(&info.git_commit)
                    ));
                    events.push(PluginJobEvent::MarketplaceUpdated(info));
                    if is_auto {
                        // Only install plugins not already present — avoids
                        // AlreadyInstalledError noise on every upgrade.
                        let installed = list_installed().unwrap_or_default();
                        let installed_names: std::collections::HashSet<&str> =
                            installed.iter().map(|i| i.plugin.as_str()).collect();
                        let new_plugins: Vec<String> = plugins
                            .iter()
                            .filter(|p| !installed_names.contains(p.as_str()))
                            .cloned()
                            .collect();
                        if !new_plugins.is_empty() {
                            install_plugins_from_marketplace(&mut events, &name, &new_plugins);
                        }
                    }
                }
            }
            Err(e) => {
                let msg = format!("auto-update of marketplace `{}` failed: {e:#}", entry.name);
                log_to_file(&format!("⚠ {msg}"));
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
    fn auto_install_failure_msg_keeps_recovery_hint_on_first_line() {
        // The driver renders this as a one-line Warning (`msg.lines().next()`),
        // and `{e:#}` is often multi-line git stderr — the actionable
        // `/plugin marketplace add` hint MUST survive on line 1.
        let err = anyhow::anyhow!("git clone failed")
            .context("stderr line 1\nstderr line 2\nstderr line 3");
        let msg = auto_install_failure_msg("https://atomgit.com/x/y.git", &err);
        let first = msg.lines().next().unwrap();
        assert!(
            first.contains("/plugin marketplace add https://atomgit.com/x/y.git"),
            "recovery hint must be on the first line: {first:?}"
        );
        assert!(first.contains("when ready"), "first line: {first:?}");
        // The error detail is still present (later in the message) — nothing lost.
        assert!(
            msg.contains("stderr line 3"),
            "full error retained: {msg:?}"
        );
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
