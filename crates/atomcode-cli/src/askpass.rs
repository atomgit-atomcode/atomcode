//! Blocking helper for the `atomcode __askpass` subcommand.
//!
//! Connects to the askpass Unix-domain socket, sends a `Request` frame
//! (nonce token + prompt text), and reads back a `Response` frame that
//! contains the password.  Uses blocking `std` I/O — no async runtime —
//! because the helper is a tiny short-lived process invoked by sudo/ssh.

#![cfg(unix)]

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use atomcode_askpass::protocol::{read_frame, write_frame, Request, Response};

/// Connect to `sock`, authenticate with `token`, send `prompt`, and return
/// the password the TUI user typed.  Returns `None` on any I/O or protocol
/// error.
pub fn run_askpass(prompt: &str, sock: &Path, token: &str) -> Option<String> {
    let mut stream = UnixStream::connect(sock).ok()?;
    let req = Request {
        token: token.to_owned(),
        prompt: prompt.to_owned(),
    };
    write_frame(&mut stream, &req).ok()?;
    let mut reader = BufReader::new(stream);
    let resp: Response = read_frame(&mut reader).ok()?;
    resp.password
}
