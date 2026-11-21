//! Human-only Grape controls. Authentication stays in the launcher, off the UI loop.
use async_trait::async_trait;
use atomcode_harness::seams::UiSvc;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::{
    command::{Command, CommandSet, Outcome},
    plugin::{CommandsSvc, RepaintSvc},
};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

pub const ROW: &str = "tui-grape";
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}
pub struct GrapeRow {
    pub user_dir: PathBuf,
}
type Active = Arc<Mutex<Option<Arc<AtomicBool>>>>;
fn cancel(active: &Active) -> bool {
    if let Some(stop) = active.lock().expect("grape flow poisoned").as_ref() {
        stop.store(true, Ordering::Release);
        true
    } else {
        false
    }
}
#[async_trait]
impl Plugin for GrapeRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "human Grape cloud authorization and connection status"
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        let active = Arc::new(Mutex::new(None));
        let ending = active.clone();
        let _ = ctx.effect(move || {
            cancel(&ending);
        });
        ctx.require::<CommandsSvc>()
            .map_err(|e| e.to_string())?
            .add(Arc::new(GrapeCommands {
                user_dir: self.user_dir.clone(),
                active,
            }))?;
        Ok(())
    }
}
struct GrapeCommands {
    user_dir: PathBuf,
    active: Active,
}
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Status,
    Connect(String),
    Disconnect,
    Cancel,
    Help,
}
fn action(args: &str, connected: bool) -> Result<Action, &'static str> {
    let parts: Vec<_> = args.split_whitespace().collect();
    match parts.as_slice() {
        [] if connected => Ok(Action::Status),
        [] | ["connect"] => Ok(Action::Connect(atomcode_auth::grape::DEFAULT_SERVER.into())),
        ["connect", url] => Ok(Action::Connect((*url).into())),
        ["status"] => Ok(Action::Status),
        ["disconnect"] => Ok(Action::Disconnect),
        ["cancel"] => Ok(Action::Cancel),
        ["help"] => Ok(Action::Help),
        _ => Err("用法：/grape [connect [HTTPS地址]|status|disconnect|cancel|help]"),
    }
}
#[async_trait]
impl CommandSet for GrapeCommands {
    fn id(&self) -> &'static str {
        "cmd-tui-grape"
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            "grape",
            "[connect|status|disconnect|cancel]".into(),
            "连接 Grape 云端，查询账号与授权状态".into(),
        )]
    }
    async fn run(&self, _: &str, args: &str, ctx: &Context) -> Outcome {
        // Parse cancel/help without opening the credential store or waiting for network I/O.
        if args.trim() == "cancel" {
            return Outcome::Said(
                if cancel(&self.active) {
                    "已请求取消 Grape 授权，正在结束当前请求。"
                } else {
                    "当前没有 Grape 操作。"
                }
                .into(),
            );
        }
        let ui = match ctx.require::<UiSvc>() {
            Ok(ui) => ui,
            Err(e) => return Outcome::Refused(e.to_string()),
        };
        let repaint = ctx.service::<RepaintSvc>();
        let mut slot = self.active.lock().expect("grape flow poisoned");
        if slot.is_some() {
            return Outcome::Refused(
                "Grape 操作正在进行；使用 /grape cancel 取消授权等待。".into(),
            );
        }
        let stop = Arc::new(AtomicBool::new(false));
        *slot = Some(stop.clone());
        drop(slot);
        let active = self.active.clone();
        let path = self.user_dir.clone();
        let args = args.to_string();
        tokio::task::spawn_blocking(move || {
            let say = |line: &str| {
                ui.say(line);
                if let Some(repaint) = &repaint {
                    repaint.now();
                }
            };
            let result = (|| -> anyhow::Result<()> {
                let connected = if args.trim().is_empty() {
                    atomcode_auth::grape::load(&path)?.is_some()
                } else {
                    false
                };
                match action(&args, connected).map_err(anyhow::Error::msg)? {
                    Action::Status => say(&crate::grape_connect::status(&path)?),
                    Action::Help => say("/grape：连接官方云端或查看已连接账号\n/grape connect [HTTPS地址]：授权连接\n/grape status：本地状态\n/grape disconnect：撤销授权\n/grape cancel：取消等待\n云端内测申请：https://www.grapedev.cn/app/（GitCode 登录）"),
                    Action::Cancel => {},
                    Action::Connect(server) => crate::grape_connect::connect(&path, &server, false, &say, &|| stop.load(Ordering::Acquire))?,
                    Action::Disconnect => {
                        crate::grape_connect::online()?;
                        let _lease = atomcode_auth::grape::lease(&path)?;
                        atomcode_auth::grape::disconnect(&path)?;
                        say("Grape 已断开连接。");
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                say(&format!("Grape：{error}"));
            }
            active.lock().expect("grape flow poisoned").take();
        });
        Outcome::Quiet
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloud_is_default_and_private_hosts_are_explicit() {
        assert_eq!(
            action("", false).unwrap(),
            Action::Connect("https://www.grapedev.cn/".into())
        );
        assert_eq!(action("", true).unwrap(), Action::Status);
        assert_eq!(
            action("connect https://private.example/", true).unwrap(),
            Action::Connect("https://private.example/".into())
        );
        assert!(action("connect https://private.example extra", false).is_err());
    }
    #[test]
    fn cancel_stops_the_current_operation_without_freeing_its_slot_early() {
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(Some(stop.clone())));
        assert!(cancel(&active));
        assert!(stop.load(Ordering::Acquire));
        assert!(active.lock().unwrap().is_some());
        active.lock().unwrap().take();
        assert!(!cancel(&active));
    }
}
