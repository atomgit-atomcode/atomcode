//! Metadata-only Windows shell diagnostics. Never log command text or env values.
use std::{path::PathBuf, process::Command};

#[derive(Debug)]
pub(super) struct SpawnSummary {
    pub program: PathBuf,
    pub cwd: Option<PathBuf>,
    pub argument_count: usize,
    pub non_ascii_arguments: bool,
    pub encoded_powershell: bool,
}

pub(super) fn summarize(command: &Command) -> SpawnSummary {
    SpawnSummary {
        program: command.get_program().into(),
        cwd: command.get_current_dir().map(PathBuf::from),
        argument_count: command.get_args().count(),
        non_ascii_arguments: command
            .get_args()
            .any(|arg| !arg.to_string_lossy().is_ascii()),
        encoded_powershell: command.get_args().any(|arg| {
            arg.to_string_lossy()
                .eq_ignore_ascii_case("-EncodedCommand")
        }),
    }
}

#[cfg(windows)]
pub(super) fn log_spawn(command: &Command) {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetACP() -> u32;
        fn GetOEMCP() -> u32;
        fn GetConsoleCP() -> u32;
        fn GetConsoleOutputCP() -> u32;
    }
    // These getters neither change code pages nor retain pointers. A console
    // code page of zero means there is no console (e.g. a headless daemon).
    let (ansi, oem, input, output) =
        unsafe { (GetACP(), GetOEMCP(), GetConsoleCP(), GetConsoleOutputCP()) };
    let summary = summarize(command);
    tracing::debug!(target: "atomcode::shell::windows",
        program = ?summary.program, cwd = ?summary.cwd,
        argument_count = summary.argument_count,
        non_ascii_arguments = summary.non_ascii_arguments,
        encoded_powershell = summary.encoded_powershell,
        ansi_code_page = ansi, oem_code_page = oem,
        console_input_code_page = input, console_output_code_page = output,
        "spawning Windows shell; code pages describe the parent, not every child program");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_preserves_chinese_cwd_without_recording_commands_or_credentials() {
        let mut command = Command::new("powershell.exe");
        command
            .args(["-Command", "秘密-command secret-value"])
            .current_dir(r"D:\2026增肌\工作区_wb06")
            .env("API_TOKEN", "private-token");
        let summary = summarize(&command);
        assert_eq!(summary.program, PathBuf::from("powershell.exe"));
        assert_eq!(
            summary.cwd.as_deref(),
            Some(std::path::Path::new(r"D:\2026增肌\工作区_wb06"))
        );
        assert_eq!(summary.argument_count, 2);
        assert!(summary.non_ascii_arguments);
        assert!(!summary.encoded_powershell);
        let log = format!("{summary:?}");
        assert!(!log.contains("秘密-command"));
        assert!(!log.contains("secret-value"));
        assert!(!log.contains("private-token"));
    }

    #[test]
    fn encoded_transport_is_distinguished_from_a_raw_script_argument() {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-EncodedCommand", "YQBiAGM="]);
        let summary = summarize(&command);
        assert!(summary.encoded_powershell);
        assert!(!summary.non_ascii_arguments);
    }
}
