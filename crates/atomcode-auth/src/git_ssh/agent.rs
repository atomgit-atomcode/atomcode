//! Do not send decrypted identities to an arbitrary environment-selected agent.
use anyhow::{bail, Result};
use std::path::PathBuf;

#[cfg(unix)]
pub(super) fn verify(socket: Option<&std::path::Path>) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    use std::io;
    use std::{mem, os::fd::AsRawFd, os::unix::net::UnixStream};
    let inherited = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    let path = socket
        .or(inherited.as_deref())
        .ok_or_else(|| anyhow::anyhow!("No SSH agent socket is configured"))?;
    let stream =
        UnixStream::connect(path).map_err(|_| anyhow::anyhow!("Cannot connect to SSH agent"))?;
    #[cfg(target_os = "linux")]
    let pid = {
        let mut peer: libc::ucred = unsafe { mem::zeroed() };
        let mut size = mem::size_of_val(&peer) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut size,
            )
        } == -1
            || peer.uid != unsafe { libc::geteuid() }
        {
            bail!("Cannot verify SSH agent ownership");
        }
        peer.pid
    };
    #[cfg(target_os = "macos")]
    let pid = {
        let mut pid: libc::pid_t = 0;
        let mut size = mem::size_of_val(&pid) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut size,
            )
        } == -1
        {
            bail!("Cannot identify SSH agent process");
        }
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == -1
            || (pid != 1 && uid != unsafe { libc::geteuid() })
        {
            bail!("SSH agent belongs to a different OS user");
        }
        pid
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (stream, io::Error::last_os_error(), mem::size_of::<usize>());
        bail!("Agent identity verification is unavailable on this platform");
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if pid <= 0 {
            bail!("Cannot identify SSH agent process");
        }
        // macOS's system agent inherits its listening socket from launchd;
        // LOCAL_PEERPID reports launchd (pid 1), not the later acceptor.
        #[cfg(target_os = "macos")]
        if pid == 1 {
            return verify_launchd_agent(path);
        }
        #[cfg(target_os = "linux")]
        let actual = std::fs::read_link(format!("/proc/{pid}/exe"))?;
        #[cfg(target_os = "macos")]
        let actual = {
            use std::os::unix::ffi::OsStringExt;
            let mut bytes = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            let n =
                unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
            if n <= 0 {
                return Err(io::Error::last_os_error().into());
            }
            bytes.truncate(bytes.iter().position(|b| *b == 0).unwrap_or(n as usize));
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        };
        let actual = std::fs::canonicalize(actual)?;
        if !["/usr/bin/ssh-agent", "/bin/ssh-agent"]
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .any(|p| p == actual)
        {
            bail!("Saved credentials can only load into a system OpenSSH agent; custom agents require external setup");
        }
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&actual)?;
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            bail!("SSH agent executable is not system-owned and protected");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn launchd_description_matches(text: &str, socket: &std::path::Path) -> bool {
    let Some(socket) = socket.to_str() else {
        return false;
    };
    // Require the unique top-level job fields, not a matching substring in
    // an environment value or nested job field.
    let program: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("\tprogram = "))
        .collect();
    let plist: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("\tpath = "))
        .collect();
    let lines: Vec<_> = text.lines().map(str::trim).collect();
    program == ["\tprogram = /usr/bin/ssh-agent"]
        && plist == ["\tpath = /System/Library/LaunchAgents/com.openssh.ssh-agent.plist"]
        && lines.contains(&format!("SSH_AUTH_SOCK => {socket}").as_str())
        && lines.contains(&format!("path = {socket}").as_str())
        && lines.contains(&"secure key = SSH_AUTH_SOCK")
}

#[cfg(target_os = "macos")]
fn verify_launchd_agent(socket: &std::path::Path) -> Result<()> {
    use std::{
        io::Read,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    // Query only the protected system job, never parse arbitrary askpass text.
    // The full description (which can contain environment values) is discarded.
    let job = format!("gui/{}/com.openssh.ssh-agent", unsafe { libc::geteuid() });
    let mut child = Command::new("/bin/launchctl")
        .args(["print", &job])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        stdout.take(65537).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader
        .join()
        .ok()
        .and_then(|r| r.ok())
        .ok_or_else(|| anyhow::anyhow!("Cannot inspect system SSH agent job"))?;
    if !success
        || bytes.len() > 65536
        || !std::str::from_utf8(&bytes).is_ok_and(|text| launchd_description_matches(text, socket))
    {
        bail!("Agent socket does not match the protected macOS OpenSSH launchd job");
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn system_directory() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut path = [0u16; 32768];
    let n = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
            path.as_mut_ptr(),
            path.len() as u32,
        )
    } as usize;
    if n == 0 || n >= path.len() {
        bail!("Cannot locate Windows system directory");
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&path[..n])))
}

