use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::stream::ProviderError;
use atomcode_kernel::tool::{Tool, ToolContext, ToolDef, ToolResult};
use serde_json::json;

pub(crate) const PATCH_BYTES: usize = 48 * 1024;
pub(crate) const REQUEST_BYTES: usize = 192 * 1024;
pub(crate) const CONCURRENCY: usize = 2;
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) const MAX_ROUNDS: u32 = 12;
pub(crate) const INPUT_BUDGET_CODE: &str = "review_input_budget";
const EVIDENCE_RESULT_BYTES: usize = 8 * 1024;
const RETAINED_EVIDENCE_BYTES: usize = 32 * 1024;
const PATCH_TOOL: &str = "read_review_patch";
const EVIDENCE_EVICTED: &str = "[Earlier evidence omitted to stay within the review budget. Re-read the smallest relevant range if needed.]";

fn pinned_result(tool: &str) -> bool {
    matches!(tool, PATCH_TOOL | "report_finding")
}

pub(crate) fn limit_evidence_result(tool: &str, text: &mut String) {
    if pinned_result(tool) || text.len() <= EVIDENCE_RESULT_BYTES {
        return;
    }
    let hint = match tool {
        "read_file" => "Read a smaller line range using offset and limit.",
        "grep" => "Narrow the path or pattern, or reduce max_results and context.",
        _ => "Request a narrower result if you need more evidence.",
    };
    let notice = format!("\n[Review evidence truncated. {hint}]");
    let mut end = EVIDENCE_RESULT_BYTES - notice.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = text[..end]
        .rfind('\n')
        .filter(|last| *last >= end / 2)
        .unwrap_or(end);
    text.truncate(end);
    text.push_str(&notice);
}

/// Keep the tool-call/result pairing and provider reasoning intact. Budget
/// only supporting evidence; the assigned patch and finding receipts stay.
pub(crate) fn fit_request<'a>(
    messages: &'a [Message],
    tools: &[ToolDef],
    options: &ChatOptions,
    window: u32,
) -> Result<Cow<'a, [Message]>, ProviderError> {
    let pinned: HashSet<&str> = messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .filter(|call| pinned_result(&call.name))
        .map(|call| call.id.as_str())
        .collect();
    let auxiliary = |message: &Message| {
        message.role == Role::Tool
            && !message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| pinned.contains(id))
    };
    let tools_bytes = serde_json::to_vec(tools).map_err(encode_error)?.len();
    let input_limit = atomcode_kernel::agent::effective_input_limit(window, options.max_tokens);
    let measure = |messages: &[Message]| -> Result<(usize, u64), ProviderError> {
        let bytes = serde_json::to_vec(&(messages, tools, options.max_tokens))
            .map_err(encode_error)?
            .len();
        let tokens = messages
            .iter()
            .map(|message| u64::from(message.estimate_tokens()))
            .sum::<u64>()
            + tools_bytes.div_ceil(3) as u64;
        Ok((bytes, tokens))
    };
    let fits = |(bytes, tokens): (usize, u64)| {
        bytes <= REQUEST_BYTES && (window == 0 || tokens < u64::from(input_limit))
    };

    let mut projected = Cow::Borrowed(messages);
    let mut remaining = RETAINED_EVIDENCE_BYTES;
    for (index, message) in messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| auxiliary(message))
    {
        if message.text.len() <= remaining {
            remaining -= message.text.len();
        } else {
            projected.to_mut()[index].text = EVIDENCE_EVICTED.into();
        }
    }
    let size = measure(&projected)?;
    if fits(size) {
        return Ok(projected);
    }

    // Reasoning or schemas may have grown more than anticipated. Try once
    // with just the pinned evidence before asking the batch runner to split.
    for message in projected.to_mut() {
        if auxiliary(message) {
            message.text = EVIDENCE_EVICTED.into();
        }
    }
    let (bytes, tokens) = measure(&projected)?;
    if fits((bytes, tokens)) {
        return Ok(projected);
    }
    Err(ProviderError {
        code: Some(INPUT_BUDGET_CODE.into()),
        message: format!(
            "Review request exceeds its input budget ({bytes} bytes, {tokens} estimated tokens)."
        ),
        ..Default::default()
    })
}

