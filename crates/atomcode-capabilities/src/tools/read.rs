//! `read_file` — read a file (or list a directory) with line numbers and optional
//! slicing. Non-destructive ⇒ always `Safe`. Neutral core ported from the production
//! reader, minus the coding enrichments (semantic skeleton, read_cache, file_store).

use super::{err, looks_binary, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;

/// Refuse a full (un-sliced) read above this size; the model must slice instead.
const MAX_FULL_BYTES: u64 = 5 * 1024 * 1024;
/// Per-line display cap (very long minified lines are truncated with a marker).
const MAX_LINE_LEN: usize = 2000;

pub struct ReadFileTool;

#[derive(Deserialize)]
struct Args {
    file_path: String,
    #[serde(default, deserialize_with = "lenient_usize")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "lenient_usize")]
    limit: Option<usize>,
}

/// Deserialize a usize that weak models may send as a float or a string (`50`, `"50"`,
/// `50.0`, `"50.0"`) instead of an integer. Absent / null / empty → `None`.
fn lenient_usize<'de, D>(d: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num {
        U(u64),
        F(f64),
        S(String),
    }
    Ok(match Option::<Num>::deserialize(d)? {
        None => None,
        Some(Num::U(n)) => Some(n as usize),
        Some(Num::F(f)) => Some(f.max(0.0) as usize),
        Some(Num::S(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                let v = t.parse::<usize>().or_else(|_| t.parse::<f64>().map(|f| f.max(0.0) as usize));
                Some(v.map_err(serde::de::Error::custom)?)
            }
        }
    })
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file from the filesystem. Returns the contents prefixed with 1-based \
         line numbers (`<n>\\t<content>`). For a large file, read a slice with `offset` \
         (1-based start line) and `limit` (max lines). If the path is a directory its \
         entries are listed instead. Relative paths resolve against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to read (absolute, or relative to the working directory)" },
                "offset": { "type": "integer", "description": "Start line, 1-based. Omit to read from the beginning." },
                "limit": { "type": "integer", "description": "Max number of lines to read. Omit to read to the end." }
            },
            "required": ["file_path"]
        })
    }
    // read is non-destructive → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "read_file: invalid arguments: {e}. Expected {{\"file_path\": \"<path>\"}}."
                ))
            }
        };
        let path = resolve_path(&a.file_path, &ctx.working_dir);

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                return err(format!(
                    "Error: no such file: {} (resolved to {})",
                    a.file_path,
                    path.display()
                ))
            }
        };

        if meta.is_dir() {
            let mut entries = Vec::new();
            if let Ok(mut rd) = tokio::fs::read_dir(&path).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    let name = e.file_name().to_string_lossy().to_string();
                    entries.push(if is_dir { format!("{name}/") } else { name });
                }
            }
            entries.sort();
            return ok(format!(
                "[NOTE: {} is a directory. Its contents:]\n{}",
                path.display(),
                entries.join("\n")
            ));
        }

        if a.offset.is_none() && a.limit.is_none() && meta.len() > MAX_FULL_BYTES {
            return err(format!(
                "File too large to read in full: {} bytes ({:.1} MB). Read a slice with \
                 offset/limit (e.g. read_file({{\"file_path\":\"{}\",\"offset\":1,\"limit\":200}})), \
                 or use bash (wc -l / sed -n).",
                meta.len(),
                meta.len() as f64 / 1_048_576.0,
                a.file_path
            ));
        }

        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return err(format!("read_file: failed to read {}: {e}", path.display())),
        };
        if looks_binary(&bytes) {
            return ok(format!("Binary file ({} bytes), cannot display as text.", bytes.len()));
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let start = a.offset.unwrap_or(1).max(1); // 1-based
        let start_idx = start - 1;
        if start_idx >= total {
            return ok(format!("[no lines in requested range (start={start}, total={total})]"));
        }
        let end_idx = match a.limit {
            Some(l) => start_idx.saturating_add(l).min(total),
            None => total,
        };

        let mut out = String::new();
        for (i, line) in lines[start_idx..end_idx].iter().enumerate() {
            let n = start + i;
            if line.chars().count() > MAX_LINE_LEN {
                let head: String = line.chars().take(MAX_LINE_LEN).collect();
                out.push_str(&format!("{n:>6}\t{head}... (line truncated to {MAX_LINE_LEN} chars)\n"));
            } else {
                out.push_str(&format!("{n:>6}\t{line}\n"));
            }
        }
        if start > 1 || end_idx < total {
            out.push_str(&format!("[Showing lines {start}-{end_idx} of {total}]"));
        }
        ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext { working_dir: dir.to_path_buf(), cancel: CancellationToken::new() }
    }

    #[tokio::test]
    async fn reads_with_line_numbers() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "first\nsecond\nthird\n").unwrap();
        let r = ReadFileTool.execute(r#"{"file_path":"a.txt"}"#, &ctx(d.path())).await;
        assert!(!r.is_error);
        assert!(r.content.contains("     1\tfirst"), "{}", r.content);
        assert!(r.content.contains("     3\tthird"), "{}", r.content);
    }

    #[tokio::test]
    async fn offset_and_limit_slice() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let r = ReadFileTool.execute(r#"{"file_path":"a.txt","offset":2,"limit":2}"#, &ctx(d.path())).await;
        assert!(r.content.contains("     2\tl2"), "{}", r.content);
        assert!(r.content.contains("     3\tl3"), "{}", r.content);
        assert!(!r.content.contains("\tl1"), "{}", r.content);
        assert!(!r.content.contains("\tl4"), "{}", r.content);
        assert!(r.content.contains("[Showing lines 2-3 of 5]"), "{}", r.content);
    }

    #[tokio::test]
    async fn binary_file_is_reported() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        let r = ReadFileTool.execute(r#"{"file_path":"b.bin"}"#, &ctx(d.path())).await;
        assert!(!r.is_error);
        assert!(r.content.starts_with("Binary file"), "{}", r.content);
    }

    #[tokio::test]
    async fn directory_lists_contents() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.txt"), "hi").unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let r = ReadFileTool.execute(r#"{"file_path":"."}"#, &ctx(d.path())).await;
        assert!(r.content.contains("is a directory"), "{}", r.content);
        assert!(r.content.contains("sub/"), "{}", r.content);
        assert!(r.content.contains("x.txt"), "{}", r.content);
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = ReadFileTool.execute(r#"{"file_path":"nope.txt"}"#, &ctx(d.path())).await;
        assert!(r.is_error);
        assert!(r.content.contains("no such file"), "{}", r.content);
    }

    #[tokio::test]
    async fn lenient_offset_limit_accepts_float_strings() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "l1\nl2\nl3\nl4\n").unwrap();
        // Weak models send "2.0" / "2.0" instead of integers.
        let r = ReadFileTool
            .execute(r#"{"file_path":"a.txt","offset":"2.0","limit":"2.0"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("     2\tl2"), "{}", r.content);
        assert!(r.content.contains("     3\tl3"), "{}", r.content);
        assert!(!r.content.contains("\tl4"), "{}", r.content);
    }

    #[tokio::test]
    async fn long_line_is_truncated() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("long.txt"), "x".repeat(5000)).unwrap();
        let r = ReadFileTool.execute(r#"{"file_path":"long.txt"}"#, &ctx(d.path())).await;
        assert!(r.content.contains("line truncated to 2000 chars"), "{}", r.content);
    }
}
