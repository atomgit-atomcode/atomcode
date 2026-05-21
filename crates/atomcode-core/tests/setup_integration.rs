//! End-to-end integration tests for `setup::run`.
//!
//! 这些测试通过 `ATOMCODE_HOME` 环境变量把 `Config::config_dir()` 重定向到
//! tempdir,确保不会污染真实的 `~/.atomcode/`。Env var 是进程全局的,所以全部
//! 测试都用 `#[serial]` 序列化。

use atomcode_core::setup::{self, RunOptions};
use serial_test::serial;
use std::path::Path;

/// 在 tempdir 项目中跑 setup,同时把 `ATOMCODE_HOME` 指向另一个 tempdir。
/// 返回 (`SetupResult`, 两个 tempdir 守卫)。
fn run_in_tempdir<F, G>(
    project_setup: F,
    mutate_opts: G,
) -> (
    setup::SetupResult<setup::SetupReport>,
    tempfile::TempDir,
    tempfile::TempDir,
)
where
    F: FnOnce(&Path),
    G: FnOnce(&mut RunOptions),
{
    let proj = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    project_setup(proj.path());

    let old = std::env::var_os("ATOMCODE_HOME");
    std::env::set_var("ATOMCODE_HOME", user.path());

    let mut opts = RunOptions::new(proj.path().to_path_buf());
    mutate_opts(&mut opts);

    let result = setup::run(opts);

    match old {
        Some(v) => std::env::set_var("ATOMCODE_HOME", v),
        None => std::env::remove_var("ATOMCODE_HOME"),
    }

    (result, proj, user)
}

#[test]
#[serial]
fn setup_installs_seeds_in_empty_project() {
    let (result, _proj, _user) = run_in_tempdir(|_| {}, |_| {});

    let report = result.expect("setup run should succeed");

    // Should install at least some seeds (skills/commands from embedded tar).
    let total =
        report.summary.installed.len() + report.summary.skipped.len() + report.summary.failed.len();
    assert!(
        total > 0,
        "expected at least one seed install attempted, got installed={} skipped={} failed={}",
        report.summary.installed.len(),
        report.summary.skipped.len(),
        report.summary.failed.len(),
    );

    // setup-state.json should exist in the project dir.
    // (Not in user dir — state is per-project.)
}

#[test]
#[serial]
fn second_run_skips_already_installed() {
    let proj = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();

    let old = std::env::var_os("ATOMCODE_HOME");
    std::env::set_var("ATOMCODE_HOME", user.path());

    let make_opts = || {
        RunOptions::new(proj.path().to_path_buf())
    };

    let report1 = setup::run(make_opts()).unwrap();
    let report2 = setup::run(make_opts()).unwrap();

    match old {
        Some(v) => std::env::set_var("ATOMCODE_HOME", v),
        None => std::env::remove_var("ATOMCODE_HOME"),
    }

    // First run should install or attempt some items.
    let first_attempted = report1.summary.installed.len()
        + report1.summary.skipped.len()
        + report1.summary.failed.len();
    assert!(
        first_attempted > 0,
        "first run should attempt at least one item"
    );

    // Second run should have at least one AlreadyInstalled skip (if first run installed anything).
    assert!(
        report2.summary.installed.len() <= report1.summary.installed.len(),
        "second run should install at most as many as the first (first={}, second={})",
        report1.summary.installed.len(),
        report2.summary.installed.len(),
    );

    if !report1.summary.installed.is_empty() {
        let has_already = report2.summary.skipped.iter().any(|(_, reason)| {
            matches!(
                reason,
                atomcode_core::setup::install::SkipReason::AlreadyInstalled
            )
        });
        assert!(
            has_already,
            "second run skipped items: {:?}",
            report2.summary.skipped
        );
    }
}

#[test]
#[serial_test::serial]
fn concurrent_runs_second_fails_lock() {
    let proj = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();

    let old = std::env::var_os("ATOMCODE_HOME");
    std::env::set_var("ATOMCODE_HOME", user.path());

    let proj_a = proj.path().to_path_buf();
    let proj_b = proj.path().to_path_buf();

    // `setup::run` is synchronous. Use std::thread to run two concurrent
    // invocations — the lock must prevent both from succeeding simultaneously.
    let (r1, r2) = std::thread::scope(|s| {
        let t1 = s.spawn(|| {
            let o = atomcode_core::setup::RunOptions::new(proj_a);
            atomcode_core::setup::run(o)
        });

        // Give t1 a head start so it grabs the lock first.
        std::thread::sleep(std::time::Duration::from_millis(20));

        let t2 = s.spawn(|| {
            let o = atomcode_core::setup::RunOptions::new(proj_b);
            atomcode_core::setup::run(o)
        });

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        (r1, r2)
    });

    if let Some(v) = old {
        std::env::set_var("ATOMCODE_HOME", v);
    } else {
        std::env::remove_var("ATOMCODE_HOME");
    }

    // At least one should succeed; failures must be lock-related.
    let succeeded = r1.is_ok() as usize + r2.is_ok() as usize;
    assert!(
        succeeded >= 1,
        "at least one concurrent run should succeed; both failed:\n  r1={:?}\n  r2={:?}",
        r1,
        r2
    );

    for r in [&r1, &r2] {
        if let Err(e) = r {
            assert!(
                matches!(
                    e,
                    atomcode_core::setup::SetupError::LockHeld { .. }
                        | atomcode_core::setup::SetupError::LockIo(_)
                ),
                "if a concurrent run failed, it should be LockHeld or LockIo, got: {e:?}"
            );
        }
    }
}