fn encode_error(error: serde_json::Error) -> ProviderError {
    ProviderError {
        message: error.to_string(),
        ..Default::default()
    }
}

#[derive(Debug)]
pub(crate) struct Batch {
    pub patch: String,
    pub files: Vec<String>,
}

// Git C-quotes unusual paths and escapes non-ASCII UTF-8 bytes as octal.
fn decode_git_path(path: &str, prefix: &str) -> Option<String> {
    let path = path.trim_end_matches('\t');
    let Some(quoted) = path.strip_prefix('"') else {
        return Some(path.strip_prefix(prefix)?.to_owned());
    };
    let mut input = quoted.strip_suffix('"')?.bytes();
    let mut decoded = Vec::with_capacity(quoted.len());
    while let Some(byte) = input.next() {
        let byte = if byte == b'\\' {
            match input.next()? {
                first @ b'0'..=b'3' => {
                    let second = input.next()?.checked_sub(b'0').filter(|digit| *digit < 8)?;
                    let third = input.next()?.checked_sub(b'0').filter(|digit| *digit < 8)?;
                    (first - b'0') * 64 + second * 8 + third
                }
                b'a' => 7,
                b'b' => 8,
                b't' => b'\t',
                b'n' => b'\n',
                b'v' => 11,
                b'f' => 12,
                b'r' => b'\r',
                escaped @ (b'\\' | b'"') => escaped,
                _ => return None,
            }
        } else {
            byte
        };
        decoded.push(byte);
    }
    Some(
        std::str::from_utf8(&decoded)
            .ok()?
            .strip_prefix(prefix)?
            .to_owned(),
    )
}

impl Batch {
    fn new(patch: String) -> Self {
        let mut files = Vec::new();
        let mut lines = patch.lines().peekable();
        while let Some(header) = lines.next() {
            let Some(header) = header.strip_prefix("diff --git ") else {
                continue;
            };
            let mut source = None;
            let mut destination = None;
            while lines
                .peek()
                .is_some_and(|line| !line.starts_with("diff --git "))
            {
                let line = lines.next().expect("peeked diff line");
                if let Some(path) = line.strip_prefix("+++ ") {
                    destination = decode_git_path(path, "b/");
                } else if let Some(path) = line.strip_prefix("--- ") {
                    source = decode_git_path(path, "a/");
                } else if let Some(path) = line
                    .strip_prefix("rename to ")
                    .or_else(|| line.strip_prefix("copy to "))
                {
                    destination = decode_git_path(path, "");
                }
            }
            let path = destination.or(source).or_else(|| {
                // Binary and mode-only patches may have no unified file markers.
                let offset = header.rfind(" \"b/").or_else(|| header.rfind(" b/"))?;
                decode_git_path(&header[offset + 1..], "b/")
            });
            if let Some(path) = path {
                if !files.contains(&path) {
                    files.push(path);
                }
            }
        }
        Self { patch, files }
    }

    pub(crate) fn describe(&self) -> String {
        match self.files.as_slice() {
            [] => "file metadata".into(),
            [file] => file.clone(),
            [first, .., last] => format!("{first} … {last}, {} files", self.files.len()),
        }
    }
}

