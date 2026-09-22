//! `/proxy`:出站代理怎么走——跟随系统、固定当前的、不走代理。
//!
//! 经典界面的 `/proxy` 在新屏幕上的对应(翻默认后「原有的功能不能丢」,
//! `docs/plans/2026-09-19-remaining-gaps.md` 2026-09-22)。三档与经典界面一致:
//! `follow_system`(默认)、`default_proxy`(把这次启动时环境里的代理固定写进配置)、
//! `no_proxy`。
//!
//! **为什么是启动器的行**:写 `config.toml`、改进程的代理环境,都是这个二进制的事,
//! 屏幕不该认识配置文件(`docs/adr/0022` §3)。
//!
//! **生效要三步,少一步都是假生效**:写进文件 → 改进程的代理环境
//! (`apply_process_proxy_config`,之后新建的 HTTP 客户端照它走)→ 让宿主重连模型
//! (`HostCommand::Reload`)。最后一步不能省:活着的 provider 客户端在建的时候就把
//! 旧代理定死了,经典界面曾经漏过这一步,结果所有请求一直往一个已经不在的代理上走,
//! 直到重启。宿主判断「重读还是重建」看的是整个配置文件的指纹,文件变了就一定重建。
//!
//! **屏幕上不出现代理地址**:地址里可能带账号密码,所以只说「固定了几个变量」
//! (`ProxyConfig::summary`)。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_config::proxy::{ProxyConfig, ProxyMode};
use atomcode_host_api::HostCommand;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::overlay::{Choice, Picker};
use atomcode_tui::plugin::{AgentClientSvc, CommandsSvc};
use serde_json::Value;

/// 行的名字。
pub const ROW: &str = "tui-proxy";

/// 命令名。
pub const COMMAND: &str = "proxy";

/// 三档,按选择框里的顺序。
const MODES: [ProxyMode; 3] = [
    ProxyMode::FollowSystem,
    ProxyMode::DefaultProxy,
    ProxyMode::NoProxy,
];

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 挂上 `/proxy`。
pub struct ProxyRow {
    pub config_path: PathBuf,
}

#[async_trait]
impl Plugin for ProxyRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "the outbound proxy: follow the system, pin the current one, or none"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        commands.add(Arc::new(ProxyCommands {
            config_path: self.config_path.clone(),
        }))?;
        Ok(())
    }
}

struct ProxyCommands {
    config_path: PathBuf,
}

#[async_trait]
impl CommandSet for ProxyCommands {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            COMMAND,
            tr(SMsg::ProxyTakes),
            tr(SMsg::CmdAboutProxy),
        )]
    }

    async fn run(&self, _name: &str, args: &str, ctx: &Context) -> Outcome {
        let wanted = args.trim();
        let current = current(&self.config_path);
        if wanted.is_empty() {
            return Outcome::Open(picker(&current));
        }
        let Some(next) = desired(&current, wanted) else {
            return Outcome::Refused(tr(SMsg::ProxyUnknown { wanted }).into_owned());
        };
        if let Err(error) = save(&self.config_path, &next) {
            return Outcome::Refused(tr(SMsg::ProxySaveFailed { error: &error }).into_owned());
        }
        atomcode_config::proxy::apply_process_proxy_config(&next);
        let summary = next.summary();
        match reconnect(ctx).await {
            Ok(()) => Outcome::Said(tr(SMsg::ProxySet { summary: &summary }).into_owned()),
            Err(error) => Outcome::Said(
                tr(SMsg::ProxySetNotReconnected {
                    summary: &summary,
                    error: &error,
                })
                .into_owned(),
            ),
        }
    }
}

/// 配置文件里现在写的;没有文件就是默认。
fn current(path: &Path) -> ProxyConfig {
    if !path.exists() {
        return ProxyConfig::default();
    }
    atomcode_config::config::Config::load(path)
        .map(|config| config.network.proxy)
        .unwrap_or_default()
}

/// 选中某一档之后要写进去的。与经典界面同一套规则:跟随系统 / 不走代理只换
/// 模式、留着已固定的变量(以后再切回 `default_proxy` 前它们不起作用);固定则
/// 重新抓这次启动时的环境。
pub(crate) fn desired(current: &ProxyConfig, wanted: &str) -> Option<ProxyConfig> {
    match wanted {
        "follow_system" => Some(ProxyConfig {
            mode: ProxyMode::FollowSystem,
            ..current.clone()
        }),
        "default_proxy" => Some(ProxyConfig::capture_from_env()),
        "no_proxy" => Some(ProxyConfig {
            mode: ProxyMode::NoProxy,
            ..current.clone()
        }),
        _ => None,
    }
}

fn picker(current: &ProxyConfig) -> Arc<Picker> {
    let captured = ProxyConfig::capture_from_env().summary();
    let choices = MODES
        .iter()
        .map(|mode| {
            let about = match mode {
                ProxyMode::FollowSystem => tr(SMsg::ProxyFollowSystemAbout),
                ProxyMode::DefaultProxy => tr(SMsg::ProxyDefaultProxyAbout {
                    captured: &captured,
                }),
                ProxyMode::NoProxy => tr(SMsg::ProxyNoProxyAbout),
            };
            Choice::new(format!("/{COMMAND} {}", mode.as_str()), mode.as_str())
                .about(about)
                .marked(current.mode == *mode)
        })
        .collect();
    Picker::new(
        COMMAND,
        tr(SMsg::ProxyPickerTitle {
            current: &current.summary(),
        }),
        choices,
    )
}

fn save(path: &Path, next: &ProxyConfig) -> Result<(), String> {
    atomcode_config::ConfigStore::new(path.to_path_buf())
        .update(|config| {
            config.network.proxy = next.clone();
            Ok(())
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// 让宿主重连模型。每次问 `ctx` 要连接,不持着:连接会换(换会话、重连)。
async fn reconnect(ctx: &Context) -> Result<(), String> {
    let client = ctx
        .service::<AgentClientSvc>()
        .ok_or_else(|| tr(SMsg::HostUnavailable).into_owned())?;
    let control = client
        .control()
        .ok_or_else(|| tr(SMsg::HostHasNoControl).into_owned())?;
    control
        .call(HostCommand::Reload {
            session: client.root(),
        })
        .await
        .map(|_| ())
        .map_err(crate::tui_tools::said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_mode_keeps_what_was_pinned_and_pinning_captures_afresh() {
        let pinned = ProxyConfig {
            mode: ProxyMode::DefaultProxy,
            http: Some("http://127.0.0.1:7890".into()),
            ..ProxyConfig::default()
        };
        let off = desired(&pinned, "no_proxy").expect("a mode");
        assert_eq!(off.mode, ProxyMode::NoProxy);
        assert_eq!(
            off.http, pinned.http,
            "turning it off keeps the pinned values"
        );
        let follow = desired(&pinned, "follow_system").expect("a mode");
        assert_eq!(follow.mode, ProxyMode::FollowSystem);
        assert_eq!(
            desired(&pinned, "default_proxy").expect("a mode"),
            ProxyConfig::capture_from_env(),
            "pinning takes this launch's environment, not what was pinned before"
        );
        assert!(desired(&pinned, "socks").is_none());
    }
}
