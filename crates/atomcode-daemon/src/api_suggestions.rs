//! GET /project/suggestions — 新对话落地页的动态项目建议。
//!
//! 采集当前 working_dir 的项目上下文（git 快照 + 顶层文件 + 项目类型标志），
//! 调用默认 provider 让 LLM 生成 4 条具体可执行的工作建议，按 working_dir
//! 缓存（`refresh=true` 绕过）。任何失败（无 provider / API 出错 / 解析失败）
//! 都返回空数组 + HTTP 200，由前端回退到静态兜底建议。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use atomcode_core::config::Config;
use atomcode_core::conversation::message::{Message, Role};
use atomcode_core::ctx::EnvSnapshot;
use atomcode_core::provider;
use atomcode_core::stream::StreamEvent;

use crate::AppState;

/// 进程内缓存：working_dir -> 已生成的建议。
pub type SuggestionsCache = Arc<Mutex<HashMap<PathBuf, Vec<Suggestion>>>>;

/// 返回的建议条数上限。
const SUGGESTIONS_COUNT: usize = 4;

/// LLM 调用的整体超时（建议生成应当很快；超时即回退到空数组）。
const SUGGESTIONS_TIMEOUT: Duration = Duration::from_secs(45);

/// 顶层条目列举的数量上限，避免大目录撑爆 prompt。
const TOP_LEVEL_MAX: usize = 40;

/// 常见项目类型的标志文件 -> 人类可读标签。
const PROJECT_MARKERS: &[(&str, &str)] = &[
    ("package.json", "Node.js/前端"),
    ("Cargo.toml", "Rust"),
    ("go.mod", "Go"),
    ("pyproject.toml", "Python"),
    ("requirements.txt", "Python"),
    ("pom.xml", "Java/Maven"),
    ("build.gradle", "Java/Gradle"),
    ("Gemfile", "Ruby"),
    ("composer.json", "PHP"),
    ("CMakeLists.txt", "C/C++/CMake"),
];

/// 单条建议：`label` 是按钮短文案，`prompt` 是点击后填入输入框的完整首条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub label: String,
    pub prompt: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct SuggestionsResponse {
    pub working_dir: String,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SuggestionsQuery {
    /// 跳过缓存，强制重新生成。
    #[serde(default)]
    pub refresh: bool,
}

/// GET /project/suggestions
pub(crate) async fn get_project_suggestions(
    State(state): State<AppState>,
    Query(q): Query<SuggestionsQuery>,
) -> impl IntoResponse {
    let working_dir = { state.project.read().await.working_dir.clone() };

    if !q.refresh {
        if let Some(cached) = state.suggestions_cache.lock().await.get(&working_dir) {
            return Json(SuggestionsResponse {
                working_dir: working_dir.display().to_string(),
                suggestions: cached.clone(),
            });
        }
    }

    let suggestions = generate_suggestions(&working_dir).await;

    // 只缓存非空结果：瞬时失败不应被钉死，下次进页可重试。
    if !suggestions.is_empty() {
        state
            .suggestions_cache
            .lock()
            .await
            .insert(working_dir.clone(), suggestions.clone());
    }

    Json(SuggestionsResponse {
        working_dir: working_dir.display().to_string(),
        suggestions,
    })
}

/// 调用默认 provider 生成建议；任何失败都返回空 vec。
async fn generate_suggestions(working_dir: &Path) -> Vec<Suggestion> {
    let config = match Config::load(&Config::default_path()) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let provider_config = match config.providers.get(&config.default_provider) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let provider = match provider::create_provider(provider_config) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let messages = build_suggestions_messages(working_dir);

    let collect = async {
        let mut stream = match provider.chat_stream(&messages, None) {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StreamEvent::Delta(d)) => text.push_str(&d),
                Ok(StreamEvent::Done { .. }) => break,
                Ok(StreamEvent::Error(_)) | Err(_) => break,
                _ => {}
            }
        }
        text
    };

    match tokio::time::timeout(SUGGESTIONS_TIMEOUT, collect).await {
        Ok(raw) => parse_suggestions(&raw),
        Err(_) => Vec::new(),
    }
}

