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
    pub file: String,       // Short file name (e.g., "TagRebuildTaskService.java")
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
    pub fn extract_from_plan(&mut self, plan_text: &str) {
        let mut files = Vec::new();
        let mut seen = HashSet::new();

        // Extract file names from text (*.java, *.vue, *.ts, *.py, etc.)
        for word in plan_text.split(|c: char| c.is_whitespace() || c == ',' || c == '`' || c == '"' || c == '\'' || c == '(' || c == ')') {
            let trimmed = word.trim().trim_matches(|c: char| c == '`' || c == '*' || c == ':');
            if trimmed.is_empty() { continue; }

            let is_source = trimmed.ends_with(".java") || trimmed.ends_with(".vue")
                || trimmed.ends_with(".ts") || trimmed.ends_with(".tsx")
                || trimmed.ends_with(".py") || trimmed.ends_with(".rs")
                || trimmed.ends_with(".go") || trimmed.ends_with(".js");

            if is_source {
                // Extract just the file name (last path component)
                let file_name = trimmed.rsplit('/').next().unwrap_or(trimmed);
                if !file_name.is_empty() && seen.insert(file_name.to_string()) {
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
            let a_backend = a.ends_with(".java") || a.ends_with(".py") || a.ends_with(".go") || a.ends_with(".rs");
            let b_backend = b.ends_with(".java") || b.ends_with(".py") || b.ends_with(".go") || b.ends_with(".rs");
            b_backend.cmp(&a_backend) // backend first
        });

        self.subtasks = files.into_iter().map(|f| Subtask { file: f, done: false }).collect();
        self.current_idx = 0;
        self.active = true;
    }

    /// Get the instruction to inject for the current subtask.
    /// Returns None if all subtasks are done or driver is inactive.
    pub fn current_instruction(&self) -> Option<String> {
        if !self.active { return None; }
        let task = self.subtasks.get(self.current_idx)?;
        if task.done { return None; }

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
            "[Subtask {}/{}: Edit {} — make ALL needed changes in ONE edit. {}]",
            self.current_idx + 1, total, task.file, next_hint,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_files_from_plan() {
        let plan = "我计划修改以下文件：
1. TagRebuildTaskService.java — 添加 token 统计
2. AITagExtractionService.java — 返回 token 消耗
3. SettingsView.vue — 前端显示";

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
    fn instruction_format() {
        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan("修改 TagService.java 和 SettingsView.vue");

        let instr = driver.current_instruction().unwrap();
        assert!(instr.contains("Subtask 1/2"));
        assert!(instr.contains("TagService.java"));
        assert!(instr.contains("ONE edit"));
    }

    #[test]
    fn advance_through_subtasks() {
        let mut driver = SubtaskDriver::new();
        driver.extract_from_plan("修改 A.java 和 B.vue");

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
        driver.extract_from_plan("我觉得需要修改一些代码");
        assert!(!driver.active);
    }
}
