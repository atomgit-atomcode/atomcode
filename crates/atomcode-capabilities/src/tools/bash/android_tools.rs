//! What this device's shell can actually run — and telling the model that.
//!
//! The `bash` tool's description used to carry a hard-coded list of what an
//! Android phone lacks ("`git`, `ripgrep`, `node`/`npm`, `python3`, `jq`,
//! `sed -i` and many coreutils are absent"). It was wrong in both
//! directions, and the model believed it:
//!
//! * `git` is bundled into the APK and IS present (the host links it into
//!   `<data>/bin` and puts that directory on `PATH`);
//! * `sed -i`, `awk`, `grep`, `find`, `tar`, `unzip`, `sort`, `cut`, `tr`,
//!   `wc`, `xargs`, `diff`, `patch`, `sha256sum`, `xxd`, `strings`, … are all
//!   in `/system/bin` (toybox) — `sed -i` included, which the description
//!   explicitly denied.
//!
//! A description that lies about the environment is worse than no
//! description: the model refuses to use tools that work, and offers to
//! install ones that cannot be installed. So the list is probed from the real
//! `PATH` instead of being asserted.
//!
//! The probe reads the filesystem only, so it is cheap, needs no subprocess,
//! and is unit-testable off-device.

use std::path::{Path, PathBuf};

/// The command names worth reporting to the model.
///
/// Deliberately a short, practical list: what an agent actually reaches for
/// when working in a repository, plus the interpreters it might assume are
/// there. Reporting every toybox applet would bloat the description without
/// telling the model anything it can act on.
pub const TOOL_NAMES: &[&str] = &[
    // Text and stream processing (toybox provides these on Android).
    "sed",
    "awk",
    "grep",
    "cut",
    "sort",
    "uniq",
    "tr",
    "wc",
    "head",
    "tail",
    "xargs",
    "diff",
    "patch",
    "tee",
    // Files and archives.
    "find",
    "tar",
    "unzip",
    "gzip",
    "sha256sum",
    "xxd",
    "strings",
    // Build and version control.
    "git",
    "make",
    "gcc",
    "cc",
    "clang",
    "cargo",
    // Interpreters and network tools.
    "node",
    "npm",
    "python3",
    "python",
    "perl",
    "jq",
    "rg",
    "curl",
    "wget",
];

/// Whether `dir/name` is a file this shell could execute.
///
/// The executable bit is checked where the platform has one: a file the
/// shell cannot run is not a tool, and reporting it as one sends the model
/// into a command that fails.
fn is_executable(dir: &Path, name: &str) -> bool {
    let path = dir.join(name);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The tools from `names` that the given `PATH` can resolve.
///
/// `PATH` is passed in rather than read from the environment so the function
/// stays pure and testable.
pub fn resolve_from_path(names: &[&'static str], path_entries: &[PathBuf]) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    for name in names {
        if path_entries.iter().any(|dir| is_executable(dir, name)) {
            found.push(*name);
        }
    }
    found
}

/// The `PATH` the shell will use, as directories.
///
/// This is the daemon process's own `PATH`, which the shell children inherit
/// — and the Android host has already prepended the bundled git's directory
/// to it before the runtime starts.
pub fn path_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default()
}

/// The tools this device can run, in the order they are worth mentioning.
pub fn available_tools() -> Vec<&'static str> {
    resolve_from_path(TOOL_NAMES, &path_entries())
}

