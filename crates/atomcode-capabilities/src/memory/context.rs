//! Derived prompt selection and diagnostics. Disk is a preview, never a claim
//! about what an already-running session has received.
use super::entry::{parse_line, MemoryEntry, MemoryStatus};
use super::MemoryStore;
use atomcode_i18n::product::{t, Msg};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const CHAR_BUDGET: usize = 4000;
const FILE_BYTES: u64 = 64 * 1024;
const ENTRY_CHARS: usize = 500;
const INDEX_CHARS: usize = 1000;
pub const WARNING_PREFIX: &str = "Memory warning: ";

#[derive(Debug)]
pub struct EntrySelection {
    pub entry: MemoryEntry,
    pub injected: Option<String>,
}
#[derive(Debug)]
pub struct ScopeSelection {
    pub scope: &'static str,
    pub path: PathBuf,
    pub entries: Vec<EntrySelection>,
    pub limited: bool,
    pub error: Option<String>,
}
#[derive(Debug)]
pub struct KnowledgeIndex {
    pub path: PathBuf,
    pub injected: String,
    pub limited: bool,
    pub error: Option<String>,
}
#[derive(Debug)]
pub struct MemoryContext {
    pub prompt: String,
    pub scopes: Vec<ScopeSelection>,
    pub knowledge: KnowledgeIndex,
    pub warning: Option<String>,
    pub evaluated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Default)]
struct Source {
    text: String,
    limited: bool,
    error: Option<String>,
}
pub(super) fn read_tail(path: &Path) -> std::io::Result<String> {
    let source = read_source(path, true);
    match source.error {
        Some(error) => Err(std::io::Error::other(error)),
        None => Ok(source.text),
    }
}

fn read_source(path: &Path, tail: bool) -> Source {
    let read = || -> std::io::Result<Source> {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Source::default()),
            Err(e) => return Err(e),
        };
        if !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Source::default()),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let limited = meta.len() > FILE_BYTES;
        if limited && tail {
            file.seek(SeekFrom::Start(meta.len() - FILE_BYTES))?;
        }
        let mut bytes = Vec::new();
        file.take(FILE_BYTES).read_to_end(&mut bytes)?;
        if limited && tail {
            // Drop the incomplete first line (also avoids splitting UTF-8).
            let start = bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |n| n + 1);
            bytes.drain(..start);
        }
        if limited && !tail {
            if let Err(error) = std::str::from_utf8(&bytes) {
                if error.error_len().is_none() {
                    bytes.truncate(error.valid_up_to());
                }
            }
        }
        let text = String::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Source {
            text,
            limited,
            error: None,
        })
    };
    read().unwrap_or_else(|e| Source {
        error: Some(e.to_string()),
        ..Source::default()
    })
}

/// Scan oversized files without retaining their full contents. This prevents
/// an old pinned fact from disappearing merely because it precedes the tail.
fn older_pinned(
    path: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> std::io::Result<Vec<MemoryEntry>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut kept = std::collections::VecDeque::new();
    let mut bytes = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.by_ref().take(FILE_BYTES + 1).read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if read as u64 > FILE_BYTES {
            return Err(super::entry::invalid(
                "memory line exceeds 64 KiB; split it before enabling metadata selection",
            ));
        }
        if let Some(entry) = parse_line(&line)? {
            if entry.metadata.pinned && entry.eligible_at(now) {
                bytes += read;
                kept.push_back((entry, read));
                while bytes as u64 > FILE_BYTES {
                    if let Some((_, n)) = kept.pop_front() {
                        bytes -= n;
                    }
                }
            }
        }
    }
    Ok(kept.into_iter().map(|(entry, _)| entry).collect())
}
fn select(
    scopes: &mut [ScopeSelection],
    budget: &mut usize,
    pinned: bool,
    now: chrono::DateTime<chrono::Utc>,
) {
    for scope in scopes.iter_mut().rev() {
        for entry in
            scope.entries.iter_mut().rev().filter(|entry| {
                entry.entry.eligible_at(now) && entry.entry.metadata.pinned == pinned
            })
        {
            let text = &entry.entry.content;
            let capped = if text.chars().count() > ENTRY_CHARS {
                format!("{} …", text.chars().take(ENTRY_CHARS).collect::<String>())
            } else {
                text.clone()
            };
            let cost = capped.chars().count() + 3;
            if cost <= *budget {
                *budget -= cost;
                entry.injected = Some(capped);
            }
        }
    }
}

