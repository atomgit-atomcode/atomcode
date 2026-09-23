//! `/openrouter`:一条命令接上 OpenRouter 的免费模型。
//!
//! 经典界面的同名命令在新屏幕上的对应(翻默认后原有功能不能丢)。两种给 key 的方式,
//! 与经典界面一致:`/openrouter` 走浏览器授权(PKCE + 本机回调),`/openrouter <key>`
//! 直接用人给的 key。拿到 key 之后一样:拉前几个免费模型、写进配置、让活着的东西重载。
//!
//! **为什么在 cli**:凭据与配置文件是宿主的事,屏幕只负责显示说了什么
//! (`docs/adr/0022` §3,与 `/login`、`/proxy` 同一条线)。
//!
//! **为什么在后台线程跑**:授权要等人去浏览器点,最长三分钟。命令里直接 await
//! 会让屏幕停在那儿不画也不收键 —— `/login` 踩过,理由记在那儿。
//!
//! **写进配置是幂等的**:账号固定一个 id,已经在就只更新 key;模型已经在就跳过;
//! `default_model` 只在还没有的时候才设。同一条命令敲两遍,配置文件不会长出第二份。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_auth::openrouter::FreeModel;
use atomcode_config::config::provider::{
    default_context_window_for, ModelProfileConfig, ProviderAccountConfig,
};
use atomcode_config::config::provider_preset::preset_or_compatible;
use atomcode_config::config::Config;
use atomcode_harness::seams::{UiSvc, UserInterface};
use atomcode_host_api::HostCommand;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::plugin::{AgentClientSvc, CommandsSvc, Repaint, RepaintSvc};
use serde_json::Value;

/// 行的名字。
pub const ROW: &str = "tui-openrouter";

/// 命令名。
pub const COMMAND: &str = "openrouter";

/// 账号在配置里的固定 id。与经典界面同一个,所以两边接出来的是同一份配置。
const ACCOUNT: &str = "openrouter";

/// 接几个免费模型。与经典界面同一个数。
const FREE_MODEL_LIMIT: usize = 5;

/// 授权最多等多久。人要切到浏览器点一下。
const AUTHORISE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 挂上 `/openrouter`。
pub struct OpenRouterRow {
    pub config_path: PathBuf,
}

#[async_trait]
impl Plugin for OpenRouterRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "connecting OpenRouter's free models in one command"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        let repaint = ctx.service::<RepaintSvc>();
        let ui = ctx.require::<UiSvc>().map_err(|e| e.to_string())?;
        let world = World::production(self.config_path.clone(), ctx.clone());
        commands.add(Arc::new(OpenRouterCommands {
            world: Arc::new(world),
            ui,
            repaint,
        }))?;
        Ok(())
    }
}

/// 人怎么给 key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    /// 浏览器授权。
    Browser,
    /// 人自己贴的 key。
    Given(String),
}

impl Mode {
    pub(crate) fn of(args: &str) -> Self {
        let given = args.trim();
        if given.is_empty() {
            Mode::Browser
        } else {
            Mode::Given(given.to_string())
        }
    }
}

/// 落地的结果:加了哪些模型、默认模型是哪个。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Provisioned {
    pub added: Vec<String>,
    pub default_model: String,
}

