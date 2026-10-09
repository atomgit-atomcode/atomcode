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

/// What `run_shell` puts before every stderr chunk it hands the callback.
const STDERR: &str = "[stderr] ";

struct Here {
    working_dir: std::path::PathBuf,
}

#[async_trait]
impl Shell for Here {
    async fn run(&self, command: &str, within: Duration) -> Ran {
        self.run_streaming(command, within, &|_| {}).await
    }

    async fn run_streaming(
        &self,
        command: &str,
        within: Duration,
        line: &(dyn Fn(String) + Send + Sync),
    ) -> Ran {
        self.run_streaming_in(None, command, within, line).await
    }

    /// In the session's directory when the screen knows it — after a `/cd`
    /// that is not where this row was set up — and in the launch directory
    /// otherwise.
    async fn run_streaming_in(
        &self,
        dir: Option<&std::path::Path>,
        command: &str,
        within: Duration,
        line: &(dyn Fn(String) + Send + Sync),
    ) -> Ran {
        use atomcode_capabilities::tools::{run_shell, ShellExit};
        // 一次一整行地交出去:块的边界是一次读了多少,不是一行,按块画会把一行
        // 劈成两截。stdout 和 stderr **各攒各的**:`run_shell` 把两路的块交给同
        // 一个回调(stderr 的块前面带 `[stderr] `),攒在一处的话,stdout 半行没
        // 换行时来一块 stderr 就会被粘成一行。`[stderr] ` 也按行加,而不是按块。
        let pending = std::sync::Mutex::new((String::new(), String::new()));
        let emit = |buf: &mut String, prefix: &str| {
            while let Some(nl) = buf.find('\n') {
                let whole: String = buf.drain(..=nl).collect();
                line(format!("{prefix}{}", whole.trim_end_matches(['\n', '\r'])));
            }
        };
        let outcome = run_shell(
            &atomcode_capabilities::world::LocalShell,
            command,
            dir.unwrap_or(&self.working_dir),
            within.as_secs(),
            |chunk| {
                let mut held = pending.lock().expect("pending poisoned");
                let (out, err) = &mut *held;
                match chunk.strip_prefix(STDERR) {
                    Some(rest) => {
                        err.push_str(rest);
                        emit(err, STDERR);
                    }
                    None => {
                        out.push_str(chunk);
                        emit(out, "");
                    }
                }
            },
        )
        .await;
        let (out, err) = std::mem::take(&mut *pending.lock().expect("pending poisoned"));
        if !out.is_empty() {
            line(out);
        }
        if !err.is_empty() {
            line(format!("{STDERR}{err}"));
        }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A `!` runs where the session works when the screen knows it — after a
    /// `/cd`, not where this row was set up — and where it was set up otherwise.
    #[tokio::test]
    async fn a_command_runs_in_the_directory_it_is_given() {
        let set_up = tempfile::tempdir().expect("dir");
        let moved = tempfile::tempdir().expect("dir");
        let here = Here {
            working_dir: set_up.path().to_path_buf(),
        };
        let ran = here
            .run_streaming_in(
                Some(moved.path()),
                "pwd -P",
                Duration::from_secs(10),
                &|_| {},
            )
            .await;
        let moved_real = moved.path().canonicalize().expect("real");
        assert_eq!(
            ran.output.trim(),
            moved_real.display().to_string(),
            "{ran:?}"
        );
        let ran = here
            .run_streaming_in(None, "pwd -P", Duration::from_secs(10), &|_| {})
            .await;
        let set_up_real = set_up.path().canonicalize().expect("real");
        assert_eq!(
            ran.output.trim(),
            set_up_real.display().to_string(),
            "{ran:?}"
        );
    }

    /// stdout and stderr are kept apart while they stream: a stdout line still
    /// being written when stderr speaks stays one line, and every stderr line —
    /// not only the first of its chunk — is marked as stderr.
    #[tokio::test]
    async fn the_two_streams_are_not_glued_together() {
        let here = Here {
            working_dir: std::env::temp_dir(),
        };
        let seen = std::sync::Mutex::new(Vec::<String>::new());
        let take = |line: String| seen.lock().expect("seen poisoned").push(line);
        let ran = here
            .run_streaming(
                "printf 'half'; sleep 0.2; printf 'e1\\ne2\\n' >&2; sleep 0.2; printf ' done\\n'",
                Duration::from_secs(10),
                &take,
            )
            .await;
        assert_eq!(ran.code, Some(0), "{ran:?}");
        let seen = seen.into_inner().expect("seen poisoned");
        assert!(seen.contains(&"half done".to_string()), "{seen:?}");
        assert!(seen.contains(&"[stderr] e1".to_string()), "{seen:?}");
        assert!(seen.contains(&"[stderr] e2".to_string()), "{seen:?}");
        assert!(
            !seen
                .iter()
                .any(|l| l.contains("half") && l.contains("stderr")),
            "no row glues the two: {seen:?}"
        );
    }
}
