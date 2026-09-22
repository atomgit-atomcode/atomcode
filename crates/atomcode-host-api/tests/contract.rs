//! The host-control contract keeps its shape (`docs/adr/0021` §2).
//!
//! It moved out of the kernel on 2026-09-18 and this criterion came with it: a
//! rule about a file belongs beside that file, or it goes on passing while
//! reading something that is no longer there.

use std::path::Path;

/// Host control carries intents, never the host's implementation: no
/// generation, no whole conversation, no agent configuration (`docs/adr/0021`
/// §2 and its failure conditions). Comments may say what is kept out; code may
/// not carry it.
#[test]
fn host_control_names_no_implementation() {
    let host = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let text = std::fs::read_to_string(&host).expect("the host contract");
    let offenders: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| {
            let lower = line.to_lowercase();
            ["generation", "sessionsnapshot", "codingagentconfig"]
                .iter()
                .any(|word| lower.contains(word))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "host control must not carry the host's implementation: {offenders:#?}"
    );
}

/// Nothing in here reaches for a product's vocabulary.
///
/// The rule the 2026-09-18 revision added: a command belongs in this contract
/// only if it means something to a host that is **not** driving a repository —
/// a daemon, a test. `git`, `worktree` and their kin mean nothing there, and
/// `Worktree` was withdrawn to a capability row in the same batch that found
/// this. Without a criterion the next one walks back in.
#[test]
fn the_contract_speaks_no_products_vocabulary() {
    let host = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let text = std::fs::read_to_string(&host).expect("the host contract");
    let offenders: Vec<&str> = text
        .lines()
        .filter(|line| {
            !line.trim_start().starts_with("//") && !line.trim_start().starts_with("///")
        })
        .filter(|line| {
            // Whole words. A substring match called `atomgit` a git reference
            // and this criterion red on its own fixture — and a rule that cries
            // wolf gets weakened rather than obeyed.
            let words: Vec<String> = line
                .to_lowercase()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .map(str::to_string)
                .collect();
            ["worktree", "git", "branch", "commit", "marketplace"]
                .iter()
                .any(|word| words.iter().any(|w| w == word))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "host control is neutral; these belong to a capability row: {offenders:#?}"
    );
}
