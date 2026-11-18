//! The sentences a report shows.
//!
//! One place, so the CLI and the screen cannot say the same thing two ways.
//! Every function here has an entry in the product message table.

use atomcode_i18n::product::{t, Msg};

pub fn skill_active() -> String {
    t(Msg::MigrationSkillActive).into_owned()
}
pub fn skill_rejected(reason: &str) -> String {
    t(Msg::MigrationSkillRejected { reason }).into_owned()
}
pub fn skill_overridden(name: &str) -> String {
    t(Msg::MigrationSkillOverridden { name }).into_owned()
}
pub fn unreadable_file(reason: &str) -> String {
    t(Msg::MigrationUnreadableFile { reason }).into_owned()
}
pub fn skill_depth_limit() -> String {
    t(Msg::MigrationSkillDepthLimit).into_owned()
}
pub fn skill_symlink_cycle() -> String {
    t(Msg::MigrationSkillSymlinkCycle).into_owned()
}
/// A `SKILL.md` that exists but is not a regular file (a directory, say).
pub fn skill_md_not_a_file() -> String {
    t(Msg::MigrationSkillMdNotAFile).into_owned()
}
/// The asset is in someone else's tree: it is not in effect, and this is
/// where to put it to bring it over.
///
/// **The wording promises the ACTION, not the outcome** — whether the copy
/// wins against a same-named higher-priority definition is not ours to
/// promise.
pub fn skill_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationSkillCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}
/// The asset is not in effect and nothing says why.
pub fn skill_not_in_effect() -> String {
    t(Msg::MigrationSkillNotInEffect).into_owned()
}
/// The file cannot be parsed, so copying it over would only move the
/// failure. The registry never looked at this file — this is our own
/// reading of it.
pub fn foreign_unparseable() -> String {
    t(Msg::MigrationForeignUnparseable).into_owned()
}
/// The entry is a symlink whose target is gone.
pub fn broken_link() -> String {
    t(Msg::MigrationBrokenLink).into_owned()
}
/// The entry is there, but it is not a regular file — a FIFO, a socket.
pub fn not_a_regular_file() -> String {
    t(Msg::MigrationNotARegularFile).into_owned()
}
/// The tree root is there, but it is not a directory we can walk.
pub fn not_a_directory() -> String {
    t(Msg::MigrationNotADirectory).into_owned()
}
/// **The report's own walk** stopped at the depth limit — not the loader's.
///
/// The loader has a depth limit of its own and says so in its own words,
/// but only about a tree it scans. In a tree it never scanned, the REPORT's
/// walk is what stopped, and "the skill loader rejected it" would put words
/// in the loader's mouth about a file the loader never looked at.
pub fn report_depth_limit() -> String {
    t(Msg::MigrationReportDepthLimit).into_owned()
}
/// **The report's own walk** found a symlink cycle — not the loader's.
pub fn report_symlink_cycle() -> String {
    t(Msg::MigrationReportSymlinkCycle).into_owned()
}
/// The ENTRY is there, but it is not something the loader can use — a
/// dangling symlink, a FIFO, a socket.
///
/// **The report's own finding, not the loader's.** The loader never treated
/// this entry as a skill at all, so putting its words in its mouth would
/// send the person to fix the wrong thing.
pub fn report_entry_unusable(reason: &str) -> String {
    t(Msg::MigrationReportEntryUnusable { reason }).into_owned()
}

pub fn agents_blocked() -> String {
    t(Msg::MigrationAgentsBlocked).into_owned()
}

pub fn hooks_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationHooksCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}

pub fn instructions_active() -> String {
    t(Msg::MigrationInstructionsActive).into_owned()
}

pub fn instructions_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationInstructionsCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}

pub fn instructions_empty() -> String {
    t(Msg::MigrationInstructionsEmpty).into_owned()
}

pub fn instructions_shadowed(by: &str) -> String {
    t(Msg::MigrationInstructionsShadowed { by }).into_owned()
}

pub fn mcp_active(count: usize) -> String {
    t(Msg::MigrationMcpActive { count }).into_owned()
}

pub fn mcp_all_disabled(count: usize) -> String {
    t(Msg::MigrationMcpAllDisabled { count }).into_owned()
}

pub fn mcp_copyable(count: usize, target: &std::path::Path) -> String {
    t(Msg::MigrationMcpCopyable {
        count,
        target: &target.display().to_string(),
    })
    .into_owned()
}

pub fn opencode_plaintext_credentials() -> String {
    t(Msg::MigrationOpencodePlaintextCredentials).into_owned()
}

pub fn permissions_active() -> String {
    t(Msg::MigrationPermissionsActive).into_owned()
}

pub fn permissions_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationPermissionsCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}

pub fn plugins_blocked() -> String {
    t(Msg::MigrationPluginsBlocked).into_owned()
}

pub fn provider_copyable(base_url: Option<&str>, key_kind: Option<&str>) -> String {
    t(Msg::MigrationProviderCopyable {
        base_url: base_url.unwrap_or("-"),
        key: key_kind.unwrap_or("-"),
    })
    .into_owned()
}

pub fn provider_name_unusable(name: &str) -> String {
    t(Msg::MigrationProviderNameUnusable { name }).into_owned()
}

pub fn provider_nothing_to_connect_to() -> String {
    t(Msg::MigrationProviderNothingToConnectTo).into_owned()
}

pub fn provider_protocol_unsupported(protocol: &str) -> String {
    t(Msg::MigrationProviderProtocolUnsupported { protocol }).into_owned()
}

pub fn rules_rejected() -> String {
    t(Msg::MigrationRulesRejected).into_owned()
}

pub fn sessions_blocked(count: usize) -> String {
    t(Msg::MigrationSessionsBlocked { count }).into_owned()
}

pub fn unreadable_config() -> String {
    t(Msg::MigrationUnreadableConfig).into_owned()
}
