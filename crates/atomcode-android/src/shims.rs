//! The bundled `git`, and how the rest of the process finds it.
//!
//! Android 10+ refuses to execute files in an app's data directory — not an ELF
//! and not a shell script; the app's SELinux domain has no `exec` on
//! `app_data_file`. The APK's native library directory (`nativeLibraryDir`) is
//! the one place that stays executable, and Gradle only packages `lib*.so` names
//! there. That is why git ships as `libgit.so` and its smart-HTTP helper as
//! `libgit-remote-https.so` (see `scripts/build-android-git.sh`).
//!
//! Two consequences shape this module:
//!
//! * git looks its own programs up by bare name (`git` for the child it spawns
//!   for every builtin, `git-remote-https` for the HTTP transport). A patched
//!   git maps a failed name lookup onto `<GIT_EXEC_PATH>/lib<name>.so`, so the
//!   library directory itself can be the exec path.
//! * everything else in the process — the agent's `bash` tool, session rewind —
//!   runs `git` by name through `PATH`. A directory of symlinks into the library
//!   directory is enough for that: the symlink lives in the data dir, but the
//!   inode being executed is the one in the library dir, which Android allows.
//!
//! On a desktop build the library dir has no `libgit.so`, [`install`] returns
//! `None`, and the environment is left alone.

use std::path::{Path, PathBuf};

/// The bundled git executable.
pub const GIT_LIB: &str = "libgit.so";
/// `git-remote-https`: what `git clone https://…` execs. In git's own install
/// `git-remote-http` is a hardlink of the same program, so one file serves both
/// and the APK carries it once.
pub const GIT_REMOTE_HTTPS_LIB: &str = "libgit-remote-https.so";

/// The bundled JSON processor (`jq`).
pub const JQ_LIB: &str = "libjq.so";
/// The bundled recursive searcher (`ripgrep`).
pub const RG_LIB: &str = "librg.so";
/// The bundled HTTP client (`curl`).
pub const CURL_LIB: &str = "libcurl.so";
/// The bundled Python 3 interpreter (RustPython).
pub const PYTHON3_LIB: &str = "libpython3.so";
/// The launcher that runs an installed runtime package (`dlopen`s its `.so`).
pub const PLUGINHOST_LIB: &str = "libpluginhost.so";

/// What [`install`] did, so the host can say it once at start-up.
#[derive(Debug, Clone)]
pub struct InstalledTools {
    /// The directory of symlinks added to `PATH` (`<data>/bin`).
    pub bin_dir: PathBuf,
    /// The library directory git uses as its exec path.
    pub lib_dir: PathBuf,
    /// `git --version` through the `PATH` entry, or why it could not run — the
    /// one line that tells a person whether the bundle actually works here.
    pub git_version: Result<String, String>,
    /// The extra command-line tools this build ships, and whether each one
    /// actually runs here. A phone has no package manager, so a tool that is
    /// NOT in this list simply cannot be obtained.
    pub extra_tools: Vec<(String, Result<String, String>)>,
    /// Command names contributed by the optional helper APK (`make`, `tcc`,
    /// `cc`). Empty when it is not installed.
    pub helper_tools: Vec<String>,
}

/// The command-line tools that ship in the APK besides git.
///
/// Each one is a standalone program in the library directory, linked into
/// `<data>/bin` under its plain name. They are here because the platform
/// offers no way to install them: Android has no package manager, and the
/// system image has no `jq`, no `ripgrep` and no `curl`.
///
/// `jq` (JSON) and `ripgrep` (fast, gitignore-aware search) are what an agent
/// reaches for most; `curl` is what makes a network check possible without a
/// tool that can write files.
pub const EXTRA_TOOLS: &[(&str, &str)] = &[
    ("jq", JQ_LIB),
    ("rg", RG_LIB),
    ("curl", CURL_LIB),
    // Python 3 (RustPython). Its standard library is frozen into the binary, so
    // there is no separate `Lib/` tree to ship. Both spellings resolve to the
    // same executable — the one a script's shebang or a model's habit expects.
    ("python3", PYTHON3_LIB),
    ("python", PYTHON3_LIB),
    // Runs an installed runtime package. A package's own files can never be
    // exec'd (Android blocks that for targetSdk 29+), so this launcher — which
    // lives in `nativeLibraryDir` — `dlopen`s the package library instead.
    ("pluginhost", PLUGINHOST_LIB),
];