struct RouteTable<'a> {
    introduction: &'a str,
    header: [&'a str; 2],
    rows: Vec<&'a str>,
}

fn route_cells(line: &str) -> Option<[&str; 4]> {
    let mut cells = line.strip_prefix('|')?.strip_suffix('|')?.split('|');
    let cells4 = [
        cells.next()?.trim(),
        cells.next()?.trim(),
        cells.next()?.trim(),
        cells.next()?.trim(),
    ];
    (cells.next().is_none() && cells4.iter().all(|cell| !cell.is_empty())).then_some(cells4)
}

impl<'a> RouteTable<'a> {
    fn parse(text: &'a str, truncated_row: bool) -> Option<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.trim_end().split_inclusive('\n');
        let mut start = 0;
        let heading = loop {
            let line = lines.next()?;
            let trimmed = line.trim();
            if trimmed.starts_with('|') {
                break trimmed;
            }
            // Keep examples inside fenced code on the plain-text path.
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                return None;
            }
            start += line.len();
        };
        if route_cells(heading)? != ["主题", "适用范围", "关联理由", "文件"] {
            return None;
        }
        let delimiter = lines.next()?.trim();
        if !route_cells(delimiter)?.iter().all(|cell| {
            let dashes = cell.strip_prefix(':').unwrap_or(cell);
            let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
            dashes.len() >= 3 && dashes.bytes().all(|byte| byte == b'-')
        }) {
            return None;
        }
        let rows: Vec<_> = lines
            .map(str::trim)
            .map(|line| route_cells(line).map(|_| line))
            .collect::<Option<_>>()?;
        (!rows.is_empty() || truncated_row).then_some(Self {
            introduction: text[..start].trim(),
            header: [heading, delimiter],
            rows,
        })
    }

    fn render(&self, budget: usize) -> (String, bool) {
        let mut table = self.header.join("\n");
        let mut remaining = budget.saturating_sub(table.chars().count());
        let mut limited = false;
        let mut has_row = false;
        for row in &self.rows {
            let cost = row.chars().count() + 1;
            if cost <= remaining {
                remaining -= cost;
                table.push('\n');
                table.push_str(row);
                has_row = true;
            } else {
                limited = true;
            }
        }
        if !has_row {
            return (String::new(), true);
        }
        let mut intro = String::new();
        remaining = remaining.min(INDEX_CHARS / 2);
        for line in self.introduction.lines() {
            let cost = line.chars().count() + 1;
            if cost >= remaining {
                limited = true;
                break;
            }
            remaining -= cost;
            intro.push_str(line);
            intro.push('\n');
        }
        if !intro.is_empty() {
            intro.push('\n');
        }
        intro.push_str(&table);
        (intro, limited)
    }
}

fn select_index(path: &Path, source: Source, budget: &mut usize) -> KnowledgeIndex {
    let mut knowledge = KnowledgeIndex {
        path: path.into(),
        injected: String::new(),
        limited: source.limited,
        error: source.error,
    };
    let text = source.text.trim();
    if text.is_empty() {
        return knowledge;
    }
    // A capped read may end midway through a row, even on a valid UTF-8 boundary.
    let (complete_lines, truncated_row) = if source.limited && !source.text.ends_with('\n') {
        source
            .text
            .rsplit_once('\n')
            .map_or(("", false), |(lines, tail)| {
                (lines, tail.trim_start().starts_with('|'))
            })
    } else {
        (source.text.as_str(), false)
    };
    let block = if let Some(routes) = RouteTable::parse(complete_lines, truncated_row) {
        let partial =
            "\n[Partial index: read the complete index above for omitted routes or notes.]";
        let prefix = format!(
            "\n[Knowledge index: {}]\nL1 routes: match the task to each route's applicability. Read the linked L2 summary, then its L3 evidence when needed using file/search tools.\n",
            path.display()
        );
        let available = budget.saturating_sub(prefix.chars().count() + 1);
        let (mut content, limited) = routes.render(available.min(INDEX_CHARS));
        knowledge.limited |= limited;
        if knowledge.limited {
            let available = available.saturating_sub(partial.chars().count());
            content = routes.render(available.min(INDEX_CHARS)).0;
        }
        let notice = if knowledge.limited { partial } else { "" };
        format!("{prefix}{content}{notice}\n")
    } else {
        let partial = "\n[Partial index: read the complete index above before following an incomplete route.]";
        let content: String = text.chars().take(INDEX_CHARS).collect();
        knowledge.limited |= text.chars().count() > INDEX_CHARS;
        let notice = if knowledge.limited { partial } else { "" };
        format!("\n[Knowledge index: {}]\nRouting index only. Read relevant linked files with file/search tools when needed; do not load the whole library.\n{content}{notice}\n", path.display())
    };
    if block.chars().count() <= *budget {
        *budget -= block.chars().count();
        knowledge.injected = block;
    } else {
        knowledge.limited = true;
    }
    knowledge
}

