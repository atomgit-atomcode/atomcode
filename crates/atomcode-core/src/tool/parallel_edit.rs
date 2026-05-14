//! Active-dispatch fork sub-agent tool.
//!
//! Replaces the prior PASSIVE flow where the agent loop parsed the model's
//! plan text, inferred edit intent via keyword soup, and dispatched fork
//! sub-agents without asking. That design forced a brittle keyword gate,
//! mis-fired on planning/exploration turns, and gave the model no way to
//! reason about cross-file invariants (each sub-agent saw only its
//! assigned file plus a 30-line skeleton of siblings).
//!
//! With active dispatch, the model invokes `parallel_edit_files` as a
//! tool when it judges parallel edit is the right move. The framework
//! does no inference. The tool's args carry:
//!   - `files: [{path, instruction}, ...]` — ≥2, ≤12
//!   - `contract: ""` — cross-file invariants (shared trait/type/interface
//!      contracts) injected verbatim into every sub-agent's user message
//!
//! Each sub-agent sees its own file content + the contract, runs through
//! the existing `SubAgentPool` resilience layer, and returns a status
//! row. After all settle, a build-marker probe (Cargo / npm / mvn / go)
//! runs once to catch cross-file dep regressions; failures are surfaced
//! verbatim so the model can fix without reverse-engineering.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};
use crate::agent::sub_agent;
use crate::agent::AgentEvent;
use crate::config::Config;
use crate::provider::LlmProvider;

/// One file's edit assignment. The model writes both fields; the
/// framework treats `instruction` as opaque guidance to the sub-agent.
#[derive(Debug, Deserialize)]
struct ParallelEditFile {
    path: String,
    instruction: String,
}

#[derive(Debug, Deserialize)]
struct ParallelEditArgs {
    files: Vec<ParallelEditFile>,
    /// Cross-file invariants the model expects every sub-agent to honour.
    /// Forwarded verbatim so a sub-agent editing one half of a trait
    /// boundary can see what the other half is doing — the previous
    /// passive flow's biggest failure mode (mod.rs edited but unix.rs
    /// trait impl missed) is impossible when the model writes a contract
    /// covering both files.
    #[serde(default)]
    contract: String,
}

pub struct ParallelEditTool {
    pub provider: Arc<dyn LlmProvider>,
    pub config: Config,
    pub event_tx: mpsc::UnboundedSender<AgentEvent>,
}

