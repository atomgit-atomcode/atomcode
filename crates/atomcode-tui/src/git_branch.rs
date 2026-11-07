//! Read local symbolic HEAD without starting a process in the render loop.
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) fn current(cwd: &str) -> Option<String> {
    static CACHE: Mutex<Option<(String, Instant, Option<String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((path, at, branch)) = cache.as_ref() {
        if path == cwd && at.elapsed() < Duration::from_secs(1) {
            return branch.clone();
        }
    }
    let expanded = crate::text::expand_home_with(cwd, crate::text::home_dir().as_deref());
    let branch = read(Path::new(&expanded));
    *cache = Some((cwd.into(), Instant::now(), branch.clone()));
    branch
}

fn read(cwd: &Path) -> Option<String> {
    atomcode_capabilities::git_branch::read(cwd)
}
