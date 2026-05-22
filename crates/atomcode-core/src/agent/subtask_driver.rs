//! ATLAS-style subtask decomposition driver.
//!
//! After the model outputs a plan (Phase 2 planning phase), the Agent
//! extracts target files and drives execution file-by-file:
//!   1. "Now edit backend/Service.java — make ALL changes in ONE edit"
//!   2. Auto-compile after each file
//!   3. If fail → model fixes (same file)
//!   4. If pass → next file
//!
//! This prevents fragmented edits (10 small changes) and catches errors early.

use std::collections::HashSet;

/// A subtask = one file to modify.
#[derive(Debug, Clone)]
pub struct Subtask {
    pub file: String, // Short file name (e.g., "TagRebuildTaskService.java")
    pub done: bool,
}

/// Driver state for subtask execution.
#[derive(Debug, Clone)]
pub struct SubtaskDriver {
    pub subtasks: Vec<Subtask>,
    pub current_idx: usize,
    pub active: bool,
}

impl SubtaskDriver {
    pub fn new() -> Self {
        Self {
            subtasks: Vec::new(),
            current_idx: 0,
            active: false,
        }
    }

    /// Extract subtasks from model's plan text.
    /// Each unique file name mentioned = one subtask.
    /// Backend files first, then frontend.
    /// Filters out files mentioned only as references (e.g., "参考 ProductCenter.vue 的风格").
    pub fn extract_from_plan(&mut self, plan_text: &str) {
        let mut files = Vec::new();
        let mut seen = HashSet::new();

        // Identify files that are only mentioned as references (not edit targets).
        // Strategy: on lines containing a reference keyword, split at the first
        // modify keyword. Files appearing BEFORE the modify keyword are references.
        // E.g., "参考 ProductCenter.vue 的风格，修改 TestCenter.vue"
        //        ^^^^ reference portion ^^^^     ^^^^ modify portion ^^^^
        let reference_files = extract_reference_files(plan_text);

        // Extract all file names, skip those that are reference-only.
        // Splitter must include Chinese full-width punctuation — without it
        // a sentence like "constants.rs，types.rs。platform.rs 和 mod.rs"
        // tokenizes into ONE giant string ending in `.rs` (passes
        // is_source_file but is unfindable on disk). 2026-05-03 datalog
        // showed this collapsing 4 valid files into 1 broken path,
        // making sub-agent dispatch silently fall back to serial.
        for word in plan_text.split(|c: char| {
            c.is_whitespace()
                || c == ','
                || c == '`'
                || c == '"'
                || c == '\''
                || c == '('
                || c == ')'
                || c == '['
                || c == ']'
                // Chinese full-width punctuation
                || c == '\u{FF0C}' // ，
                || c == '\u{3002}' // 。
                || c == '\u{3001}' // 、
                || c == '\u{FF1B}' // ；
                || c == '\u{FF1A}' // ：
                || c == '\u{FF08}' // （
                || c == '\u{FF09}' // ）
                || c == '\u{300A}' // 《
                || c == '\u{300B}' // 》
                || c == '\u{300C}' // 「
                || c == '\u{300D}' // 」
                || c == '\u{FF1F}' // ？
                || c == '\u{FF01}' // ！
                || c == '\u{2014}' // —
        }) {
            let trimmed = word
                .trim()
                .trim_matches(|c: char| c == '`' || c == '*' || c == ':');
            if trimmed.is_empty() {
                continue;
            }

            if is_source_file(trimmed) {
                let file_name = trimmed.rsplit('/').next().unwrap_or(trimmed);
                if !file_name.is_empty()
                    && seen.insert(file_name.to_string())
                    && !reference_files.contains(file_name)
                {
                    files.push(file_name.to_string());
                }
            }
        }

        if files.is_empty() {
            self.active = false;
            return;
        }

        // Sort: backend files first (.java), then frontend (.vue/.ts/.js)
        files.sort_by(|a, b| {
            let a_backend = a.ends_with(".java")
                || a.ends_with(".py")
                || a.ends_with(".go")
                || a.ends_with(".rs");
            let b_backend = b.ends_with(".java")
                || b.ends_with(".py")
                || b.ends_with(".go")
                || b.ends_with(".rs");
            b_backend.cmp(&a_backend) // backend first
        });

        self.subtasks = files
            .into_iter()
            .map(|f| Subtask {
                file: f,
                done: false,
            })
            .collect();
        self.current_idx = 0;
        self.active = true;
    }

