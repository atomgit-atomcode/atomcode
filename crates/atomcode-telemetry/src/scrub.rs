//! Scrub paths and truncate strings before events leave the process.

use std::path::Path;

pub const HEAD_MAX: usize = 200;

pub fn scrub_path(s: &str, home: Option<&Path>, cwd: Option<&Path>) -> String {
    let mut out = s.to_string();
    if let Some(h) = home.and_then(|p| p.to_str()) {
        if !h.is_empty() {
            out = out.replace(h, "<HOME>");
        }
    }
    if let Some(c) = cwd.and_then(|p| p.to_str()) {
        if !c.is_empty() {
            out = out.replace(c, "<CWD>");
        }
    }
    out
}

pub fn truncate_head(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

pub fn backtrace_top_k(bt: &str, k: usize, home: Option<&Path>, cwd: Option<&Path>) -> Vec<String> {
    bt.lines()
        .take(k)
        .map(|line| {
            let scrubbed = scrub_path(line, home, cwd);
            if let Some(idx) = scrubbed.find(" at ") {
                let (head, rest) = scrubbed.split_at(idx + 4);
                let short = match rest.rsplit_once('/') {
                    Some((_, tail)) => tail.to_string(),
                    None => rest.to_string(),
                };
                format!("{}{}", head, short)
            } else {
                scrubbed
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scrubs_home_path() {
        let home = PathBuf::from("/Users/lichao");
        let s = "panic at /Users/lichao/project/foo.rs:10";
        assert_eq!(
            scrub_path(s, Some(&home), None),
            "panic at <HOME>/project/foo.rs:10"
        );
    }

    #[test]
    fn scrubs_cwd_path() {
        let cwd = PathBuf::from("/tmp/proj");
        let s = "error in /tmp/proj/src/main.rs:3";
        assert_eq!(
            scrub_path(s, None, Some(&cwd)),
            "error in <CWD>/src/main.rs:3"
        );
    }

    #[test]
    fn truncate_head_respects_char_boundary() {
        let s = "中文字串";
        let out = truncate_head(s, 3);
        assert_eq!(out, "中文字");
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn truncate_head_returns_full_when_short() {
        assert_eq!(truncate_head("hi", 200), "hi");
    }

    #[test]
    fn backtrace_strips_abs_paths_keeps_basename() {
        let bt = "\
   0: my_crate::fn_a
      at /Users/lichao/p/src/a.rs:10
   1: my_crate::fn_b
      at /Users/lichao/p/src/b.rs:20
   2: my_crate::fn_c
      at /Users/lichao/p/src/c.rs:30
   3: frame4
   4: frame5
   5: frame6_should_be_dropped";
        let frames = backtrace_top_k(bt, 5, Some(Path::new("/Users/lichao")), None);
        assert_eq!(frames.len(), 5);
        assert!(frames[1].ends_with("a.rs:10"), "got {:?}", frames[1]);
        assert!(!frames[1].contains("/Users/lichao"));
    }
}
