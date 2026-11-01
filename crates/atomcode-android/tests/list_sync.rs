//! The lists that have to agree, checked instead of remembered.
//!
//! One set of provider packages is described from four directions, and a mistake
//! in any one of them is silent at build time and confusing on the device:
//!
//! * `AndroidManifest.xml` declares which packages this app may *see* — since
//!   Android 11 a package it has not declared raises `NameNotFoundException` even
//!   when the provider is installed;
//! * `AgentService.kt` names the same packages so it can collect each one's
//!   `nativeLibraryDir`, which is what makes `make` / `zig` / `node` resolvable;
//! * `shims.rs` links the binaries those packages carry into `<data>/bin`;
//! * `packages.rs` (daemon) is what the package panel reports as available.
//!
//! Each file says "this must stay in sync with …" in a comment, which is a
//! promise no compiler checks. This test is the check: two of the four are
//! Kotlin/XML, which Rust cannot link against, so they are read as source text and
//! the lists are compared. A file that loses the expected shape fails loudly
//! instead of the test quietly passing on an empty list.

use std::path::PathBuf;

use atomcode_android::shims::{HELPER_TOOLS, PROVIDER_TOOLS};

/// Read a file from the repository, resolved from this crate's directory.
fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
}

/// Every double-quoted string between `start` and the first `end` after it.
///
/// Deliberately a string scan rather than a regex: the three files are not code
/// the test compiles, and the shapes it looks for (`listOf(…)`, `&[…];`) are
/// stable, single-level constructs.
fn quoted_between(source: &str, start: &str, end: &str) -> Vec<String> {
    let from = source
        .find(start)
        .unwrap_or_else(|| panic!("`{start}` is no longer in this file; update this test"))
        + start.len();
    let rest = &source[from..];
    let to = rest
        .find(end)
        .unwrap_or_else(|| panic!("`{end}` no longer closes `{start}`"));
    rest[..to]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The `com.atomcode.*` packages the manifest lets this app see.
fn declared_provider_packages() -> Vec<String> {
    let manifest = repo_file("android/app/src/main/AndroidManifest.xml");
    quoted_between(&manifest, "<queries>", "</queries>")
        .into_iter()
        .filter(|name| name.starts_with("com.atomcode."))
        .collect()
}

/// The packages `AgentService` asks the package manager about.
fn kotlin_helper_packages() -> Vec<String> {
    let source = repo_file("android/app/src/main/java/com/atomcode/app/AgentService.kt");
    quoted_between(&source, "HELPER_PACKAGES = listOf(", "\n        )")
}

/// The tool names the daemon's `/packages/helper` reports as present.
fn daemon_reported_commands() -> Vec<String> {
    let source = repo_file("crates/atomcode-daemon/src/packages.rs");
    quoted_between(&source, "const HELPER_COMMANDS: &[&str] = &[", "];")
}

/// A provider the app links but the manifest hides: the links are never written
/// and nothing says why. This is the pair that failed silently before the note in
/// `docs/android-next-steps.md` — the check is worth more than the note.
#[test]
fn the_manifest_lets_this_app_see_every_provider_it_links() {
    let mut declared = declared_provider_packages();
    let mut listed = kotlin_helper_packages();
    declared.sort();
    listed.sort();
    assert_eq!(
        declared, listed,
        "AndroidManifest.xml `<queries>` and AgentService.HELPER_PACKAGES must name \
         the same packages: the manifest decides what this app can see, the Kotlin \
         list decides whose nativeLibraryDir it links"
    );
}

/// The panel is built from `/packages/helper`, which filters `HELPER_COMMANDS`
/// against the links in `<data>/bin`. A tool the host links but the daemon does
/// not name is installed and invisible at the same time — the "装了不生效"
/// report from the device.
#[test]
fn every_helper_tool_is_reported_by_the_package_panel() {
    let reported = daemon_reported_commands();
    let missing: Vec<&str> = HELPER_TOOLS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !reported.iter().any(|seen| seen == name))
        .collect();
    assert!(
        missing.is_empty(),
        "packages.rs HELPER_COMMANDS is missing {missing:?}; the package panel would \
         not list them even though the host links them"
    );
}

/// Two entries for one command name would have the link loop write the same path
/// twice, with the last provider silently winning — `cc` and `tcc` are one binary
/// and are still two names, which is why this is checked by name and not by file.
#[test]
fn no_command_name_is_claimed_by_two_entries() {
    let mut names: Vec<&str> = HELPER_TOOLS
        .iter()
        .map(|(name, _)| *name)
        .chain(PROVIDER_TOOLS.iter().map(|(name, _)| *name))
        .collect();
    names.sort_unstable();
    let duplicates: Vec<&str> = names
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .map(|pair| pair[0])
        .collect();
    assert!(
        duplicates.is_empty(),
        "these command names are linked twice: {duplicates:?}"
    );
}
