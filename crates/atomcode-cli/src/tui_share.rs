//! 把当前终端会话共享出去:浏览器(`/webui`)、手机(`/app`)、以及只挂不开的 `/sync`。
//!
//! 共享的意思是**同一个会话**:浏览器里看到的不是另一段对话,是这一段;在那边打字
//! 也是这一段往下走。做法是把这个 runtime 挂进 daemon 的 live hub(经典界面里
//! `attach_live_runtime` 做的同一件事),网页端连的是同一个 hub。
//!
//! **为什么在 cli**:hub 是 daemon 的东西,屏幕不认识 daemon(`docs/adr/0022` §3)。
//! 屏幕只说「挂上 / 摘下 / 开浏览器」。
//!
//! **三个接点**,少一个网页那边就看不全:
//! 1. **挂上**:把 runtime 的句柄交给 hub,它由此能把网页发来的话派进来;
//! 2. **事件**:runtime 每条事件都推给 hub,网页照它画(`crate::host` 的那趟循环);
//! 3. **本地输入回显**:终端里打的字,网页端是从 hub 的「输入被接受」那条事实看到的
//!    ——不告诉它,网页上就只见回答不见问题。
//!
//! **挂着的东西是一个,不是每块屏幕一个**:hub 一次只绑一个 runtime,所以绑定放在
//! 这里的静态槽里,`/webui`、`/app`、`/sync` 共用它(经典界面同样如此)。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::{Arc, Mutex, OnceLock};

use atomcode_coding::{CodingRuntimeHandle, SequencedRuntimeEvent, UserInput};
use atomcode_daemon::live_hub::LiveBinding;

/// 活着的那个 runtime,照共享要用的样子。
///
/// 由 `crate::host::connect` 在接上时交过来:句柄不在宿主契约里(它是产品的东西),
/// 而共享要的正是它——hub 拿着它才能把网页发来的话派进来。会话 id 要**每次现问**,
/// 不能记下来:`/clear`、`/resume` 都会换会话,记住的那个共享出去就是另一段对话。
pub trait Live: Send + Sync {
    fn handle(&self) -> CodingRuntimeHandle;
    fn session(&self) -> String;
    fn working_dir(&self) -> std::path::PathBuf;
}

fn live() -> &'static Mutex<Option<Arc<dyn Live>>> {
    static LIVE: OnceLock<Mutex<Option<Arc<dyn Live>>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(None))
}

/// 接上时把它交过来。
pub fn remember(what: Arc<dyn Live>) {
    *live().lock().expect("live poisoned") = Some(what);
}

/// 当前挂着的那个绑定。
fn bound() -> &'static Mutex<Option<LiveBinding>> {
    static BOUND: OnceLock<Mutex<Option<LiveBinding>>> = OnceLock::new();
    BOUND.get_or_init(|| Mutex::new(None))
}

/// 现在共享着吗。
pub fn sharing() -> bool {
    bound().lock().expect("binding poisoned").is_some()
}

/// 把这个 runtime 挂进 hub。已经挂着就当做成了——`/webui` 与 `/app` 各自都要确保
/// 挂上,而它们可能先后被敲。
pub async fn attach(config_path: &std::path::Path) -> Result<(), String> {
    if sharing() {
        return Ok(());
    }
    let live = live()
        .lock()
        .expect("live poisoned")
        .clone()
        .ok_or_else(|| tr(SMsg::HostUnavailable).into_owned())?;
    let handle = live.handle();
    let session_id = live.session();
    let working_dir = live.working_dir();
    let config =
        atomcode_config::config::Config::load(config_path).map_err(|error| error.to_string())?;
    let (selection, _model) = resolved_selection(&config);
    if selection.is_empty() {
        return Err(tr(SMsg::ShareNoModel).into_owned());
    }
    let fingerprint = atomcode_daemon::native_live::provider_fingerprint(&config, &selection)?;
    // 快照是网页端的开场:它进来时对话已经进行了一半,得先看到之前说了什么。
    let snapshot = handle.snapshot().await.map_err(|error| error.to_string())?;
    // 句柄给的是共享的那一份;hub 要自己的一份,它此后会跟着网页端的事件往前走。
    let snapshot = (*snapshot).clone();
    let binding = atomcode_daemon::native_live::register_embedded_runtime(
        session_id.to_string(),
        working_dir.to_path_buf(),
        selection,
        fingerprint,
        snapshot,
        Arc::new(handle.clone()),
    )
    .map_err(|error| format!("{error:?}"))?;
    *bound().lock().expect("binding poisoned") = Some(binding);
    Ok(())
}

