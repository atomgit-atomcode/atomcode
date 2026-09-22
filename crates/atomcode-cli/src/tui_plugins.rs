//! 插件端口:`atomcode --tui` 这一侧的另一半。
//!
//! `atomcode_tui::plugins` 画列表、走光标、收按键;这里知道一个市场是一次
//! `git clone`、插件落在磁盘的哪个目录、`installed_plugins.json` 里记着什么
//! （`docs/adr/0022` §3）。屏幕那边一个文件名都叫不出来,这是故意的。
//!
//! **为什么住在 cli 而不在 coding**:`docs/plans/2026-09-19-remaining-gaps.md`
//! 决策 9。给 coding 开 `atomcode-capabilities` 的 `plugin` feature,等于把
//! 市场和 git 那一套拉进 agent 进程,只为了重复这个二进制已经会做的动作。cli
//! 本来就开着这个 feature（`atomcode plugin` 子命令用的是同一套函数）。
//!
//! **慢活都在 `spawn_blocking` 里**:每一次 install / add / update 都要跑 `git`,
//! 1 到 10 秒。async 的门面下面是阻塞的实现,而屏幕那边在这段时间里画着「正在
//! 装…」并且只收 Esc。
//!
//! **取消要收拾干净。** 人按 Esc 的时候克隆往往已经在路上了:它会照样落地,把
//! 文件写进磁盘、把条目写进 `installed_plugins.json`。所以取消不是「当它没发生」,
//! 是记下这件活,等它落地之后把它卸掉——否则磁盘上会留下一个人已经放弃、却装好了
//! 的插件。老前端在模态里做的也是这件事（`cancelled_installs`）。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::plugin::installer::{self, InstalledPluginInfo};
use atomcode_capabilities::plugin::marketplace::{self, MarketplaceInfo};
use atomcode_capabilities::plugin::InstallScope;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::ModulesSvc;
use atomcode_tui::plugins::{MarketRow, PluginRow, Plugins, PluginsView, Scope};
use serde_json::Value;

/// 行的名字,插件和点它的那一层共用一个串。
pub const ROW: &str = "tui-panel-plugins";

/// 把插件面板挂到屏幕上的那一行,以及开机那一下。
///
/// **启动器的行,不是屏幕的**,和 `tui-panel-providers` 一样:`atomcode-tui` 带的是
/// 面板的*画法*,它不知道插件市场是什么。这一行存在,是因为*这个*产品有插件可管。
///
/// 开机那一下也归它:一台新机器上第一次跑起来,自带的市场要先取下来,否则面板是空的,
/// 而人会以为这块功能没接。跑不跑、多久跑一次由配置说了算
/// （`plugin.auto_install_default_skills` / `plugin.auto_update_marketplaces`,
/// 后者自带节流）,而且**全程在后台**:一次克隆 5 到 10 秒,挡在输入框前面的 10 秒
/// 是一个开不了机的产品。失败不影响任何事,说一句就过去。
pub struct PluginsRow {
    /// 开机那一下读的配置。持路径而不持已加载的 `Config`,理由同设置端口。
    pub config_path: PathBuf,
}

#[async_trait]
impl Plugin for PluginsRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the plugins panel: marketplaces and what they carry, as this launcher reads them"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::plugins::Plugins>::new());
        let id = <atomcode_tui::modules::plugins::Plugins as atomcode_tui::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        bootstrap(ctx.clone(), self.config_path.clone());
        Ok(())
    }
}