/// Pack line-numbered, immutable evidence. Split an oversized file at line
/// boundaries, repeating its file and hunk headers so each part is intelligible.
pub(crate) fn plan(diff: &str, limit: usize) -> (Vec<Batch>, usize) {
    let mut batches = Vec::new();
    let mut batch = String::new();
    let mut omitted = 0;
    let mut file_header = String::new();
    let mut hunk = "";
    let mut need_header = false;
    let flush = |batch: &mut String, batches: &mut Vec<Batch>| {
        if !batch.is_empty() {
            batches.push(Batch::new(std::mem::take(batch)));
        }
    };
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            file_header.clear();
            hunk = "";
            need_header = false;
        }
        if line.starts_with("@@") {
            hunk = line;
        }
        if hunk.is_empty() {
            file_header.push_str(line);
        }
        if batch.len() + line.len() > limit {
            flush(&mut batch, &mut batches);
            need_header = !line.starts_with("diff --git ");
        }
        if need_header {
            let header = if hunk.is_empty() {
                &file_header[..file_header.len() - line.len()]
            } else {
                file_header.as_str()
            };
            let hunk_header = if line == hunk { "" } else { hunk };
            if header.len() + hunk_header.len() + line.len() > limit {
                omitted += 1;
                continue;
            }
            batch.push_str(header);
            batch.push_str(hunk_header);
            need_header = false;
        }
        if line.len() > limit {
            omitted += 1;
            continue;
        }
        batch.push_str(line);
    }
    flush(&mut batch, &mut batches);
    (batches, omitted)
}

pub(crate) struct ReviewPatch {
    patch: String,
    read: AtomicBool,
}

impl ReviewPatch {
    pub(crate) fn new(patch: String) -> Self {
        Self {
            patch,
            read: AtomicBool::new(false),
        }
    }

    pub(crate) fn was_read(&self) -> bool {
        self.read.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Tool for ReviewPatch {
    fn name(&self) -> &str {
        PATCH_TOOL
    }
    fn description(&self) -> &str {
        "Read the immutable, line-numbered diff assigned to this review batch. Call once before reviewing."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    fn self_bounds_output(&self) -> bool {
        true
    }
    async fn execute(&self, _: &str, _: &ToolContext) -> ToolResult {
        ToolResult {
            content: if self.read.swap(true, Ordering::Relaxed) {
                "The patch is already in the preceding tool result.".into()
            } else {
                self.patch.clone()
            },
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolCall;

    #[test]
    fn repeated_evidence_reads_keep_the_patch_reasoning_and_latest_results() {
        let patch = "+ changed line\n".repeat(2400);
        let mut messages = vec![Message::user("Review the assigned batch.")];
        for (id, name, output) in std::iter::once(("patch".to_owned(), PATCH_TOOL, patch.clone()))
            .chain((0..10).map(|i| {
                (
                    format!("read-{i}"),
                    "read_file",
                    format!("{i} evidence row\n").repeat(1100),
                )
            }))
        {
            let mut turn = Message::assistant(
                "",
                vec![ToolCall {
                    id: id.clone(),
                    name: name.into(),
                    arguments: "{}".into(),
                }],
            );
            turn.reasoning = Some("Retain provider reasoning".into());
            messages.push(turn);
            let mut result = Message::tool_result(id, output, false);
            limit_evidence_result(name, &mut result.text);
            messages.push(result);
        }
        let projected = fit_request(&messages, &[], &ChatOptions::default(), 512_000).unwrap();
        assert_eq!(projected.len(), messages.len());
        assert_eq!(projected[2].text, patch);
        assert_eq!(projected[4].text, EVIDENCE_EVICTED);
        assert!(projected.last().unwrap().text.starts_with("9 evidence row"));
        assert!(projected
            .last()
            .unwrap()
            .text
            .ends_with("offset and limit.]"));
        assert!(projected.last().unwrap().text.len() <= EVIDENCE_RESULT_BYTES);
        for (before, after) in messages.iter().zip(projected.iter()) {
            assert_eq!(before.role, after.role);
            assert_eq!(before.tool_call_id, after.tool_call_id);
            assert_eq!(before.reasoning, after.reasoning);
        }
        assert!(messages[4].text.starts_with("0 evidence row"));
    }

    #[test]
    fn a_request_that_still_cannot_fit_reports_the_structured_budget_error() {
        let task = [Message::user("x".repeat(REQUEST_BYTES))];
        let error = fit_request(&task, &[], &ChatOptions::default(), 512_000).unwrap_err();
        assert_eq!(error.code.as_deref(), Some(INPUT_BUDGET_CODE));
        assert!(!error.retryable);
    }
}
