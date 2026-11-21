//! Local setup port. The host owns scanning, installed-binary checks, configuration
//! edits and confirmed installation; the screen owns lists and confirmation sheets.
//! Seed bootstrap remains available for the model-driven automation recommender.
//! This crate does not enable the capabilities `setup` feature or own MCP clients.

/// Local capability setup and the existing seed-skill bootstrap.
///
/// 由启动屏幕的那一方填，和 [`crate::plugins::Plugins`] 同一个形状、同一条理由。
/// 回来的 `Ok` 是一句给人看的话（产物自带的报告原样带回来），`Err` 是启动器的拒绝。
#[async_trait::async_trait]
pub trait Setup: Send + Sync {
    /// 种子 skill 在不在。读本地几个小文件，同步。
    ///
    /// 拿来省掉一次多余的安装：装过的机器上重跑 `/setup`，本来只需要把那条命令
    /// 转发给 agent，再解压一遍、再锁一次文件、再重建一遍能力图都是白做的。
    fn installed(&self) -> bool;

    /// 装上种子，回一句给人看的话（装了什么、跳过了什么、有没有失败的）。
    async fn install(&self) -> Result<String, String>;

    fn capability_setup(&self) -> bool {
        false
    }

    /// Local capability inspection. `None` preserves seed-only embedders.
    async fn check(
        &self,
        _root: &std::path::Path,
        _force: bool,
    ) -> Result<Option<CapabilityReport>, String> {
        Ok(None)
    }
    async fn plan(&self, _root: &std::path::Path, _id: &str) -> Result<CapabilityPlan, String> {
        Err("Capability installation is unavailable".into())
    }
    async fn apply(
        &self,
        _root: &std::path::Path,
        _id: &str,
        _token: &str,
    ) -> Result<String, String> {
        Err("Capability installation is unavailable".into())
    }
    async fn cancel(&self) -> Result<String, String> {
        Err("No setup operation is running".into())
    }
    async fn dismiss(&self, _root: &std::path::Path) -> Result<String, String> {
        Ok(String::new())
    }
}

#[derive(Debug, Clone)]
pub struct CapabilityReport {
    pub summary: String,
    pub entries: Vec<CapabilityEntry>,
}

#[derive(Debug, Clone)]
pub struct CapabilityEntry {
    pub id: String,
    pub name: String,
    pub status: String,
    pub detail: String,
    pub installable: bool,
}

#[derive(Debug, Clone)]
pub struct CapabilityPlan {
    pub id: String,
    pub detail: String,
    /// Host-generated identity of the exact plan the user is confirming.
    pub token: String,
}