/// 摘下来。没挂着返回 `false`——`/sync off` 要能说出「本来就没共享」。
pub fn detach() -> Result<bool, String> {
    let taken = bound().lock().expect("binding poisoned").take();
    let Some(binding) = taken else {
        return Ok(false);
    };
    atomcode_daemon::native_live::unregister_embedded_runtime(&binding)
        .map_err(|error| format!("{error:?}"))?;
    Ok(true)
}

/// runtime 的一条事件,推给网页端。没挂着就什么也不做。
///
/// 迟到的事件(hub 已经换了绑定)按「过期」丢掉,不当错误:那是另一个 runtime 的
/// 事了,而这趟循环还在收着上一个的尾巴。
pub fn publish(event: &SequencedRuntimeEvent) {
    let guard = bound().lock().expect("binding poisoned");
    let Some(binding) = guard.as_ref() else {
        return;
    };
    let _ = atomcode_daemon::native_live::publish(binding, event.clone());
}

/// 终端里打的这句话,让网页端也看见。
pub fn echo_local_input(input: &UserInput) {
    if !sharing() {
        return;
    }
    let _ = atomcode_daemon::native_live::accept_local_input(input.clone());
}

/// 当前该用哪个模型选择(与经典界面同一条解析边界)。
fn resolved_selection(config: &atomcode_config::config::Config) -> (String, String) {
    if let Ok(resolved) = config.resolve_model(None) {
        return (resolved.selection_id, resolved.model);
    }
    let mut ids: Vec<String> = config.logical_models().into_keys().collect();
    ids.sort();
    ids.into_iter()
        .find_map(|id| {
            config
                .resolve_model(Some(&id))
                .ok()
                .map(|resolved| (resolved.selection_id, resolved.model))
        })
        .unwrap_or_default()
}

// ---- 命令 -------------------------------------------------------------------

/// 行的名字。
pub const ROW: &str = "tui-share";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 挂上 `/webui`、`/sync`、`/desktop`。
pub struct ShareRow {
    pub config_path: std::path::PathBuf,
}

#[async_trait::async_trait]
impl atomcode_plexus::Plugin for ShareRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "sharing this terminal session: the browser, and the desktop app"
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _config: &serde_json::Value,
    ) -> Result<(), String> {
        let commands = ctx
            .require::<atomcode_tui::plugin::CommandsSvc>()
            .map_err(|e| e.to_string())?;
        commands.add(Arc::new(ShareCommands {
            config_path: self.config_path.clone(),
        }))?;
        Ok(())
    }
}

struct ShareCommands {
    config_path: std::path::PathBuf,
}

#[async_trait::async_trait]
impl atomcode_tui::command::CommandSet for ShareCommands {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<atomcode_tui::command::Command> {
        use atomcode_tui::command::Command;
        vec![
            Command::said_taking("webui", tr(SMsg::WebuiTakes), tr(SMsg::CmdAboutWebui)),
            Command::said_taking("sync", tr(SMsg::SyncTakes), tr(SMsg::CmdAboutSync)),
            Command::said("desktop", tr(SMsg::CmdAboutDesktop)),
        ]
    }

