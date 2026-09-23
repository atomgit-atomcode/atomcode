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
    // 远端那边的「模式」徽标读的是 daemon 里的一个全局值,不是事件流。挂上时先
    // 把当前模式告诉它,否则手机和网页顶上写的是默认值——看着像你没在 plan 里,
    // 而你在。
    if let Ok(mode) = handle.mode().await {
        atomcode_daemon::live_set_mode(mode);
    }
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

/// 模式换了,远端的徽标跟着换。没共享就什么也不做。
pub fn mode_changed(mode: atomcode_coding::RuntimeMode) {
    if !sharing() {
        return;
    }
    atomcode_daemon::live_set_mode(mode);
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

// ---- 给手机:中继 + 配对链接 ----------------------------------------------

/// 跑着的那个中继客户端子进程。`/app stop` 靠它把进程收掉;进程退出不代表共享结束,
/// 所以它和绑定分开记。
fn relay_child() -> &'static Mutex<Option<tokio::process::Child>> {
    static CHILD: OnceLock<Mutex<Option<tokio::process::Child>>> = OnceLock::new();
    CHILD.get_or_init(|| Mutex::new(None))
}

/// 中继地址推出两样:拨号用的 ws、手机用的 https 根。与经典界面同一套推法。
pub(crate) fn relay_urls(base: &str) -> (String, String) {
    let trimmed = base.trim().trim_end_matches('/');
    let https_base = if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{}", rest.trim_end_matches("/ws/daemon"))
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{}", rest.trim_end_matches("/ws/daemon"))
    } else {
        trimmed.to_string()
    };
    let ws = if let Some(rest) = https_base.strip_prefix("https://") {
        format!("wss://{rest}/ws/daemon")
    } else if let Some(rest) = https_base.strip_prefix("http://") {
        format!("ws://{rest}/ws/daemon")
    } else {
        format!("wss://{https_base}/ws/daemon")
    };
    (ws, https_base)
}

/// URL 里放得下的写法。手机扫到的是这串,所以编码错一个字节就配不上对。
pub(crate) fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 手机扫的那串。
pub(crate) fn pair_uri(https_base: &str, token: &str, machine: Option<&str>) -> String {
    let machine = machine
        .map(|name| format!("&m={}", escaped(name)))
        .unwrap_or_default();
    format!(
        "atomcode-link://pair?r={}&t={token}{machine}",
        escaped(https_base)
    )
}

/// 起 App 那一侧:本机 server + 中继客户端,返回手机要扫的那串。
async fn start_for_phone(config_path: &std::path::Path) -> Result<String, String> {
    if !atomcode_config::endpoints::relay_enabled() {
        return Err(tr(SMsg::AppRelayDisabled).into_owned());
    }
    // 先把旧的收掉:两个中继客户端连同一个 token,手机连上的是哪一个说不准。
    stop_for_phone();
    let signed_in = atomcode_auth::oauth::get_stored_auth();
    let (_host, port) = atomcode_daemon::ensure_app_server(
        "127.0.0.1",
        atomcode_daemon::APP_DEFAULT_PORT,
        signed_in.as_ref().map(|auth| auth.user.id.clone()),
    )
    .await?;
    let token = match &signed_in {
        Some(auth) => format!("{}.{}", auth.user.id, uuid::Uuid::new_v4().simple()),
        None => format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
    };
    let (ws, https_base) = relay_urls(atomcode_config::endpoints::relay_url());
    let machine = std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.is_empty());
    let binary = crate::relay::ensure_relay_client_bin()?;
    let mut command = tokio::process::Command::new(&binary);
    command
        .arg("run")
        .arg("--relay")
        .arg(&ws)
        .arg("--token")
        .arg(&token)
        .arg("--daemon")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg("--supervise-daemon")
        .arg("false")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(name) = &machine {
        command.arg("--machine-name").arg(name);
    }
    if let Some(secret) = std::env::var("ATOMCODE_APP_RELAY_SECRET")
        .ok()
        .or_else(|| std::env::var("ATOM_RELAY_REGISTER_SECRET").ok())
        .filter(|secret| !secret.is_empty())
    {
        command.arg("--register-secret").arg(secret);
    }
    let child = command.spawn().map_err(|error| {
        tr(SMsg::AppRelayNotStarted {
            error: &error.to_string(),
            path: &binary,
        })
        .into_owned()
    })?;
    *relay_child().lock().expect("relay child poisoned") = Some(child);
    // 挂上要排在最后:挂不上就把刚拉起来的收掉,不留一个连着中继、却不共享任何
    // 会话的进程。
    if let Err(why) = attach(config_path).await {
        stop_for_phone();
        return Err(why);
    }
    Ok(pair_uri(&https_base, &token, machine.as_deref()))
}

/// 收掉中继客户端和 App server。返回是否真有东西被收掉。
fn stop_for_phone() -> bool {
    let child = relay_child().lock().expect("relay child poisoned").take();
    let killed = match child {
        Some(mut child) => {
            let _ = child.start_kill();
            true
        }
        None => false,
    };
    atomcode_daemon::stop_app_server() || killed
}