/// The names `PATH` needs to resolve. `git` covers direct invocations; the rest
/// are the helpers git execs itself (the patch lets it find them by name).
const PROGRAMS: [&str; 6] = [
    "git",
    "git-remote-https",
    "git-remote-http",
    "git-upload-pack",
    "git-receive-pack",
    // `credential.helper = store` is a git subcommand, spawned as
    // `git-credential-store`. Without this link a stored token is never read and
    // every push to a private remote fails as if it were never given one.
    "git-credential-store",
];

/// The library each name resolves to. `git-remote-http` shares the one binary
/// with `git-remote-https`; the pack transport helpers and the credential store
/// are the git program run under that name, which is exactly how a normal git
/// install hardlinks them.
fn target_for(name: &str, lib_dir: &Path) -> PathBuf {
    match name {
        "git-remote-https" | "git-remote-http" => lib_dir.join(GIT_REMOTE_HTTPS_LIB),
        _ => lib_dir.join(GIT_LIB),
    }
}

/// Point `PATH` at a directory of symlinks into `lib_dir`, and `GIT_EXEC_PATH`
/// at `lib_dir` itself.
///
/// The extra tools (`jq`, `rg`, `curl`) are linked the same way: their
/// executable inode lives in the library directory, which Android allows, and
/// only the symlink lives in the data dir.
/// Tools provided by the optional helper APK (`com.atomcode.tools`).
///
/// The helper carries its own binaries, and Android lets *this* app execute
/// another package's `nativeLibraryDir` — measured on device, see
/// `docs/android-port.md` §P5.9. That is why the helper needs no service, no
/// Binder and no IPC: it is a file holder whose directory we point links at.
///
/// This matters for tools the plugin system cannot host: `make` spawns a
/// compiler and a compiler spawns an assembler/linker, and `execve` on our own
/// data directory is forbidden. Inside the helper those children live in the
/// same executable directory, so the chain works.
pub const HELPER_TOOLS: &[(&str, &str)] = &[
    ("make", "libmake.so"),
    // Makefiles ask for `cc` by name; on a phone the C compiler we ship is tcc.
    ("cc", "libtcc.so"),
    ("tcc", "libtcc.so"),
    // The CLI itself, so a person can run `atomcode daemon` on the device (see
    // `docs/external-daemon.md`). It has to live in the helper's
    // `nativeLibraryDir` for the same reason the others do: Android refuses to
    // execute anything outside it. Older helpers were built without it, and
    // the link is skipped when the file is not there.
    ("atomcode", "libatomcode.so"),
];

/// Tools a *provider APK* may supply, looked up across every provider directory.
///
/// `zig` is here rather than in [`HELPER_TOOLS`] because its 141 MB binary was the
/// whole reason the helper weighed 82 MB; it ships in its own small APK
/// (`com.atomcode.tools.zig`) and only its standard library stays a market package
/// (`zig-lib`). A provider is a data-less file holder exactly like the helper —
/// `execve` on our own data directory is forbidden (measured: exit 126 from the real
/// app process), so an APK directory is the only way to run a process-model tool.
///
/// Absence is handled by the same loop as the helper's tools: if no provider supplies
/// the library the link is removed, so an uninstalled provider leaves nothing behind.
/// This also covers upgrades: a device whose old helper shipped `zig` keeps that link
/// only while a provider still has the binary.
pub const PROVIDER_TOOLS: &[(&str, &str)] = &[("zig", "libzig.so")];

/// Point `<data>/bin` at the provider APKs' binaries.
///
/// `provider_lib_dirs` is empty when no provider is installed: stale links from a
/// previous install are removed then, so a removed provider stops resolving instead
/// of failing later with a loader error from a directory that is gone. Each tool is
/// looked up across every provider, so `make` can come from one APK and `zig` from
/// another. Returns the command names that are now available.
pub fn link_helper_tools(provider_lib_dirs: &[PathBuf], data_dir: &Path) -> Vec<String> {
    let bin_dir = data_dir.join("bin");
    if std::fs::create_dir_all(&bin_dir).is_err() {
        return Vec::new();
    }
    let mut available = Vec::new();
    for (name, lib) in HELPER_TOOLS.iter().chain(PROVIDER_TOOLS) {
        let link = bin_dir.join(name);
        if claimed_by_package(&bin_dir, name) {
            continue;
        }
        match find_provider_lib(provider_lib_dirs, lib) {
            Some(target) => {
                let _ = std::fs::remove_file(&link);
                if symlink(&target, &link).is_ok() {
                    available.push((*name).to_string());
                }
            }
            None => {
                let _ = std::fs::remove_file(&link);
            }
        }
    }
    available
}

