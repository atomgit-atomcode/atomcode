// Test the helper's pure logic via the exposed `run_askpass` function.
#[cfg(unix)]
#[test]
fn helper_sends_token_and_returns_password() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let dir = std::env::temp_dir().join(format!("ak-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s.sock");
    let _ = std::fs::remove_file(&sock);
    let l = UnixListener::bind(&sock).unwrap();
    let sock2 = sock.clone();
    let h = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
        assert!(line.contains("\"token\":\"tok\""), "must send token: {line}");
        s.write_all(b"{\"password\":\"pw\"}\n").unwrap();
    });
    let pw = atomcode::askpass::run_askpass("[sudo] password:", &sock2, "tok");
    assert_eq!(pw.as_deref(), Some("pw"));
    h.join().unwrap();
}