/// 开机那一下:自带的市场取下来,在册的市场按节流拉一次。
///
/// 完全在后台,而且**只说落地了的事**:拉不动一个市场不是错误,是「今天没拉到」,
/// 而把它写成一行红字会让每一次没网的启动看起来像出了故障。老前端在这儿的分寸
/// 是一样的（`handle_plugin_job_event` 里那句「calm one-line warning」）。
fn bootstrap(ctx: Context, config_path: PathBuf) {
    tokio::spawn(async move {
        let config = match atomcode_config::config::Config::load(&config_path) {
            Ok(config) => config,
            // 配置读不出来的时候不替人做主:开机自动装东西是配置说了算的事,而
            // 「读不出来」不等于「默认打开」。
            Err(_) => return,
        };
        if !config.plugin.auto_install_default_skills && !config.plugin.auto_update_marketplaces {
            return;
        }
        let Ok(events) = tokio::task::spawn_blocking(move || {
            atomcode_capabilities::plugin::bootstrap::run_startup_hooks(&config)
        })
        .await
        else {
            return;
        };
        let said = startup_lines(&events);
        if said.is_empty() {
            return;
        }
        if let Some(ui) = ctx.service::<atomcode_harness::seams::UiSvc>() {
            for line in &said {
                ui.say(line);
            }
        }
        // 装下来的东西要进得了这一局,否则人得重启一次才用得上。
        if let Some(client) = ctx.service::<atomcode_tui::plugin::AgentClientSvc>() {
            if let Some(control) = client.control() {
                let _ = control
                    .call(atomcode_host_api::HostCommand::Reload {
                        session: client.root(),
                    })
                    .await;
            }
        }
    });
}

/// 开机那一下有什么值得说的。
///
/// 只说**变化**:装上了什么、加上了哪个市场。更新到同一个 commit、没网、没装 git,
/// 都不说——每次启动都念一遍「今天也没拉到」的产品,人会学会不看它说的任何话。
fn startup_lines(events: &[atomcode_capabilities::plugin::PluginJobEvent]) -> Vec<String> {
    use atomcode_capabilities::plugin::PluginJobEvent as Ev;
    let mut out = Vec::new();
    for event in events {
        match event {
            Ev::MarketplaceAdded(info) => out.push(
                tr(SMsg::SeedMarketFetched {
                    name: &info.name,
                    plugins: info.plugins.len(),
                })
                .into_owned(),
            ),
            Ev::PluginInstalled(info) => out.push(
                tr(SMsg::SeedPluginInstalled {
                    plugin: &info.plugin,
                    marketplace: &info.marketplace,
                })
                .into_owned(),
            ),
            Ev::MarketplaceUpdated(_)
            | Ev::PluginUpdated(_)
            | Ev::PluginAlreadyInstalled { .. }
            | Ev::Failed { .. }
            | Ev::GitNotFound => {}
        }
    }
    out
}

/// 把这一行放上屏幕的那一层。
///
/// `[[insert]]` 而不是补丁:屏幕自己的树根本没提这一行,因为一个没有插件端口的屏幕
/// 没有插件可管。所以由启动器插入它自己那一行,两者一起走。
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 磁盘上的插件,以及改它们的那几个动作。
pub struct DiskPlugins {
    /// 项目范围的插件按它解析。和设置端口持路径而不持已加载的 `Config` 同一条
    /// 理由:面板要显示的是*此刻*磁盘上是什么。
    working_dir: PathBuf,
    /// 人已经不等了的那几件活。落地时照着它回滚。
    given_up: Mutex<HashSet<String>>,
}

impl DiskPlugins {
    pub fn new(working_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            working_dir,
            given_up: Mutex::new(HashSet::new()),
        })
    }

    /// 这件活还要不要?要的话顺手把它从名单里划掉。
    fn wanted(&self, job: &str) -> bool {
        !self.given_up.lock().expect("given up poisoned").remove(job)
    }
}

fn scope_in(scope: Scope) -> InstallScope {
    match scope {
        Scope::User => InstallScope::User,
        Scope::Project => InstallScope::Project,
        Scope::Local => InstallScope::Local,
    }
}

fn scope_out(scope: &InstallScope) -> Scope {
    match scope {
        InstallScope::User => Scope::User,
        InstallScope::Project => Scope::Project,
        InstallScope::Local => Scope::Local,
    }
}

/// 这个构建自带的市场。
///
/// 删不掉:下一次启动 bootstrap 会把它拉回来,而中间那段时间人会以为自己删掉了
/// 什么东西。判断按来源而不按名字——名字是从地址推出来的,同一个仓库换个写法就
/// 是另一个名字了。
fn official(source: &str) -> bool {
    atomcode_capabilities::plugin::bootstrap::default_skills_urls()
        .iter()
        .any(|url| same_repo(url, source))
}

