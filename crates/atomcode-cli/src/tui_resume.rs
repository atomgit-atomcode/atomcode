//! 恢复面板的挂载:把 `atomcode-tui` 的恢复面板画法挂到屏幕上,并接上「删掉一个
//! 会话」那一条端口。
//!
//! 列表不走这一层:会话目录是 `/resume` 命令自己那一趟异步往返读的
//! (`atomcode_tui::commands`),答案经 `Action::OpenResume` 带进面板。删除要走,
//! 因为它是面板自己发起的,而会话存在磁盘上——屏幕不认识磁盘(`docs/adr/0022` §3)。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_host_api::HostCommand;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::{AgentClientSvc, ModulesSvc, ResumeSvc};
use serde_json::Value;

/// 行的名字,插件和点它的那一层共用一个串。
pub const ROW: &str = "tui-panel-resume";

/// 把恢复面板挂到屏幕上的那一行。
///
/// **启动器的行,不是屏幕的**,和别的面板行一样:`atomcode-tui` 带的是面板的*画法*,
/// 它不认宿主控制契约。
pub struct ResumeRow;

#[async_trait]
impl Plugin for ResumeRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-resume", "tui-resume-store"]
    }
    fn description(&self) -> &'static str {
        "the resume panel: the sessions you can pick up again"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::resume::Resume>::new());
        let id = <atomcode_tui::modules::resume::Resume as atomcode_tui::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        let _ = ctx
            .provide::<ResumeSvc>(Arc::new(ResumePort { ctx: ctx.clone() }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 这一行的那一层,给装配用。
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 一趟宿主往返。每次问 `ctx` 要连接,不持着:连接会换(换会话、重连)。
struct ResumePort {
    ctx: Context,
}

#[async_trait]
impl atomcode_tui::resume::Resume for ResumePort {
    async fn delete(&self, id: &str) -> Result<(), String> {
        let client = self
            .ctx
            .service::<AgentClientSvc>()
            .ok_or_else(|| tr(SMsg::HostUnavailable).into_owned())?;
        let control = client
            .control()
            .ok_or_else(|| tr(SMsg::HostHasNoControl).into_owned())?;
        control
            .call(HostCommand::DeleteSession {
                session: id.to_string(),
            })
            .await
            .map(|_| ())
            .map_err(crate::tui_tools::said)
    }
}
