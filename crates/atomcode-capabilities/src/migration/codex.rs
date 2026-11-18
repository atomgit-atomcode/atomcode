//! Codex assets. Filled in by Task 3.

use super::report::AssetReport;
use super::{MigrationSource, ScanContext, SkillTruth};

pub struct CodexSource;

impl MigrationSource for CodexSource {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    fn detect(&self, _ctx: &ScanContext, _truth: &SkillTruth) -> Vec<AssetReport> {
        Vec::new()
    }
}
