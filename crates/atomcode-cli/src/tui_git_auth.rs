//! Human-only SSH credential management. Secret input never enters commands or facts.
use async_trait::async_trait;
use atomcode_harness::seams::UiSvc;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::{
    command::{Command, CommandSet, Outcome},
    keymap::Action,
    plugin::{CommandsSvc, RepaintSvc, SecretInputSvc},
    sheet::{List, Piece, Row, Sheet, Tone},
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

pub const ROW: &str = "tui-git-auth";
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}
pub struct GitAuthRow {
    pub user_dir: PathBuf,
}
#[derive(Default)]
struct State {
    next: u64,
    operation: Option<Operation>,
}
struct Operation {
    id: u64,
    stop: Arc<AtomicBool>,
    waiting: Arc<AtomicBool>,
    abort: tokio::task::AbortHandle,
}
type Shared = Arc<Mutex<State>>;
fn cancel(state: &Shared, disposing: bool) -> bool {
    let state = state.lock().expect("git-auth state poisoned");
    if let Some(operation) = &state.operation {
        operation.stop.store(true, Ordering::Release);
        if disposing || operation.waiting.load(Ordering::Acquire) {
            operation.abort.abort();
        }
        true
    } else {
        false
    }
}
struct Flight {
    state: Shared,
    id: u64,
}
impl Drop for Flight {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("git-auth state poisoned");
        if state.operation.as_ref().is_some_and(|op| op.id == self.id) {
            state.operation = None;
        }
    }
}
#[async_trait]
impl Plugin for GitAuthRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands", "tui-secret-input"]
    }
    fn description(&self) -> &'static str {
        "human-only SSH authentication setup through masked input"
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        let state = Arc::new(Mutex::new(State::default()));
        let ending = state.clone();
        let _ = ctx.effect(move || {
            cancel(&ending, true);
        });
        ctx.require::<CommandsSvc>()
            .map_err(|e| e.to_string())?
            .add(Arc::new(GitAuthCommands {
                root: self.user_dir.clone(),
                state,
            }))?;
        Ok(())
    }
}
struct GitAuthCommands {
    root: PathBuf,
    state: Shared,
}
fn command_path(action: &str, path: &Path) -> String {
    format!(
        "/git-auth {action} {}",
        serde_json::to_string(&path.to_string_lossy()).expect("string serializes")
    )
}
fn path_arg(args: &str) -> Result<PathBuf, &'static str> {
    let path = if args.starts_with('"') {
        serde_json::from_str::<String>(args)
            .map_err(|_| "密钥路径格式不正确；口令只能在专用隐藏输入框中填写。")?
    } else {
        args.to_owned()
    };
    if path.is_empty() || path.len() > 8192 || path.contains(['\r', '\n', '\0']) {
        return Err("请输入密钥文件路径，勿在命令参数中输入口令。");
    }
    let path = if let Some(tail) = path.strip_prefix("~/") {
        dirs::home_dir().ok_or("无法确定用户主目录")?.join(tail)
    } else {
        PathBuf::from(path)
    };
    Ok(path)
}
fn open(title: &str, rows: Vec<Row>, summary: &str) -> Outcome {
    Outcome::Do(Action::OpenSheet(Sheet::list(
        List::new("git-auth", title, rows)
            .summary(vec![Piece::new(summary, Tone::Muted)])
            .empty(
                "未发现密钥。可输入 /git-auth remember 后跟密钥文件路径；口令将另开隐藏输入框。",
            ),
    )))
}
fn menu(root: &Path, busy: bool) -> anyhow::Result<Outcome> {
    let keys = atomcode_auth::git_ssh::registered_keys(root)?;
    let mut rows = vec![
        Row::new("/git-auth keys", "选择 SSH 密钥并记住口令")
            .about("隐藏输入；默认遵循系统 agent 生命周期"),
        Row::new("/git-auth unlock", "加载已登记的密钥")
            .about("需要已解锁的系统凭据库和可用 agent"),
        Row::new("/git-auth status", "检查本地认证状态").about("不联网，不验证 push 权限"),
    ];
    for key in keys {
        rows.push(
            Row::new(
                command_path("key", &key.path),
                key.path.display().to_string(),
            )
            .about(if key.active {
                match key.lifetime_secs {
                    Some(seconds) => format!("自动加载已启用 · 请求有效期 {} 分钟", seconds / 60),
                    None => "自动加载已启用 · 遵循系统 agent 生命周期".into(),
                }
            } else {
                "保存/清理未完成 · 自动加载已禁用".into()
            }),
        );
    }
    if busy {
        rows.push(Row::new("/git-auth cancel", "请求取消当前操作"));
    }
    Ok(open(
        "Git SSH 认证",
        rows,
        if busy {
            "操作进行中。取消或关闭后，已加载的 agent 密钥仍可能有效。"
        } else {
            "口令仅存系统凭据库。登记状态不代表密钥仍在 agent 中或远端允许 push。"
        },
    ))
}
fn candidates(directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.take(256).flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "pub") {
                let private = path.with_extension("");
                if private.is_file() && private.to_str().is_some() {
                    paths.push(private);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}
#[async_trait]
impl CommandSet for GitAuthCommands {
    fn id(&self) -> &'static str {
        ROW
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            "git-auth",
            "[keys|remember 路径|unlock|status|forget 路径|cancel]".into(),
            "管理 SSH 认证，口令使用专用隐藏输入".into(),
        )
        .taking_a_secret()]
    }
    async fn run(&self, _: &str, args: &str, ctx: &Context) -> Outcome {
        let (verb, rest) = args
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((args.trim(), ""));
        let rest = rest.trim();
        if verb == "cancel" {
            return Outcome::Said(if cancel(&self.state, false) { "已请求取消。系统调用可能仍在结束，操作也可能已提交；请稍后查看状态。已加载密钥不会自动撤销。" } else { "当前没有认证操作。" }.into());
        }
        if verb.is_empty() || verb == "help" {
            return menu(&self.root, self.state.lock().unwrap().operation.is_some())
                .unwrap_or_else(|e| Outcome::Refused(format!("认证登记读取失败：{e}")));
        }
        if verb == "keys" {
            let paths = dirs::home_dir()
                .map(|home| candidates(&home.join(".ssh")))
                .unwrap_or_default();
            return open("选择 SSH 私钥", paths.iter().map(|path| Row::new(command_path("setup", path), path.display().to_string())).collect(), "仅列出有同名 .pub 的私钥路径，不读取私钥内容。选择后在隐藏输入框填写口令；Esc 取消。");
        }
        let path = if matches!(
            verb,
            "setup"
                | "remember"
                | "remember-hour"
                | "remember-day"
                | "key"
                | "forget"
                | "confirm-forget"
        ) {
            match path_arg(rest) {
                Ok(path) => Some(path),
                Err(e) => return Outcome::Refused(e.into()),
            }
        } else if rest.is_empty() && matches!(verb, "unlock" | "status") {
            None
        } else {
            return Outcome::Refused("使用 /git-auth 打开认证面板；不要把口令写在命令中。".into());
        };
        if verb == "setup" {
            let path = path.unwrap();
            return open("选择密钥加载有效期", vec![
                Row::new(command_path("remember", &path), "遵循系统 agent 生命周期（默认）"),
                Row::new(command_path("remember-hour", &path), "1 小时"),
                Row::new(command_path("remember-day", &path), "24 小时"),
            ], "有效期只限制 agent 中加载的密钥；保存的口令保留到主动忘记。默认不附加 AtomCode 时间限制，可能持续到 agent 退出或手动移除。");
        }
        let lifetime = match verb {
            "remember-hour" => Some(3600),
            "remember-day" => Some(86400),
            _ => None,
        };
        let verb = if matches!(verb, "remember-hour" | "remember-day") {
            "remember"
        } else {
            verb
        };
        if verb == "key" {
            let path = path.unwrap();
            return open(
                "SSH 密钥操作",
                vec![Row::new(
                    command_path("forget", &path),
                    "忘记口令 / 清理登记记录",
                )],
                &path.display().to_string(),
            );
        }
        if verb == "forget" {
            let path = path.unwrap();
            return open(
                "确认忘记此密钥的口令？",
                vec![
                    Row::new(command_path("confirm-forget", &path), "确认忘记保存的口令"),
                    Row::new("/git-auth", "返回"),
                ],
                "将禁用自动加载并删除保存的口令；不会删除私钥，也不会移除 agent 中已加载的身份。",
            );
        }
        let ui = match ctx.require::<UiSvc>() {
            Ok(ui) => ui,
            Err(_) => return Outcome::Refused("当前没有可用界面".into()),
        };
        let secrets = match ctx.require::<SecretInputSvc>() {
            Ok(port) => port,
            Err(_) => return Outcome::Refused("安全输入不可用".into()),
        };
        let repaint = ctx.service::<RepaintSvc>();
        let mut state = self.state.lock().unwrap();
        if state.operation.is_some() {
            return Outcome::Refused(
                "已有认证操作进行中；可使用 /git-auth cancel 请求取消。".into(),
            );
        }
        state.next += 1;
        let id = state.next;
        let stop = Arc::new(AtomicBool::new(false));
        let waiting = Arc::new(AtomicBool::new(verb == "remember"));
        let stopping = stop.clone();
        let awaiting_secret = waiting.clone();
        let flight = Flight {
            state: self.state.clone(),
            id,
        };
        let root = self.root.clone();
        let verb = verb.to_owned();
        let task = tokio::spawn(async move {
            let result: anyhow::Result<String> = async {
                if verb == "status" { let _flight = flight; return crate::git_auth_cmd::status_text(&root).await; }
                let secret = if verb == "remember" {
                    let prompt = format!("保存 SSH 口令（Esc 取消）：{}", path.as_ref().unwrap().display());
                    let answer = secrets.read(&prompt).await;
                    awaiting_secret.store(false, Ordering::Release);
                    let Some(answer) = answer else { anyhow::bail!("已取消输入或安全输入正忙；未保存口令"); };
                    Some(answer)
                } else { None };
                tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                    let _flight = flight;
                    if stopping.load(Ordering::Acquire) { anyhow::bail!("已取消"); }
                    match verb.as_str() {
                        "remember" => {
                            atomcode_auth::git_ssh::remember_cancellable(&root, &path.unwrap(), lifetime, secret.unwrap(), || stopping.load(Ordering::Acquire))?;
                            Ok("口令已存入系统凭据库，密钥已加载；后续启动会尝试自动加载。".into())
                        }
                        "unlock" => {
                            let count = atomcode_auth::git_ssh::unlock_registered_cancellable(&root, &std::env::current_exe()?, || stopping.load(Ordering::Acquire))?;
                            Ok(format!("已加载 {count} 把登记密钥。此结果不代表远端允许 push。"))
                        }
                        "confirm-forget" => {
                            atomcode_auth::git_ssh::forget(&root, &path.unwrap())?;
                            Ok("已忘记保存的口令并禁用自动加载；agent 中已加载的身份仍需单独移除。".into())
                        }
                        _ => unreachable!(),
                    }
                }).await.map_err(|_| anyhow::anyhow!("认证任务未正常结束；请检查登记与待清理状态"))?
            }.await;
            match result {
                Ok(text) => ui.say(&text),
                Err(error) => ui.say(&format!("Git 认证：{error}")),
            }
            if let Some(repaint) = repaint {
                repaint.now();
            }
        });
        state.operation = Some(Operation {
            id,
            stop,
            waiting,
            abort: task.abort_handle(),
        });
        drop(state);
        Outcome::Quiet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip_without_shell_interpretation_and_arguments_are_private() {
        for text in [
            "/tmp/my key",
            r"C:\Users\name\.ssh\id_ed25519",
            "/tmp/$(touch nope)",
        ] {
            let path = Path::new(text);
            let command = command_path("remember", path);
            assert_eq!(
                path_arg(command.strip_prefix("/git-auth remember ").unwrap()).unwrap(),
                path
            );
        }
        assert!(path_arg("bad\npath").is_err());
        let commands = GitAuthCommands {
            root: PathBuf::new(),
            state: Arc::default(),
        };
        assert!(commands.commands()[0].secret_args);
    }

    #[test]
    fn picker_only_lists_private_files_with_a_public_companion() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "id_ed25519",
            "id_ed25519.pub",
            "orphan.pub",
            "config",
            "another",
        ] {
            std::fs::write(temp.path().join(name), "not read by picker").unwrap();
        }
        assert_eq!(
            candidates(temp.path()),
            vec![temp.path().join("id_ed25519")]
        );
    }

    #[tokio::test]
    async fn an_old_operation_cannot_clear_its_replacement() {
        let state: Shared = Arc::default();
        let task = tokio::spawn(std::future::pending::<()>());
        let stop = Arc::new(AtomicBool::new(false));
        state.lock().unwrap().operation = Some(Operation {
            id: 2,
            stop: stop.clone(),
            waiting: Arc::new(AtomicBool::new(true)),
            abort: task.abort_handle(),
        });
        drop(Flight {
            state: state.clone(),
            id: 1,
        });
        assert!(state.lock().unwrap().operation.is_some());
        assert!(cancel(&state, false));
        assert!(stop.load(Ordering::Acquire));
        assert!(task.await.unwrap_err().is_cancelled());
        drop(Flight {
            state: state.clone(),
            id: 2,
        });
        assert!(state.lock().unwrap().operation.is_none());
    }
}
