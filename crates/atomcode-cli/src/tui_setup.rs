//! Host implementation of the TUI setup port. Capability checks and confirmed
//! actions follow the current session directory supplied by the screen. Settings
//! are reread from the launcher's selected config file, not copied into row config.
//! Seed skills remain user-wide and bootstrap through the existing setup installer.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::ProductDirs;
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
    dirs: ProductDirs,
    config_path: PathBuf,
    operations: SetupOperations,
}

impl DiskSetup {
    pub fn new(working_dir: PathBuf, dirs: ProductDirs, config_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            home: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            working_dir,
            dirs,
            config_path,
            operations: SetupOperations::default(),
        })
    }
}

impl DiskSetup {
    fn capabilities(&self, root: &std::path::Path) -> crate::project_setup::ProjectSetup {
        crate::project_setup::ProjectSetup {
            root: root.into(),
            dirs: self.dirs.clone(),
            config_path: self.config_path.clone(),
        }
    }
}

/// One local setup operation at a time; cancellation drops its owned process future.
#[derive(Default)]
struct SetupOperations {
    active: std::sync::Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

struct OperationGuard<'a>(&'a SetupOperations);
impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.0
            .active
            .lock()
            .expect("setup operation poisoned")
            .take();
    }
}

impl SetupOperations {
    async fn run(
        &self,
        operation: impl std::future::Future<Output = Result<String, String>>,
    ) -> Result<String, String> {
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        {
            let mut active = self.active.lock().expect("setup operation poisoned");
            if active.is_some() {
                return Err("Setup is already running; use /setup cancel first".into());
            }
            *active = Some(stop);
        }
        let _guard = OperationGuard(self);
        tokio::select! {
            biased;
            _ = stopped.changed() => Err("Setup cancelled; inspect capability state before retrying".into()),
            result = operation => result,
        }
    }
    fn cancel(&self) -> Result<String, String> {
        let active = self.active.lock().expect("setup operation poisoned");
        let stop = active.as_ref().ok_or("No setup operation is running")?;
        stop.send(true)
            .map_err(|_| "Setup has already finished".to_string())?;
        Ok("Cancelling setup…".into())
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
/// 而不必去动真实家目录。
fn seed_installed(home: &std::path::Path, project: &std::path::Path, dirs: &ProductDirs) -> bool {
    let scanned = atomcode_capabilities::skills::standard_skill_dirs(
        atomcode_capabilities::skills::SkillRoots {
            home,
            project,
            dirs,
        },
    );
    atomcode_capabilities::skills::SkillRegistry::load(&scanned)
        .user_invocable()
        .iter()
        .any(|skill| skill.name.rsplit(':').next() == Some("setup"))
}

#[async_trait]
impl Setup for DiskSetup {
    fn installed(&self) -> bool {
        seed_installed(&self.home, &self.working_dir, &self.dirs)
    }

    fn capability_setup(&self) -> bool {
        true
    }

    async fn check(
        &self,
        root: &std::path::Path,
        force: bool,
    ) -> Result<Option<atomcode_tui::setup::CapabilityReport>, String> {
        self.capabilities(root)
            .check(force)
            .await
            .map(Some)
            .map_err(|e| e.to_string())
    }
    async fn plan(
        &self,
        root: &std::path::Path,
        id: &str,
    ) -> Result<atomcode_tui::setup::CapabilityPlan, String> {
        self.capabilities(root)
            .plan(id)
            .await
            .map_err(|e| e.to_string())
    }
    async fn apply(&self, root: &std::path::Path, id: &str, token: &str) -> Result<String, String> {
        let service = self.capabilities(root);
        self.operations
            .run(async { service.apply(id, token).await.map_err(|e| e.to_string()) })
            .await
    }
    async fn cancel(&self) -> Result<String, String> {
        self.operations.cancel()
    }
    async fn dismiss(&self, root: &std::path::Path) -> Result<String, String> {
        self.capabilities(root).dismiss().map_err(|e| e.to_string())
    }

    async fn install(&self) -> Result<String, String> {
        let root = self.working_dir.clone();
        // 装在项目上,但写的是用户级的 skill 目录 —— 见文件头「种子装到哪」。
        let opts = atomcode_capabilities::setup::RunOptions::new(root, self.dirs.clone());
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

    /// 一个**空的家目录**加一棵临时的用户目录:种子写进用户目录,查的时候也只查那儿,
    /// 否则机器上真实的 `~/.claude/skills` 里恰好有个 `setup` 就能让判据永远为真。
    /// 目录是交进来的,不经进程环境,所以测试之间不必互相排队。
    fn with_home<T>(f: impl FnOnce(std::path::PathBuf, ProductDirs) -> T) -> T {
        let home = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(config.path().to_path_buf(), ".ours");
        f(home.path().to_path_buf(), dirs)
    }

    fn port(home: std::path::PathBuf, dirs: ProductDirs) -> (tempfile::TempDir, Arc<DiskSetup>) {
        let project = tempfile::tempdir().unwrap();
        let setup = Arc::new(DiskSetup {
            home,
            working_dir: project.path().to_path_buf(),
            config_path: dirs.user().join("config.toml"),
            operations: SetupOperations::default(),
            dirs,
        });
        (project, setup)
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancellation_settles_the_pending_operation_and_allows_another() {
        let operations = Arc::new(SetupOperations::default());
        let worker = operations.clone();
        let task = tokio::spawn(async move { worker.run(std::future::pending()).await });
        tokio::task::yield_now().await;
        assert!(operations.run(async { Ok("second".into()) }).await.is_err());
        operations.cancel().unwrap();
        assert!(task.await.unwrap().unwrap_err().contains("cancelled"));
        assert_eq!(
            operations.run(async { Ok("next".into()) }).await.unwrap(),
            "next"
        );
        assert!(operations.cancel().is_err());
    }
    /// 没装过就是没装过:一台新机器上 `/setup` 之所以需要屏幕侧那一步,正是因为这
    /// 时候 agent 的命令目录里没有它。
    #[test]
    fn a_machine_that_never_ran_setup_has_none() {
        with_home(|home, dirs| {
            let (_project, port) = port(home, dirs);
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
        with_home(|home, dirs| {
            let (_project, port) = port(home, dirs);
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
        let config = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(config.path().to_path_buf(), ".ours");
        assert!(
            !seed_installed(empty.path(), project.path(), &dirs),
            "an empty home has no skills, whatever this machine has"
        );
    }
}