/// The first provider directory that actually carries `lib`.
///
/// Providers are independent APKs, so which one has a given binary is a property of
/// what is installed — `make` comes from the tools helper while `zig` comes from its
/// own package, and either may be missing.
fn find_provider_lib(provider_lib_dirs: &[PathBuf], lib: &str) -> Option<PathBuf> {
    provider_lib_dirs
        .iter()
        .map(|dir| dir.join(lib))
        .find(|target| target.is_file())
}

/// Whether a runtime package has taken this command name over.
///
/// Installing a package may claim a host-provided name — a CPython package
/// becoming `python3` is the case that motivated it. The host writes its own
/// links on every start-up, so without this check a restart would silently take
/// `python3` back from the package the person installed on purpose.
fn claimed_by_package(bin_dir: &Path, name: &str) -> bool {
    bin_dir
        .parent()
        .map(|product| product.join(".bin-overrides").join(name).exists())
        .unwrap_or(false)
}

fn write_links(lib_dir: &Path, bin_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(bin_dir)?;
    for name in PROGRAMS {
        if claimed_by_package(bin_dir, name) {
            continue;
        }
        let link = bin_dir.join(name);
        // A stale link from an older layout is removed first; `symlink` would
        // otherwise fail with EEXIST.
        let _ = std::fs::remove_file(&link);
        symlink(&target_for(name, lib_dir), &link)?;
    }
    for (name, lib) in EXTRA_TOOLS {
        // A build may ship only some of them; a missing library is simply not
        // linked, so the name stays unresolved (and the tool is reported as
        // unavailable rather than failing at exec time).
        if claimed_by_package(bin_dir, name) {
            continue;
        }
        let target = lib_dir.join(lib);
        if !target.is_file() {
            continue;
        }
        let link = bin_dir.join(name);
        let _ = std::fs::remove_file(&link);
        symlink(&target, &link)?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// Wire the bundled git into this process.
///
/// `data_dir` holds the `PATH` directory, the default gitconfig and the CA
/// bundle. `HOME` becomes `data_dir` when it is unset: a phone has no `/home`,
/// and an unset `HOME` would send git's config lookups to `/`.
///
/// Returns `None` on a build without the bundled git (the desktop case), in
/// which case the environment is left untouched.
pub fn install(
    native_lib_dir: &Path,
    data_dir: &Path,
    provider_lib_dirs: &[PathBuf],
) -> Option<InstalledTools> {
    if !native_lib_dir.join(GIT_LIB).is_file() {
        return None;
    }

    let bin_dir = data_dir.join("bin");
    if let Err(error) = write_links(native_lib_dir, &bin_dir) {
        tracing::warn!(%error, dir = %bin_dir.display(), "could not link the bundled git");
        return None;
    }

    prepend_path(&bin_dir);
    std::env::set_var("GIT_EXEC_PATH", native_lib_dir);
    // `git init`/`clone` copy the template dir (hooks, description) into the new
    // repository. Git's compiled-in path points at an install prefix we did not
    // ship; without this every clone prints "templates not found".
    let templates = data_dir.join("git-templates");
    if std::fs::create_dir_all(&templates).is_ok() {
        std::env::set_var("GIT_TEMPLATE_DIR", &templates);
    }
    // A phone is not a place to page into, and a credential prompt would hang a
    // turn waiting for an answer nobody can type.
    std::env::set_var("GIT_PAGER", "cat");
    std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    // Unconditional, not "if unset": git's `credential.helper = store` resolves
    // `~/.git-credentials` from `HOME`, and the daemon's token endpoint writes
    // there. An inherited `HOME` (or none) would put the credential somewhere
    // nobody reads.
    std::env::set_var("HOME", data_dir);
    let ca_bundle = write_ca_bundle(Path::new(SYSTEM_CA_DIR), data_dir);
    if let Some(bundle) = &ca_bundle {
        // Every bundled TLS client reads its own variable — see
        // [`ca_env_vars`] for why all of them are set.
        for (name, value) in ca_env_vars(bundle) {
            std::env::set_var(name, value);
        }
    }
    // Pin the global config instead of relying on `HOME` resolving where we
    // think: it carries the credential store, `safe.directory` and the CA path.
    std::env::set_var("GIT_CONFIG_GLOBAL", global_config(data_dir));
    write_default_gitconfig(data_dir, ca_bundle.as_deref());

    let git_version = probe_version(&bin_dir);
    let extra_tools = probe_extra_tools(&bin_dir);
    // The helper APK's binaries are linked last: it is optional, and its absence
    // is normal rather than an error.
    let helper_tools = link_helper_tools(provider_lib_dirs, data_dir);
    // Zig 在 APK 的 nativeLibraryDir 里找 `../lib`（不存在），只认 `$ZIG_LIB_DIR`。
    // 标准库由 `zig-lib` 数据包链接到 `<data>/zig`——链接本身就是标准库目录（下面直接
    // 就是 `std/`），所以不能再拼一层 `lib`。
    let zig_lib = data_dir.join("zig");
    if zig_lib.is_dir() {
        std::env::set_var("ZIG_LIB_DIR", &zig_lib);
    } else if bin_dir.join("zig").exists() {
        tracing::info!(
            dir = %zig_lib.display(),
            "zig is available but its standard library is not installed; install the zig-lib package"
        );
    }
    Some(InstalledTools {
        bin_dir,
        lib_dir: native_lib_dir.to_path_buf(),
        git_version,
        extra_tools,
        helper_tools,
    })
}

/// Probe each bundled extra tool through the `PATH` entry, so the report says
/// whether it actually RUNS here — not merely whether it was packaged.
///
/// `rg --version` prints a multi-line banner; the first line is the version.
fn probe_extra_tools(bin_dir: &Path) -> Vec<(String, Result<String, String>)> {
    let mut probed = Vec::new();
    for (name, _) in EXTRA_TOOLS {
        let link = bin_dir.join(name);
        if !link.exists() {
            probed.push((
                (*name).to_string(),
                Err("not packaged in this build".into()),
            ));
            continue;
        }
        let output = std::process::Command::new(&link).arg("--version").output();
        match output {
            Ok(output) if output.status.success() => {
                let first_line = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                probed.push(((*name).to_string(), Ok(first_line)));
            }
            Ok(output) => {
                let said = String::from_utf8_lossy(&output.stderr);
                probed.push((
                    (*name).to_string(),
                    Err(format!("exited with {}: {}", output.status, said.trim())),
                ));
            }
            Err(error) => probed.push(((*name).to_string(), Err(error.to_string()))),
        }
    }
    probed
}

/// Put `dir` first in `PATH`, keeping whatever the platform handed us.
fn prepend_path(dir: &Path) {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut parts = vec![dir.to_path_buf()];
    parts.extend(std::env::split_paths(&existing));
    if let Ok(joined) = std::env::join_paths(parts) {
        std::env::set_var("PATH", joined);
    }
}

/// `git --version` through the `PATH` entry, so the check exercises the same
/// lookup a turn will: `PATH` → symlink → library directory → exec.
fn probe_version(bin_dir: &Path) -> Result<String, String> {
    let output = std::process::Command::new(bin_dir.join("git"))
        .arg("--version")
        .output()
        .map_err(|error| format!("could not run the bundled git: {error}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "bundled git exited with {}: {}",
            output.status,
            said.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The global git config file, at the same place git would look for it under
/// `HOME` — but named explicitly so an inherited `HOME` cannot change it.
fn global_config(data_dir: &Path) -> PathBuf {
    data_dir.join(".gitconfig")
}

fn write_default_gitconfig(data_dir: &Path, ca_bundle: Option<&Path>) {
    let path = global_config(data_dir);
    if path.exists() {
        return;
    }
    // `safe.directory = *` matters on phones: a repository on shared storage can
    // be owned by a different uid than the app (a desktop copy, an earlier
    // install), and git refuses to touch those otherwise. A personal phone has
    // no multi-user threat model to protect here.
    //
    // `credential.helper = store` is how a token survives between turns: the
    // person gives the agent a personal access token once, the agent writes it
    // to `~/.git-credentials` (app-private `HOME`), and later clones/pushes read
    // it from there. There is no terminal on the phone, so a prompt would only
    // hang a turn (`GIT_TERMINAL_PROMPT=0` in [`install`]).
    let mut body = String::from(
        "\
[user]
\tname = AtomCode
\temail = atomcode@localhost
[init]
\tdefaultBranch = main
[core]
\tautocrlf = false
\tquotePath = false
[credential]
\thelper = store
[safe]
\tdirectory = *
",
    );
    // Android keeps its trusted roots as OpenSSL-hashed files, not as a bundle;
    // `http.sslCAInfo` is the spelling both git and its libcurl respect.
    if let Some(bundle) = ca_bundle {
        body.push_str(&format!(
            "[http]\n\tsslCAInfo = {}\n",
            bundle.to_string_lossy()
        ));
    }
    if let Err(error) = std::fs::write(&path, body) {
        tracing::warn!(%error, path = %path.display(), "could not write the default gitconfig");
    }
}

/// Where Android keeps its trusted roots.
const SYSTEM_CA_DIR: &str = "/system/etc/security/cacerts";

/// Concatenate `system_dir` into one PEM under `data_dir`, because the bundled
/// `git` links libcurl against OpenSSL, which looks for a *bundle* by default — a
/// path that does not exist on Android. Written once: a later Android CA update
/// leaves it stale until the app's files are cleared, which is the same lifetime
/// as the rest of the product tree.
fn write_ca_bundle(system_dir: &Path, data_dir: &Path) -> Option<PathBuf> {
    let path = data_dir.join("ca-bundle.pem");
    if path.is_file() {
        return Some(path);
    }
    let entries = std::fs::read_dir(system_dir).ok()?;
    let mut pem = String::new();
    let mut found = 0usize;
    for entry in entries.flatten() {
        let file = entry.path();
        let wanted = file.is_file()
            && file
                .file_name()
                .map(|name| name.to_string_lossy().ends_with(".0"))
                .unwrap_or(false);
        if !wanted {
            continue;
        }
        if let Ok(cert) = std::fs::read_to_string(&file) {
            pem.push_str(&cert);
            if !cert.ends_with('\n') {
                pem.push('\n');
            }
            found += 1;
        }
    }
    if found == 0 {
        tracing::warn!(dir = %system_dir.display(), "no system CA certificates found");
        return None;
    }
    if let Err(error) = std::fs::write(&path, pem) {
        tracing::warn!(%error, path = %path.display(), "could not write the CA bundle");
        return None;
    }
    Some(path)
}

/// The environment each bundled TLS client needs to trust the device's roots.
///
/// There is no single variable: git reads `GIT_SSL_CAINFO`, the bundled `curl`
/// reads `CURL_CA_BUNDLE`, and some OpenSSL builds read `SSL_CERT_FILE`. Setting
/// only git's is what produced a half-broken phone — `git clone https://…`
/// worked while `curl https://…` failed with "unable to get local issuer
/// certificate" (exit 60), because the same bundle was trusted by one tool and
/// not the other. Set them all; they point at the same file.
fn ca_env_vars(bundle: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("GIT_SSL_CAINFO", bundle.to_path_buf()),
        ("CURL_CA_BUNDLE", bundle.to_path_buf()),
        ("SSL_CERT_FILE", bundle.to_path_buf()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_without_the_bundle_changes_nothing() {
        let work = tempfile::tempdir().unwrap();
        let lib = work.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        assert!(install(&lib, work.path(), &[]).is_none());
        assert!(!work.path().join("bin").exists());
    }

    // A package that took a host name over must keep it across a restart: the
    // host rewrites its links on every start-up, and doing that here would
    // silently steal `python3` back from the package the person installed.
    #[cfg(unix)]
    #[test]
    fn a_name_a_package_claimed_is_not_rewritten_by_the_host() {
        let work = tempfile::tempdir().unwrap();
        let lib = work.path().join("lib");
        let bin = work.path().join("bin");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(lib.join(PYTHON3_LIB), b"stub").unwrap();
        // The daemon records the takeover beside `bin/`.
        let overrides = work.path().join(".bin-overrides");
        std::fs::create_dir_all(&overrides).unwrap();
        std::fs::write(overrides.join("python3"), "libpython3.so").unwrap();
        // The package's own link is already in place.
        symlink("pluginhost", &bin.join("python3")).unwrap();

        write_links(&lib, &bin).unwrap();

        assert_eq!(
            std::fs::read_link(bin.join("python3")).unwrap(),
            Path::new("pluginhost"),
            "the host must leave a claimed name alone"
        );
        // Everything else is still written as usual.
        assert!(std::fs::read_link(bin.join("git")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn every_name_points_into_the_library_directory() {
        let work = tempfile::tempdir().unwrap();
        let lib = work.path().join("lib");
        let bin = work.path().join("bin");
        std::fs::create_dir_all(&lib).unwrap();
        write_links(&lib, &bin).unwrap();
        for name in PROGRAMS {
            let link = bin.join(name);
            let target = std::fs::read_link(&link).unwrap();
            assert_eq!(target, target_for(name, &lib), "{name} points elsewhere");
        }
        assert_eq!(
            std::fs::read_link(bin.join("git-remote-http")).unwrap(),
            lib.join(GIT_REMOTE_HTTPS_LIB),
            "the two HTTP spellings share one binary"
        );
    }

    // The extra tools (jq / rg / curl) are linked the same way git is: the
    // executable inode stays in the library directory (the only place Android
    // lets an app execute from) and only a symlink lives in the data dir.
    #[cfg(unix)]
    #[test]
    fn the_extra_tools_are_linked_into_the_path_directory() {
        let work = tempfile::tempdir().unwrap();
        let lib = work.path().join("lib");
        let bin = work.path().join("bin");
        std::fs::create_dir_all(&lib).unwrap();
        // A build may ship only some of them, so every library file is created
        // to check that each name resolves to ITS OWN library.
        for (_, lib_name) in EXTRA_TOOLS {
            std::fs::write(lib.join(lib_name), b"binary").unwrap();
        }
        write_links(&lib, &bin).unwrap();
        for (name, lib_name) in EXTRA_TOOLS {
            let link = bin.join(name);
            let target = std::fs::read_link(&link).unwrap();
            assert_eq!(target, lib.join(lib_name), "{name} points elsewhere");
        }
    }

    // Which provider carries which binary is a lookup, not a fixed path: this part
    // needs no symlinks, so it runs on every build machine (the linking test below
    // is `cfg(unix)`).
    #[test]
    fn a_tool_is_found_in_whichever_provider_carries_it() {
        let work = tempfile::tempdir().unwrap();
        let tools = work.path().join("tools");
        let zig = work.path().join("zig");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&zig).unwrap();
        std::fs::write(tools.join("libmake.so"), b"binary").unwrap();
        std::fs::write(zig.join("libzig.so"), b"binary").unwrap();

        let dirs = vec![tools.clone(), zig.clone()];
        assert_eq!(
            find_provider_lib(&dirs, "libmake.so"),
            Some(tools.join("libmake.so"))
        );
        assert_eq!(
            find_provider_lib(&dirs, "libzig.so"),
            Some(zig.join("libzig.so"))
        );
        assert_eq!(
            find_provider_lib(&dirs[..1], "libzig.so"),
            None,
            "an uninstalled provider resolves nothing"
        );
    }

    // `zig` comes from its own provider APK now; `make` still comes from the helper.
    // Both are linked from one list, and a provider whose library is gone must take
    // only its own link with it.
    #[cfg(unix)]
    #[test]
    fn tools_are_linked_from_whichever_provider_supplies_them() {
        let work = tempfile::tempdir().unwrap();
        let tools = work.path().join("tools");
        let zig = work.path().join("zig");
        let bin = work.path().join("bin");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&zig).unwrap();
        std::fs::write(tools.join("libmake.so"), b"binary").unwrap();
        std::fs::write(zig.join("libzig.so"), b"binary").unwrap();

        let linked = link_helper_tools(&[tools.clone(), zig.clone()], work.path());
        assert!(linked.contains(&"make".to_string()), "{linked:?}");
        assert!(linked.contains(&"zig".to_string()), "{linked:?}");
        assert_eq!(
            std::fs::read_link(bin.join("zig")).unwrap(),
            zig.join("libzig.so"),
            "zig must point into the provider that has it"
        );

        // The zig provider is uninstalled: its link goes, `make` stays.
        link_helper_tools(&[tools], work.path());
        assert!(
            !bin.join("zig").exists(),
            "an absent provider leaves no link behind"
        );
        assert!(std::fs::read_link(bin.join("make")).is_ok());
    }

    // A build that ships only git must not leave dangling links for the tools
    // it does not have: an unresolved name is reported as "not packaged",
    // while a link to a missing file would fail at exec time with a confusing
    // error instead.
    #[cfg(unix)]
    #[test]
    fn a_tool_that_is_not_packaged_is_not_linked() {
        let work = tempfile::tempdir().unwrap();
        let lib = work.path().join("lib");
        let bin = work.path().join("bin");
        std::fs::create_dir_all(&lib).unwrap();
        write_links(&lib, &bin).unwrap();
        for (name, _) in EXTRA_TOOLS {
            assert!(
                !bin.join(name).exists(),
                "{name} must not be linked when it was not packaged"
            );
        }
    }

    #[test]
    fn a_missing_extra_tool_is_reported_as_not_packaged() {
        let work = tempfile::tempdir().unwrap();
        let bin = work.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let probed = probe_extra_tools(&bin);
        assert_eq!(probed.len(), EXTRA_TOOLS.len());
        for (name, result) in probed {
            let error = result.expect_err("{name} must not resolve without a build");
            assert!(
                error.contains("not packaged"),
                "{name} must say it was not packaged: {error}"
            );
        }
    }

    #[test]
    fn the_default_gitconfig_keeps_tokens_and_trusts_the_bundle() {
        let work = tempfile::tempdir().unwrap();
        write_default_gitconfig(work.path(), Some(Path::new("/data/ca-bundle.pem")));
        let body = std::fs::read_to_string(work.path().join(".gitconfig")).unwrap();
        assert!(
            body.contains("helper = store"),
            "tokens must survive a turn"
        );
        assert!(body.contains("sslCAInfo = /data/ca-bundle.pem"));
        assert!(body.contains("directory = *"));
        assert!(
            !body.contains("sslVerify = false"),
            "never disable TLS checks"
        );
    }

    #[test]
    fn a_ca_bundle_is_stacked_from_the_hashed_roots() {
        let work = tempfile::tempdir().unwrap();
        let system = work.path().join("cacerts");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(system.join("9aef356c.0"), "CERT-A\n").unwrap();
        std::fs::write(system.join("1d3472b9.0"), "CERT-B").unwrap();
        // Anything that is not a hashed root is not a certificate.
        std::fs::write(system.join("README"), "ignore me").unwrap();

        let bundle = write_ca_bundle(&system, work.path()).expect("roots were found");
        let body = std::fs::read_to_string(bundle).unwrap();
        assert!(body.contains("CERT-A"));
        assert!(
            body.contains("CERT-B\n"),
            "a cert without a newline gets one"
        );
        assert!(!body.contains("ignore me"));
    }

    #[test]
    fn a_machine_without_system_roots_is_not_an_error() {
        let work = tempfile::tempdir().unwrap();
        let missing = work.path().join("no-such-dir");
        assert!(write_ca_bundle(&missing, work.path()).is_none());
    }

    // Every bundled TLS client must be pointed at the CA bundle. Setting only
    // git's variable left `curl https://…` failing with "unable to get local
    // issuer certificate" on a device where `git clone https://…` worked —
    // the same bundle trusted by one tool and not the other.
    #[test]
    fn every_bundled_tls_client_is_told_where_the_roots_are() {
        let bundle = Path::new("/data/ca-bundle.pem");
        let vars = ca_env_vars(bundle);
        for wanted in ["GIT_SSL_CAINFO", "CURL_CA_BUNDLE", "SSL_CERT_FILE"] {
            let value = vars
                .iter()
                .find(|(name, _)| *name == wanted)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("{wanted} must be set for the bundled clients"));
            assert_eq!(value, bundle, "{wanted} must point at the CA bundle");
        }
    }
}
