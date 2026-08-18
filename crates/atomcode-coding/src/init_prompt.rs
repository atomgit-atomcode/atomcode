//! Prompt submitted by `/init`: the driver selects the current UI language and
//! may append user-owned requirements from a configured UTF-8 file.

use std::path::{Path, PathBuf};
use std::{fs::File, io::Read};

use atomcode_config::locale::Locale;

const MAX_CUSTOM_INIT_PROMPT_BYTES: u64 = 64 * 1024;

/// English fallback retained for callers that have not migrated to
/// [`build_init_prompt`].
pub const INIT_PROMPT: &str = "\
Analyze this repository and create (or improve) an `AGENTS.md` file at the project root \
that helps an AI coding agent work in this codebase.

Explore first: identify the build system, the exact build / test / lint / format commands, \
the top-level directory layout and architecture, key conventions, and any NON-OBVIOUS \
gotchas a newcomer would trip on.

Write the result with `write_file`, keeping it concise (~200-400 words), actionable, and \
focused on non-obvious, project-specific information — do NOT include generic advice like \
\"follow existing patterns\" or \"write tests\".

IMPORTANT — pick the RIGHT file: check for an existing project instruction file in this \
precedence order — `.atomcode.md`, `AGENTS.md`, `CLAUDE.md`. If ONE already EXISTS, read it \
and improve THAT SAME file in place (it is the file the agent actually loads — writing a \
different filename would be shadowed and never take effect): preserve the useful content, \
fill gaps, and fix anything stale; do NOT wipe and rewrite it from scratch. Only if NONE of \
those files exists, create a new `AGENTS.md` at the project root.

Include this maintenance rule in the generated instruction file: when project structure, \
build/test commands, architecture boundaries, development conventions, or other facts \
documented there change, update this instruction file in the same change.";

pub const INIT_PROMPT_ZH_CN: &str = "\
分析当前代码仓库，并在项目根目录创建（或完善）`AGENTS.md`，帮助 AI 编程代理正确地在该项目中工作。

先进行调研：识别构建系统，准确的构建、测试、检查和格式化命令，顶层目录与架构，关键开发约定，
以及新参与者容易忽略的、项目特有的注意事项。

使用 `write_file` 写入结果。内容保持简洁（约 200～400 字）、可执行，并聚焦于不明显的项目特有信息；
不要写“遵循现有模式”“编写测试”之类通用建议。

重要——选择正确的文件：按 `.atomcode.md`、`AGENTS.md`、`CLAUDE.md` 的优先顺序检查已有项目指令文件。
如果其中一个已存在，先读取并原地完善同一个文件（代理实际加载的是该文件，另写其他文件会被遮蔽而不生效）；
保留有用内容、补齐缺失信息并修正过期内容，不要清空后重写。仅当这些文件都不存在时，才在项目根目录创建
新的 `AGENTS.md`。

在生成的项目指令中加入以下维护规则：当项目结构、构建测试命令、架构边界、开发约定，或该指令文件中
记录的其他事实发生变化时，必须在同一次改动中同步更新该指令文件。最终文件使用简体中文编写。";

/// Build the effective `/init` prompt. Custom content is appended instead of
/// replacing the built-in contract, so file-selection and preservation rules
/// cannot be accidentally removed by configuration.
pub fn build_init_prompt(locale: Locale, custom_file: Option<&Path>) -> Result<String, String> {
    let builtin = match locale {
        Locale::En => INIT_PROMPT,
        Locale::ZhCn => INIT_PROMPT_ZH_CN,
    };
    let Some(custom_file) = custom_file else {
        return Ok(builtin.to_string());
    };
    let path = resolve_custom_prompt_path(custom_file);
    let file = File::open(&path).map_err(|error| {
        format!(
            "failed to read custom /init prompt {}: {error}",
            path.display()
        )
    })?;
    let size = file
        .metadata()
        .map_err(|error| {
            format!(
                "failed to inspect custom /init prompt {}: {error}",
                path.display()
            )
        })?
        .len();
    if size > MAX_CUSTOM_INIT_PROMPT_BYTES {
        return Err(custom_prompt_too_large(&path, size));
    }
    // Keep a read-time limit as well as the metadata check: the file may be
    // replaced or extended between those operations.
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(MAX_CUSTOM_INIT_PROMPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "failed to read custom /init prompt {}: {error}",
                path.display()
            )
        })?;
    if bytes.len() as u64 > MAX_CUSTOM_INIT_PROMPT_BYTES {
        return Err(custom_prompt_too_large(&path, bytes.len() as u64));
    }
    let custom = String::from_utf8(bytes).map_err(|error| {
        format!(
            "custom /init prompt must be UTF-8 ({}): {error}",
            path.display()
        )
    })?;
    let custom = custom.trim();
    if custom.is_empty() {
        return Err(format!("custom /init prompt is empty: {}", path.display()));
    }
    Ok(format!(
        "{builtin}\n\n=== CUSTOM /init REQUIREMENTS ===\n{custom}"
    ))
}

fn custom_prompt_too_large(path: &Path, actual: u64) -> String {
    format!(
        "custom /init prompt is too large: {} is {actual} bytes; maximum is {MAX_CUSTOM_INIT_PROMPT_BYTES} bytes",
        path.display()
    )
}

fn resolve_custom_prompt_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let raw = path.to_string_lossy();
    if raw == "~" {
        return atomcode_config::util::real_home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = atomcode_config::util::real_home_dir() {
            return home.join(rest);
        }
    }
    atomcode_config::Config::config_dir().join(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_follow_locale_and_keep_maintenance_contract() {
        let en = build_init_prompt(Locale::En, None).unwrap();
        let zh = build_init_prompt(Locale::ZhCn, None).unwrap();
        assert!(en.contains("AGENTS.md"));
        assert!(en.contains("update this instruction file"));
        assert!(zh.contains("简体中文"));
        assert!(zh.contains("同步更新该指令文件"));
    }

    #[test]
    fn custom_requirements_are_appended_without_replacing_builtin_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.md");
        std::fs::write(&path, "额外检查数据库迁移命令").unwrap();
        let prompt = build_init_prompt(Locale::ZhCn, Some(&path)).unwrap();
        assert!(prompt.contains("选择正确的文件"));
        assert!(prompt.contains("额外检查数据库迁移命令"));
    }

    #[test]
    fn empty_custom_prompt_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.md");
        std::fs::write(&path, " \n").unwrap();
        assert!(build_init_prompt(Locale::En, Some(&path))
            .unwrap_err()
            .to_string()
            .contains("is empty"));
    }

    #[test]
    fn oversized_custom_prompt_is_rejected_before_submission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.md");
        std::fs::write(&path, vec![b'x'; MAX_CUSTOM_INIT_PROMPT_BYTES as usize + 1]).unwrap();
        let error = build_init_prompt(Locale::En, Some(&path)).unwrap_err();
        assert!(error.contains("too large"));
        assert!(error.contains("65536"));
    }
}