#[cfg(windows)]
pub(super) fn verify(_: Option<&std::path::Path>) -> Result<()> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING},
        System::{
            Pipes::GetNamedPipeServerProcessId,
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };
    struct Handle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let pipe: Vec<u16> = r"\\.\pipe\openssh-ssh-agent"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let raw = unsafe {
        CreateFileW(
            pipe.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        bail!("Windows OpenSSH agent service is unavailable");
    }
    let pipe = Handle(raw);
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut pid) } == 0 {
        bail!("Cannot identify Windows SSH agent");
    }
    verify_windows_service_pid(pid)?;
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        bail!("Cannot inspect Windows SSH agent");
    }
    let process = Handle(raw);
    let mut path = [0u16; 32768];
    let mut size = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut size) } == 0 {
        bail!("Cannot identify Windows SSH agent executable");
    }
    let actual = std::fs::canonicalize(PathBuf::from(std::ffi::OsString::from_wide(
        &path[..size as usize],
    )))?;
    let expected = std::fs::canonicalize(system_directory()?.join("OpenSSH/ssh-agent.exe"))?;
    if !actual
        .as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
    {
        bail!("Saved credentials require the system Windows OpenSSH agent service");
    }
    Ok(())
}

#[cfg(windows)]
fn verify_windows_service_pid(pid: u32) -> Result<()> {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_HANDLE,
        SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
        SERVICE_STATUS_PROCESS,
    };
    struct Service(SC_HANDLE);
    impl Drop for Service {
        fn drop(&mut self) {
            unsafe {
                CloseServiceHandle(self.0);
            }
        }
    }
    let raw = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if raw.is_null() {
        bail!("Cannot inspect the Windows OpenSSH service");
    }
    let manager = Service(raw);
    let name: Vec<u16> = "ssh-agent".encode_utf16().chain(Some(0)).collect();
    let raw = unsafe { OpenServiceW(manager.0, name.as_ptr(), SERVICE_QUERY_STATUS) };
    if raw.is_null() {
        bail!("Windows OpenSSH agent service is not configured");
    }
    let service = Service(raw);
    let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of_val(&status) as u32,
            &mut needed,
        )
    } == 0
        || status.dwCurrentState != SERVICE_RUNNING
        || status.dwProcessId != pid
    {
        bail!("Agent pipe does not belong to the running Windows OpenSSH service");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(super) fn verify(_: Option<&std::path::Path>) -> Result<()> {
    bail!("Agent verification unsupported")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_agent_requires_the_system_job_and_exact_socket_binding() {
        let description = "\tprogram = /usr/bin/ssh-agent\n\tpath = /System/Library/LaunchAgents/com.openssh.ssh-agent.plist\nSSH_AUTH_SOCK => /tmp/system.sock\npath = /tmp/system.sock\nsecure key = SSH_AUTH_SOCK";
        let socket = std::path::Path::new("/tmp/system.sock");
        assert!(launchd_description_matches(description, socket));
        assert!(!launchd_description_matches(
            description,
            std::path::Path::new("/tmp/fake.sock")
        ));
        assert!(!launchd_description_matches(
            &description.replace("/usr/bin/ssh-agent", "/tmp/fake-agent"),
            socket
        ));
        assert!(!launchd_description_matches(
            &description.replace("/System/Library/", "/tmp/"),
            socket
        ));
        assert!(!launchd_description_matches(
            &format!("{description}\n\tprogram = /tmp/fake"),
            socket
        ));
    }

    #[test]
    fn a_fake_agent_cannot_receive_a_registered_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fake.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(verify(Some(&path)).is_err());
        let mut peer = listener.accept().unwrap().0;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut peer, &mut bytes).unwrap();
        assert!(
            bytes.is_empty(),
            "agent verification must not send identity material"
        );
    }
}