/// 两个地址指的是不是同一个仓库。
///
/// `https://…/x.git`、`git@…:x.git`、结尾多不多一个 `.git` —— 同一个仓库有好几种
/// 写法,而「这是不是自带的市场」这个问题不该因为写法不同就答错。
fn same_repo(a: &str, b: &str) -> bool {
    fn tail(url: &str) -> String {
        url.trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .take(2)
            .collect::<Vec<_>>()
            .join("/")
            .to_lowercase()
    }
    a == b || tail(a) == tail(b)
}

/// 市场目录上一次动过是多久以前——已经排好版的一句话。
///
/// 说「几天前」而不是一个日期:人来这一页是想知道「这份清单新不新」,而那是个相对
/// 的问题。也不用日期库:这个 crate 里没有,而为了一行字拉一个时区实现进来不值。
fn updated(name: &str) -> String {
    let Some(root) = atomcode_capabilities::plugin::marketplaces_root() else {
        return String::new();
    };
    let dir = root.join(name);
    // 清单文件本身最能说明「这份清单是什么时候的」;没有就退回 `.git`,再退回目录。
    let target = [
        dir.join(".atomcode-plugin/marketplace.json"),
        dir.join(".git"),
        dir.clone(),
    ]
    .into_iter()
    .find(|p| p.exists());
    let Some(modified) = target
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
    else {
        return String::new();
    };
    let Ok(ago) = std::time::SystemTime::now().duration_since(modified) else {
        // 时钟往回跳过,或者文件的时间在未来。不猜。
        return String::new();
    };
    match ago.as_secs() / 86_400 {
        0 => tr(SMsg::UpdatedToday).into_owned(),
        1 => tr(SMsg::UpdatedYesterday).into_owned(),
        days => tr(SMsg::UpdatedDaysAgo { days }).into_owned(),
    }
}

/// 一个市场里每个插件的说明,从它自己的清单里读。
fn descriptions(market: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(root) = atomcode_capabilities::plugin::marketplaces_root() else {
        return out;
    };
    if let Ok(Some(manifest)) =
        atomcode_capabilities::plugin::load_marketplace_manifest(&root.join(market))
    {
        for entry in manifest.plugins {
            if let Some(text) = entry.description {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    out.insert(entry.name, text);
                }
            }
        }
    }
    out
}

/// 一个已经装上的插件的说明,从它装下来的那份清单里读。
///
/// 和市场清单里那一份分开,因为两处都可能有、也都可能没有:装下来的那份是插件
/// 作者写的,市场那份是市场维护者写的,后者常常更短。先用前者。
fn installed_description(
    info: &InstalledPluginInfo,
    working_dir: &std::path::Path,
) -> Option<String> {
    let root = match info.scope {
        InstallScope::User => atomcode_capabilities::plugin::plugins_root(),
        InstallScope::Project | InstallScope::Local => {
            atomcode_capabilities::plugin::project_plugins_root(working_dir, &info.scope)
        }
    }?;
    let manifest =
        atomcode_capabilities::plugin::load_plugin_manifest(&root.join(&info.plugin_dir)).ok()?;
    manifest
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
}

/// 装上的那份条目,按「插件名 + 市场」认。
///
/// 名字要过一遍 `sanitize_name`:磁盘上记的是规范化之后的名字,而市场清单里写的是
/// 原样的,两者不一定一样——老前端在这儿栽过,已装的插件在列表里不显示已装。
fn installed_of<'a>(
    installed: &'a [InstalledPluginInfo],
    name: &str,
    market: &str,
) -> Option<&'a InstalledPluginInfo> {
    let key = marketplace::sanitize_name(name);
    installed
        .iter()
        .find(|i| i.marketplace == market && (i.plugin == name || i.plugin == key))
}

#[async_trait]
impl Plugins for DiskPlugins {
    fn rows(&self) -> PluginsView {
        let markets = marketplace::list_marketplaces().unwrap_or_default();
        let installed = installer::list_installed().unwrap_or_default();
        let described: std::collections::HashMap<
            String,
            std::collections::HashMap<String, String>,
        > = markets
            .iter()
            .map(|m| (m.name.clone(), descriptions(&m.name)))
            .collect();
        let about = |market: &str, plugin: &str, info: Option<&InstalledPluginInfo>| {
            info.and_then(|i| installed_description(i, &self.working_dir))
                .or_else(|| described.get(market).and_then(|d| d.get(plugin)).cloned())
                .unwrap_or_default()
        };
        let plugins = merge(&markets, &installed, &about);
        let markets = markets
            .into_iter()
            .map(|m| MarketRow {
                installed: installed.iter().filter(|i| i.marketplace == m.name).count(),
                plugins: m.plugins.len(),
                updated: updated(&m.name),
                official: official(&m.source),
                name: m.name,
                source: m.source,
            })
            .collect();
        PluginsView::new(plugins, markets)
    }