impl MemoryContext {
    /// `index` is an optional file, not a directory to recursively inject.
    /// Links inside it are plain text, read only through existing file tools.
    pub fn load(
        global: &MemoryStore,
        project: &MemoryStore,
        local: &MemoryStore,
        project_name: &str,
        index: &Path,
    ) -> Self {
        let now = chrono::Utc::now();
        let mut scopes: Vec<_> = [("global", global), ("project", project), ("local", local)]
            .into_iter()
            .map(|(scope, store)| {
                let source = read_source(store.path(), true);
                let parsed =
                    super::document::Document::parse(&source.text).map(|doc| doc.entries());
                let parsed = parsed.and_then(|mut entries| {
                    if source.limited && source.error.is_none() {
                        let ids: std::collections::HashSet<_> = entries
                            .iter()
                            .map(|e: &MemoryEntry| e.metadata.id.clone())
                            .collect();
                        let mut older = older_pinned(store.path(), now)?;
                        older.retain(|e| !ids.contains(&e.metadata.id));
                        older.append(&mut entries);
                        entries = older;
                    }
                    Ok(entries)
                });
                let (entries, error) = match parsed {
                    Ok(entries) => (
                        entries
                            .into_iter()
                            .map(|entry| EntrySelection {
                                entry,
                                injected: None,
                            })
                            .collect(),
                        source.error,
                    ),
                    Err(e) => (Vec::new(), Some(e.to_string())),
                };
                ScopeSelection {
                    scope,
                    path: store.path().into(),
                    limited: source.limited,
                    error,
                    entries,
                }
            })
            .collect();
        let header = "=== MEMORY ===\nHistorical facts and preferences; verify changeable claims against current sources. Follow current user instructions when memory conflicts.\n";
        let mut budget = CHAR_BUDGET - header.chars().count() - 512; // labels + bounded warning
        select(&mut scopes, &mut budget, true, now);
        let knowledge = select_index(index, read_source(index, false), &mut budget);
        select(&mut scopes, &mut budget, false, now);
        let omitted = scopes
            .iter()
            .flat_map(|s| &s.entries)
            .filter(|e| e.entry.eligible_at(now) && e.injected.is_none())
            .count();
        let truncated = scopes
            .iter()
            .flat_map(|s| &s.entries)
            .filter(|e| {
                e.injected
                    .as_ref()
                    .is_some_and(|text| text != &e.entry.content)
            })
            .count();
        let failures = scopes.iter().filter(|s| s.error.is_some()).count()
            + usize::from(knowledge.error.is_some());
        let limited = scopes.iter().any(|s| s.limited) || knowledge.limited;
        let warning = (omitted > 0 || truncated > 0 || failures > 0 || limited).then(|| format!(
            "{WARNING_PREFIX}{omitted} entries omitted to fit the budget; {truncated} entries shortened; {failures} read errors; partial source/index: {limited}. Run /memory to review."
        ));
        let mut prompt = String::new();
        if scopes
            .iter()
            .any(|s| s.entries.iter().any(|e| e.injected.is_some()))
            || !knowledge.injected.is_empty()
            || warning.is_some()
        {
            prompt.push_str(header);
            if let Some(warning) = &warning {
                prompt.push_str(warning);
                prompt.push('\n');
            }
            for scope in &scopes {
                if !scope.entries.iter().any(|e| e.injected.is_some()) {
                    continue;
                }
                let label = match scope.scope {
                    "global" => "Global".into(),
                    "local" => "Local".into(),
                    _ => format!(
                        "Project: {}",
                        project_name.chars().take(80).collect::<String>()
                    ),
                };
                prompt.push_str(&format!("\n[{label}]\n"));
                for entry in &scope.entries {
                    if let Some(text) = &entry.injected {
                        prompt.push_str(&format!("- {text}\n"));
                    }
                }
            }
            prompt.push_str(&knowledge.injected);
        }
        Self {
            prompt,
            scopes,
            knowledge,
            warning,
            evaluated_at: now,
        }
    }

    pub fn review(&self) -> String {
        self.review_filtered(None, None)
    }