#[async_trait]
impl Tool for ParallelEditTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "parallel_edit_files",
            description:
                "Edit multiple INDEPENDENT files in parallel via fork sub-agents.\n\n\
                Use ONLY when:\n\
                - You have 2+ DIFFERENT concrete files to edit, each with a clear instruction\n\
                - Edits in different files don't depend on each other\n\
                - You can express any cross-file invariants (shared trait/type/interface) in `contract`\n\n\
                Do NOT use when:\n\
                - You're still exploring or the edit isn't fully decided\n\
                - You want to make several edits to the SAME file (use N sequential edit_file / search_replace calls instead — listing the same path twice causes a race where edits clobber each other)\n\
                - Files have impl/decl splits that need coordinated edits (use sequential edit_file)\n\
                - You want to read more files first (use read_file)\n\n\
                Every `path` in `files` MUST be unique. The tool rejects duplicate paths because \
                sub-agents run concurrently — two sub-agents editing the same file race on read/write \
                and the later writer overwrites the earlier one's edits. To fill N sections of one \
                file (e.g. a skeleton HTML with N <!-- SECTION:* --> markers), call edit_file N times \
                sequentially.\n\n\
                Each sub-agent sees only its assigned file content + the contract you provide. \
                Cross-file changes that aren't expressed in `contract` will be missed by the merge — \
                the sub-agents cannot see each other's edits. After all sub-agents settle, the \
                framework runs a build probe (cargo/npm/mvn/go) and surfaces compile errors so you \
                can repair cross-file gaps."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 12,
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "File path. Absolute, or relative to the working directory."
                                },
                                "instruction": {
                                    "type": "string",
                                    "description": "Concrete edit description for THIS file. Be specific: what to add/modify/remove and why. The sub-agent sees only this instruction + the file content + the contract — no other context."
                                }
                            },
                            "required": ["path", "instruction"]
                        }
                    },
                    "contract": {
                        "type": "string",
                        "description": "Cross-file invariants every sub-agent must honour: shared traits, type signatures, interface contracts, naming conventions. Empty if files are fully independent."
                    }
                },
                "required": ["files"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        let parsed: ParallelEditArgs = serde_json::from_str(args).map_err(|e| {
            format!(
                "{} (parallel_edit_files arguments must be {{\"files\": [{{\"path\": \"…\", \"instruction\": \"…\"}}, …], \"contract\": \"…\"?}})",
                e
            )
        })?;
        if parsed.files.len() < 2 {
            return Err(
                "parallel_edit_files requires at least 2 files. For a single file, call edit_file directly."
                    .to_string(),
            );
        }
        if parsed.files.len() > 12 {
            return Err(format!(
                "parallel_edit_files capped at 12 files; you sent {}. Split into smaller batches or run sequentially.",
                parsed.files.len()
            ));
        }
        // Track first-seen index per normalised path so a duplicate
        // detection error can point at BOTH offending entries — the
        // model needs to know which two indices to merge, not just
        // "you have a duplicate somewhere". Trim only; we don't
        // case-fold or resolve symlinks here because:
        //   - case-sensitivity is FS-dependent (case-insensitive on
        //     APFS-default macOS / Windows NTFS, case-sensitive on
        //     ext4 / btrfs / case-sensitive APFS), so a normalised
        //     comparison here would lie on half the platforms
        //   - resolving requires async I/O against a sub-agent that
        //     hasn't been dispatched yet; cheaper to let the literal
        //     duplicate (the only case glm-5.1 actually produces —
        //     5/13 atomgr session emitted security-audit-report.html
        //     × 7 verbatim) get rejected here, and trust the OS to
        //     race-protect any legitimate case-variant collisions
        let mut seen_paths: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(parsed.files.len());
        for (i, f) in parsed.files.iter().enumerate() {
            let normalised = f.path.trim();
            if normalised.is_empty() {
                return Err(format!("files[{}].path is empty", i));
            }
            if f.instruction.trim().is_empty() {
                return Err(format!(
                    "files[{}].instruction is empty. Each file needs a concrete edit description; \
                     a sub-agent with no instruction will either fake an edit or burn its budget.",
                    i
                ));
            }
            if let Some(&first_idx) = seen_paths.get(normalised) {
                let dup_count = parsed
                    .files
                    .iter()
                    .filter(|other| other.path.trim() == normalised)
                    .count();
                return Err(format!(
                    "parallel_edit_files rejected: path `{}` appears {} times \
                     (first at files[{}], duplicate at files[{}]). Sub-agents run \
                     concurrently — N sub-agents on the same file race on read/write \
                     and the later writer clobbers earlier edits, so this is unsafe by \
                     construction. To make {} edits to the same file, call edit_file \
                     (or search_replace) {} times SEQUENTIALLY instead; only use \
                     parallel_edit_files when the files are genuinely different.",
                    normalised, dup_count, first_idx, i, dup_count, dup_count,
                ));
            }
            seen_paths.insert(normalised.to_string(), i);
        }
        Ok(())
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ParallelEditArgs = serde_json::from_str(args)?;

        let working_dir = ctx.working_dir.read().await.clone();
        let registry = match ctx.tool_registry.as_ref() {
            Some(r) => r.clone(),
            None => {
                // Should not happen in production — AgentLoop::new sets this
                // before any turn runs. Headless contexts that don't wire it
                // can't dispatch fork sub-agents (and shouldn't register the
                // tool in the first place).
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "parallel_edit_files unavailable: tool registry not wired in this context."
                        .to_string(),
                    success: false,
                });
            }
        };

        // Resolve + read every file up front. Aborting before any sub-agent
        // runs means a typo in one path doesn't leave half the dispatch
        // half-done.
        let mut all_file_contents: Vec<(String, String)> = Vec::with_capacity(parsed.files.len());
        for spec in &parsed.files {
            let path = if std::path::Path::new(&spec.path).is_absolute() {
                std::path::PathBuf::from(&spec.path)
            } else {
                working_dir.join(&spec.path)
            };
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "Cannot read `{}`: {}. Aborted dispatch — fix the path or use a different approach.",
                            spec.path, e
                        ),
                        success: false,
                    });
                }
            };
            all_file_contents.push((path.to_string_lossy().to_string(), content));
        }

        // Build SubAgentTask per file. Each task carries siblings as
        // 30-line skeletons so a sub-agent has minimal cross-file context;
        // the model's `contract` argument carries the binding invariants.
        let mut tasks = Vec::with_capacity(parsed.files.len());
        for i in 0..parsed.files.len() {
            let mut siblings = String::new();
            for (j, (sib_path, sib_content)) in all_file_contents.iter().enumerate() {
                if i == j {
                    continue;
                }
                let short = std::path::Path::new(sib_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| sib_path.clone());
                let skeleton: String =
                    sib_content.lines().take(30).collect::<Vec<_>>().join("\n");
                siblings.push_str(&format!("### {}\n```\n{}\n```\n\n", short, skeleton));
            }
            tasks.push(sub_agent::SubAgentTask {
                file_path: all_file_contents[i].0.clone(),
                file_content: all_file_contents[i].1.clone(),
                task_instruction: parsed.files[i].instruction.clone(),
                contract: parsed.contract.clone(),
                sibling_skeletons: siblings,
            });
        }

        // Lifecycle events for the TUI. Build per-task descriptors so
        // the renderer can pre-allocate display slots and disambiguate
        // same-path entries with `(#2)`, `(#3)` suffixes — three
        // sub-agents on `tunnel.rs` would otherwise show up as three
        // identical rows the user can't tell apart.
        let paths: Vec<&str> = tasks.iter().map(|t| t.file_path.as_str()).collect();
        let task_infos = build_task_infos_with_dedup(&paths);
        let _ = self
            .event_tx
            .send(AgentEvent::SubAgentDispatchStart { tasks: task_infos });

        let pool = sub_agent::SubAgentPool {
            tasks,
            max_concurrent: self.config.subagent.max_concurrent,
            timeout_secs: self.config.subagent.timeout_secs,
        };
        let results = pool
            .execute_all(
                self.provider.clone(),
                registry,
                &self.config,
                &working_dir,
                &self.event_tx,
            )
            .await;
        let _ = self.event_tx.send(AgentEvent::SubAgentDispatchEnd);

        // Build the tool result: per-task status block + build-probe
        // outcome. This is what the MODEL sees — it must contain enough
        // signal to decide whether to retry / fix-up. The TUI renders
        // this same content collapsed (single aggregate line); the
        // duplicate-display problem is solved at the UI layer, not by
        // shrinking the message the model needs to read.
        //
        // Format change: pipe-table ("- file | OK | 2 turns | model said: ...")
        // dropped. Hard to scan, eyes have to stop at every `|`, and
        // `model said:` quotes were truncating mid-word at terminal
        // width. New format is one task per line, status icon prefix,
        // full path, time/turns in compact bracket, summary in plain
        // prose so wrapping is natural.
        let ok_count = results.iter().filter(|r| r.success).count();
        let fail_count = results.len() - ok_count;
        let mut summary = format!(
            "Sub-agents: {} ok, {} fail (of {})\n",
            ok_count,
            fail_count,
            results.len(),
        );
        let mut all_success = fail_count == 0;
        for r in &results {
            let icon = if r.success { "✓" } else { "✗" };
            // Time isn't tracked on SubAgentResult — the per-task UI
            // events carry elapsed_ms and the user already saw it
            // stream in. The model only needs turn count to decide
            // between rescue / retry / abandon, and a one-line summary.
            let one_line = r.summary.lines().next().unwrap_or("").trim();
            summary.push_str(&format!(
                "  {} {} ({}T) — {}\n",
                icon, r.file_path, r.turns_used, one_line,
            ));
            if !r.success {
                all_success = false;
                for failure in &r.failures {
                    summary.push_str(&format!("      reason: {:?}\n", failure));
                }
            }
        }

        // Build verification — best-effort, structural detector (probes
        // for build-system markers, not model intent). On miss the table
        // is the final answer.
        if let Some((cmd, build_dir)) = find_build_command(&working_dir) {
            let output = tokio::process::Command::new("sh")
                .args(["-c", &cmd])
                .current_dir(&build_dir)
                .output()
                .await;
            if let Ok(out) = output {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{}{}", stdout, stderr);
                if !out.status.success() || combined.to_lowercase().contains("error") {
                    let err_lines: String =
                        combined.lines().take(15).collect::<Vec<_>>().join("\n");
                    summary.push_str(&format!(
                        "\n⚠ BUILD ERRORS after merge:\n{}\nFix these before proceeding.\n",
                        err_lines
                    ));
                    all_success = false;
                } else {
                    summary.push_str("\n✓ Build verification passed.\n");
                }
            }
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: summary,
            success: all_success,
        })
    }
}