    async fn install(&self, plugin: &str, market: &str, scope: Scope) -> Result<String, String> {
        let job = format!("{plugin}@{market}");
        let (plugin, market) = (plugin.to_string(), market.to_string());
        let scope = scope_in(scope);
        let done = spawn(move || installer::install(&plugin, &market, scope)).await?;
        self.settle(job, done, &tr(SMsg::PluginInstalledVerb))
    }

    async fn update(&self, plugin: &str, market: &str, scope: Scope) -> Result<String, String> {
        let job = format!("{plugin}@{market}");
        let (plugin, market) = (plugin.to_string(), market.to_string());
        let scope = scope_in(scope);
        let done = spawn(move || {
            // 先卸后装,两步当一件事:中途断了该说的是「更新没成」,而不是「卸好了」
            // 再加一句「装不上」。卸不掉不当失败——它可能本来就没装干净,而这一步
            // 的目的是让下面那一步能装。
            let _ = installer::uninstall(&plugin, &market, scope.clone());
            installer::install(&plugin, &market, scope)
        })
        .await?;
        self.settle(job, done, &tr(SMsg::PluginUpdatedVerb))
    }

    async fn uninstall(&self, plugin: &str, market: &str, scope: Scope) -> Result<String, String> {
        let id = format!("{plugin}@{market}");
        let (p, m) = (plugin.to_string(), market.to_string());
        let scope = scope_in(scope);
        spawn(move || installer::uninstall(&p, &m, scope))
            .await?
            .map_err(|e| {
                tr(SMsg::UninstallFailed {
                    error: &format!("{e:#}"),
                })
                .into_owned()
            })?;
        Ok(tr(SMsg::Uninstalled { id: &id }).into_owned())
    }

    async fn add_market(&self, url: &str) -> Result<String, String> {
        let job = format!("market:{url}");
        let url = url.to_string();
        let added = spawn(move || marketplace::add_marketplace(&url))
            .await?
            .map_err(|e| {
                tr(SMsg::MarketAddFailed {
                    error: &format!("{e:#}"),
                })
                .into_owned()
            })?;
        if !self.wanted(&job) {
            let name = added.name.clone();
            let _ = spawn(move || marketplace::remove_marketplace(&name)).await;
            return Ok(tr(SMsg::CancelledNothingLeft { what: &added.name }).into_owned());
        }
        // **加市场不等于装插件**,这是这句话存在的理由:老前端反复见到人加完市场就去
        // 用插件带的命令,然后以为坏了。所以把它带了什么、下一步怎么装,一起说出来。
        let joiner = tr(SMsg::ListJoiner);
        let names = match added.plugins.len() {
            0 => tr(SMsg::MarketCarriesNothing).into_owned(),
            n if n > 5 => tr(SMsg::MarketCarriesSome {
                n,
                names: &added.plugins[..5].join(&joiner),
            })
            .into_owned(),
            n => tr(SMsg::MarketCarriesAll {
                n,
                names: &added.plugins.join(&joiner),
            })
            .into_owned(),
        };
        Ok(tr(SMsg::MarketAdded {
            name: &added.name,
            source: short(&added.git_commit),
            carries: &names,
        })
        .into_owned())
    }

    async fn update_market(&self, name: &str) -> Result<String, String> {
        let job = format!("market:{name}");
        let name = name.to_string();
        let info: MarketplaceInfo = spawn(move || marketplace::update_marketplace(&name))
            .await?
            .map_err(|e| {
                tr(SMsg::MarketUpdateFailed {
                    error: &format!("{e:#}"),
                })
                .into_owned()
            })?;
        let _ = self.wanted(&job);
        Ok(tr(SMsg::MarketUpdated {
            name: &info.name,
            commit: short(&info.git_commit),
            plugins: info.plugins.len(),
        })
        .into_owned())
    }

