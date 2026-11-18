//! Claude Code assets. Filled in by Task 2.

use super::report::AssetReport;
use super::{MigrationSource, ScanContext, SkillTruth};

pub struct ClaudeSource;

impl MigrationSource for ClaudeSource {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn detect(&self, _ctx: &ScanContext, _truth: &SkillTruth) -> Vec<AssetReport> {
        Vec::new()
    }
}
