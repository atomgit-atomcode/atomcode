//! `pluginhost` — run an installed runtime package.
//!
//! Android forbids `execve` on a file in the app's data directory (targetSdk
//! 29+), so a downloaded interpreter can never be started like a program. What
//! the platform does allow is `dlopen` on a `.so` living there — measured on
//! device (see `docs/android-port.md` §P5.7).
//!
//! So an installed package is a shared library, and this binary is the piece
//! allowed to run: it lives in `nativeLibraryDir` (the one executable place),
//! `dlopen`s the package's library, and calls its entry point in THIS process.
//! The plugin's exit code becomes ours, and its stdio is already ours, so from
//! the caller's point of view it behaves like the interpreter itself.
//!
//! ```text
//! pluginhost <package> [args…]
//! ```
//!
//! The package must sit at `<packages-root>/<name>/` and carry a
//! `package.json`:
//!
//! ```json
//! { "name": "cpython", "version": "3.12.10",
//!   "entry": "lib/libpython3.12plugin.so", "symbol": "plugin_main" }
//! ```
//!
//! # Plugin ABI
//!
//! ```c
//! int plugin_main(int argc, char **argv);
//! ```
//!
//! `argv[0]` is the package name and `argv[1..]` are the caller's arguments.
//! Before the call this sets `PLUGIN_DIR` to the package's directory, which is
//! how a plugin finds the data files shipped beside it (a standard library, for
//! instance) without guessing its own location.

#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::os::raw::{c_char, c_int, c_void};
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

/// `RTLD_NOW` — resolve every symbol up front so a broken package fails here
/// with a message, not halfway through a script.
#[cfg(unix)]
const RTLD_NOW: c_int = 2;

/// `RTLD_GLOBAL` — publish the package's symbols process-wide.
///
/// This is not optional for CPython. Its extension modules
/// (`lib-dynload/_socket.so`, `_asyncio.so`, …) do **not** link against
/// `libpython`: they expect the interpreter's symbols to be visible in the
/// process (that is how `python` itself is built). Loading the plugin privately
/// left every one of them failing with `dlopen failed: cannot locate symbol
/// "PyExc_OSError"` — see `docs/android-port.md` §P5.10.
///
/// The same is harmless for a self-contained plugin (Lua, SQLite): it only
/// widens the search scope.
#[cfg(unix)]
const RTLD_GLOBAL: c_int = 0x100;

/// The entry point every package library exports.
#[cfg(unix)]
type PluginMain = unsafe extern "C" fn(c_int, *const *const c_char) -> c_int;

#[cfg(unix)]
#[derive(serde::Deserialize)]
struct Manifest {
    name: String,
    #[serde(default)]
    version: String,
    entry: String,
    #[serde(default = "default_symbol")]
    symbol: String,
    /// Extra names this package answers to; the explicit `pluginhost <alias>`
    /// form and `<product>/bin` links both go through the same lookup.
    #[serde(default)]
    aliases: Vec<String>,
    /// Libraries to load **globally** before the entry point, as paths relative
    /// to the package root.
    ///
    /// CPython needs this and cannot express it any other way: its extension
    /// modules (`lib-dynload/_socket.so`, …) do not link against `libpython`,
    /// they expect the interpreter's symbols to be in the process's global
    /// scope. Loading `libpython` as a dependency of the entry library is not
    /// enough — bionic keeps a library in the group where it was first loaded
    /// and does not promote it, so `RTLD_GLOBAL` on the entry alone leaves every
    /// extension failing with `cannot locate symbol "PyExc_OSError"`.
    /// Pre-loading it here is exactly what `LD_PRELOAD` does, without asking the
    /// caller to set an environment variable.
    #[serde(default)]
    preload: Vec<String>,
}

#[cfg(unix)]
fn default_symbol() -> String {
    "plugin_main".to_string()
}

