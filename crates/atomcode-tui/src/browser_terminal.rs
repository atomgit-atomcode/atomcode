//! Browser terminal servers own the client clipboard, even when they launch a
//! local shell without SSH environment variables. Do not infer this from xterm:
//! desktop terminals use the same TERM value.

pub(crate) fn detected() -> bool {
    static DETECTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            return false;
        }
        has_browser_ancestor(std::process::id(), |pid| {
            std::fs::read_to_string(format!("/proc/{pid}/status")).ok()
        })
    })
}

fn has_browser_ancestor(mut pid: u32, read: impl Fn(u32) -> Option<String>) -> bool {
    // Bound both filesystem work and malformed/cyclic process trees. An
    // inaccessible /proc is unknown, never proof of a browser terminal.
    for _ in 0..64 {
        let Some(status) = read(pid) else {
            return false;
        };
        let name = status.lines().find_map(|line| line.strip_prefix("Name:"));
        if name.is_some_and(|name| matches!(name.trim(), "gotty" | "ttyd")) {
            return true;
        }
        let parent = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|value| value.trim().parse::<u32>().ok());
        match parent {
            Some(parent) if parent != 0 && parent != pid => pid = parent,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_a_browser_server_through_intermediate_shells() {
        for server in ["gotty", "ttyd"] {
            assert!(has_browser_ancestor(40, |pid| match pid {
                40 => Some("Name:\tatomcode\nPPid:\t30\n".into()),
                30 => Some("Name:\tbash\nPPid:\t20\n".into()),
                20 => Some(format!("Name:\t{server}\nPPid:\t1\n")),
                _ => None,
            }));
        }
    }

    #[test]
    fn desktop_shells_and_similar_process_names_are_not_browser_terminals() {
        for name in ["bash", "xterm", "gotty-helper", "my-ttyd", "tmux: server"] {
            assert!(!has_browser_ancestor(40, |_| {
                Some(format!("Name:\t{name}\nPPid:\t0\n"))
            }));
        }
    }

    #[test]
    fn missing_malformed_and_cyclic_process_trees_are_bounded() {
        assert!(!has_browser_ancestor(40, |_| None));
        assert!(!has_browser_ancestor(40, |_| Some("PPid: broken".into())));
        assert!(!has_browser_ancestor(40, |pid| {
            Some(format!(
                "Name: bash\nPPid: {}\n",
                if pid == 40 { 30 } else { 40 }
            ))
        }));
    }
}
