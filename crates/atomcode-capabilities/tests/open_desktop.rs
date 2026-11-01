//! The desktop opener, launched for real.
//!
//! Ignored by default: it puts a window on the screen of whoever runs it. Run it
//! deliberately when changing how a file reaches the desktop, e.g.
//!
//! ```text
//! ATOMCODE_OPEN_COMMAND=notepad cargo test -p atomcode-capabilities \
//!     --test open_desktop -- --ignored --nocapture
//! ```
//!
//! With `ATOMCODE_OPEN_COMMAND` unset the same call goes to the platform's default
//! handler (`explorer.exe` / `xdg-open` / `open`), so either way the expected
//! result is a window showing the probe file — and the returned message says which
//! launcher was used, which is what this test asserts.

use atomcode_capabilities::tools::{LocalOpener, OpenTarget, Opener};

#[tokio::test]
#[ignore = "launches a GUI window on the machine running the test"]
async fn opens_a_real_file_through_the_configured_or_default_launcher() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("open-desktop-probe.txt");
    std::fs::write(&file, "hello from the open_file tool\n").expect("write the probe file");

    let message = LocalOpener
        .open(&OpenTarget::Path(file))
        .await
        .expect("the opener should hand the file over");
    println!("{message}");
    assert!(
        message.contains("open-desktop-probe.txt"),
        "the message should name the file it opened: {message}"
    );
    assert!(
        message.contains("Opened"),
        "the message should say it opened something: {message}"
    );
}