/// Where installed packages live.
///
/// `ATOMCODE_PACKAGES_DIR` wins so a caller can point at a tree explicitly;
/// otherwise the product dir (`ATOMCODE_HOME`) and finally `$HOME` — on Android
/// the host sets `HOME` to the product tree, so the last fallback lands in the
/// right place there too.
///
/// `$HOME/packages` alone is not enough. Run from a plain shell — `adb shell`,
/// or any command that inherits the app's data directory rather than its product
/// directory — `HOME` is the data dir, so the products live one level deeper and
/// every lookup fails with "cannot read …/packages". The launcher itself sits in
/// `<product>/bin`, so its own path answers the question when the environment
/// does not; that is the second candidate below.
#[cfg(unix)]
fn packages_root() -> PathBuf {
    if let Some(explicit) = std::env::var_os("ATOMCODE_PACKAGES_DIR") {
        return PathBuf::from(explicit);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("ATOMCODE_HOME") {
        candidates.push(PathBuf::from(home).join("packages"));
    }
    // `<product>/bin/<command>` → `<product>/packages`.
    if let Some(invoked) = std::env::args_os().next() {
        let invoked = PathBuf::from(invoked);
        if invoked.is_absolute() {
            if let Some(product) = invoked.parent().and_then(Path::parent) {
                candidates.push(product.join("packages"));
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join("packages"));
        // The Android host keeps the product tree under the app data dir.
        candidates.push(home.join("files").join("atomcode").join("packages"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_dir())
        .unwrap_or_else(|| PathBuf::from("packages"))
}

#[cfg(unix)]
fn last_error() -> String {
    let pointer = unsafe { dlerror() };
    if pointer.is_null() {
        return "unknown loader error".to_string();
    }
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .to_string()
}

fn fail(message: impl AsRef<str>) -> ! {
    eprintln!("pluginhost: {}", message.as_ref());
    std::process::exit(2);
}

/// Only Android needs this launcher: everywhere else a package's files can be
/// executed directly, so the binary exists just to keep the workspace's
/// `--all-targets` check green off-device.
#[cfg(not(unix))]
fn main() {
    fail("pluginhost is only used on Android");
}

#[cfg(unix)]
/// Whether this is the launcher itself rather than a package link. The staged
/// file is named `libpluginhost.so` (only `lib*.so` is packaged), so both
/// spellings mean "explicit form".
fn is_launcher_name(name: &str) -> bool {
    name == "pluginhost" || name == "libpluginhost.so"
}

/// A command name is also a directory name and a link name: no separators, no
/// `..`, nothing that could step out of the packages root.
#[cfg(unix)]
fn is_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric())
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[cfg(unix)]
fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join("package.json");
    let body = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&body).map_err(|error| format!("{} is invalid: {error}", path.display()))
}

/// Find the package that answers to `command` — by its own name or an alias.
///
/// The direct path (`<root>/<command>`) is tried first because it is the common
/// case; the scan that follows is what makes an alias work when the command and
/// the package name differ.
#[cfg(unix)]
fn resolve(command: &str) -> Result<(String, PathBuf, Manifest), String> {
    let root = packages_root();
    let direct = root.join(command);
    if let Ok(manifest) = read_manifest(&direct) {
        if claims(&manifest, command) {
            return Ok((manifest.name.clone(), direct, manifest));
        }
    }
    let entries = std::fs::read_dir(&root)
        .map_err(|error| format!("cannot read {}: {error}", root.display()))?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(manifest) = read_manifest(&dir) else {
            continue;
        };
        if claims(&manifest, command) {
            return Ok((manifest.name.clone(), dir, manifest));
        }
    }
    Err(format!(
        "no package provides `{command}` (looked in {})",
        root.display()
    ))
}

#[cfg(unix)]
fn claims(manifest: &Manifest, command: &str) -> bool {
    manifest.name == command || manifest.aliases.iter().any(|alias| alias == command)
}

/// A package that passed the load-time checks: its manifest, where it lives, and
/// the entry symbol already resolved. The `dlopen` handle is deliberately
/// leaked — it lives for as long as the process does, and closing it before the
/// plugin is done would be the only wrong thing to do.
#[cfg(unix)]
struct Loaded {
    name: String,
    dir: PathBuf,
    manifest: Manifest,
    entry: *mut c_void,
}

/// A manifest path that must stay inside the package directory: no absolute
/// path, no `..`, no drive prefix.
#[cfg(unix)]
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut any = false;
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                out.push(part);
                any = true;
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    any.then_some(out)
}

/// Re-run ourselves with `LD_PRELOAD` when the package asks for globally
/// preloaded libraries.
///
/// Measured on device, and the reason this looks indirect: an in-process
/// `dlopen(path, RTLD_GLOBAL)` does **not** make a library visible to a later
/// `dlopen` on bionic — CPython's extensions have no `DT_NEEDED` on
/// `libpython` and keep failing with `cannot locate symbol "PyExc_OSError"`,
/// even though the library exports it. Starting the process with `LD_PRELOAD`
/// does work. So: exec ourselves once with the variable set, and let the linker
/// do at start-up what it will not do on request.
///
/// The marker keeps this from recursing, and the child inherits stdio and exit
/// status so the caller cannot tell the difference.
#[cfg(unix)]
fn reexec_with_preload(command: &str) {
    const MARKER: &str = "ATOMCODE_PLUGIN_PRELOADED";
    if std::env::var_os(MARKER).is_some() {
        return;
    }
    let Ok((_, dir, manifest)) = resolve(command) else {
        return; // `load` will report the real problem
    };
    if manifest.preload.is_empty() {
        return;
    }
    let mut paths: Vec<String> = Vec::with_capacity(manifest.preload.len());
    for relative in &manifest.preload {
        let Some(relative_path) = safe_relative(Path::new(relative)) else {
            continue;
        };
        let target = dir.join(&relative_path);
        if target.is_file() {
            paths.push(target.to_string_lossy().to_string());
        }
    }
    if paths.is_empty() {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    // `argv[0]` MUST be carried over. `current_exe()` resolves to the real
    // binary, so a plain exec would make the child see `libpluginhost.so` as its
    // name — losing the alias (`python3`) and, worse, being treated as the
    // explicit `pluginhost <command>` form, so `-c` would be read as a command.
    use std::os::unix::process::CommandExt;
    let invoked = std::env::args_os().next().unwrap_or_default();
    let status = std::process::Command::new(&executable)
        .arg0(invoked)
        .args(std::env::args_os().skip(1))
        .env("LD_PRELOAD", paths.join(":"))
        .env(MARKER, "1")
        .status();
    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        // If the exec itself failed there is nothing to do but carry on: the
        // package will report whatever is actually broken.
        Err(_) => {}
    }
}