/// 屏幕之外的那几件事,各自一个口子——判据换掉它们就不必联网,也不必开浏览器。
struct World {
    /// 拿到 key:人给的,或走一趟授权(过程中说自己在干什么)。
    key: Arc<dyn Fn(Mode, &dyn Fn(String)) -> Result<String, String> + Send + Sync>,
    /// 这个 key 能用的免费模型。
    models: Arc<dyn Fn(&str) -> Result<Vec<FreeModel>, String> + Send + Sync>,
    /// 写进配置文件。
    save: Arc<dyn Fn(&str, &[FreeModel]) -> Result<Provisioned, String> + Send + Sync>,
    /// 告诉活着的东西配置变了。
    reload: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

impl World {
    fn production(config_path: PathBuf, ctx: Context) -> Self {
        let path = config_path.clone();
        // Taken in an async context, used from the blocking thread the command
        // spawns — the same reason `/login` takes one.
        let runtime = tokio::runtime::Handle::current();
        Self {
            key: Arc::new(|mode, say| match mode {
                Mode::Given(key) => Ok(key),
                Mode::Browser => authorise(say),
            }),
            models: Arc::new(|key| {
                atomcode_auth::openrouter::fetch_top_free_models(key, FREE_MODEL_LIMIT)
                    .map_err(|error| format!("{error:#}"))
            }),
            save: Arc::new(move |key, models| {
                let mut outcome = Provisioned::default();
                atomcode_config::ConfigStore::new(path.clone())
                    .update(|config| {
                        outcome = provision(config, key, models);
                        Ok(())
                    })
                    .map_err(|error| error.to_string())?;
                Ok(outcome)
            }),
            reload: Arc::new(move || {
                let client = ctx
                    .service::<AgentClientSvc>()
                    .ok_or_else(|| tr(SMsg::HostUnavailable).into_owned())?;
                let control = client
                    .control()
                    .ok_or_else(|| tr(SMsg::HostHasNoControl).into_owned())?;
                let session = client.root();
                runtime
                    .block_on(control.call(HostCommand::Reload { session }))
                    .map(|_| ())
                    .map_err(crate::tui_tools::said)
            }),
        }
    }
}

/// 一趟浏览器授权:起本机回调、开浏览器、把地址也说出来(浏览器没自动打开时人能
/// 自己复制)、等那一下点击,再把 code 换成 key。
fn authorise(say: &dyn Fn(String)) -> Result<String, String> {
    use atomcode_auth::openrouter as or;
    let pkce = or::generate_pkce();
    let callback = or::start_local_callback().map_err(|error| format!("{error:#}"))?;
    let callback_url = format!("http://127.0.0.1:{}/callback", callback.port());
    let auth_url = or::build_auth_url(Some(&callback_url), &pkce.challenge);
    let _ = atomcode_auth::oauth::open_browser(&auth_url);
    say(tr(SMsg::OpenRouterAuthorise { url: &auth_url }).into_owned());
    let code = callback
        .wait_for_code(AUTHORISE_TIMEOUT, &AtomicBool::new(false))
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(|| tr(SMsg::OpenRouterNoAnswer).into_owned())?;
    or::exchange_code_for_key(&code, &pkce.verifier).map_err(|error| format!("{error:#}"))
}

struct OpenRouterCommands {
    world: Arc<World>,
    ui: Arc<dyn UserInterface>,
    repaint: Option<Arc<dyn Repaint>>,
}

#[async_trait]
impl CommandSet for OpenRouterCommands {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            COMMAND,
            tr(SMsg::OpenRouterTakes),
            tr(SMsg::CmdAboutOpenRouter),
        )]
    }

    async fn run(&self, _name: &str, args: &str, _ctx: &Context) -> Outcome {
        let mode = Mode::of(args);
        let world = self.world.clone();
        let ui = self.ui.clone();
        let repaint = self.repaint.clone();
        // 离开循环跑,理由同 `/login`:授权要等人去浏览器点。
        tokio::task::spawn_blocking(move || {
            let painted = move || {
                if let Some(repaint) = repaint.as_ref() {
                    repaint.now();
                }
            };
            let say = |line: String| {
                ui.say(&line);
                painted();
            };
            connect(&world, mode, &say);
        });
        Outcome::Said(tr(SMsg::OpenRouterConnecting).into_owned())
    }
}

/// 整个过程,说给人听。四步任一步失败都当场说清并停下——半接上的配置比没接更难查。
fn connect(world: &World, mode: Mode, say: &dyn Fn(String)) {
    let key = match (world.key)(mode, say) {
        Ok(key) => key,
        Err(error) => return say(tr(SMsg::OpenRouterFailed { error: &error }).into_owned()),
    };
    let models = match (world.models)(&key) {
        Ok(models) => models,
        Err(error) => return say(tr(SMsg::OpenRouterFailed { error: &error }).into_owned()),
    };
    if models.is_empty() {
        return say(tr(SMsg::OpenRouterNoFreeModels).into_owned());
    }
    let outcome = match (world.save)(&key, &models) {
        Ok(outcome) => outcome,
        Err(error) => return say(tr(SMsg::OpenRouterFailed { error: &error }).into_owned()),
    };
    say(tr(SMsg::OpenRouterConnected {
        added: outcome.added.len(),
        default: &outcome.default_model,
    })
    .into_owned());
    if let Err(error) = (world.reload)() {
        say(tr(SMsg::OpenRouterNotReloaded { error: &error }).into_owned());
    }
}