/// 不再把这个会话给任何人看:收掉配对的手机,并从 hub 上摘下来。
///
/// 存在的理由是登出(`HostCommand::SignOut`)。共享是「谁能看到这个会话」,凭据是
/// 「我是谁」——登出只删后者,就会出现「我登出了」和「手机上还看得见这段对话」
/// 同时为真。
///
/// **收的是这个会话,不是 webui server**。摘下绑定之后,浏览器那边就没有这个会话
/// 可看了;而那个 server 可能是人自己起来当服务用的,也可能正服务着别的东西——
/// 登出不该顺手关掉它。要关有 `/webui stop`。手机那半不同:中继子进程与这一次
/// 配对是一一对应的,所以连它一起收。
///
/// **一步都不许失败终止**:登出不能因为收不掉一个中继子进程而办不成。每一步都
/// 尽力做完,返回是否真有东西被收掉。
pub fn stop_all_sharing() -> bool {
    let phone = stop_for_phone();
    let bound = detach().unwrap_or(false);
    phone || bound
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
            Command::said_taking("app", tr(SMsg::AppTakes), tr(SMsg::CmdAboutApp)),
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
            "app" if args == "stop" => {
                let stopped = stop_for_phone();
                let _ = detach();
                Outcome::Said(
                    match stopped {
                        true => tr(SMsg::AppStopped),
                        false => tr(SMsg::AppWasNotOn),
                    }
                    .into_owned(),
                )
            }
            "app" => match start_for_phone(&self.config_path).await {
                Ok(uri) => Outcome::Open(pairing_overlay(uri)),
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

    /// 中继给的是一个地址,拨号要 ws、手机要 https ——从同一个地址推出来,而不是
    /// 让人配两遍。
    #[test]
    fn one_relay_address_gives_both_the_dial_and_the_phones_url() {
        for (given, ws, https) in [
            (
                "wss://relay.example/ws/daemon",
                "wss://relay.example/ws/daemon",
                "https://relay.example",
            ),
            (
                "https://relay.example",
                "wss://relay.example/ws/daemon",
                "https://relay.example",
            ),
            (
                "ws://127.0.0.1:8080/ws/daemon",
                "ws://127.0.0.1:8080/ws/daemon",
                "http://127.0.0.1:8080",
            ),
            (
                "relay.example/",
                "wss://relay.example/ws/daemon",
                "relay.example",
            ),
        ] {
            assert_eq!(
                relay_urls(given),
                (ws.to_string(), https.to_string()),
                "{given}"
            );
        }
    }

    /// **码里是原串,手输的是它的 base64**:App 的扫一扫读链接本身,手输框收的是
    /// 经典界面称作「口令」的那串 base64。印反了,扫得出来的人没事,扫不出来的人
    /// 配不上对——这正是只有手输的人才会撞上的那种错。
    #[test]
    fn the_password_is_the_link_encoded_not_the_link() {
        let uri = pair_uri("https://relay.example", "tok-1", None);
        let password = password_for(&uri);
        assert_ne!(password, uri, "口令不是链接本身");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(password.as_bytes())
            .expect("口令是 base64");
        assert_eq!(
            String::from_utf8(decoded).expect("解出来是文本"),
            uri,
            "解开之后正是那条配对链接"
        );
    }

    /// 手机扫到的那串里,地址和机器名都得是编码过的——里面有 `:` `/` 和中文时,
    /// 不编码就是另一个链接。
    #[test]
    fn what_the_phone_scans_survives_slashes_and_spaces() {
        let uri = pair_uri("https://relay.example:8443", "tok-1", Some("我的 Mac"));
        assert!(uri.starts_with("atomcode-link://pair?r="), "{uri}");
        assert!(uri.contains("https%3A%2F%2Frelay.example%3A8443"), "{uri}");
        assert!(uri.contains("&t=tok-1"), "{uri}");
        assert!(
            uri.contains("&m=%E6%88%91%E7%9A%84%20Mac"),
            "机器名编码过:{uri}"
        );
        assert!(
            !pair_uri("https://r", "t", None).contains("&m="),
            "没有机器名就不写这一段"
        );
    }

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

/// 手输配对用的那串「口令」:配对链接的 base64。与经典界面同一个形状——App 认的
/// 是这个,不是链接本身。
pub(crate) fn password_for(uri: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(uri.as_bytes())
}

/// 扫码配对那一屏:一张码、底下是同一串文字(码扫不出来时还能手打)。
///
/// 用向导而不是回一行字:二维码是一张位图,得按格子画,而且码要用扫码器认得的
/// 颜色——不是主题色(`docs/adr/0027`)。向导已经为登录那一步把这件事做对了。
fn pairing_overlay(uri: String) -> Arc<atomcode_tui::wizard::Wizard> {
    use atomcode_tui::wizard::{StepDef, StepKind, Wizard};
    // **码里是原串,手输的是它的 base64**。两者不是一回事:App 的「扫一扫」读
    // `atomcode-link://pair?…` 本身,而它的手输框收的是经典界面称作「口令」的
    // 那串 base64。印错一个,扫得出来的人没事,扫不出来的人配不上对。
    let password = password_for(&uri);
    let mut step = StepDef::new("pair", tr(SMsg::AppPairTitle), StepKind::Note).saying(vec![
        tr(SMsg::AppPairScan).into_owned(),
        String::new(),
        tr(SMsg::AppPairType).into_owned(),
        password,
    ]);
    if let Some(code) = atomcode_tui::qr::code(&uri) {
        step = step.showing(code);
    }
    Wizard::new(
        "app-pair",
        tr(SMsg::AppPairTitle),
        vec![step],
        Box::new(|_| {}),
        "app-paired",
    )
}
