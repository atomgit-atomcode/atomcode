//! Lazy local adapter discovery. No processes at startup and no downloads.
use super::{Adapter, Settings};
use serde_json::{json, Value};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
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

fn on_path(name: &str, cwd: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    };
    std::env::split_paths(&path)
        .map(|dir| {
            if dir.is_absolute() {
                dir.join(&name)
            } else {
                cwd.join(dir).join(&name)
            }
        })
        .find(|candidate| executable(candidate))
}

fn python(cwd: &Path) -> Option<PathBuf> {
    let bin = if cfg!(windows) {
        "Scripts/python.exe"
    } else {
        "bin/python"
    };
    // Do not silently switch to global Python when a project's environment
    // lacks debugpy: that can run its code with entirely different dependencies.
    for root in [
        Some(cwd.join(".venv")),
        Some(cwd.join("venv")),
        std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    {
        let candidate = root.join(bin);
        if executable(&candidate) {
            return Some(candidate);
        }
    }
    on_path("python3", cwd).or_else(|| on_path("python", cwd))
}

fn native(cwd: &Path, requested: Option<&str>) -> Option<PathBuf> {
    match requested {
        Some("lldb-vscode") => on_path("lldb-vscode", cwd),
        Some("lldb-dap") => on_path("lldb-dap", cwd),
        _ => on_path("lldb-dap", cwd).or_else(|| on_path("lldb-vscode", cwd)),
    }
}

pub(super) fn ids(settings: &Settings) -> Vec<String> {
    let mut ids: Vec<_> = settings
        .adapters
        .keys()
        .cloned()
        .chain(["python", "debugpy", "lldb", "lldb-dap", "lldb-vscode"].map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

pub(super) fn status(settings: &Settings, cwd: &Path) -> Value {
    json!({"adapters":ids(settings),"configured_adapters":settings.adapters.keys().collect::<Vec<_>>(),
        "discovery":{
            "python":{"interpreter":python(cwd),"requires":"debugpy; checked on launch, never installed automatically"},
            "lldb":{"command":native(cwd, None)}
        },
        "usage":"launch(program=..., breakpoints=[{file,line}]) selects Python for .py/.pyw and LLDB for compiled executables. adapter is optional; explicitly configured adapters override built-ins."})
}

pub(super) async fn resolve(
    settings: &Settings,
    requested: Option<&str>,
    program: &str,
    cwd: &Path,
) -> Result<(String, Adapter), String> {
    let id = match requested {
        Some(id) => id,
        None => match Path::new(program).extension().and_then(OsStr::to_str).unwrap_or("").to_ascii_lowercase().as_str() {
            "py" | "pyw" => "python",
            "" | "exe" | "out" => "lldb",
            "rs" | "c" | "cc" | "cpp" | "cxx" => return Err("For Rust/C/C++, build with debug information first and pass the compiled executable as program, not a source file.".into()),
            _ => return Err("No built-in DAP adapter for this target. Pass adapter explicitly and configure [dap.adapters.<id>] command/args, or use Python (.py) / an LLDB-compatible executable.".into()),
        }
    };
    if let Some(adapter) = settings.adapters.get(id) {
        return Ok((id.into(), adapter.clone()));
    }
    match id {
        "python" | "debugpy" => {
            let path = python(cwd).ok_or("Python was not found. Install Python with debugpy, or configure [dap.adapters.python] command/args.")?;
            // Module check is performed only within the approved launch call.
            // Isolated mode avoids importing a project-local debugpy impostor.
            let mut probe = Command::new(&path);
            probe.args(["-I", "-c", "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec('debugpy') else 1)"])
                .current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
            crate::process_utils::suppress_console_window(&mut probe);
            let found = tokio::time::timeout(Duration::from_secs(2), probe.status()).await;
            if !matches!(found, Ok(Ok(status)) if status.success()) {
                return Err(format!("debugpy is unavailable or its interpreter probe failed in {}. Install it into that interpreter with: \"{}\" -m pip install debugpy. Then retry launch; no DAP config is required. No package was installed automatically.", path.display(), path.display()));
            }
            Ok((id.into(), Adapter {command:path.to_string_lossy().into_owned(), args:vec!["-I".into(), "-m".into(), "debugpy.adapter".into()]}))
        }
        "lldb" | "lldb-dap" | "lldb-vscode" => {
            let command = native(cwd, Some(id)).ok_or("LLDB DAP adapter not found on PATH. Install lldb-dap (or lldb-vscode), or configure [dap.adapters.lldb] command/args. Pass a compiled executable built with debug information.")?;
            Ok((id.into(), Adapter {command:command.to_string_lossy().into_owned(),args:vec![]}))
        }
        _ => Err(format!("DAP adapter {id:?} is not configured. Use status to see built-ins or configure [dap.adapters.{id}].")),
    }
}
