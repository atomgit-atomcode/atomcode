//! 恢复面板的挂载:把 `atomcode-tui` 的恢复面板画法挂到屏幕上。
//!
//! 和别的面板行(`tui-panel-rewind` 等)不一样,这一行**只挂画法**,不带 seam:会话
//! 目录是 `/resume` 命令自己那一趟异步往返读的(`atomcode_tui::commands`),答案经
//! `Action::OpenResume` 带进面板,不用这一层再开一条宿主端口。

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::ModulesSvc;
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
        &["tui-resume"]
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
        Ok(())
    }
}

/// 这一行的那一层,给装配用。
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}
