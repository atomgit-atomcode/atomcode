//! 种子安装端口:`atomcode --tui` 这一侧的另一半。
//!
//! `atomcode_tui::commands::SetupCommands` 编排那三步(装 → 重载 → 转发);
//! 这里只回答「装过没有」和「装一遍,装在哪个项目上」(`docs/adr/0022` §3)。
//!
//! **为什么住在 cli 而不在 tui**:装种子要 `atomcode-capabilities` 的 `setup`
//! feature —— 解压内嵌的 `setup-seeds.tar.zst`、扫描项目、原子写、文件锁。
//! `atomcode-tui` 刻意把 feature 收到 `tools`(见它的 `Cargo.toml`),而 cli 本来就
//! 开着 `setup`(`atomcode setup` 子命令用的是同一套函数)。给 tui 开这个 feature,
//! 等于为了重复这个二进制已经会做的动作,把解压栈拉进每一个用它的前端。
//!
//! **装是阻塞的,所以走 `spawn_blocking`**:`setup::run` 通篇是文件 I/O(自带注释
//! 说明「If called from an async context, use `tokio::task::spawn_blocking`」)。
//! 老前端在 tokio 里用 `block_in_place` 硬扛,那是它跑在事件循环线程上的形状;
//! 这里的端口是 async 的,于是一个阻塞线程就够,而且不拖住别人的回合。
//!
//! **种子装到哪**:`$ATOMCODE_HOME`(装了 `ATOMCODE_HOME` 就是它,否则 `~/.atomcode`)
//! —— 和 `SkillRegistry` 扫的是同一个目录(`runtime_skill_dirs`),所以装完重载一下
//! agent 就看得见。老前端装的是**同一个位置**,不是项目级:`/setup` 在一台新机器上
//! 装的那一个 `setup` skill,是全机器共用的。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use atomcode_tui::setup::Setup;

/// 磁盘上的种子。
///
/// 持**项目根**与**用户目录**而不持已加载的配置,和 `DiskPlugins` 同一条理由:装的那
/// 一下,项目根是启动时定下来的那个,而「此刻装的是什么」不该受中途改配置影响。
/// 用户目录同样在构造时定一次 —— 会话活着的这段时间里它不会变,而每次问一遍
/// `dirs::home_dir()` 只会让「种子装在哪」有两个答案。
pub struct DiskSetup {
    home: PathBuf,
    working_dir: PathBuf,
}

impl DiskSetup {
    pub fn new(working_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            home: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            working_dir,
        })
    }
}

/// 种子 skill 在不在 `home` + `project` 这套目录里，**且它真的会变成一条命令**。
///
/// 问的是「`/setup` 这条命令会不会被登记出来」，不是「磁盘上有没有叫 setup 的文件」
/// —— 两个问题差一个条件，而差的那个正是这条命令存不存在：登记走的是
/// `user_invocable()`（`harness/plugins/capabilities.rs::register_skill_commands`），
/// 一个 `user-invocable: false` 的 skill 在目录里却打不出来。谓词和登记那侧用同一个，
/// 是这里能省掉一次白装的全部依据。
///
/// 走注册表而不是自己拼路径：`SkillRegistry` 扫哪些目录、按什么优先级，是加载器的
/// 事，第二份路径知识迟早和它分家。拆成自由函数是为了让判据能拿临时目录问一遍，
/// 而不必去动进程里的 `ATOMCODE_HOME` 和真实家目录。
fn seed_installed(home: &std::path::Path, project: &std::path::Path) -> bool {
    let dirs = atomcode_capabilities::skills::runtime_skill_dirs(home, project);
    atomcode_capabilities::skills::SkillRegistry::load(&dirs)
        .user_invocable()
        .iter()
        .any(|skill| skill.name.rsplit(':').next() == Some("setup"))
}

#[async_trait]
impl Setup for DiskSetup {
    fn installed(&self) -> bool {
        seed_installed(&self.home, &self.working_dir)
    }

    async fn install(&self) -> Result<String, String> {
        let root = self.working_dir.clone();
        // 装在项目上,但写的是用户级的 skill 目录 —— 见文件头「种子装到哪」。
        let opts = atomcode_capabilities::setup::RunOptions::new(root);
        let report = tokio::task::spawn_blocking(move || atomcode_capabilities::setup::run(opts))
            .await
            .map_err(|e| {
                tr(SMsg::SetupJobDidNotStart {
                    error: &e.to_string(),
                })
                .into_owned()
            })?
            .map_err(|e| {
                tr(SMsg::SetupFailed {
                    error: &e.to_string(),
                })
                .into_owned()
            })?;
        Ok(report.render_cli())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// `ATOMCODE_HOME` 是进程级的,而装种子和查种子都读它。nextest 下每个测试是
    /// 自己的进程,这个锁是给 `cargo test` 那种同进程跑法留的。
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// 设 `ATOMCODE_HOME` 到临时目录,跑完还原 —— 和 `schedule_cmd` 的
    /// `with_temp_home` 同一条理由,只是这里还要一个**空的家目录**:种子写进
    /// `ATOMCODE_HOME`,查的时候也要只查那儿,否则机器上真实的 `~/.claude/skills`
    /// 里恰好有个 `setup` 就能让判据永远为真。
    fn with_home<T>(f: impl FnOnce(std::path::PathBuf) -> T) -> T {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var_os("ATOMCODE_HOME");
        let home = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", config.path());
        let result = f(home.path().to_path_buf());
        match previous {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
        result
    }

    fn port(home: std::path::PathBuf) -> (tempfile::TempDir, Arc<DiskSetup>) {
        let project = tempfile::tempdir().unwrap();
        let setup = Arc::new(DiskSetup {
            home,
            working_dir: project.path().to_path_buf(),
        });
        (project, setup)
    }

    /// 没装过就是没装过:一台新机器上 `/setup` 之所以需要屏幕侧那一步,正是因为这
    /// 时候 agent 的命令目录里没有它。
    #[test]
    fn a_machine_that_never_ran_setup_has_none() {
        with_home(|home| {
            let (_project, port) = port(home);
            assert!(
                !port.installed(),
                "a fresh home must not claim to have the seeds"
            );
        });
    }

    /// 装一遍,种子就出现在**注册表自己扫的那个位置**——这是这条判据的全部意义:
    /// 装完之后 agent 那边会多出一条第 `name: setup` 的 user_invocable skill,
    /// `/setup` 才会在重载后存在。装到别处等于没装。
    #[test]
    fn installing_puts_the_seed_where_the_registry_looks() {
        with_home(|home| {
            let (_project, port) = port(home);
            assert!(!port.installed(), "nothing yet");

            let report = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(port.install())
                .expect("installing the seeds");
            assert!(
                report.contains("Setup"),
                "the report says what happened: {report}"
            );

            assert!(
                port.installed(),
                "the seed is not where the registry scans — `/setup` would still not exist"
            );
        });
    }

    /// 查的是注册表扫的目录,不是随手拼的一条路径。把家目录换成一个空目录,答的必须
    /// 是「没有」——即使这台机器上装着种子。
    #[test]
    fn the_answer_comes_from_the_dirs_the_registry_scans() {
        let empty = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        assert!(
            !seed_installed(empty.path(), project.path()),
            "an empty home has no skills, whatever this machine has"
        );
    }
}