    async fn run(
        &self,
        name: &str,
        args: &str,
        _ctx: &atomcode_plexus::Context,
    ) -> atomcode_tui::command::Outcome {
        use atomcode_tui::command::Outcome;
        let args = args.trim();
        match name {
            "desktop" => Outcome::Said(open_desktop()),
            "sync" if args == "off" => match detach() {
                Ok(true) => Outcome::Said(tr(SMsg::ShareStopped).into_owned()),
                Ok(false) => Outcome::Said(tr(SMsg::ShareWasNotOn).into_owned()),
                Err(why) => Outcome::Refused(why),
            },
            "sync" => match attach(&self.config_path).await {
                Ok(()) => Outcome::Said(tr(SMsg::ShareStarted).into_owned()),
                Err(why) => Outcome::Refused(why),
            },
            "webui" if args == "stop" => {
                let said = atomcode_daemon::stop_server();
                let _ = detach();
                Outcome::Said(said)
            }
            "webui" => {
                // 挂上再开浏览器:反过来的话,网页先连上一个还没绑定的 hub,
                // 第一眼是空的。
                if let Err(why) = attach(&self.config_path).await {
                    return Outcome::Refused(why);
                }
                let host = bind_host(args);
                Outcome::Said(
                    atomcode_daemon::ensure_server_and_open(
                        &host,
                        atomcode_daemon::WEBUI_DEFAULT_PORT,
                        true,
                    )
                    .await,
                )
            }
            _ => Outcome::Quiet,
        }
    }
}

/// `/webui` 绑哪个地址。默认只听本机;`lan`(或直接写地址)才暴露出去——把一台
/// 能改你代码的机器暴露到网上是一个决定,不是默认值。
pub(crate) fn bind_host(args: &str) -> String {
    let args = args.trim();
    if args == "lan" || args == "0.0.0.0" {
        return "0.0.0.0".to_string();
    }
    let tokens: Vec<&str> = args.split_whitespace().collect();
    for (at, token) in tokens.iter().enumerate() {
        if let Some(host) = token.strip_prefix("--host=") {
            if !host.is_empty() {
                return host.to_string();
            }
        }
        if *token == "--host" {
            if let Some(host) = tokens.get(at + 1) {
                return (*host).to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

/// 打开桌面端,或说它没装、去哪儿下载。
fn open_desktop() -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let env = |key: &str| std::env::var(key).ok();
    let candidates = crate::desktop::candidate_apps(&home, &env);
    match crate::desktop::detect(&candidates, |path| path.exists()) {
        Some(found) => {
            let path = found.path.display().to_string();
            match crate::desktop::launch(found) {
                Ok(()) => tr(SMsg::DesktopOpening {
                    name: found.display_name,
                    path: &path,
                })
                .into_owned(),
                Err(error) => tr(SMsg::DesktopLaunchFailed {
                    path: &path,
                    error: &error.to_string(),
                })
                .into_owned(),
            }
        }
        None => tr(SMsg::DesktopNotInstalled {
            url: crate::desktop::download_url(),
        })
        .into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认只听本机。把一台能改你代码的机器暴露到网上是一个决定,不是默认值。
    #[test]
    fn sharing_stays_on_this_machine_unless_asked_otherwise() {
        assert_eq!(bind_host(""), "127.0.0.1");
        assert_eq!(bind_host("lan"), "0.0.0.0");
        assert_eq!(bind_host("0.0.0.0"), "0.0.0.0");
        assert_eq!(bind_host("--host 192.168.1.9"), "192.168.1.9");
        assert_eq!(bind_host("--host=192.168.1.9"), "192.168.1.9");
        // 写了 --host 却没给地址:当没说,回到只听本机——而不是把它当成
        // 「随便绑哪儿」。
        assert_eq!(bind_host("--host"), "127.0.0.1");
        assert_eq!(bind_host("--host="), "127.0.0.1");
    }
}
