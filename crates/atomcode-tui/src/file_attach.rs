use std::path::Path;
use std::process::Command;

/// A file attached to the next message (shown as tag above input).
#[derive(Debug, Clone)]
pub struct AttachedFile {
    pub path: String,
    pub filename: String,
    pub file_type: String,
    pub size_bytes: u64,
}

/// Result of reading/extracting a file's content.
pub struct FileContent {
    pub filename: String,
    pub file_type: String,
    pub content: String,
    pub size_bytes: u64,
}

/// Does the string start with a Windows drive-letter prefix like `C:\` or `D:/`?
fn has_windows_drive_prefix(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(c), Some(':'), Some('\\' | '/')) if c.is_ascii_alphabetic()
    )
}

/// Detect if a string looks like a file path that exists.
pub fn detect_file_path(input: &str, working_dir: &Path) -> Option<AttachedFile> {
    // Strip surrounding quotes — drag-and-drop from Windows Explorer wraps paths
    // with spaces in double quotes (e.g. `"C:\My Docs\file.txt"`).
    let trimmed = input.trim().trim_matches(|c: char| c == '"' || c == '\'');
    if trimmed.is_empty() || trimmed.len() < 2 {
        return None;
    }

    // Must look like a path: Unix-style prefix OR a Windows drive letter.
    let looks_like_path = trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with("~/")
        || has_windows_drive_prefix(trimmed);

    if !looks_like_path {
        return None;
    }

    // Resolve the path
    let resolved = if trimmed.starts_with('~') {
        dirs::home_dir()
            .map(|h| h.join(trimmed.strip_prefix("~/").unwrap_or(&trimmed[1..])))
            .unwrap_or_else(|| std::path::PathBuf::from(trimmed))
    } else if trimmed.starts_with('/') || has_windows_drive_prefix(trimmed) {
        std::path::PathBuf::from(trimmed)
    } else {
        working_dir.join(trimmed)
    };

    if !resolved.exists() || resolved.is_dir() {
        return None;
    }

    let filename = resolved.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    let ext = resolved.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);

    let file_type = match ext.as_str() {
        "pdf" => "PDF",
        "xlsx" | "xls" => "Excel",
        "pptx" | "ppt" => "PPT",
        "docx" | "doc" => "Word",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => "Image",
        "zip" | "tar" | "gz" | "tgz" => "Archive",
        "rs" => "Rust",
        "py" => "Python",
        "js" | "jsx" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "go" => "Go",
        "java" => "Java",
        "html" => "HTML",
        "css" | "scss" => "CSS",
        "json" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "md" => "Markdown",
        "sql" => "SQL",
        "sh" | "bash" => "Shell",
        "vue" => "Vue",
        "csv" => "CSV",
        "xml" => "XML",
        "txt" | "log" => "Text",
        "" => "File",
        other => return Some(AttachedFile {
            path: resolved.to_string_lossy().to_string(),
            filename,
            file_type: other.to_uppercase(),
            size_bytes: size,
        }),
    }.to_string();

    Some(AttachedFile {
        path: resolved.to_string_lossy().to_string(),
        filename,
        file_type,
        size_bytes: size,
    })
}

/// Split a pasted block into (detected file attachments, remaining non-path text).
///
/// Drag-and-drop from Explorer / Finder delivers multiple file paths in a single
/// paste event — usually either newline-separated or space-separated with quoted
/// paths for any that contain spaces. Without this, the whole block goes into
/// `pasted_blocks` as opaque text and the user sees one generic "Pasted text"
/// tag instead of N individual file attachments.
///
/// Splits first on newlines, then on whitespace while respecting single/double
/// quotes. Each token is fed through `detect_file_path`. Tokens that resolve to
/// a real existing file become `AttachedFile`s; everything else is returned in
/// the leftover string (with original line breaks preserved) so the caller can
/// stash it as a plain paste block.
pub fn extract_file_paths(input: &str, working_dir: &Path) -> (Vec<AttachedFile>, String) {
    let mut attached: Vec<AttachedFile> = Vec::new();
    let mut remainder_lines: Vec<String> = Vec::new();

    for raw_line in input.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.trim().is_empty() {
            remainder_lines.push(String::new());
            continue;
        }

        let tokens = split_respecting_quotes(line);
        let mut non_path_tokens: Vec<String> = Vec::new();
        for tok in tokens {
            if let Some(file) = detect_file_path(&tok, working_dir) {
                // Dedup against already-extracted paths in this call.
                if !attached.iter().any(|f| f.path == file.path) {
                    attached.push(file);
                }
            } else {
                non_path_tokens.push(tok);
            }
        }

        if non_path_tokens.is_empty() {
            // Entire line was paths — don't preserve the blank row in remainder.
        } else {
            remainder_lines.push(non_path_tokens.join(" "));
        }
    }

    // Trim trailing empty remainder lines caused by all-path input.
    while remainder_lines.last().map_or(false, |s| s.is_empty()) {
        remainder_lines.pop();
    }

    (attached, remainder_lines.join("\n"))
}

