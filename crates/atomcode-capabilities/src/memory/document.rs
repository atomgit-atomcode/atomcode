//! Lossless surrounding Markdown and one serialized transaction for all writers.
use super::entry::{invalid, parse_line, MemoryEntry, MARKER};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

pub(crate) struct Line {
    pub raw: String,
    pub entry: Option<MemoryEntry>,
}
pub(crate) struct Document {
    pub lines: Vec<Line>,
}
impl Document {
    pub fn parse(text: &str) -> io::Result<Self> {
        let mut lines = Vec::new();
        let mut ids = std::collections::HashMap::<String, bool>::new();
        for raw in text.split_inclusive('\n') {
            let entry = parse_line(raw)?;
            if let Some(entry) = &entry {
                let managed = raw.contains(MARKER);
                if let Some(previous_managed) = ids.insert(entry.metadata.id.clone(), managed) {
                    if managed || previous_managed {
                        return Err(invalid("duplicate persisted memory ID"));
                    }
                }
            }
            lines.push(Line {
                raw: raw.into(),
                entry,
            });
        }
        Ok(Self { lines })
    }
    pub fn entries(&self) -> Vec<MemoryEntry> {
        self.lines
            .iter()
            .filter_map(|line| line.entry.clone())
            .collect()
    }
    pub fn text(&self) -> String {
        self.lines.iter().map(|line| line.raw.as_str()).collect()
    }
    pub fn push(&mut self, entry: MemoryEntry) -> io::Result<()> {
        if let Some(last) = self.lines.last_mut() {
            if !last.raw.ends_with('\n') {
                last.raw.push('\n');
            }
        }
        let raw = format!("{}\n", entry.render()?);
        self.lines.push(Line {
            raw,
            entry: Some(entry),
        });
        Ok(())
    }
    pub fn replace(&mut self, index: usize, entry: MemoryEntry) -> io::Result<()> {
        let raw = &self.lines[index].raw;
        let ending = if raw.ends_with("\r\n") {
            "\r\n"
        } else if raw.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        self.lines[index] = Line {
            raw: format!("{}{ending}", entry.render()?),
            entry: Some(entry),
        };
        Ok(())
    }
}
const MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;

pub(super) fn read(path: &Path) -> io::Result<String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("memory source is not a regular file"));
    }
    let too_large = || {
        io::Error::new(io::ErrorKind::InvalidData,
        "memory file exceeds the 1 MiB review/edit limit; reduce it with an external editor before retrying")
    };
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    // The file may grow after metadata was read. Never read an unbounded stream.
    file.take(MAX_DOCUMENT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(too_large());
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
pub(crate) fn edit<R>(
    path: &Path,
    change: impl FnOnce(&mut Document) -> io::Result<R>,
) -> io::Result<R> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    // Resolve aliases to the same lock; preserve symlinks instead of replacing
    // the link itself with a new file.
    let target = match fs::symlink_metadata(path) {
        Ok(_) => fs::canonicalize(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::canonicalize(parent)?.join(
            path.file_name()
                .ok_or_else(|| invalid("memory file has no name"))?,
        ),
        Err(e) => return Err(e),
    };
    let parent = target
        .parent()
        .ok_or_else(|| invalid("memory file has no parent"))?;
    let locks = parent.join(".memory-locks");
    fs::create_dir_all(&locks)?;
    let sentinel = locks.join(".gitignore");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sentinel)
    {
        Ok(mut file) => file.write_all(b"*\n")?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let lock_path = locks.join(format!(
        "{}.lock",
        target.file_name().unwrap().to_string_lossy()
    ));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let before = read(&target)?;
    let mut document = Document::parse(&before)?;
    let result = change(&mut document)?;
    let after = document.text();
    if after.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory edit would exceed the 1 MiB review/edit limit; reduce the file before retrying",
        ));
    }
    if after == before {
        return Ok(result);
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    match fs::metadata(&target) {
        Ok(meta) => temp.as_file().set_permissions(meta.permissions())?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    temp.write_all(after.as_bytes())?;
    temp.as_file().sync_all()?;
    if read(&target)? != before {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "memory file changed outside the managed writer; retry after reviewing it",
        ));
    }
    temp.persist(&target).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent).and_then(|directory| directory.sync_all()).map_err(|e| io::Error::new(e.kind(), format!("memory replacement committed but directory sync failed (uncertain durability): {e}")))?;
    Ok(result)
}