/// 写进配置。幂等:账号已在就只换 key,模型已在就跳过,`default_model` 只在空着时才设。
///
/// 从经典界面的 `openrouter_connect::provision_openrouter` 原样搬来——它是纯函数,
/// 而这一段决定了两个前端接出来的是不是同一份配置。
pub(crate) fn provision(config: &mut Config, api_key: &str, models: &[FreeModel]) -> Provisioned {
    let preset = preset_or_compatible(ACCOUNT);
    let provider_type = preset.provider_type.wire().to_string();

    config
        .provider_accounts
        .entry(ACCOUNT.to_string())
        .and_modify(|account| account.api_key = Some(api_key.to_string()))
        .or_insert_with(|| ProviderAccountConfig {
            provider: ACCOUNT.to_string(),
            display_name: None,
            api_key: Some(api_key.to_string()),
            base_url: None,
            user_agent: None,
            skip_tls_verify: false,
            enterprise_url: None,
            ephemeral: false,
        });

    let mut added = Vec::new();
    let mut first: Option<String> = None;
    for model in models {
        let selection = format!("{ACCOUNT}/{}", model.id);
        if first.is_none() {
            first = Some(selection.clone());
        }
        if config.selection_exists(&selection) {
            continue;
        }
        config.models.insert(
            selection.clone(),
            ModelProfileConfig {
                account: ACCOUNT.to_string(),
                model: model.id.clone(),
                display_name: model.name.clone(),
                context_window: if model.context_length > 0 {
                    model.context_length as usize
                } else {
                    default_context_window_for(&provider_type)
                },
                system_prompt: None,
                supports_vision: None,
                max_tokens: None,
                capable_model: None,
                note: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                reasoning_effort_levels: None,
                thinking_enabled: None,
                thinking_budget: None,
                retry_max_attempts: None,
            },
        );
        added.push(selection);
    }

    let default_model = first.unwrap_or_default();
    if config.default_model.is_none() && !default_model.is_empty() {
        config.default_model = Some(default_model.clone());
    }
    Provisioned {
        added,
        default_model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn model(id: &str) -> FreeModel {
        FreeModel {
            id: id.to_string(),
            name: Some(format!("{id} (free)")),
            context_length: 32_768,
        }
    }

    /// 同一条命令敲两遍,配置不会长出第二份:账号只换 key,模型不重复,
    /// 已经选定的默认模型不被它抢走。
    #[test]
    fn connecting_twice_updates_the_key_and_adds_nothing_twice() {
        let mut config = Config::default();
        let models = [model("a/free"), model("b/free")];

        let first = provision(&mut config, "key-1", &models);
        assert_eq!(first.added.len(), 2);
        assert_eq!(first.default_model, "openrouter/a/free");
        assert_eq!(config.default_model.as_deref(), Some("openrouter/a/free"));

        let again = provision(&mut config, "key-2", &models);
        assert!(again.added.is_empty(), "第二遍不再加:{:?}", again.added);
        assert_eq!(config.models.len(), 2);
        assert_eq!(
            config.provider_accounts[ACCOUNT].api_key.as_deref(),
            Some("key-2"),
            "但 key 换成了新的"
        );

        config.default_model = Some("someone/else".into());
        provision(&mut config, "key-3", &models);
        assert_eq!(
            config.default_model.as_deref(),
            Some("someone/else"),
            "人自己选过的默认模型,接一次 OpenRouter 不该把它顶掉"
        );
    }

    struct Fake {
        said: Arc<Mutex<Vec<String>>>,
        saved: Arc<Mutex<usize>>,
        reloaded: Arc<Mutex<usize>>,
    }

    impl Fake {
        fn world(&self, models: Result<Vec<FreeModel>, String>) -> World {
            let saved = self.saved.clone();
            let reloaded = self.reloaded.clone();
            World {
                key: Arc::new(|mode, _say| match mode {
                    Mode::Given(key) => Ok(key),
                    Mode::Browser => Ok("from-the-browser".into()),
                }),
                models: Arc::new(move |_| models.clone()),
                save: Arc::new(move |_, models| {
                    *saved.lock().unwrap() += 1;
                    Ok(Provisioned {
                        added: models.iter().map(|m| m.id.clone()).collect(),
                        default_model: models[0].id.clone(),
                    })
                }),
                reload: Arc::new(move || {
                    *reloaded.lock().unwrap() += 1;
                    Ok(())
                }),
            }
        }

        fn new() -> Self {
            Self {
                said: Arc::new(Mutex::new(Vec::new())),
                saved: Arc::new(Mutex::new(0)),
                reloaded: Arc::new(Mutex::new(0)),
            }
        }

        fn say(&self) -> impl Fn(String) + '_ {
            move |line| self.said.lock().unwrap().push(line)
        }
    }

    #[test]
    fn a_key_that_reaches_models_is_saved_and_the_session_reloaded() {
        let fake = Fake::new();
        let world = fake.world(Ok(vec![model("a/free")]));
        connect(&world, Mode::Given("k".into()), &fake.say());
        assert_eq!(*fake.saved.lock().unwrap(), 1);
        assert_eq!(*fake.reloaded.lock().unwrap(), 1);
        let said = fake.said.lock().unwrap().join("\n");
        assert!(said.contains("a/free"), "说出接上了什么:{said}");
    }

    /// 拉不到模型就停在那儿,不写配置——半接上的配置比没接更难查。
    #[test]
    fn a_step_that_fails_stops_there_and_says_why() {
        let fake = Fake::new();
        let world = fake.world(Err("429 从 OpenRouter".into()));
        connect(&world, Mode::Browser, &fake.say());
        assert_eq!(*fake.saved.lock().unwrap(), 0, "没写配置");
        assert_eq!(*fake.reloaded.lock().unwrap(), 0);
        let said = fake.said.lock().unwrap().join("\n");
        assert!(said.contains("429"), "把原因说出来:{said}");
    }

    /// 一个免费模型都没有,也不写配置:写下一个没有模型的账号,下次启动只会更困惑。
    #[test]
    fn no_free_models_writes_nothing() {
        let fake = Fake::new();
        let world = fake.world(Ok(Vec::new()));
        connect(&world, Mode::Browser, &fake.say());
        assert_eq!(*fake.saved.lock().unwrap(), 0);
        let said = fake.said.lock().unwrap().join("\n");
        assert!(!said.is_empty(), "也要说一句");
    }
}
