//! `!cmd` 真正开进程的那一半。
//!
//! 屏幕认得出 `!` 这个手势，也知道结果该画成什么样；开一个子进程是操作系统的
//! 事，而那块屏幕碰不到操作系统（`gates/tui-layers.sh`）。所以它是一行：填了
//! 这条缝，`!` 就能用；没填，`!git status` 还是一句发给模型的话——daemon 和
//! ACP 正是后者，它们也不该凭一条通道就能在别人机器上开进程。
//!
//! **跑的是 `bash` 那个工具用的同一个 `LocalShell`。** 不是第二份实现：进程组
//! 怎么建、超时怎么杀、Windows 上的 job object 怎么收，那些都在里面，而第二份
//! 实现会在其中某一条上和第一份分道扬镳。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::plugin::ShellSvc;
use atomcode_tui::shell::{Ran, Shell};
use serde_json::Value;

/// 行的名字。
pub const ROW: &str = "tui-shell";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 把 `!` 接到这台机器上的那一行。
pub struct ShellRow {
    /// 在哪儿跑。会话的工作目录，和模型的 `bash` 工具同一个地方——两者跑出
    /// 不同的结果会是最难看的一种不一致。
    pub working_dir: std::path::PathBuf,
}

#[async_trait]
impl Plugin for ShellRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-shell"]
    }
    fn description(&self) -> &'static str {
        "running a command on this machine for the `!` gesture, in the session's working directory"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<ShellSvc>(Arc::new(Here {
                working_dir: self.working_dir.clone(),
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct Here {
    working_dir: std::path::PathBuf,
}

#[async_trait]
impl Shell for Here {
    async fn run(&self, command: &str, within: Duration) -> Ran {
        use atomcode_capabilities::tools::{run_shell, ShellExit};
        let outcome = run_shell(
            &atomcode_capabilities::world::LocalShell,
            command,
            &self.working_dir,
            within.as_secs(),
            // 不逐块回调：这一层答的是「跑完了，结果是这些」。边跑边画是另一件
            // 事，要的话得先有一条把片段送上屏幕的缝，而现在没有。
            |_| {},
        )
        .await;
        // stdout 和 stderr 合起来，按它们本来的顺序读不出来——所以 stderr 排在
        // 后面并原样保留。人敲 `!` 多半正是想看报错。
        let mut output = outcome.stdout;
        if !outcome.stderr.is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&outcome.stderr);
        }
        match outcome.exit {
            ShellExit::Exited { code, .. } => Ran {
                code,
                output,
                timed_out: false,
            },
            // 被当成卡住杀掉、或者撞了墙钟上限：两者对人是同一件事——它没跑完。
            ShellExit::KilledIdle | ShellExit::KilledTimeout => Ran {
                code: None,
                output,
                timed_out: true,
            },
        }
    }
}
