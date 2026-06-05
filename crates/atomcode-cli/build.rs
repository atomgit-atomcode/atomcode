fn main() {
    // Best-effort rebuild hints. These paths do NOT exist inside a git
    // worktree (where .git is a file, not a directory), so cargo will
    // silently treat them as "always re-run" — that's fine: the git
    // subprocess calls are cheap, and the env-var output is stable
    // between commits anyway. If you build inside a regular checkout
    // these become real cache hints.
    //
    // We watch .git/index (not .git/refs/heads/ like the tui crate does)
    // because dirty detection cares about staging-area changes, not
    // branch switches.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    // Inject git short hash as ATOMCODE_BUILD_ID at compile time.
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Detect dirty working tree. --untracked-files=no matches the
    // 'git describe --dirty' convention: only tracked-file changes count.
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=ATOMCODE_BUILD_ID={}", hash);
    println!(
        "cargo:rustc-env=ATOMCODE_BUILD_DIRTY={}",
        if dirty { "+dirty" } else { "" }
    );

    // Embed icon when targeting Windows. Gated on CARGO_CFG_TARGET_OS
    // (the *build target*, not the host) so cross-compile from macOS
    // fires this while native mac/linux builds skip it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon = "assets/atomcode.ico";
        println!("cargo:rerun-if-changed={}", icon);
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon);

        // Embed the UTF-8 activeCodePage manifest so the process runs with
        // UTF-8 as its ANSI code page from the moment the loader creates it.
        // This is more reliable than SetConsoleCP(CP_UTF8) alone, which is a
        // best-effort console-level setting that some IMEs ignore. Supported
        // on Windows 10 1903+; silently ignored on older builds.
        //
        // The manifest also includes standard DPI awareness and supportedOS
        // declarations so we don't lose the defaults that the MSVC CRT would
        // otherwise embed. For mingw targets (which this project uses), these
        // are not provided by the toolchain, so we supply them ourselves.
        res.set_manifest(
            r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application>
    <windowsSettings>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
    <supportedOS>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}"/>
      <supportedOS Id="{35138b9a-5d96-4fbd-8e2d-a2440225f93a}"/>
    </supportedOS>
  </application>
</assembly>"#,
        );
        // FILETYPE must be set for winresource to write the manifest block
        // into the .rc file (it gates on this key at line 578 of lib.rs).
        res.set_version_info(winresource::VersionInfo::FILETYPE, 1);

        if let Err(e) = res.compile() {
            println!("cargo:warning=winresource compile failed: {}", e);
        }
    }
}
