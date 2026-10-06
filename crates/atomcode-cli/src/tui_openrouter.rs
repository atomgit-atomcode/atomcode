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
//! **写进配置**走 `atomcode_auth::openrouter::provision`,两个前端同一份:账号固定
//! 一个 id、只换 key;它加的免费模型带来源标记,再敲一遍就整批换成当前的;人自己配的
//! 模型一概不动。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_auth::openrouter::{provision_with_listed, FreeCatalog, Provisioned};
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

/// 屏幕之外的那几件事,各自一个口子——判据换掉它们就不必联网,也不必开浏览器。
struct World {
    /// 拿到 key:人给的,或走一趟授权(过程中说自己在干什么)。
    key: Arc<dyn Fn(Mode, &dyn Fn(String)) -> Result<String, String> + Send + Sync>,
    /// 这个 key 能用的免费模型,以及 OpenRouter 现在列出的全部模型。
    models: Arc<dyn Fn(&str) -> Result<FreeCatalog, String> + Send + Sync>,
    /// 写进配置文件。
    save: Arc<dyn Fn(&str, &FreeCatalog) -> Result<Provisioned, String> + Send + Sync>,
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
                atomcode_auth::openrouter::fetch_free_catalog(key, FREE_MODEL_LIMIT)
                    .map_err(|error| format!("{error:#}"))
            }),
            save: Arc::new(move |key, catalog| {
                let mut outcome = Provisioned::default();
                atomcode_config::ConfigStore::new(path.clone())
                    .update(|config| {
                        outcome =
                            provision_with_listed(config, key, &catalog.free, &catalog.listed);
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
        // `/openrouter <key>`: the key is echoed as a mask, never as itself.
        vec![Command::said_taking(
            COMMAND,
            tr(SMsg::OpenRouterTakes),
            tr(SMsg::CmdAboutOpenRouter),
        )
        .taking_a_secret()]
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
    if models.free.is_empty() {
        return say(tr(SMsg::OpenRouterNoFreeModels).into_owned());
    }
    let outcome = match (world.save)(&key, &models) {
        Ok(outcome) => outcome,
        Err(error) => return say(tr(SMsg::OpenRouterFailed { error: &error }).into_owned()),
    };
    let default = outcome.default_model.clone().unwrap_or_default();
    // The swapped-out free models only: the ones OpenRouter took down are said
    // on their own line below, because "your own models were not touched" is
    // not true of them.
    say(tr(SMsg::OpenRouterConnected {
        added: outcome.added.len(),
        removed: outcome.removed.len() - outcome.retired.len(),
        default: &default,
    })
    .into_owned());
    // 「你自己配置的模型没有改动」对这几条不成立,所以单独说:哪几条、为什么。
    if !outcome.retired.is_empty() {
        let names = outcome.retired.join("、");
        say(tr(SMsg::OpenRouterRetired { names: &names }).into_owned());
    }
    if let Some(from) = &outcome.default_replaced {
        let said = match outcome.retired.contains(from) {
            true => SMsg::OpenRouterDefaultRetired { from, to: &default },
            false => SMsg::OpenRouterDefaultReplaced { from, to: &default },
        };
        say(tr(said).into_owned());
    }
    if let Err(error) = (world.reload)() {
        say(tr(SMsg::OpenRouterNotReloaded { error: &error }).into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_auth::openrouter::FreeModel;
    use std::sync::Mutex;

    fn model(id: &str) -> FreeModel {
        FreeModel {
            id: id.to_string(),
            name: Some(format!("{id} (free)")),
            context_length: 32_768,
        }
    }

    struct Fake {
        said: Arc<Mutex<Vec<String>>>,
        saved: Arc<Mutex<usize>>,
        reloaded: Arc<Mutex<usize>>,
    }

    impl Fake {
        fn world(&self, models: Result<Vec<FreeModel>, String>) -> World {
            let models = models.map(|free| FreeCatalog {
                free,
                ..FreeCatalog::default()
            });
            let saved = self.saved.clone();
            let reloaded = self.reloaded.clone();
            World {
                key: Arc::new(|mode, _say| match mode {
                    Mode::Given(key) => Ok(key),
                    Mode::Browser => Ok("from-the-browser".into()),
                }),
                models: Arc::new(move |_| models.clone()),
                save: Arc::new(move |_, catalog| {
                    *saved.lock().unwrap() += 1;
                    let models = &catalog.free;
                    Ok(Provisioned {
                        added: models.iter().map(|m| m.id.clone()).collect(),
                        default_model: Some(models[0].id.clone()),
                        ..Provisioned::default()
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

    /// 再敲一遍换掉了旧的免费模型,人要被告知换掉了几个;默认模型因此被换了,也要说
    /// 换成了哪个——不然下一句话落在另一个模型上,人不知道为什么。
    #[test]
    fn a_swap_says_what_went_and_where_the_default_moved() {
        let said = Arc::new(Mutex::new(Vec::new()));
        let world = World {
            key: Arc::new(|_, _| Ok("k".into())),
            models: Arc::new(|_| {
                Ok(FreeCatalog {
                    free: vec![model("b/free")],
                    ..FreeCatalog::default()
                })
            }),
            save: Arc::new(|_, _| {
                Ok(Provisioned {
                    added: vec!["openrouter/b/free".into()],
                    removed: vec![
                        "openrouter/a/free".into(),
                        "openrouter/stealth/space-bunny-alpha".into(),
                    ],
                    retired: vec!["openrouter/stealth/space-bunny-alpha".into()],
                    default_model: Some("openrouter/b/free".into()),
                    default_replaced: Some("openrouter/a/free".into()),
                })
            }),
            reload: Arc::new(|| Ok(())),
        };
        let lines = said.clone();
        connect(&world, Mode::Browser, &move |line| {
            lines.lock().unwrap().push(line)
        });
        let said = said.lock().unwrap().join("\n");
        assert!(
            said.contains(&*tr(SMsg::OpenRouterConnected {
                added: 1,
                removed: 1,
                default: "openrouter/b/free",
            })),
            "{said}"
        );
        // A model OpenRouter took down is named, since "your own models were
        // not touched" is not true of it.
        assert!(
            said.contains(&*tr(SMsg::OpenRouterRetired {
                names: "openrouter/stealth/space-bunny-alpha",
            })),
            "{said}"
        );
        assert!(
            said.contains(&*tr(SMsg::OpenRouterDefaultReplaced {
                from: "openrouter/a/free",
                to: "openrouter/b/free",
            })),
            "{said}"
        );
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