/// 拼装喂给 LLM 的系统 + 用户消息。
fn build_suggestions_messages(working_dir: &Path) -> Vec<Message> {
    let dir_name = working_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| working_dir.display().to_string());

    let mut ctx = String::new();
    ctx.push_str(&format!("项目目录名: {}\n", dir_name));
    ctx.push_str(&format!("路径: {}\n", working_dir.display()));

    let markers = detect_project_markers(working_dir);
    if !markers.is_empty() {
        ctx.push_str(&format!("项目类型标志: {}\n", markers.join(", ")));
    }

    let entries = list_top_level(working_dir);
    if !entries.is_empty() {
        ctx.push_str(&format!("顶层条目: {}\n", entries.join(", ")));
    }

    let git = EnvSnapshot::capture(working_dir).as_prompt_section();
    if !git.is_empty() {
        ctx.push_str(&git);
    }

    let system = "你是 AtomCode 的编码助手。根据给定的项目上下文，为开发者推荐 4 个此刻最可能想做的、\
具体可执行的任务。\n\
只返回一个 JSON 数组，不要任何额外文字、解释或代码围栏。每个元素形如 \
{\"label\": \"...\", \"prompt\": \"...\"}：\n\
- label：2-6 个字的中文按钮文案（如「修复未提交改动」「补单元测试」「解释项目结构」）。\n\
- prompt：一条完整、可直接发送的中文首条消息，描述要做的事，必要时引用上下文中的具体文件/分支。\n\
建议要贴合该项目的实际状态（git 改动、项目类型、文件结构），避免空泛套话。";

    vec![
        Message::new(Role::System, system),
        Message::new(
            Role::User,
            format!("项目上下文：\n{}\n\n请给出 4 条建议。", ctx),
        ),
    ]
}

/// 检测目录中存在哪些项目类型标志文件。
fn detect_project_markers(dir: &Path) -> Vec<&'static str> {
    PROJECT_MARKERS
        .iter()
        .filter(|(f, _)| dir.join(f).exists())
        .map(|(_, label)| *label)
        .collect()
}

/// 列举顶层条目（跳过隐藏项），排序后截断。
fn list_top_level(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            out.push(name);
        }
    }
    out.sort();
    out.truncate(TOP_LEVEL_MAX);
    out
}

/// 从 LLM 原始输出中解析建议数组。剥离可选的 ``` 代码围栏、定位最外层
/// `[ ... ]`，过滤空条目并截断到上限。任何失败返回空 vec。
fn parse_suggestions(raw: &str) -> Vec<Suggestion> {
    let unfenced = strip_code_fence(raw.trim());
    let json = extract_json_array(unfenced).unwrap_or(unfenced);
    match serde_json::from_str::<Vec<Suggestion>>(json) {
        Ok(mut v) => {
            v.retain(|s| !s.label.trim().is_empty() && !s.prompt.trim().is_empty());
            v.truncate(SUGGESTIONS_COUNT);
            v
        }
        Err(_) => Vec::new(),
    }
}

/// 剥离 ```lang ... ``` 围栏；无围栏时原样返回。
fn strip_code_fence(s: &str) -> &str {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // 丢掉围栏首行（``` 或 ```json），再去掉结尾的 ```。
    let rest = rest.splitn(2, '\n').nth(1).unwrap_or("").trim_end();
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

/// 截取最外层 `[ ... ]`（模型可能在数组前后夹带散文）。
fn extract_json_array(s: &str) -> Option<&str> {
    let start = s.find('[')?;
    let end = s.rfind(']')?;
    (end > start).then(|| &s[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json_array() {
        let raw = r#"[{"label":"A","prompt":"do a"},{"label":"B","prompt":"do b"}]"#;
        let v = parse_suggestions(raw);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].label, "A");
        assert_eq!(v[1].prompt, "do b");
    }

    #[test]
    fn strips_json_code_fence() {
        let raw = "```json\n[{\"label\":\"A\",\"prompt\":\"do a\"}]\n```";
        let v = parse_suggestions(raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].prompt, "do a");
    }

    #[test]
    fn strips_bare_code_fence() {
        let raw = "```\n[{\"label\":\"X\",\"prompt\":\"y\"}]\n```";
        let v = parse_suggestions(raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].label, "X");
    }

    #[test]
    fn extracts_array_amid_prose() {
        let raw = "Here you go:\n[{\"label\":\"A\",\"prompt\":\"x\"}]\nHope that helps";
        let v = parse_suggestions(raw);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn drops_empty_entries_and_truncates_to_four() {
        let raw = r#"[
            {"label":"","prompt":"x"},
            {"label":"A","prompt":"a"},
            {"label":"B","prompt":"b"},
            {"label":"C","prompt":"c"},
            {"label":"D","prompt":"d"},
            {"label":"E","prompt":"e"}
        ]"#;
        let v = parse_suggestions(raw);
        assert_eq!(v.len(), 4); // 空 label 丢弃，剩 5 条截断到 4
        assert_eq!(v[0].label, "A");
        assert_eq!(v[3].label, "D");
    }

    #[test]
    fn invalid_input_returns_empty() {
        assert!(parse_suggestions("not json at all").is_empty());
        assert!(parse_suggestions("").is_empty());
        assert!(parse_suggestions("{\"label\":\"A\"}").is_empty()); // 对象而非数组
    }

    #[test]
    fn detects_rust_marker_in_repo_root() {
        // 本 crate 所在 workspace 根含 Cargo.toml；用 CARGO_MANIFEST_DIR 定位。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(detect_project_markers(dir).contains(&"Rust"));
    }
}