    async fn remove_market(&self, name: &str) -> Result<String, String> {
        let name = name.to_string();
        let working_dir = self.working_dir.clone();
        let _ = working_dir;
        spawn(move || {
            // 先把从它装的插件卸掉,再删市场。反过来的话,市场没了,那些插件就再也
            // 没有出处,卸载时连去哪儿找它们都说不清。一个卸不掉不挡着别的——这是
            // 清场,不是事务。
            let mut failed: Vec<String> = Vec::new();
            for info in installer::list_installed().unwrap_or_default() {
                if info.marketplace != name {
                    continue;
                }
                if installer::uninstall(&info.plugin, &info.marketplace, info.scope).is_err() {
                    failed.push(info.plugin);
                }
            }
            marketplace::remove_marketplace(&name)
                .map(|()| (name, failed))
                .map_err(|e| {
                    tr(SMsg::MarketRemoveFailed {
                        error: &format!("{e:#}"),
                    })
                    .into_owned()
                })
        })
        .await?
        .map(|(name, failed)| match failed.is_empty() {
            true => tr(SMsg::MarketRemoved { name: &name }).into_owned(),
            // 说出来而不是吞掉:剩在磁盘上的那几个,人下次在「已装」那一页还会看见。
            false => tr(SMsg::MarketRemovedWithLeftovers {
                name: &name,
                failed: &failed.join(&tr(SMsg::ListJoiner)),
            })
            .into_owned(),
        })
    }

    fn cancel(&self, job: &str) {
        self.given_up
            .lock()
            .expect("given up poisoned")
            .insert(job.to_string());
    }
}

impl DiskPlugins {
    /// 一件装/更新的活落地了:人还要不要它?
    ///
    /// 不要了就回滚。这是取消唯一真正做得到的事——克隆已经跑完了,文件已经在磁盘上,
    /// 能做的只有把它拿掉。
    fn settle(
        &self,
        job: String,
        done: Result<InstalledPluginInfo, anyhow::Error>,
        verb: &str,
    ) -> Result<String, String> {
        let info = match done {
            Ok(info) => info,
            Err(e) => {
                let _ = self.wanted(&job);
                if let Some(already) = e.downcast_ref::<installer::AlreadyInstalledError>() {
                    return Err(tr(SMsg::PluginAlreadyInstalled { id: &already.id }).into_owned());
                }
                return Err(tr(SMsg::PluginInstallFailed {
                    error: &format!("{e:#}"),
                })
                .into_owned());
            }
        };
        let id = format!("{}@{}", info.plugin, info.marketplace);
        if !self.wanted(&job) {
            let (p, m, s) = (info.plugin, info.marketplace, info.scope);
            let _ = installer::uninstall(&p, &m, s);
            return Ok(tr(SMsg::CancelledNothingLeft { what: &id }).into_owned());
        }
        Ok(format!(
            "{verb} {id}{}",
            brought(&info.plugin, &info.marketplace, &self.working_dir)
        ))
    }
}

/// 一个装好的插件带来了什么。
///
/// 这句话回答的是人装完之后唯一真正关心的问题:**它生效了吗**。老前端在这儿报的是
/// 「加载了 N 个 skill,跳过 M 个」,那个数字是**整个注册表**重新加载之后的总数,所以
/// 装一个不带技能的插件也会显示一个很大的数字,看上去像是成功了。这里数的是这一个
/// 插件自己带来的东西。
///
/// 数不出来就什么都不说:一个空的「带来 0 个技能」会让人以为装错了,而有些插件本来
/// 就只带钩子。
fn brought(plugin: &str, market: &str, working_dir: &std::path::Path) -> String {
    let key = marketplace::sanitize_name(plugin);
    let Some(assets) =
        atomcode_capabilities::plugin::loader::iter_installed_plugin_assets_for(working_dir)
            .into_iter()
            .find(|a| a.marketplace == market && (a.plugin == plugin || a.plugin == key))
    else {
        return String::new();
    };
    let skills: usize = assets
        .skills_dirs()
        .iter()
        .map(|dir| count_in(dir, is_skill))
        .sum();
    let commands = count_in(&assets.commands_dir(), is_command);
    let hooks = !atomcode_capabilities::plugin::loader::plugin_file_cc_hooks(&assets.plugin_dir)
        .is_empty()
        || assets.hooks_file().exists();
    tally(skills, commands, hooks)
}