    /// Get the instruction to inject for the current subtask.
    /// Returns None if all subtasks are done or driver is inactive.
    pub fn current_instruction(&self) -> Option<String> {
        if !self.active {
            return None;
        }
        let task = self.subtasks.get(self.current_idx)?;
        if task.done {
            return None;
        }

        let total = self.subtasks.len();
        let remaining: Vec<&str> = self.subtasks[self.current_idx + 1..]
            .iter()
            .filter(|t| !t.done)
            .map(|t| t.file.as_str())
            .collect();

        let next_hint = if remaining.is_empty() {
            "This is the last file.".to_string()
        } else {
            format!("After this: {}", remaining.join(", "))
        };

        Some(format!(
            "[Subtask {}/{}: Edit {} \u{2014} make ALL needed changes in ONE edit. {}]",
            self.current_idx + 1,
            total,
            task.file,
            next_hint,
        ))
    }

    /// Mark current subtask as done, advance to next.
    pub fn advance(&mut self) {
        if let Some(task) = self.subtasks.get_mut(self.current_idx) {
            task.done = true;
        }
        self.current_idx += 1;
        if self.current_idx >= self.subtasks.len() {
            self.active = false;
        }
    }

    /// Check if an edited file matches the current subtask.
    pub fn matches_current(&self, edited_file: &str) -> bool {
        if let Some(task) = self.subtasks.get(self.current_idx) {
            edited_file.contains(&task.file) || task.file.contains(edited_file)
        } else {
            false
        }
    }

    /// Check if all subtasks are done.
    pub fn all_done(&self) -> bool {
        self.subtasks.iter().all(|t| t.done)
    }
}

/// Check if a string looks like a source file name.
fn is_source_file(s: &str) -> bool {
    s.ends_with(".java")
        || s.ends_with(".vue")
        || s.ends_with(".ts")
        || s.ends_with(".tsx")
        || s.ends_with(".py")
        || s.ends_with(".rs")
        || s.ends_with(".go")
        || s.ends_with(".js")
        || s.ends_with(".svelte")
}