/// Detect the workspace's primary build command by probing for canonical
/// project-root marker files. Structural (one marker per ecosystem), not
/// inference — the markers are the build system's own signature, not the
/// Build `SubAgentTaskInfo` descriptors with per-occurrence `(#N)`
/// disambiguation when the same path appears more than once in the
/// dispatch list. Unique paths get an empty `dedup_suffix`. Order
/// matches the input — index N in `paths` maps to index N in the
/// returned vec, so the `index` field on lifecycle events stays a
/// valid lookup key.
fn build_task_infos_with_dedup(paths: &[&str]) -> Vec<crate::agent::SubAgentTaskInfo> {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for p in paths {
        *counts.entry(*p).or_insert(0) += 1;
    }
    paths
        .iter()
        .map(|p| {
            let total = counts.get(*p).copied().unwrap_or(1);
            let dedup_suffix = if total > 1 {
                let n = seen.entry(*p).or_insert(0);
                *n += 1;
                format!(" (#{})", *n)
            } else {
                String::new()
            };
            crate::agent::SubAgentTaskInfo {
                path: p.to_string(),
                dedup_suffix,
            }
        })
        .collect()
}

/// model's text. Searches the working directory then immediate
/// subdirectories so nested project layouts (a Cargo workspace under a
/// monorepo) still resolve.
fn find_build_command(wd: &std::path::Path) -> Option<(String, std::path::PathBuf)> {
    let markers: &[(&str, &str)] = &[
        ("package.json", "npm run build 2>&1 | head -30"),
        ("Cargo.toml", "cargo check 2>&1 | tail -20"),
        ("pom.xml", "mvn compile -q 2>&1 | tail -20"),
        ("go.mod", "go build ./... 2>&1 | tail -20"),
    ];

    for &(marker, cmd) in markers {
        if wd.join(marker).exists() {
            return Some((cmd.to_string(), wd.to_path_buf()));
        }
    }

    if let Ok(entries) = std::fs::read_dir(wd) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let sub = entry.path();
                let name = sub.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                for &(marker, cmd) in markers {
                    if sub.join(marker).exists() {
                        return Some((cmd.to_string(), sub));
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod validate_args_tests {
    use super::*;
    use crate::stream::StreamEvent;
    use std::pin::Pin;
    use tokio::sync::mpsc;

    /// Stub provider — `validate_args` doesn't touch it, but the struct
    /// fields require something that implements `LlmProvider`.
    struct StubProvider;

    impl LlmProvider for StubProvider {
        fn chat_stream(
            &self,
            _messages: &[crate::conversation::message::Message],
            _tools: Option<&[crate::tool::ToolDef]>,
        ) -> anyhow::Result<
            Pin<
                Box<
                    dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send,
                >,
            >,
        > {
            unimplemented!()
        }
        fn model_name(&self) -> &str {
            "stub"
        }
    }

    fn blank_config() -> Config {
        Config {
            default_provider: String::new(),
            default_workdir: None,
            providers: std::collections::HashMap::new(),
            datalog: Default::default(),
            auto_update: true,
            notifications: Default::default(),
            telemetry: Default::default(),
            lsp: Default::default(),
            auto_commit: false,
            subagent: Default::default(),
        }
    }

    fn tool() -> ParallelEditTool {
        let (tx, _rx) = mpsc::unbounded_channel();
        ParallelEditTool {
            provider: Arc::new(StubProvider),
            config: blank_config(),
            event_tx: tx,
        }
    }

    #[test]
    fn rejects_single_file_dispatch() {
        // The whole point of this tool is parallelism; a 1-file call
        // should route to edit_file directly. Without this guard the
        // pool runs one sub-agent serially, paying the dispatch overhead
        // for zero parallelism gain.
        let args = r#"{"files":[{"path":"a.rs","instruction":"edit"}]}"#;
        let err = tool().validate_args(args).unwrap_err();
        assert!(err.contains("at least 2 files"), "got: {}", err);
    }

    #[test]
    fn rejects_empty_instruction() {
        // Empty instruction is the failure mode that motivated active
        // dispatch in the first place: passive flow's
        // `extract_file_instruction` synthesized "Edit X according to
        // the plan." for files with no plan-text presence, the
        // sub-agent had no actual directive, the model either faked an
        // edit (corrupted file) or burned its budget on
        // BudgetExhaustedNoEdits. Reject up-front so the model gets a
        // structured retry hint.
        let args = r#"{"files":[
            {"path":"a.rs","instruction":"add field"},
            {"path":"b.rs","instruction":"  "}
        ]}"#;
        let err = tool().validate_args(args).unwrap_err();
        assert!(err.contains("instruction is empty"), "got: {}", err);
    }

    #[test]
    fn rejects_empty_path() {
        let args = r#"{"files":[
            {"path":"","instruction":"edit"},
            {"path":"b.rs","instruction":"edit"}
        ]}"#;
        let err = tool().validate_args(args).unwrap_err();
        assert!(err.contains("path is empty"), "got: {}", err);
    }

    #[test]
    fn rejects_more_than_twelve_files() {
        // 12 is the cap. Beyond that, parallel saturation hurts more
        // than helps (each sub-agent still costs an LLM round-trip)
        // and the merge probability of cross-file gaps grows roughly
        // O(n²). Force the model to chunk into smaller batches.
        let files: Vec<String> = (0..13)
            .map(|i| format!(r#"{{"path":"f{}.rs","instruction":"edit"}}"#, i))
            .collect();
        let args = format!(r#"{{"files":[{}]}}"#, files.join(","));
        let err = tool().validate_args(&args).unwrap_err();
        assert!(err.contains("capped at 12"), "got: {}", err);
    }

    #[test]
    fn accepts_valid_two_file_dispatch() {
        let args = r#"{"files":[
            {"path":"a.rs","instruction":"add field X"},
            {"path":"b.rs","instruction":"wire X into Y"}
        ],"contract":"X is a u32"}"#;
        assert!(tool().validate_args(args).is_ok());
    }

    #[test]
    fn accepts_minimal_args_without_contract() {
        // contract is optional — defaults to empty when files are fully
        // independent (no shared trait/type).
        let args = r#"{"files":[
            {"path":"a.rs","instruction":"add log"},
            {"path":"b.rs","instruction":"add log"}
        ]}"#;
        assert!(tool().validate_args(args).is_ok());
    }

    #[test]
    fn rejects_unparseable_json() {
        let args = "not json at all";
        let err = tool().validate_args(args).unwrap_err();
        assert!(err.contains("parallel_edit_files arguments"), "got: {}", err);
    }

    /// Real-world failure mode from 5/13 atomgr session: glm-5.1 emitted
    /// parallel_edit_files with 7 entries ALL pointing at
    /// security-audit-report.html. Sub-agents ran concurrently and the
    /// last writer clobbered the previous six edits. Even worse, the
    /// post-dispatch round-trip silently returned an empty response —
    /// 4-5 min of dead time per attempt.
    ///
    /// The validator must reject duplicate paths up-front so the model
    /// receives a structured retry hint ("use N sequential edit_file
    /// calls") instead of a corrupted file + agent confusion.
    #[test]
    fn rejects_duplicate_paths() {
        let args = r#"{"files":[
            {"path":"/abs/report.html","instruction":"fill section A"},
            {"path":"/abs/report.html","instruction":"fill section B"}
        ]}"#;
        let err = tool().validate_args(args).unwrap_err();
        assert!(err.contains("appears 2 times"), "got: {}", err);
        assert!(
            err.contains("files[0]") && err.contains("files[1]"),
            "must name both offending indices for the model to merge them; got: {}",
            err
        );
        assert!(
            err.contains("edit_file") && err.contains("SEQUENTIALLY"),
            "must hint at the correct alternative tool + ordering; got: {}",
            err
        );
    }

    /// 7-way duplication (the actual session shape) — count must be
    /// exact so the model knows how many sequential edit_file calls
    /// to make.
    #[test]
    fn duplicate_error_reports_exact_count() {
        let files: Vec<String> = (0..7)
            .map(|i| {
                format!(
                    r#"{{"path":"/abs/x.html","instruction":"section {}"}}"#,
                    i
                )
            })
            .collect();
        let args = format!(r#"{{"files":[{}]}}"#, files.join(","));
        let err = tool().validate_args(&args).unwrap_err();
        assert!(err.contains("appears 7 times"), "got: {}", err);
    }

    /// Whitespace-only differences in `path` ARE treated as duplicates
    /// (we trim before comparison). `" foo.rs"` and `"foo.rs"` resolve
    /// to the same file on every platform we care about, so dispatching
    /// both would still race.
    #[test]
    fn duplicate_detection_ignores_surrounding_whitespace() {
        let args = r#"{"files":[
            {"path":" /abs/x.rs ","instruction":"a"},
            {"path":"/abs/x.rs","instruction":"b"}
        ]}"#;
        let err = tool().validate_args(args).unwrap_err();
        assert!(err.contains("appears 2 times"), "got: {}", err);
    }

    // ── dedup-suffix logic ──

    #[test]
    fn dedup_suffix_empty_for_unique_paths() {
        let infos = super::build_task_infos_with_dedup(&[
            "src/server/api.rs",
            "src/client/mod.rs",
            "src/server/mod.rs",
        ]);
        for i in &infos {
            assert_eq!(i.dedup_suffix, "", "{} should be unique", i.path);
        }
    }

    #[test]
    fn dedup_suffix_numbers_repeats_in_order() {
        let infos = super::build_task_infos_with_dedup(&[
            "src/server/tunnel.rs",
            "src/client/tunnel.rs",
            "src/server/tunnel.rs",
            "src/server/tunnel.rs",
        ]);
        assert_eq!(infos[0].dedup_suffix, " (#1)");
        assert_eq!(infos[1].dedup_suffix, "");
        assert_eq!(infos[2].dedup_suffix, " (#2)");
        assert_eq!(infos[3].dedup_suffix, " (#3)");
    }

    #[test]
    fn dedup_suffix_preserves_input_order() {
        // Index in returned vec must align with the input — the dispatcher
        // emits `SubAgentTaskStarted { index: N }` events that the UI
        // resolves by indexing into this vec.
        let paths = ["a.rs", "b.rs", "a.rs"];
        let infos = super::build_task_infos_with_dedup(&paths);
        assert_eq!(infos.len(), 3);
        assert_eq!(infos[0].path, "a.rs");
        assert_eq!(infos[1].path, "b.rs");
        assert_eq!(infos[2].path, "a.rs");
    }
}
