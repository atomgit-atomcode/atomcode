//! Every name this build occupies on the local machine, in one place.
//!
//! The companion to [`crate::endpoints`]: that module owns the addresses a
//! build *talks to*, this one owns the names it *takes* — the config dir, the
//! ports it listens on, the executables it installs, the scratch files
//! self-update leaves beside them.
//!
//! # Why one module
//!
//! These are shared contracts between crates that cannot see each other, and
//! before this they were maintained by copying a literal:
//!
//! - the daemon port was written twice, in `atomcode-daemon`'s `DEFAULT_PORT`
//!   and again as a clap `default_value` string in `atomcode-cli`. (The webui
//!   port next to it already did this correctly, via a shared `const` — the
//!   daemon port was simply missed.)
//! - the three self-update scratch filenames were written in `atomcode-updater`
//!   (which creates them), in the uninstaller's scan (which must delete them),
//!   and again in `uninstall.sh` / `uninstall.ps1`. A rename in one place
//!   leaves the others silently sweeping nothing.
//! - the release asset name is built in one function and pattern-matched in
//!   another; they must agree or stale downloads are never reaped.
//! - the uninstaller's "is this one of ours?" process filter hardcoded the two
//!   executable names with no link to `[[bin]] name`.
//!
//! Every value here keeps exactly the value it replaced, so this is a
//! consolidation and not a change.
//!
//! # For a distribution
//!
//! An internal build that must coexist with the public one — same machine,
//! side by side — has to take a different set of these names, or the two
//! overwrite each other's binaries, fight over ports, and uninstall each
//! other's files. Replacing this file retargets all of it at once, the same
//! way replacing [`crate::endpoints`] retargets the addresses.

use std::ffi::OsString;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Config tree
// ---------------------------------------------------------------------------

/// The env var the host reads to relocate the user-level tree.
///
/// Only hosts read it (`Config::config_dir`). Libraries are handed their
/// directories as `atomcode_capabilities::ProductDirs` and never look at the
/// environment, so a program that imports them picks its own tree by passing
/// it, not by exporting this variable before every entry point.
pub const HOME_ENV: &str = "ATOMCODE_HOME";

/// The config/data dir, relative to the user's home, when [`HOME_ENV`] is unset.
///
/// The ONLY place the product directory is spelled (`gates/product-dir.sh`
/// holds everyone else to that). A distribution renames its tree here.
pub const HOME_DIR_NAME: &str = ".atomcode";

/// The per-project dir under a working directory: `<project>/.atomcode` holds
/// its skills, commands, plugins, memory, setup state and session pin.
///
/// The same name as [`HOME_DIR_NAME`] because a distribution has so far always
/// wanted the two to move together; they are separate constants so one that
/// does not can say so.
///
/// A host hands both to the libraries once:
/// `ProductDirs::new(Config::config_dir(), PROJECT_DIR_NAME)`.
pub const PROJECT_DIR_NAME: &str = HOME_DIR_NAME;

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// The API daemon the VS Code extension connects to.
pub const DAEMON_PORT: u16 = 13456;

/// `atomcode webui`. Deliberately not [`DAEMON_PORT`]: sharing it would have
/// the webui steal the port the extension's daemon expects, leaving the
/// extension with 401s or no response at all.
pub const WEBUI_PORT: u16 = 13457;

/// `/app` remote-access pairing.
pub const APP_PORT: u16 = 13458;

// ---------------------------------------------------------------------------
// Executables and install locations
// ---------------------------------------------------------------------------

/// This distribution's own executables, as they appear in a process list
/// (without any platform suffix).
///
/// Used by `atomcode uninstall` to decide which running processes are its own
/// and must be stopped first. Anything not listed here is somebody else's
/// process and is left alone.
pub const PROCESS_NAMES: &[&str] = &["atomcode", "atomcode-daemon"];

/// Windows install dir, under `%LOCALAPPDATA%`. Mirrors `install.ps1`.
pub const WINDOWS_INSTALL_DIR: &str = "AtomCode";

/// Leading component of a published release asset
/// (`<prefix>-<version>-<target>`).
///
/// Both built and matched: self-update composes the name to download, and its
/// housekeeping pass reaps leftovers by this prefix. They have to agree.
pub const RELEASE_ASSET_PREFIX: &str = "atomcode";

/// Prefix for the scratch files self-update leaves NEXT TO the executable —
/// `<prefix>.download`, `<prefix>.rolling`, `<prefix>.writable-probe`.
///
/// These live in the install dir, not the config tree, so two distributions
/// installed under one prefix collide here even when everything else about
/// them is separate.
pub const UPDATE_TEMP_PREFIX: &str = ".atomcode";

/// `<UPDATE_TEMP_PREFIX>.download` — partially fetched upgrade.
pub fn update_download_name() -> String {
    format!("{UPDATE_TEMP_PREFIX}.download")
}

/// `<UPDATE_TEMP_PREFIX>.rolling` — the slot a running executable is renamed
/// into so the upgrade can take its place (Windows permits renaming a running
/// image, not overwriting it).
pub fn update_rolling_name() -> String {
    format!("{UPDATE_TEMP_PREFIX}.rolling")
}