/// Resolve, `dlopen` and `dlsym` a package. The checks a failing install should
/// have caught, and the same sequence `--check` reports on.
#[cfg(unix)]
fn load(command: &str) -> Result<Loaded, String> {
    // The command doubles as a directory name; refuse anything that could step
    // out of the packages root before touching the filesystem.
    if !is_command_name(command) {
        return Err(format!("invalid command name: {command:?}"));
    }
    let (name, dir, manifest) = resolve(command)?;

    let library = dir.join(&manifest.entry);
    if !library.is_file() {
        return Err(format!("{} is missing", library.display()));
    }

    // NOTE: `manifest.preload` is handled by re-executing with `LD_PRELOAD`
    // before this point (see [`reexec_with_preload`]). Loading it here with
    // `RTLD_GLOBAL` does not work on bionic — measured, not assumed.

    let c_path = CString::new(library.to_string_lossy().as_bytes())
        .map_err(|_| "library path contains a NUL".to_string())?;
    let handle = unsafe { dlopen(c_path.as_ptr(), RTLD_NOW | RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(format!(
            "cannot load {}: {}",
            library.display(),
            last_error()
        ));
    }
    let c_symbol =
        CString::new(manifest.symbol.as_str()).map_err(|_| "symbol contains a NUL".to_string())?;
    let entry = unsafe { dlsym(handle, c_symbol.as_ptr()) };
    if entry.is_null() {
        return Err(format!(
            "{} does not export {}: {}",
            library.display(),
            manifest.symbol,
            last_error()
        ));
    }
    Ok(Loaded {
        name,
        dir,
        manifest,
        entry,
    })
}

#[cfg(unix)]
fn main() {
    let argv: Vec<String> = std::env::args().collect();

    // `pluginhost --check <command>` loads the package exactly as a real run
    // would, reports whether it worked, and never calls the entry point. That is
    // what lets the packages panel say "this one is broken and why" without
    // starting an interpreter just to find out.
    if argv.get(1).map(String::as_str) == Some("--check") {
        let Some(command) = argv.get(2) else {
            fail("usage: pluginhost --check <command>");
        };
        // The probe must measure the same way a run does. A package whose
        // dependencies resolve only under `preload` — cpython's libpython is one,
        // because the loader matches a DT_NEEDED by SONAME — would otherwise be
        // reported as "cannot load" while `python3` works fine.
        reexec_with_preload(command);
        match load(command) {
            Ok(loaded) => {
                println!(
                    "ok {} {} {}",
                    loaded.manifest.name, loaded.manifest.version, loaded.manifest.entry
                );
                std::process::exit(0);
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
        }
    }

    // How we were invoked decides how arguments are read. `<product>/bin` holds
    // a link per package (`cpython` → `pluginhost`), so a package can be run
    // like any other command; the explicit `pluginhost <command>` form stays
    // for scripts and for names that are not installed as links.
    let invoked = argv
        .first()
        .map(|path| {
            PathBuf::from(path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let (command, rest): (String, Vec<String>) = if is_launcher_name(&invoked) {
        let Some(command) = argv.get(1).cloned() else {
            fail("usage: pluginhost <command> [args…]");
        };
        (command, argv[2..].to_vec())
    } else {
        (invoked, argv[1..].to_vec())
    };

    let Loaded {
        name,
        dir,
        manifest,
        entry,
    } = {
        // A package may need libraries in the process's global scope; the only
        // mechanism bionic honours is `LD_PRELOAD` at exec time.
        reexec_with_preload(&command);
        load(&command).unwrap_or_else(|error| fail(error))
    };

    // Tell the plugin where its own data lives before it runs.
    std::env::set_var("PLUGIN_DIR", &dir);
    std::env::set_var("PLUGIN_NAME", &name);
    std::env::set_var("PLUGIN_VERSION", &manifest.version);

    // argv[0] is how the package was invoked, so a script sees the name the
    // caller typed (`cpython script.py` gives `sys.argv[0]`-style behaviour),
    // then the caller's arguments.
    let mut owned: Vec<CString> = Vec::with_capacity(rest.len() + 1);
    for value in std::iter::once(&command).chain(rest.iter()) {
        owned
            .push(CString::new(value.as_str()).unwrap_or_else(|_| fail("argument contains a NUL")));
    }
    let pointers: Vec<*const c_char> = owned.iter().map(|item| item.as_ptr()).collect();

    let plugin: PluginMain = unsafe { std::mem::transmute(entry) };
    let code = unsafe { plugin(pointers.len() as c_int, pointers.as_ptr()) };
    std::process::exit(code);
}