/// Split a string on whitespace, but keep quoted (`"..."` or `'...'`) tokens
/// together and strip their quotes. Handles Windows drag-drop output where
/// paths containing spaces are wrapped in double quotes.
fn split_respecting_quotes(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), ch) if ch == q => {
                // Closing quote — finish the current token.
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
                quote = None;
            }
            (Some(_), ch) => buf.push(ch),
            (None, '"') | (None, '\'') => {
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
                quote = Some(c);
            }
            (None, ch) if ch.is_whitespace() => {
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
            }
            (None, ch) => buf.push(ch),
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Read a file and extract its text content.
/// Supports text files directly, and uses CLI tools for binary formats.
pub fn extract_file(path: &Path, working_dir: &Path) -> Result<FileContent, String> {
    // Resolve relative paths
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    };

    if !resolved.exists() {
        return Err(format!("File not found: {}", resolved.display()));
    }

    let metadata = std::fs::metadata(&resolved)
        .map_err(|e| format!("Cannot read file: {}", e))?;

    if metadata.is_dir() {
        return Err(format!("{} is a directory, not a file", resolved.display()));
    }

    let size = metadata.len();
    let filename = resolved.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let ext = resolved.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let (file_type, content) = match ext.as_str() {
        // Text files — read directly
        "txt" | "md" | "rs" | "py" | "js" | "ts" | "tsx" | "jsx"
        | "go" | "java" | "c" | "cpp" | "h" | "hpp" | "cs"
        | "rb" | "php" | "swift" | "kt" | "scala"
        | "html" | "css" | "scss" | "less" | "sass"
        | "json" | "yaml" | "yml" | "toml" | "xml" | "csv"
        | "sh" | "bash" | "zsh" | "fish"
        | "sql" | "graphql" | "proto"
        | "dockerfile" | "makefile" | "cmake"
        | "gitignore" | "env" | "cfg" | "ini" | "conf"
        | "vue" | "svelte" | "astro"
        | "r" | "jl" | "lua" | "zig" | "nim" | "dart" | "ex" | "exs"
        | "log" | "diff" | "patch" => {
            let text = read_text(&resolved)?;
            (format!("text ({})", ext), text)
        }

        // No extension — try as text
        "" => {
            match read_text(&resolved) {
                Ok(text) => ("text".to_string(), text),
                Err(_) => ("binary".to_string(), format!("[Binary file, {} bytes]", size)),
            }
        }

        // PDF — extract with pdftotext
        "pdf" => {
            let text = run_extractor("pdftotext", &[resolved.to_str().unwrap_or(""), "-"])
                .or_else(|_| run_extractor("python3", &["-c", &format!(
                    "import subprocess; subprocess.run(['pdftotext', '{}', '-'], check=True)",
                    resolved.display()
                )]))
                .unwrap_or_else(|_| format!("[PDF file, {} bytes. Install pdftotext for text extraction]", size));
            ("pdf".to_string(), text)
        }

        // Excel — extract with ssconvert or python
        "xlsx" | "xls" => {
            let text = run_extractor("python3", &["-c", &format!(
                "import csv, io, sys\ntry:\n    import openpyxl\n    wb = openpyxl.load_workbook('{}')\n    for ws in wb:\n        print(f'## Sheet: {{ws.title}}')\n        for row in ws.iter_rows(values_only=True):\n            print('\\t'.join(str(c) if c is not None else '' for c in row))\n        print()\nexcept ImportError:\n    print('[Install openpyxl: pip3 install openpyxl]')",
                resolved.display()
            )])
            .unwrap_or_else(|_| format!("[Excel file, {} bytes. Install openpyxl for extraction]", size));
            ("excel".to_string(), text)
        }

        // PowerPoint — extract text
        "pptx" | "ppt" => {
            let text = run_extractor("python3", &["-c", &format!(
                "try:\n    from pptx import Presentation\n    prs = Presentation('{}')\n    for i, slide in enumerate(prs.slides, 1):\n        print(f'## Slide {{i}}')\n        for shape in slide.shapes:\n            if hasattr(shape, 'text') and shape.text:\n                print(shape.text)\n        print()\nexcept ImportError:\n    print('[Install python-pptx: pip3 install python-pptx]')",
                resolved.display()
            )])
            .unwrap_or_else(|_| format!("[PowerPoint file, {} bytes. Install python-pptx for extraction]", size));
            ("powerpoint".to_string(), text)
        }

        // Word documents
        "docx" | "doc" => {
            let text = run_extractor("python3", &["-c", &format!(
                "try:\n    import docx\n    doc = docx.Document('{}')\n    for p in doc.paragraphs:\n        if p.text:\n            print(p.text)\nexcept ImportError:\n    print('[Install python-docx: pip3 install python-docx]')",
                resolved.display()
            )])
            .unwrap_or_else(|_| format!("[Word file, {} bytes. Install python-docx for extraction]", size));
            ("word".to_string(), text)
        }

        // Images — describe (base64 vision support would need API changes)
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" => {
            let desc = format!(
                "[Image file: {}, {} bytes]\nNote: Image preview not supported in terminal. \
                Use read_file to access raw content if needed.",
                filename, size
            );
            ("image".to_string(), desc)
        }

        // Archives
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "7z" | "rar" => {
            let listing = run_extractor("tar", &["-tf", resolved.to_str().unwrap_or("")])
                .or_else(|_| run_extractor("unzip", &["-l", resolved.to_str().unwrap_or("")]))
                .unwrap_or_else(|_| format!("[Archive file, {} bytes]", size));
            ("archive".to_string(), listing)
        }

        // Unknown — try as text, fallback to binary description
        _ => {
            match read_text(&resolved) {
                Ok(text) => (format!("text ({})", ext), text),
                Err(_) => (ext.clone(), format!("[Binary file .{}, {} bytes]", ext, size)),
            }
        }
    };

    // Truncate very large content
    let max_chars = 50_000;
    let content = if content.chars().count() > max_chars {
        let truncated: String = content.chars().take(max_chars).collect();
        format!("{}\n\n[... truncated, showing first {} chars of {} total]",
            truncated, max_chars, content.chars().count())
    } else {
        content
    };

    Ok(FileContent {
        filename,
        file_type,
        content,
        size_bytes: size,
    })
}

fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    // Check if content is valid UTF-8
    String::from_utf8(bytes).map_err(|_| "Not a text file".to_string())
}

fn run_extractor(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{} not found: {}", cmd, e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn quote_split_handles_windows_drag_drop_style() {
        let tokens = split_respecting_quotes(
            r#""C:\My Docs\a.txt" "C:\Users\x\b.log" C:\simple\c.rs"#
        );
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], r"C:\My Docs\a.txt");
        assert_eq!(tokens[1], r"C:\Users\x\b.log");
        assert_eq!(tokens[2], r"C:\simple\c.rs");
    }

    #[test]
    fn windows_drive_prefix_detected() {
        assert!(has_windows_drive_prefix(r"C:\foo"));
        assert!(has_windows_drive_prefix("D:/foo"));
        assert!(!has_windows_drive_prefix("C:foo"));
        assert!(!has_windows_drive_prefix("foo"));
        assert!(!has_windows_drive_prefix("1:\\foo"));
    }

    #[test]
    fn extract_multiple_paths_space_separated() {
        // Create 3 temp files and paste their paths together.
        let tmp = std::env::temp_dir();
        let paths: Vec<std::path::PathBuf> = (0..3)
            .map(|i| {
                let p = tmp.join(format!("atomcode_paste_test_{}.txt", i));
                fs::write(&p, "x").unwrap();
                p
            })
            .collect();

        let mut pasted = String::new();
        for p in &paths {
            pasted.push('"');
            pasted.push_str(&p.to_string_lossy());
            pasted.push_str("\" ");
        }

        let (attached, remainder) = extract_file_paths(&pasted, &tmp);
        assert_eq!(attached.len(), 3);
        assert!(remainder.is_empty());

        for p in &paths { let _ = fs::remove_file(p); }
    }

    #[test]
    fn extract_multiple_paths_newline_separated() {
        let tmp = std::env::temp_dir();
        let p1 = tmp.join("atomcode_paste_nl_1.txt");
        let p2 = tmp.join("atomcode_paste_nl_2.txt");
        fs::write(&p1, "x").unwrap();
        fs::write(&p2, "y").unwrap();

        let pasted = format!("{}\n{}\n", p1.display(), p2.display());
        let (attached, remainder) = extract_file_paths(&pasted, &tmp);
        assert_eq!(attached.len(), 2);
        assert!(remainder.is_empty());

        let _ = fs::remove_file(&p1);
        let _ = fs::remove_file(&p2);
    }

    #[test]
    fn extract_mixes_paths_and_text() {
        let tmp = std::env::temp_dir();
        let p = tmp.join("atomcode_paste_mixed.txt");
        fs::write(&p, "x").unwrap();

        let pasted = format!("please read {} and summarize", p.display());
        let (attached, remainder) = extract_file_paths(&pasted, &tmp);
        assert_eq!(attached.len(), 1);
        assert_eq!(remainder, "please read and summarize");

        let _ = fs::remove_file(&p);
    }
}