    /// Filter detail rows, never the total budget or the session's actual payload.
    pub fn review_filtered(&self, topic: Option<&str>, status: Option<MemoryStatus>) -> String {
        self.review_with_expiry(topic, status, false, None)
    }
    pub fn review_with_expiry(
        &self,
        topic: Option<&str>,
        status: Option<MemoryStatus>,
        expired: bool,
        within: Option<u32>,
    ) -> String {
        let mut out = t(Msg::MemoryDiskPreview {
            budget: CHAR_BUDGET,
            chars: self.prompt.chars().count(),
        })
        .into_owned();
        if self.warning.is_some() {
            let entries: Vec<_> = self.scopes.iter().flat_map(|s| &s.entries).collect();
            let warning = t(Msg::MemorySelectionWarning {
                omitted: entries
                    .iter()
                    .filter(|e| e.entry.eligible_at(self.evaluated_at) && e.injected.is_none())
                    .count(),
                shortened: entries
                    .iter()
                    .filter(|e| e.injected.as_ref().is_some_and(|v| v != &e.entry.content))
                    .count(),
                failures: self.scopes.iter().filter(|s| s.error.is_some()).count()
                    + usize::from(self.knowledge.error.is_some()),
                partial: self.scopes.iter().any(|s| s.limited) || self.knowledge.limited,
            });
            out.push_str(&format!("\n**{warning}**\n"));
        }
        for scope in &self.scopes {
            let selected = scope
                .entries
                .iter()
                .filter(|e| e.injected.is_some())
                .count();
            let shortened = scope
                .entries
                .iter()
                .filter(|e| {
                    e.injected
                        .as_ref()
                        .is_some_and(|text| text != &e.entry.content)
                })
                .count();
            out.push_str(&t(Msg::MemoryScopeSelection {
                scope: scope.scope,
                path: &scope.path.display().to_string(),
                loaded: scope.entries.len(),
                selected,
                omitted: scope
                    .entries
                    .iter()
                    .filter(|e| e.entry.eligible_at(self.evaluated_at) && e.injected.is_none())
                    .count(),
                shortened,
            }));
            if scope.limited {
                out.push_str(&t(Msg::MemoryPartialRead));
            }
            if let Some(error) = &scope.error {
                out.push_str(&t(Msg::MemoryReadFailed { error }));
            }
            for (n, entry) in scope.entries.iter().enumerate().filter(|(_, entry)| {
                entry.entry.matches(topic, status)
                    && entry
                        .entry
                        .matches_expiry(self.evaluated_at, expired, within)
            }) {
                let state = t(match &entry.injected {
                    None if entry.entry.expired_at(self.evaluated_at) => Msg::MemoryEntryExpired,
                    None if entry.entry.metadata.status != MemoryStatus::Active => {
                        Msg::MemoryEntryInactive
                    }
                    None => Msg::MemoryEntryOmitted,
                    Some(text) if text != &entry.entry.content => Msg::MemoryEntryShortened,
                    _ => Msg::MemoryEntrySelected,
                });
                out.push_str(&format!(
                    "- #{} [{state}] `{}` status={} pinned={} topic={} source={} created_at={} updated_at={} expires_at={}\n  {}\n",
                    n + 1, entry.entry.metadata.id, entry.entry.metadata.status.as_str(), entry.entry.metadata.pinned,
                    entry.entry.metadata.topic.as_deref().unwrap_or("—"), entry.entry.metadata.source.as_deref().unwrap_or("—"),
                    entry.entry.metadata.created_at.as_deref().unwrap_or("—"), entry.entry.metadata.updated_at.as_deref().unwrap_or("—"),
                    entry.entry.metadata.expires_at.as_deref().unwrap_or("—"),
                    entry.entry.content.chars().take(100).collect::<String>()
                ));
            }
        }
        let state = t(if self.knowledge.injected.is_empty() {
            Msg::MemoryIndexNotIncluded
        } else {
            Msg::MemoryIndexIncluded
        });
        out.push_str(&t(Msg::MemoryKnowledgeReview {
            path: &self.knowledge.path.display().to_string(),
            state: &state,
        }));
        if self.knowledge.limited {
            out.push_str(&t(Msg::MemoryIndexLimited));
        }
        if let Some(error) = &self.knowledge.error {
            out.push_str(&t(Msg::MemoryReadFailed { error }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn load(dir: &Path) -> MemoryContext {
        MemoryContext::load(
            &MemoryStore::new(dir.join("g.md")),
            &MemoryStore::new(dir.join("p.md")),
            &MemoryStore::new(dir.join("l.md")),
            "project",
            &dir.join("knowledge/INDEX.md"),
        )
    }
    #[test]
    fn empty_optional_sources_do_not_inject_or_create_files() {
        let dir = tempfile::tempdir().unwrap();
        let context = load(dir.path());
        assert!(context.prompt.is_empty());
        assert!(context.warning.is_none());
        assert!(!dir.path().join("knowledge").exists());
    }
    #[test]
    fn selection_reports_exact_omissions_and_shortening_with_scope_priority() {
        let _locale = atomcode_i18n::test_lock();
        atomcode_i18n::set_locale(atomcode_i18n::Locale::En);
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("g.md"),
            (0..50)
                .map(|i| format!("- entry {i} {}\n", "语".repeat(100)))
                .collect::<String>(),
        )
        .unwrap();
        fs::write(
            dir.path().join("l.md"),
            format!("- LOCAL {}\n", "文".repeat(600)),
        )
        .unwrap();
        let context = load(dir.path());
        assert!(context.prompt.chars().count() <= CHAR_BUDGET);
        assert!(context.scopes[2].entries[0].injected.is_some());
        assert!(context.scopes[0].entries[0].injected.is_none());
        assert!(context.scopes[0].entries[49].injected.is_some());
        assert!(context
            .warning
            .as_ref()
            .unwrap()
            .contains("1 entries shortened"));
        let omitted = context.scopes[0]
            .entries
            .iter()
            .filter(|e| e.entry.eligible_at(context.evaluated_at) && e.injected.is_none())
            .count();
        assert!(context
            .warning
            .as_ref()
            .unwrap()
            .contains(&format!("{omitted} entries omitted")));
        assert!(context.review().contains("[omitted]"));
        assert!(context.review().contains("[shortened]"));
    }
    #[test]
    fn knowledge_is_a_bounded_index_and_never_reads_linked_documents() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("knowledge")).unwrap();
        fs::write(
            dir.path().join("knowledge/INDEX.md"),
            format!("Architecture -> architecture.md\n{}", "架构".repeat(2000)),
        )
        .unwrap();
        fs::write(
            dir.path().join("knowledge/architecture.md"),
            "SECRET_FULL_DOCUMENT",
        )
        .unwrap();
        let context = load(dir.path());
        assert!(context.knowledge.limited);
        assert!(context.prompt.contains("Architecture -> architecture.md"));
        assert!(!context.prompt.contains("SECRET_FULL_DOCUMENT"));
        assert!(context.prompt.chars().count() <= CHAR_BUDGET);
        assert!(context.warning.is_some());
    }
    #[test]
    fn documented_routes_work_with_windows_line_endings() {
        let template = include_str!("../../../../docs/memory.md")
            .lines()
            .skip_while(|line| *line != "```markdown")
            .skip(1)
            .take_while(|line| *line != "```")
            .collect::<Vec<_>>()
            .join("\r\n");
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("knowledge")).unwrap();
        fs::write(
            dir.path().join("knowledge/INDEX.md"),
            format!("\u{feff}{template}"),
        )
        .unwrap();

        let context = load(dir.path());
        assert!(context.knowledge.injected.contains("Read the linked L2"));
        assert!(context.knowledge.injected.contains("仅旧快照格式"));
        assert!(context
            .knowledge
            .injected
            .contains(".atomcode/knowledge/concepts/session.md |"));
        assert!(!context.knowledge.limited);
        assert!(context.warning.is_none());
    }

