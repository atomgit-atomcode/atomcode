//! Real Windows filesystem/process checks; no user workspace or code-page mutation.
use super::*;
use crate::world::{Shell, SpawnOptions};
use std::path::{Path, PathBuf};

fn workspace(root: &Path) -> PathBuf {
    let cwd = root.join("2026增肌").join("工作区 wb06");
    std::fs::create_dir_all(&cwd).unwrap();
    cwd
}

fn assert_exact_tree(root: &Path, cwd: &Path) {
    assert!(cwd.join("_atomcode_archive/probe.txt").is_file());
    let names = |dir: &Path| {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(root), vec![std::ffi::OsString::from("2026增肌")]);
    assert_eq!(
        names(&root.join("2026增肌")),
        vec![std::ffi::OsString::from("工作区 wb06")]
    );
}

async fn finish(process: Arc<dyn crate::world::Process>) {
    let mut output = Vec::new();
    while let Some(chunk) = process.next_chunk().await {
        output.extend_from_slice(chunk.bytes());
    }
    assert!(
        process.wait().await.unwrap().success(),
        "{}",
        decode_output(&output)
    );
}

#[test]
fn native_windows_filesystem_keeps_chinese_path_components() {
    let root = tempfile::tempdir().unwrap();
    let cwd = workspace(root.path());
    std::fs::create_dir(cwd.join("_atomcode_archive")).unwrap();
    std::fs::write(cwd.join("_atomcode_archive/probe.txt"), "ok").unwrap();
    assert_exact_tree(root.path(), &cwd);
}

#[tokio::test(flavor = "current_thread")]
async fn cmd_wide_arguments_create_the_exact_absolute_chinese_path() {
    let root = tempfile::tempdir().unwrap();
    let cwd = workspace(root.path());
    let archive = cwd.join("_atomcode_archive");
    let script = format!(
        "mkdir \"{}\" && echo ok > \"{}\"",
        archive.display(),
        archive.join("probe.txt").display()
    );
    // Force the same cmd builder as the production fallback, even on machines
    // with Git Bash installed. Raw Unicode arguments must not be byte-redecoded.
    let process = spawn_local_command(
        build_cmd_command(&script),
        &SpawnOptions {
            cwd: Some(cwd.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    finish(process).await;
    assert_exact_tree(root.path(), &cwd);
}

#[tokio::test(flavor = "current_thread")]
async fn selected_windows_shell_keeps_chinese_cwd_for_relative_paths() {
    let root = tempfile::tempdir().unwrap();
    let cwd = workspace(root.path());
    let marker = format!(".encoding-cwd-{}", std::process::id());
    std::fs::write(cwd.join(&marker), "guard").unwrap();
    let work = "mkdir _atomcode_archive && echo ok > \"_atomcode_archive/probe.txt\"";
    let script = if detect_windows_bash().is_some() {
        format!("test -f {marker} && {work}")
    } else {
        // cmd can silently ignore UNC cwd. Refuse to write if it did so.
        format!("if exist {marker} ({work}) else (exit /b 97)")
    };
    let process = LocalShell
        .spawn(
            &script,
            &SpawnOptions {
                cwd: Some(cwd.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    finish(process).await;
    assert_exact_tree(root.path(), &cwd);
}

#[tokio::test(flavor = "current_thread")]
async fn missing_chinese_cwd_is_rejected_without_recreating_a_ghost_tree() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("不存在的工作区");
    let result = spawn_local_command(
        build_cmd_command("exit 0"),
        &SpawnOptions {
            cwd: Some(cwd.clone()),
            ..Default::default()
        },
    )
    .await;
    assert!(result.is_err());
    assert!(!cwd.exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