fn is_skill(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("SKILL.md"))
}

fn is_command(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

/// 一个目录树里有多少个这样的文件。
fn count_in(dir: &std::path::Path, matches: fn(&std::path::Path) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += count_in(&path, matches);
        } else if matches(&path) {
            n += 1;
        }
    }
    n
}

/// 数出来的东西,说成一句话。
///
/// 一样都没有就一个字都不说——有些插件只带钩子,而一句「带来 0 个技能」会让人以为
/// 装错了。
fn tally(skills: usize, commands: usize, hooks: bool) -> String {
    let mut parts = Vec::new();
    if skills > 0 {
        parts.push(tr(SMsg::TallySkills { n: skills }).into_owned());
    }
    if commands > 0 {
        parts.push(tr(SMsg::TallyCommands { n: commands }).into_owned());
    }
    if hooks {
        parts.push(tr(SMsg::TallyHooks).into_owned());
    }
    match parts.is_empty() {
        true => String::new(),
        false => tr(SMsg::TallyBrought {
            what: &parts.join(&tr(SMsg::ListJoiner)),
        })
        .into_owned(),
    }
}

/// 短 commit,给人看的那七位。
fn short(commit: &str) -> &str {
    &commit[..7.min(commit.len())]
}

/// 把一件阻塞的活丢到别的线程上去。
///
/// 每一件都要跑 `git`,而这个 future 是在画屏幕的那个运行时上被 await 的。
async fn spawn<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        tr(SMsg::JobDidNotStart {
            error: &e.to_string(),
        })
        .into_owned()
    })
}