/// Commands in the product's own bin directory that the fixed list does not
/// cover: runtime packages (`cpython`, `lua`, `sqlite3`) and the helper APK's
/// tools (`make`, `cc`, `tcc`).
///
/// They are symlinks in the same directory that holds `pluginhost`, so that
/// directory is the marker for "our own bin". Without them in the description
/// the model never learns they exist — it keeps reaching for `python3` even
/// after a CPython package was installed on purpose.
///
/// `PATH` is passed in so this stays pure and testable.
pub fn extra_commands_in(path_entries: &[PathBuf]) -> Vec<String> {
    let Some(bin) = path_entries
        .iter()
        .find(|dir| dir.join("pluginhost").exists())
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(bin) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "pluginhost" || TOOL_NAMES.contains(&name.as_str()) {
            continue;
        }
        // Our bin holds symlinks and nothing else; requiring one keeps stray
        // files out of the description.
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// [`extra_commands_in`] against the shell's real `PATH`.
pub fn extra_commands() -> Vec<String> {
    extra_commands_in(&path_entries())
}

/// The paragraph describing the device's toolset: what IS there (so the model
/// uses it) and what is NOT (so the model says so plainly instead of
/// promising to install it).
pub fn describe_toolset(available: &[&'static str]) -> String {
    let present = available
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let missing = TOOL_NAMES
        .iter()
        .filter(|name| !available.contains(name))
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "The toolset is limited. These commands ARE available on this device: {present}. \
         These are NOT available: {missing}. Do not promise to install a missing one — \
         there is no package manager here (runtimes and plugins come from the app's Plugin \
         page, and `pip` covers Python libraries). Check with `command -v <tool>` before \
         relying on one, and prefer read_file / grep / glob / edit_file over their shell \
         equivalents."
    )
}

/// What the installed runtime packages can actually do, as opposed to what the
/// command names suggest.
///
/// Two failures this prevents, both observed: a model that rewrites HTTP by hand
/// because it does not know `pip` works, and a model that gives up on compiling C
/// because `tcc` alone fails on the first `#include` (it needs the header pack and
/// two mandatory flags). Every sentence here is conditional on the package being
/// present, so nothing goes stale when it is removed.
pub fn capability_notes(extras: &[String], headers_installed: bool) -> String {
    let has = |name: &str| extras.iter().any(|extra| extra == name);
    let mut notes = String::new();

    if has("cpython") {
        notes.push_str(
            " `python3` is CPython 3.12 here: `python3 -m pip install --target \
             $HOME/python-packages <pkg>` works for pure-Python wheels (a mainland mirror is \
             preset), `sqlite3` has FTS5/RTree/math functions built in, and re-executing the \
             interpreter — `subprocess.run([sys.executable, …])`, `multiprocessing` spawn — \
             works. Native wheels from PyPI will not load (they link glibc).",
        );
    }
    if has("lua") {
        notes.push_str(
            " Lua can `require` C modules (they resolve against the interpreter), and \
             `lua-cjson` ships with it.",
        );
    }
    if has("tcc") && headers_installed {
        notes.push_str(
            " `tcc` can compile C, but only with the header pack: `tcc -shared -fPIC \
             -nostdlib -DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD -I$HOME/packages/cheaders/include \
             -I$HOME/packages/cheaders/builtin -L$HOME/packages/cheaders/lib -lc -o out.so \
             in.c` — the output is a shared object loadable by `ctypes` or Lua, never an \
             executable (both shared storage and app data are mounted noexec).",
        );
    }
    notes
}

/// Whether a data-only package directory exists under the product tree.
///
/// Derived from the bin directory that holds `pluginhost` — the same marker
/// `extra_commands_in` uses — so no product path is hardcoded here.
fn data_package_installed(name: &str) -> bool {
    path_entries()
        .iter()
        .find(|dir| dir.join("pluginhost").exists())
        .and_then(|bin| {
            bin.parent()
                .map(|product| product.join("packages").join(name))
        })
        .map(|dir| dir.is_dir())
        .unwrap_or(false)
}

/// The toolset paragraph, computed once per process.
///
/// The device's toolset cannot change while the app runs (the bundled tools
/// are linked in at start-up), so one probe is enough; the description is
/// rebuilt for every request the model makes.
///
/// Not `#[cfg]`-gated: the caller reaches this through `cfg!(target_os =
/// "android")`, which is a runtime boolean, so the function has to exist on
/// every platform. Only the Android build ever calls it.
pub fn toolset_paragraph() -> &'static str {
    use std::sync::OnceLock;
    static PARAGRAPH: OnceLock<String> = OnceLock::new();
    PARAGRAPH.get_or_init(|| {
        let mut text = describe_toolset(&available_tools());
        let extras = extra_commands();
        if !extras.is_empty() {
            let list = extras
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            // Naming them plainly is the whole point: a package can be
            // installed and usable while the model has no idea it exists.
            text.push_str(&format!(
                " Installed on top of that, callable the same way: {list}. A name that is \
                 also on the list above may now be provided by one of these instead of the \
                 built-in tool — check `command -v <name>` before assuming which."
            ));
            // A package can be installed and still be used the wrong way (pip
            // that the model does not know about, tcc that fails on the first
            // `#include`). Say what the installed pieces actually do.
            text.push_str(&capability_notes(
                &extras,
                data_package_installed("cheaders"),
            ));
        }
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_notes_only_describe_what_is_actually_installed() {
        assert!(capability_notes(&[], false).is_empty());

        let extras = vec!["cpython".to_string(), "lua".to_string(), "tcc".to_string()];
        let described = capability_notes(&extras, true);
        assert!(described.contains("pip install"), "{described}");
        assert!(described.contains("FTS5"), "{described}");
        assert!(described.contains("spawn"), "{described}");
        assert!(described.contains("lua-cjson"), "{described}");
        assert!(
            described.contains("BIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD"),
            "{described}"
        );
        assert!(described.contains("noexec"), "{described}");

        // Without the header package the tcc recipe must not be advertised: it
        // would fail on the very first `#include`.
        let without_headers = capability_notes(&extras, false);
        assert!(
            !without_headers.contains("BIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD"),
            "{without_headers}"
        );
    }

    fn write_exec(dir: &Path, name: &str) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(dir.join(name), b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        #[cfg(not(unix))]
        {
            std::fs::write(dir.join(name), b"binary").unwrap();
        }
    }

    #[test]
    fn only_executables_on_the_path_count_as_tools() {
        let dir = tempfile::tempdir().unwrap();
        write_exec(dir.path(), "sed");
        // A plain file without the executable bit is not runnable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(dir.path().join("data"), b"x").unwrap();
            std::fs::set_permissions(
                dir.path().join("data"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        let names = ["sed", "data", "jq"];
        let found = resolve_from_path(&names, &[dir.path().to_path_buf()]);
        assert_eq!(found, vec!["sed"]);
    }

    #[test]
    fn a_tool_in_a_later_path_entry_still_counts() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_exec(second.path(), "git");
        let found = resolve_from_path(
            &["git"],
            &[first.path().to_path_buf(), second.path().to_path_buf()],
        );
        assert_eq!(found, vec!["git"]);
    }

    #[test]
    fn the_description_separates_what_is_there_from_what_is_not() {
        let described = describe_toolset(&["sed", "awk", "git"]);
        assert!(described.contains("`sed`"));
        assert!(described.contains("`git`"));
        assert!(described.contains("ARE available"));
        // `jq` is in the checked list but was not reported as available.
        assert!(described.contains("`jq`"));
        assert!(described.contains("NOT available"));
    }

    #[test]
    fn the_checked_list_does_not_claim_the_bundled_git_is_absent() {
        // The old description asserted `git` was absent on a phone. That was
        // false — the APK bundles git — and the model acted on it.
        assert!(TOOL_NAMES.contains(&"git"));
    }
}