/// Extract file names that appear in "reference" context only.
/// Returns a set of file names that should NOT be treated as edit targets.
fn extract_reference_files(plan_text: &str) -> HashSet<String> {
    let mut refs = HashSet::new();

    let ref_kw: &[&str] = &[
        "\u{53C2}\u{8003}", // 参考
        "\u{53C2}\u{7167}", // 参照
        "\u{4EFF}\u{7167}", // 仿照
        "\u{7C7B}\u{4F3C}", // 类似
        "reference",
        "following",
        "same as",
        "style of",
        "follow",
    ];
    let modify_kw: &[&str] = &[
        "\u{4FEE}\u{6539}", // 修改
        "\u{7F16}\u{8F91}", // 编辑
        "\u{66F4}\u{65B0}", // 更新
        "\u{6DFB}\u{52A0}", // 添加
        "\u{5B9E}\u{73B0}", // 实现
        "\u{6539}",         // 改
        "modify",
        "edit",
        "update",
        "add",
        "change",
        "implement",
    ];

    for line in plan_text.lines() {
        let lower = line.to_lowercase();
        let has_ref = ref_kw.iter().any(|k| lower.contains(k));
        if !has_ref {
            continue;
        }

        // Find the byte position of the earliest modify keyword
        let modify_pos = modify_kw.iter().filter_map(|k| lower.find(k)).min();

        // Reference portion: text before the first modify keyword
        let ref_portion = match modify_pos {
            Some(pos) => &line[..pos],
            None => line,
        };

        // Extract source file names from reference portion only
        for word in ref_portion.split(|c: char| {
            c.is_whitespace()
                || c == ','
                || c == '`'
                || c == '"'
                || c == '\''
                || c == '('
                || c == ')'
                || c == '\u{FF0C}'
        }) {
            let trimmed = word
                .trim()
                .trim_matches(|c: char| c == '`' || c == '*' || c == ':');
            if is_source_file(trimmed) {
                let file_name = trimmed.rsplit('/').next().unwrap_or(trimmed);
                refs.insert(file_name.to_string());
            }
        }
    }

    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-05-03 datalog: deepseek-v4-flash on atomgr emitted exactly this
    /// sentence in turn 3, with Chinese full-width comma `，` and Chinese
    /// period `。` separating file names. Pre-fix the splitter only honoured
    /// ASCII punctuation, so `types.rs，它们已经有一些中文注释但不够完整。platform.rs`
    /// became a single token ending in `.rs` — extracted as ONE bogus
    /// "filename" that didn't exist on disk, killing sub-agent dispatch.
    #[test]
    fn extract_handles_chinese_punctuation_separators() {
        let plan = "\u{73B0}\u{5728}\u{9010}\u{4E00}\u{5904}\u{7406} 4 \u{4E2A}\u{6587}\u{4EF6}\u{3002}\u{5148}\u{5904}\u{7406} constants.rs \u{548C} types.rs\u{FF0C}\u{5B83}\u{4EEC}\u{5DF2}\u{7ECF}\u{6709}\u{4E00}\u{4E9B}\u{4E2D}\u{6587}\u{6CE8}\u{91CA}\u{4F46}\u{4E0D}\u{591F}\u{5B8C}\u{6574}\u{3002}platform.rs \u{548C} mod.rs \u{4E5F}\u{9700}\u{8981}\u{8865}\u{5168}\u{3002}";

        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan(plan);

        assert!(driver.active);
        assert_eq!(driver.subtasks.len(), 4, "expected 4 .rs files extracted, got: {:?}", driver.subtasks);
        let names: Vec<&str> = driver.subtasks.iter().map(|s| s.file.as_str()).collect();
        assert!(names.contains(&"constants.rs"));
        assert!(names.contains(&"types.rs"));
        assert!(names.contains(&"platform.rs"));
        assert!(names.contains(&"mod.rs"));
        // None of the extracted names should contain garbage like "，" or "。"
        for s in &driver.subtasks {
            assert!(
                !s.file.contains('\u{FF0C}') && !s.file.contains('\u{3002}'),
                "extracted name `{}` contains Chinese punctuation — splitter missed",
                s.file
            );
        }
    }

    #[test]
    fn extract_files_from_plan() {
        let plan =
            "\u{6211}\u{8BA1}\u{5212}\u{4FEE}\u{6539}\u{4EE5}\u{4E0B}\u{6587}\u{4EF6}\u{FF1A}
1. TagRebuildTaskService.java \u{2014} \u{6DFB}\u{52A0} token \u{7EDF}\u{8BA1}
2. AITagExtractionService.java \u{2014} \u{8FD4}\u{56DE} token \u{6D88}\u{8017}
3. SettingsView.vue \u{2014} \u{524D}\u{7AEF}\u{663E}\u{793A}";

        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan(plan);

        assert!(driver.active);
        assert_eq!(driver.subtasks.len(), 3);
        // Backend first
        assert!(driver.subtasks[0].file.ends_with(".java"));
        assert!(driver.subtasks[1].file.ends_with(".java"));
        // Frontend last
        assert!(driver.subtasks[2].file.ends_with(".vue"));
    }

    #[test]
    fn reference_files_filtered_out() {
        // "\u{53C2}\u{8003} ProductCenter.vue \u{7684}\u{98CE}\u{683C}\u{FF0C}\u{4FEE}\u{6539} TestCenter.vue"
        let plan = "\u{6211}\u{5C06}\u{53C2}\u{8003} ProductCenter.vue \u{7684}\u{98CE}\u{683C}\u{FF0C}\u{4FEE}\u{6539} TestCenter.vue \u{6DFB}\u{52A0}\u{72B6}\u{6001}\u{7B5B}\u{9009}\u{529F}\u{80FD}\u{3002}";

        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan(plan);

        // Only TestCenter.vue should be extracted, not ProductCenter.vue
        assert_eq!(driver.subtasks.len(), 1);
        assert_eq!(driver.subtasks[0].file, "TestCenter.vue");
    }

    #[test]
    fn reference_file_english() {
        let plan =
            "I'll follow the style of IdeaCenter.vue and modify DevCenter.vue to add code reviews.";

        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan(plan);

        assert_eq!(driver.subtasks.len(), 1);
        assert_eq!(driver.subtasks[0].file, "DevCenter.vue");
    }

    #[test]
    fn multiple_modify_targets_no_reference() {
        let plan = "\u{4FEE}\u{6539} Service.java \u{7684}\u{63A5}\u{53E3}\u{FF0C}\u{7136}\u{540E}\u{66F4}\u{65B0} Controller.java \u{7684}\u{8C03}\u{7528}";

        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan(plan);

        assert_eq!(driver.subtasks.len(), 2);
    }

    #[test]
    fn instruction_format() {
        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan("\u{4FEE}\u{6539} TagService.java \u{548C} SettingsView.vue");

        let instr = driver.current_instruction().unwrap();
        assert!(instr.contains("Subtask 1/2"));
        assert!(instr.contains("TagService.java"));
        assert!(instr.contains("ONE edit"));
    }

    #[test]
    fn advance_through_subtasks() {
        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan("\u{4FEE}\u{6539} A.java \u{548C} B.vue");

        assert_eq!(driver.current_idx, 0);
        driver.advance();
        assert_eq!(driver.current_idx, 1);
        driver.advance();
        assert!(driver.all_done());
        assert!(!driver.active);
    }

    #[test]
    fn empty_plan_no_subtasks() {
        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan("\u{6211}\u{89C9}\u{5F97}\u{9700}\u{8981}\u{4FEE}\u{6539}\u{4E00}\u{4E9B}\u{4EE3}\u{7801}");
        assert!(!driver.active);
    }
}