    const ROUTE_HEADER: &str = "| 主题 | 适用范围 | 关联理由 | 文件 |\n| --- | --- | --- | --- |";

    #[test]
    fn unrecognized_indexes_keep_the_legacy_text_path() {
        for content in [
            "# Custom notes\nRead SKILL.md".into(),
            "| 主题 | 适用场景 | 文件 |\n| --- | --- | --- |\n| 构建 | 本地 | build.md |".into(),
            format!("{ROUTE_HEADER}\n| 构建 | 本地 | build.md |"),
            format!("{ROUTE_HEADER}\n| 构建 | 本地 |  | build.md |"),
            format!("```markdown\n{ROUTE_HEADER}\n| 构建 | 本地 | 入口 | build.md |\n```"),
        ] {
            let mut budget = CHAR_BUDGET;
            let index = select_index(
                Path::new("INDEX.md"),
                Source {
                    text: content.clone(),
                    ..Source::default()
                },
                &mut budget,
            );
            assert!(index.injected.contains("Routing index only."));
            assert!(index.injected.ends_with(&format!("{content}\n")));
            assert!(!index.limited);
        }
    }

    #[test]
    fn routes_fit_the_actual_remaining_budget_as_whole_rows() {
        let oversized = format!("| 超长 | 本地 | {} | long.md |", "说".repeat(2000));
        let rows: Vec<_> = (0..12)
            .map(|i| format!("| 主题{i} | 仅测试 | 验证入口 | 知识/验证{i}.md |"))
            .collect();
        let content = format!(
            "# 项目知识\n{}\n{ROUTE_HEADER}\n{oversized}\n{}",
            "说".repeat(600),
            rows.join("\n")
        );
        let mut budget = 400;
        let index = select_index(
            Path::new("INDEX.md"),
            Source {
                text: content,
                ..Source::default()
            },
            &mut budget,
        );
        assert!(index.limited);
        assert!(index.injected.contains("for omitted routes or notes"));
        assert_eq!(index.injected.chars().count() + budget, 400);
        let selected: Vec<_> = index
            .injected
            .lines()
            .filter(|line| line.starts_with("| 主题") && !ROUTE_HEADER.contains(line))
            .collect();
        assert!(!selected.is_empty());
        assert!(selected.len() < rows.len());
        assert!(selected
            .iter()
            .zip(&rows)
            .all(|(selected, row)| *selected == row));
        assert!(!index.injected.contains("超长"));
    }

