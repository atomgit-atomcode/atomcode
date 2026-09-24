//! 后台面板的挂载:把 `atomcode-tui` 的后台面板画法挂到屏幕上
//! (`docs/plans/2026-09-25-bg-design.md` §四)。
//!
//! 不带端口:面板要做的事都说成命令(`/resume`、`/background`、`/bg`),命令走宿主
//! 控制契约;列表由宿主推(`HostEvent::BackgroundChanged`)。这一行只让屏幕有东西
//! 画它——**启动器的行,不是屏幕的**,和别的面板一样:能不能有后台会话是这个宿主
//! (`crate::background`)说了算。

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::ModulesSvc;
use serde_json::Value;

/// 行的名字,插件和点它的那一层共用一个串。
pub const ROW: &str = "tui-panel-bg";

/// 把后台面板挂到屏幕上的那一行。
pub struct BgRow;

#[async_trait]
impl Plugin for BgRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-bg"]
    }
    fn description(&self) -> &'static str {
        "the background panel: sessions kept running out of view"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::bg::Bg>::new());
        let id = <atomcode_tui::modules::bg::Bg as atomcode_tui::module::View>::id();
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