/// 市场带的 + 已经装上的 → 面板要列的那些行。
///
/// 拆成一个纯函数,因为**合并这件事本身出过错**:第一版检查的是「这个市场里已经有
/// 任何一个装上的插件了吗」,于是一个已经从市场清单里下架、却还装在机器上的插件,
/// 只要它同市场的别的插件还在册,就整个消失——而「已装」那一页正是人要去把它卸掉
/// 的地方。本机实测漏掉了一个（`clawsweeper`,它所在的市场有 466 个插件、其中 5 个
/// 装着）。查重要按**这一个插件**,不是按它的市场。
///
/// 描述从外面给进来,所以这里不读盘:什么该显示是这件事的一部分,从哪儿读不是。
fn merge(
    markets: &[MarketplaceInfo],
    installed: &[InstalledPluginInfo],
    about: &dyn Fn(&str, &str, Option<&InstalledPluginInfo>) -> String,
) -> Vec<PluginRow> {
    let mut rows: Vec<PluginRow> = Vec::new();
    for market in markets {
        for name in &market.plugins {
            let here = installed_of(installed, name, &market.name);
            rows.push(PluginRow {
                name: name.clone(),
                marketplace: market.name.clone(),
                description: about(&market.name, name, here),
                installed: here.map(|i| scope_out(&i.scope)),
            });
        }
    }
    // 装着、但市场清单里已经没有的插件也要列出来:它可能被上游下架了,也可能它的
    // 整个市场都被删了。不列它,「已装」那一页就少一行,而人正是要到那一页去卸它。
    for info in installed {
        let key = marketplace::sanitize_name(&info.plugin);
        let listed = rows.iter().any(|row| {
            row.marketplace == info.marketplace
                && (row.name == info.plugin || marketplace::sanitize_name(&row.name) == key)
        });
        if listed {
            continue;
        }
        rows.push(PluginRow {
            name: info.plugin.clone(),
            marketplace: info.marketplace.clone(),
            description: about(&info.marketplace, &info.plugin, Some(info)),
            installed: Some(scope_out(&info.scope)),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::plugin::PluginJobEvent;

    /// The marketplace this build ships is recognised however its address is
    /// written.
    ///
    /// One repository has several spellings — `https://` and `git@`, with and
    /// without `.git` — and the question this answers is "may a person delete
    /// this". Getting it wrong the permissive way offers a delete that the next
    /// launch undoes; getting it wrong the strict way refuses to delete a
    /// marketplace somebody really did add themselves.
    #[test]
    fn the_shipped_marketplace_is_recognised_however_it_is_written() {
        let shipped = atomcode_capabilities::plugin::bootstrap::default_skills_urls()
            .first()
            .cloned()
            .expect("this build ships a marketplace");
        assert!(official(&shipped), "verbatim");

        let tail = shipped
            .trim_end_matches(".git")
            .rsplit('/')
            .take(2)
            .collect::<Vec<_>>();
        let (repo, owner) = (tail[0], tail[1]);
        assert!(
            official(&format!("git@atomgit.com:{owner}/{repo}.git")),
            "over ssh"
        );
        assert!(
            official(&format!("https://atomgit.com/{owner}/{repo}")),
            "without .git"
        );

        assert!(
            !official("https://example.com/someone/their-own-marketplace.git"),
            "and somebody else's is theirs to delete"
        );
    }

    /// Giving up applies to the one job it named, once.
    ///
    /// Once, because the set is what a landing job reads: a flag left behind
    /// would roll back the *next* install of the same plugin — which is a person
    /// installing something and watching it vanish.
    #[test]
    fn giving_up_applies_to_one_job_and_only_once() {
        let port = DiskPlugins::new(PathBuf::from("."));
        assert!(port.wanted("tidy@official"), "nobody gave up on it");
        port.cancel("tidy@official");
        assert!(!port.wanted("tidy@official"), "this one is not wanted");
        assert!(
            port.wanted("tidy@official"),
            "and the next one is — the flag was for that job, not for that name"
        );
        port.cancel("tidy@official");
        assert!(port.wanted("lens@official"), "and not for anybody else's");
    }

    /// The opening line says what changed and nothing else.
    ///
    /// A product that says "today there was also nothing to fetch" on every
    /// launch teaches people not to read anything it says.
    #[test]
    fn the_opening_only_speaks_when_something_changed() {
        use atomcode_capabilities::plugin::installer::InstalledPluginInfo;
        use atomcode_capabilities::plugin::marketplace::MarketplaceInfo;
        let quiet = [
            PluginJobEvent::GitNotFound,
            PluginJobEvent::Failed {
                op: "update".into(),
                msg: "offline".into(),
            },
            PluginJobEvent::MarketplaceUpdated(MarketplaceInfo {
                name: "official".into(),
                source: "https://example.com/official.git".into(),
                git_commit: "abcdef1234".into(),
                plugins: vec!["tidy".into()],
            }),
            PluginJobEvent::PluginAlreadyInstalled { id: "x@y".into() },
        ];
        assert!(
            startup_lines(&quiet).is_empty(),
            "nothing changed, nothing said"
        );

        let loud = [
            PluginJobEvent::MarketplaceAdded(MarketplaceInfo {
                name: "official".into(),
                source: "https://example.com/official.git".into(),
                git_commit: "abcdef1234".into(),
                plugins: vec!["tidy".into(), "lens".into()],
            }),
            PluginJobEvent::PluginInstalled(InstalledPluginInfo {
                plugin: "tidy".into(),
                marketplace: "official".into(),
                plugin_dir: "marketplaces/official/tidy".into(),
                scope: InstallScope::User,
            }),
        ];
        let said = startup_lines(&loud);
        assert_eq!(said.len(), 2);
        assert!(
            said[0].contains("official") && said[0].contains('2'),
            "{said:?}"
        );
        assert!(said[1].contains("tidy@official"), "{said:?}");
    }

    /// An installed plugin is matched by the name the marketplace lists it
    /// under, even though disk records the sanitized one.
    ///
    /// The bug this pins is the quiet one: a plugin whose name has a character
    /// the filesystem will not take is installed, and the list goes on showing
    /// it as available — so the row offers to install what is already there.
    #[test]
    fn an_installed_plugin_is_found_under_the_name_the_marketplace_lists() {
        let listed = "My Plugin";
        let on_disk = marketplace::sanitize_name(listed);
        assert_ne!(listed, on_disk, "this test is about names that differ");
        let installed = vec![InstalledPluginInfo {
            plugin: on_disk,
            marketplace: "official".into(),
            plugin_dir: "marketplaces/official/my-plugin".into(),
            scope: InstallScope::Local,
        }];
        assert!(
            installed_of(&installed, listed, "official").is_some(),
            "the sanitized name on disk is the same plugin"
        );
        assert!(
            installed_of(&installed, listed, "somewhere-else").is_none(),
            "and the same name from another marketplace is another plugin"
        );
    }

    /// Something installed is listed even when its marketplace no longer
    /// carries it.
    ///
    /// Found on a real machine, not by reading the code: the first version
    /// asked "does this marketplace already have any installed plugin in the
    /// list", so `clawsweeper` — installed, dropped from a 466-plugin
    /// marketplace that still carried five other installed ones — vanished from
    /// every page, including the one a person goes to to uninstall it.
    #[test]
    fn something_installed_is_listed_even_after_the_marketplace_drops_it() {
        let markets = vec![MarketplaceInfo {
            name: "official".into(),
            source: "https://example.com/official.git".into(),
            git_commit: "abc1234".into(),
            plugins: vec!["still-here".into(), "also-here".into()],
        }];
        let installed = vec![
            InstalledPluginInfo {
                plugin: "still-here".into(),
                marketplace: "official".into(),
                plugin_dir: "d".into(),
                scope: InstallScope::User,
            },
            InstalledPluginInfo {
                plugin: "dropped".into(),
                marketplace: "official".into(),
                plugin_dir: "d".into(),
                scope: InstallScope::Project,
            },
            // And one whose whole marketplace is gone.
            InstalledPluginInfo {
                plugin: "orphan".into(),
                marketplace: "deleted-marketplace".into(),
                plugin_dir: "d".into(),
                scope: InstallScope::Local,
            },
        ];
        let rows = merge(&markets, &installed, &|_, _, _| String::new());
        let named: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            named,
            ["still-here", "also-here", "dropped", "orphan"],
            "what the marketplace carries, then what is installed and it does not"
        );
        let installed_rows: Vec<(&str, Scope)> = rows
            .iter()
            .filter_map(|r| r.installed.map(|s| (r.name.as_str(), s)))
            .collect();
        assert_eq!(
            installed_rows,
            [
                ("still-here", Scope::User),
                ("dropped", Scope::Project),
                ("orphan", Scope::Local)
            ],
            "each keeps the scope it was installed into"
        );
        assert!(
            rows.iter().filter(|r| r.name == "still-here").count() == 1,
            "and nothing is listed twice"
        );
    }

    /// A scope crosses the seam and comes back the same.
    #[test]
    fn a_scope_means_the_same_on_both_sides() {
        for scope in Scope::ALL {
            assert_eq!(scope_out(&scope_in(scope)), scope);
        }
    }

    /// What an install says it brought is counted from that plugin's own
    /// directories, nested ones included.
    ///
    /// The classic front end answered this with the size of the *whole* skill
    /// registry after a reload, so installing a plugin that carries nothing
    /// still printed a large number — which reads as success. And a plugin that
    /// carries only hooks says just that rather than "0 skills".
    #[test]
    fn an_install_says_what_that_plugin_brought() {
        let dir = std::env::temp_dir().join(format!("atomcode-brought-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("skills/one")).expect("scratch");
        std::fs::create_dir_all(dir.join("skills/deep/two")).expect("scratch");
        std::fs::write(dir.join("skills/one/SKILL.md"), "x").expect("scratch");
        std::fs::write(dir.join("skills/deep/two/SKILL.md"), "x").expect("scratch");
        std::fs::write(dir.join("skills/one/README.md"), "not a skill").expect("scratch");
        assert_eq!(
            count_in(&dir.join("skills"), is_skill),
            2,
            "nested skills count, and a README beside one is not a skill"
        );

        std::fs::create_dir_all(dir.join("commands")).expect("scratch");
        std::fs::write(dir.join("commands/go.md"), "x").expect("scratch");
        std::fs::write(dir.join("commands/notes.txt"), "x").expect("scratch");
        assert_eq!(count_in(&dir.join("commands"), is_command), 1);

        assert_eq!(count_in(&dir.join("nothing-here"), is_skill), 0);

        assert_eq!(tally(2, 1, true), ",带来 2 个技能、1 条命令、钩子");
        assert_eq!(
            tally(0, 0, true),
            ",带来 钩子",
            "hooks alone are worth saying"
        );
        assert_eq!(
            tally(0, 0, false),
            "",
            "and nothing at all is said with silence, not with a zero"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
