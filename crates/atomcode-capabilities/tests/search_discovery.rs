#![cfg(feature = "tools")]

use atomcode_capabilities::{
    tools::{GlobTool, GrepTool, ReadFileTool},
    world::{FileSystem, LocalFs, WalkOptions},
    ProductDirs,
};
use atomcode_kernel::tool::{Tool, ToolContext};
use std::{path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

fn context(root: &Path) -> ToolContext {
    ToolContext {
        working_dir: root.into(),
        cancel: CancellationToken::new(),
        progress: Default::default(),
        requester: None,
    }
}
fn dirs(root: &Path) -> ProductDirs {
    ProductDirs::new(root.join("user"), ".ours")
}
fn setup(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("vendor")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\nlinked/\n").unwrap();
    for f in [
        "visible.txt",
        "ignored.txt",
        "vendor/dependency.txt",
        ".hidden/secret.txt",
    ] {
        std::fs::write(root.join(f), "needle").unwrap();
    }
}
#[tokio::test]
async fn filters_are_independent_and_exact_reads_ignore_discovery_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup(root);
    let c = context(root);
    let glob = GlobTool::new(dirs(root));
    let grep = GrepTool::new(dirs(root));
    let ordinary = glob.execute(r#"{"pattern":"**/*.txt"}"#, &c).await;
    assert!(ordinary.content.contains("visible.txt"));
    assert!(!ordinary.content.contains("ignored.txt"));
    assert!(!ordinary.content.contains("dependency.txt"));
    assert!(!ordinary.content.contains("secret.txt"));
    let ignored = glob
        .execute(r#"{"pattern":"**/*.txt","respect_gitignore":false}"#, &c)
        .await;
    assert!(ignored.content.contains("ignored.txt"));
    assert!(ignored.content.contains("dependency.txt"));
    assert!(!ignored.content.contains("secret.txt"));
    let hidden = glob
        .execute(r#"{"pattern":"**/*.txt","include_hidden":true}"#, &c)
        .await;
    assert!(hidden.content.contains("secret.txt"));
    assert!(!hidden.content.contains("ignored.txt"));
    let results = grep
        .execute(
            r#"{"pattern":"needle","respect_gitignore":false,"include_hidden":true,"context":0}"#,
            &c,
        )
        .await;
    for f in ["ignored.txt", "dependency.txt", "secret.txt"] {
        assert!(results.content.contains(f), "{}", results.content);
    }
    let read = ReadFileTool::new(false)
        .execute(r#"{"file_path":"ignored.txt"}"#, &c)
        .await;
    assert!(!read.is_error);
    assert!(read.content.contains("needle"));
    std::fs::write(root.join("debug.log"), "unique_log_needle").unwrap();
    let log = grep
        .execute(r#"{"pattern":"unique_log_needle","path":"debug.log"}"#, &c)
        .await;
    assert!(log.content.contains("debug.log:1:"), "{}", log.content);
}
#[tokio::test]
async fn entry_limit_is_incomplete_not_a_claim_of_absence_and_mount_defaults_can_be_overridden() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path());
    let c = context(tmp.path());
    let options = WalkOptions {
        max_entries: 1,
        ..Default::default()
    };
    let result = GlobTool::new(dirs(tmp.path()))
        .with_options(options.clone())
        .execute(r#"{"pattern":"**/missing.txt"}"#, &c)
        .await;
    assert!(result.content.contains("Scan incomplete"));
    assert!(result.content.contains("entry limit reached"));
    assert!(!result.content.contains("No files matching"));
    let result = GrepTool::new(dirs(tmp.path()))
        .with_options(options)
        .execute(r#"{"pattern":"absent"}"#, &c)
        .await;
    assert!(result.content.contains("Scan incomplete"));
    assert!(!result.content.contains("No matches found"));
    let options = WalkOptions {
        respect_gitignore: false,
        ..Default::default()
    };
    let tool = GlobTool::new(dirs(tmp.path())).with_options(options);
    assert!(tool
        .execute(r#"{"pattern":"**/ignored.txt"}"#, &c)
        .await
        .content
        .contains("ignored.txt"));
    assert!(tool
        .execute(
            r#"{"pattern":"**/ignored.txt","respect_gitignore":true}"#,
            &c
        )
        .await
        .content
        .contains("No files matching"));
}
#[tokio::test]
async fn progress_names_the_scope_and_cancellation_is_not_an_empty_result() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path());
    let mut c = context(tmp.path());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = seen.clone();
    c.progress = atomcode_kernel::tool::ProgressSink::new(Arc::new(move |msg| {
        captured.lock().unwrap().push(msg)
    }));
    GlobTool::new(dirs(tmp.path()))
        .execute(r#"{"pattern":"*.txt"}"#, &c)
        .await;
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("Searching") && s.contains("30s")));
    c.cancel.cancel();
    for result in [
        GlobTool::new(dirs(tmp.path()))
            .execute(r#"{"pattern":"*.txt"}"#, &c)
            .await,
        GrepTool::new(dirs(tmp.path()))
            .execute(r#"{"pattern":"needle"}"#, &c)
            .await,
    ] {
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));
    }
}
#[cfg(any(unix, windows))]
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => true,
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(e) => panic!("{e}"),
        }
    }
}
#[cfg(any(unix, windows))]
#[tokio::test]
async fn ignored_directory_links_need_both_switches_and_cycles_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    setup(tmp.path());
    std::fs::write(outside.path().join("linked.txt"), "linked_needle").unwrap();
    if !link_dir(outside.path(), &tmp.path().join("linked")) {
        return;
    }
    let c = context(tmp.path());
    let glob = GlobTool::new(dirs(tmp.path()));
    for args in [
        r#"{"pattern":"**/linked.txt","follow_symlinks":true}"#,
        r#"{"pattern":"**/linked.txt","respect_gitignore":false}"#,
    ] {
        assert!(!glob
            .execute(args, &c)
            .await
            .content
            .contains("linked/linked.txt"));
    }
    let both = glob
        .execute(
            r#"{"pattern":"**/linked.txt","respect_gitignore":false,"follow_symlinks":true}"#,
            &c,
        )
        .await;
    assert!(
        both.content.contains("linked/linked.txt"),
        "{}",
        both.content
    );
    let exact = glob.execute(r#"{"pattern":"linked/linked.txt"}"#, &c).await;
    assert!(exact.content.contains("1 files found"));
    let read = ReadFileTool::new(false)
        .execute(r#"{"file_path":"linked/linked.txt"}"#, &c)
        .await;
    assert!(!read.is_error);
    assert!(read.content.contains("linked_needle"));
    let grep = GrepTool::new(dirs(tmp.path()))
        .execute(
            r#"{"pattern":"linked_needle","respect_gitignore":false,"follow_symlinks":true}"#,
            &c,
        )
        .await;
    assert!(grep.content.contains("linked/linked.txt"));
    assert!(link_dir(tmp.path(), &outside.path().join("cycle")));
    let skip: atomcode_capabilities::world::SkipDir = Arc::new(|_: &str| false);
    let walked = LocalFs::unfenced()
        .walk_with_options(
            tmp.path(),
            &skip,
            &CancellationToken::new(),
            &WalkOptions {
                follow_symlinks: true,
                respect_gitignore: false,
                include_hidden: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(walked.scan.incomplete());
    assert!(walked.files.len() < 50);
}
#[cfg(any(unix, windows))]
#[tokio::test]
async fn following_links_does_not_escape_a_read_fence_or_expose_credentials() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    std::fs::write(user.path().join("config.toml"), "secret_needle").unwrap();
    std::fs::write(user.path().join("public.txt"), "public_needle").unwrap();
    if !link_dir(user.path(), &project.path().join("linked")) {
        return;
    }
    let c = context(project.path());
    let d = ProductDirs::new(user.path(), ".ours");
    let opts = r#"{"pattern":"needle","include_hidden":true,"respect_gitignore":false,"follow_symlinks":true,"context":0}"#;
    let result = GrepTool::new(d.clone()).execute(opts, &c).await;
    assert!(result.content.contains("public_needle"));
    assert!(!result.content.contains("secret_needle"));
    let result = GrepTool::with_world(Arc::new(LocalFs::new(project.path())), d)
        .execute(opts, &c)
        .await;
    assert!(!result.content.contains("public_needle"));
    assert!(
        result.content.contains("Scan incomplete"),
        "{}",
        result.content
    );
}
#[cfg(unix)]
#[tokio::test]
async fn an_explicit_broken_link_reports_the_link_not_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("missing", dir.path().join("broken")).unwrap();
    let result = ReadFileTool::new(false)
        .execute(r#"{"file_path":"broken"}"#, &context(dir.path()))
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("broken symbolic link"));
    let child = ReadFileTool::new(false)
        .execute(r#"{"file_path":"broken/child.txt"}"#, &context(dir.path()))
        .await;
    assert!(child.content.contains("broken symbolic link"));
}

#[derive(Clone, Copy)]
enum WaitAt {
    Walk,
    Metadata,
    AfterSlowMetadata,
}
struct WaitingWorld(LocalFs, WaitAt);
#[async_trait::async_trait]
impl FileSystem for WaitingWorld {
    fn describe(&self) -> String {
        "waiting test world".into()
    }
    fn root(&self) -> std::path::PathBuf {
        self.0.root()
    }
    async fn read_bytes(&self, p: &Path) -> Result<Vec<u8>, atomcode_capabilities::world::FsError> {
        self.0.read_bytes(p).await
    }
    async fn write_bytes(
        &self,
        p: &Path,
        b: &[u8],
    ) -> Result<(), atomcode_capabilities::world::FsError> {
        self.0.write_bytes(p, b).await
    }
    async fn create_dir_all(&self, p: &Path) -> Result<(), atomcode_capabilities::world::FsError> {
        self.0.create_dir_all(p).await
    }
    async fn list(
        &self,
        p: &Path,
    ) -> Result<Vec<atomcode_capabilities::world::DirEntry>, atomcode_capabilities::world::FsError>
    {
        self.0.list(p).await
    }
    async fn info(
        &self,
        p: &Path,
    ) -> Result<atomcode_capabilities::world::FileInfo, atomcode_capabilities::world::FsError> {
        match self.1 {
            WaitAt::Metadata => return std::future::pending().await,
            WaitAt::AfterSlowMetadata => {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await
            }
            WaitAt::Walk => {}
        }
        self.0.info(p).await
    }
    async fn canonicalize(
        &self,
        p: &Path,
    ) -> Result<std::path::PathBuf, atomcode_capabilities::world::FsError> {
        self.0.canonicalize(p).await
    }
    async fn walk(
        &self,
        _: &Path,
        _: &atomcode_capabilities::world::SkipDir,
        _: &CancellationToken,
    ) -> Result<Vec<std::path::PathBuf>, atomcode_capabilities::world::FsError> {
        std::future::pending().await
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_blocked_world_times_out_without_claiming_files_do_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    let c = context(dir.path());
    let world = Arc::new(WaitingWorld(LocalFs::unfenced(), WaitAt::Walk));
    let options = WalkOptions {
        timeout_secs: 1,
        ..Default::default()
    };
    for tool in [
        Box::new(
            GlobTool::with_world(world.clone(), dirs(dir.path())).with_options(options.clone()),
        ) as Box<dyn Tool>,
        Box::new(
            GrepTool::with_world(world.clone(), dirs(dir.path())).with_options(options.clone()),
        ),
    ] {
        let began = tokio::time::Instant::now();
        let result = tool.execute(r#"{"pattern":"needle.*"}"#, &c).await;
        assert!(result.is_error);
        assert!(result.content.contains("timed out"));
        assert!(result.content.contains("scan incomplete"));
        assert!(!result.content.contains("No matches found"));
        assert_eq!(began.elapsed(), std::time::Duration::from_secs(1));
    }
}
#[cfg(windows)]
#[tokio::test]
async fn windows_junctions_can_be_searched_with_explicit_link_following() {
    let project = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    std::fs::write(target.path().join("junction.txt"), "junction_needle").unwrap();
    let junction = project.path().join("linked");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(target.path())
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let result = GrepTool::new(dirs(project.path()))
        .execute(
            r#"{"pattern":"junction_needle","respect_gitignore":false,"follow_symlinks":true}"#,
            &context(project.path()),
        )
        .await;
    assert!(
        result.content.contains("junction.txt"),
        "{}",
        result.content
    );
    std::fs::remove_dir(junction).unwrap();
}

#[tokio::test]
async fn hidden_search_does_not_silently_read_sensitive_files() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join(".env"), "private_needle").unwrap();
    std::fs::write(d.path().join(".env.example"), "example_needle").unwrap();
    let tool = GrepTool::new(dirs(d.path()));
    let c = context(d.path());
    let result = tool
        .execute(
            r#"{"pattern":"needle","include_hidden":true,"respect_gitignore":false}"#,
            &c,
        )
        .await;
    assert!(result.content.contains("example_needle"));
    assert!(!result.content.contains("private_needle"));
    let explicit = tool
        .execute(r#"{"pattern":"needle","path":".env"}"#, &c)
        .await;
    assert!(
        explicit.content.contains("private_needle"),
        "explicit paths retain the pre-existing approval gate contract"
    );
}

#[tokio::test]
async fn literal_globs_do_not_depend_on_ignore_rules_or_scan_the_tree() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    let tool = GlobTool::new(dirs(dir.path())).with_options(WalkOptions {
        max_entries: 1,
        ..Default::default()
    });
    let exact = tool
        .execute(r#"{"pattern":"ignored.txt"}"#, &context(dir.path()))
        .await;
    assert!(exact.content.contains("1 files found"));
    let missing = tool
        .execute(r#"{"pattern":"missing.txt"}"#, &context(dir.path()))
        .await;
    assert!(missing.content.contains("at the exact path"));
    assert!(!missing.content.contains("entry limit"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn metadata_queries_are_bounded_and_cancellable_before_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    for stage in [WaitAt::Metadata, WaitAt::AfterSlowMetadata] {
        let world = Arc::new(WaitingWorld(LocalFs::unfenced(), stage));
        let options = WalkOptions {
            timeout_secs: 1,
            ..Default::default()
        };
        for tool in [
            Box::new(
                GlobTool::with_world(world.clone(), dirs(dir.path())).with_options(options.clone()),
            ) as Box<dyn Tool>,
            Box::new(
                GrepTool::with_world(world.clone(), dirs(dir.path())).with_options(options.clone()),
            ),
        ] {
            for cancel in [false, true] {
                let mut ctx = context(dir.path());
                let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
                let captured = seen.clone();
                ctx.progress = atomcode_kernel::tool::ProgressSink::new(Arc::new(move |msg| {
                    captured.lock().unwrap().push(msg);
                }));
                let trigger = ctx.cancel.clone();
                let action = async {
                    if cancel {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        trigger.cancel();
                    }
                };
                let began = tokio::time::Instant::now();
                let (result, _) = tokio::join!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        tool.execute(r#"{"pattern":"needle.*"}"#, &ctx)
                    ),
                    action
                );
                let result = result.expect("the tool's own deadline/cancel must finish first");
                assert!(result.is_error, "{}", result.content);
                assert!(
                    result
                        .content
                        .contains(if cancel { "cancelled" } else { "timed out" }),
                    "{}",
                    result.content
                );
                assert_eq!(
                    began.elapsed(),
                    if cancel {
                        std::time::Duration::from_millis(10)
                    } else {
                        std::time::Duration::from_secs(1)
                    }
                );
                assert!(
                    !seen.lock().unwrap().is_empty(),
                    "progress must precede metadata lookup"
                );
            }
        }
    }
}