/// Whether `name` is a rename slot self-update left behind: the plain
/// [`update_rolling_name`], or one suffixed `-<n>` — taken when the plain one
/// was still held by a process running the image renamed into it (Windows
/// refuses to replace a running executable, so the swap moves aside instead).
pub fn is_update_rolling_name(name: &str) -> bool {
    let plain = update_rolling_name();
    name == plain
        || name
            .strip_prefix(&plain)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// `<UPDATE_TEMP_PREFIX>.writable-probe` — touched to test whether the install
/// dir is writable before an upgrade is attempted.
pub fn update_probe_name() -> String {
    format!("{UPDATE_TEMP_PREFIX}.writable-probe")
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// Materialise the default config dir into [`HOME_ENV`] when it is unset.
///
/// Call once, first thing in `main`, before anything reads config. The host's
/// own resolver (`Config::config_dir`, from which it builds the
/// `ProductDirs` it hands the libraries) then agrees with what child
/// processes — hooks, MCP servers, a spawned `atomcodex` — inherit, rather
/// than each re-deriving it from its own environment. Libraries do not read
/// the variable at all.
///
/// This is a no-op in effect: it writes precisely the value the resolver
/// already fall back to, which [`tests::bootstrapping_matches_the_resolver_fallback`]
/// pins against `Config::config_dir` itself. What it buys is a single place for
/// a distribution to answer the question differently.
///
/// An already-set [`HOME_ENV`] is left alone, so an explicit setting — a user's
/// export, a test harness — still wins.
pub fn bootstrap_home() {
    if let Some(value) = default_home(std::env::var_os(HOME_ENV), crate::util::real_home_dir()) {
        std::env::set_var(HOME_ENV, value);
    }
}

/// Pure core of [`bootstrap_home`]. `None` means "leave the variable alone".
///
/// An empty value counts as unset, matching `Config::resolve_config_dir` — a
/// stray `ATOMCODE_HOME=` would otherwise resolve the tree to the process's
/// working directory.
fn default_home(existing: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    if existing.is_some_and(|value| !value.is_empty()) {
        return None;
    }
    Some(home?.join(HOME_DIR_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn an_explicit_home_is_left_alone() {
        assert_eq!(
            default_home(Some("/opt/ac".into()), Some(PathBuf::from("/home/u"))),
            None
        );
    }

    #[test]
    fn an_empty_home_is_treated_as_unset() {
        // Matches `Config::resolve_config_dir`, which filters empties too;
        // honouring `ATOMCODE_HOME=` would put the whole tree in the cwd.
        assert_eq!(
            default_home(Some("".into()), Some(PathBuf::from("/home/u"))),
            Some(PathBuf::from("/home/u/.atomcode"))
        );
    }

    #[test]
    fn no_resolvable_home_leaves_the_variable_unset() {
        // Nothing to materialise. The resolvers keep their own cwd-relative
        // degradation rather than having one imposed here.
        assert_eq!(default_home(None, None), None);
    }

    /// The reason this is safe to call unconditionally: the value it writes is
    /// the one the resolvers already compute for themselves, so bootstrapping
    /// cannot move anybody's config tree.
    #[test]
    fn bootstrapping_matches_the_resolver_fallback() {
        let home = Path::new("/home/u");
        let bootstrapped = default_home(None, Some(home.to_path_buf())).unwrap();
        let resolver_fallback =
            crate::config::Config::resolve_config_dir(None, Some(home.to_path_buf()));
        assert_eq!(
            bootstrapped, resolver_fallback,
            "bootstrap_home must write exactly what the resolvers fall back to"
        );

        // …and once written, the resolver returns it unchanged.
        assert_eq!(
            crate::config::Config::resolve_config_dir(
                Some(bootstrapped.to_string_lossy().into_owned()),
                Some(home.to_path_buf()),
            ),
            resolver_fallback
        );
    }

    #[test]
    fn a_rename_slot_is_the_plain_name_or_one_numbered_after_it() {
        let plain = update_rolling_name();
        assert!(is_update_rolling_name(&plain));
        assert!(is_update_rolling_name(&format!("{plain}-4242")));
        for not in [
            format!("{plain}-"),
            format!("{plain}-x1"),
            format!("{plain}.bak"),
            update_download_name(),
            "atomcode.exe".to_string(),
        ] {
            assert!(!is_update_rolling_name(&not), "{not}");
        }
    }

    #[test]
    fn the_update_scratch_names_share_one_prefix() {
        // The updater creates these and the uninstaller deletes them; they are
        // only ever correct together.
        for name in [
            update_download_name(),
            update_rolling_name(),
            update_probe_name(),
        ] {
            assert!(
                name.starts_with(UPDATE_TEMP_PREFIX),
                "{name} must derive from UPDATE_TEMP_PREFIX"
            );
        }
    }

    #[test]
    fn the_ports_are_distinct() {
        // Two of these on one port means the webui or /app silently answers
        // requests the extension's daemon was meant to get.
        let ports = [DAEMON_PORT, WEBUI_PORT, APP_PORT];
        for (i, a) in ports.iter().enumerate() {
            for b in &ports[i + 1..] {
                assert_ne!(a, b, "ports must not collide");
            }
        }
    }
}