    #[test]
    fn structured_index_ignores_the_row_cut_at_the_file_read_limit() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("knowledge")).unwrap();
        for complete_row in ["", "| 构建 | 本地 | 验证入口 | build.md |\n"] {
            fs::write(
                dir.path().join("knowledge/INDEX.md"),
                format!(
                    "{ROUTE_HEADER}\n{complete_row}| 末行 | 本地 | {} | late.md |",
                    "语".repeat(30000)
                ),
            )
            .unwrap();
            let context = load(dir.path());
            let index = context.knowledge;
            assert!(index.limited);
            assert!(index.error.is_none());
            assert!(index.injected.contains("Read the linked L2"));
            assert!(index.injected.contains("read the complete index above"));
            assert_eq!(
                index.injected.contains("build.md |"),
                !complete_row.is_empty()
            );
            assert!(!index.injected.contains("末行"));
        }
    }

    #[test]
    fn malformed_and_non_file_sources_are_visible_failures() {
        let _locale = atomcode_i18n::test_lock();
        atomcode_i18n::set_locale(atomcode_i18n::Locale::En);
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("g.md"), [0xff]).unwrap();
        fs::create_dir(dir.path().join("p.md")).unwrap();
        let context = load(dir.path());
        assert!(context.scopes[0].error.is_some());
        assert!(context.scopes[1].error.is_some());
        assert!(context.warning.as_ref().unwrap().contains("2 read errors"));
        assert!(context.review().contains("Read failed"));
    }
    #[test]
    fn oversized_memory_reports_partial_tail_and_does_not_claim_complete_counts() {
        let _locale = atomcode_i18n::test_lock();
        atomcode_i18n::set_locale(atomcode_i18n::Locale::En);
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("p.md"),
            format!("- OLDEST\n{}- NEWEST\n", "- text\n".repeat(15000)),
        )
        .unwrap();
        let context = load(dir.path());
        assert!(context.scopes[1].limited);
        assert!(!context.prompt.contains("OLDEST"));
        assert!(context.prompt.contains("NEWEST"));
        assert!(context.review().contains("earlier entries are not counted"));
        assert!(context.prompt.chars().count() <= CHAR_BUDGET);
    }
    #[test]
    fn index_cut_at_utf8_boundary_is_still_usable_and_reported_partial() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("knowledge")).unwrap();
        fs::write(dir.path().join("knowledge/INDEX.md"), "语".repeat(30000)).unwrap();
        let context = load(dir.path());
        assert!(context.knowledge.error.is_none());
        assert!(context.knowledge.limited);
        assert!(context.prompt.contains('语'));
    }
    #[test]
    fn long_label_and_index_path_cannot_escape_combined_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("p.md"), "- content\n".repeat(1000)).unwrap();
        let store = MemoryStore::new(dir.path().join("p.md"));
        let context = MemoryContext::load(
            &store,
            &store,
            &store,
            &"项".repeat(10000),
            &dir.path().join("knowledge/INDEX.md"),
        );
        assert!(context.prompt.chars().count() <= CHAR_BUDGET);
    }
}
