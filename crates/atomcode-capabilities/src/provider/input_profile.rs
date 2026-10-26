//! Size and tool-name diagnostics of the final provider body, never prompt contents.
use std::collections::BTreeMap;
use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

#[derive(Default)]
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn bytes(value: &Value) -> usize {
    let mut count = Counter::default();
    serde_json::to_writer(&mut count, value).expect("JSON values always serialize");
    count.0
}

const LARGEST_CORE_TOOLS: usize = 10;

#[derive(Debug, Serialize)]
pub(super) struct ToolSize {
    name: String,
    tool_json_bytes: usize,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct InputProfile {
    pub request_json_bytes: usize,
    /// Disjoint JSON byte sizes; their sum equals request_json_bytes.
    pub parts_json_bytes: BTreeMap<&'static str, usize>,
    /// Raw UTF-8 text bytes, a separate view; do not add these to JSON sizes.
    pub tagged_text_bytes: BTreeMap<&'static str, usize>,
    pub mcp_tools: usize,
    pub core_tools: usize,
    /// A bounded subset of tools_core, not another disjoint accounting category.
    pub largest_core_tools: Vec<ToolSize>,
}
fn add(map: &mut BTreeMap<&'static str, usize>, key: &'static str, n: usize) {
    *map.entry(key).or_default() += n;
}

/// Text markers are a diagnostic hint, never an authority or routing decision.
fn text_sizes(text: &str, out: &mut BTreeMap<&'static str, usize>) {
    if text.starts_with("<system-reminder>") && text.contains("\"hookEventName\": \"SessionStart\"")
    {
        add(out, "session_start_hook", text.len());
        return;
    }
    let mut category = "other";
    for line in text.split_inclusive('\n') {
        if line.starts_with("=== ") {
            category = if line.starts_with("=== AVAILABLE SKILLS ===") {
                "skills_catalog"
            } else if [
                "=== PROJECT INSTRUCTIONS",
                "=== GLOBAL INSTRUCTIONS",
                "=== USER INSTRUCTIONS",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            {
                "project_and_user_instructions"
            } else if line.starts_with("=== MEMORY ===") {
                "memory"
            } else {
                "other"
            };
        }
        add(out, category, line.len());
    }
}
fn content_text(value: &Value, out: &mut BTreeMap<&'static str, usize>) {
    match value {
        Value::String(text) => text_sizes(text, out),
        Value::Array(blocks) => {
            for block in blocks {
                // No traversal of arbitrary JSON/tool arguments or image payloads.
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_sizes(text, out);
                }
            }
        }
        _ => {}
    }
}

fn message_category(message: &Value) -> &'static str {
    match message.get("role").and_then(Value::as_str) {
        Some("system" | "developer") => "system",
        Some("assistant") => "assistant_messages",
        Some("tool") => "tool_results",
        Some("user") => {
            // Anthropic represents tool results as user-role content blocks.
            let blocks = message.get("content").and_then(Value::as_array);
            if let Some(blocks) = blocks {
                let results = blocks
                    .iter()
                    .filter(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                    .count();
                if results > 0 {
                    return if results == blocks.len() {
                        "tool_results"
                    } else {
                        "mixed_user_tool_results"
                    };
                }
            }
            "user_messages"
        }
        _ if message.get("type").and_then(Value::as_str) == Some("function_call_output") => {
            "tool_results"
        }
        _ => "other_messages",
    }
}

pub(super) fn profile(body: &Value) -> InputProfile {
    let mut result = InputProfile {
        request_json_bytes: bytes(body),
        ..Default::default()
    };
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            let n = bytes(tool);
            let name = tool
                .pointer("/function/name")
                .or_else(|| tool.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let category = if name.starts_with("mcp__") {
                result.mcp_tools += 1;
                "tools_mcp"
            } else {
                result.core_tools += 1;
                result.largest_core_tools.push(ToolSize {
                    name: name.to_owned(),
                    tool_json_bytes: n,
                });
                "tools_core"
            };
            add(&mut result.parts_json_bytes, category, n);
        }
        add(
            &mut result.parts_json_bytes,
            "tools_array_framing",
            2 + tools.len().saturating_sub(1),
        );
    }
    result.largest_core_tools.sort_by(|a, b| {
        b.tool_json_bytes
            .cmp(&a.tool_json_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    result.largest_core_tools.truncate(LARGEST_CORE_TOOLS);
    for key in ["system", "instructions"] {
        if let Some(value) = body.get(key) {
            add(&mut result.parts_json_bytes, "system", bytes(value));
            content_text(value, &mut result.tagged_text_bytes);
        }
    }
    for key in ["messages", "input"] {
        let Some(value) = body.get(key) else { continue };
        if let Some(messages) = value.as_array() {
            for message in messages {
                let category = message_category(message);
                let n = bytes(message);
                add(&mut result.parts_json_bytes, category, n);
                if let Some(content) = message.get("content") {
                    content_text(content, &mut result.tagged_text_bytes);
                }
            }
            add(
                &mut result.parts_json_bytes,
                "messages_array_framing",
                2 + messages.len().saturating_sub(1),
            );
        } else {
            add(&mut result.parts_json_bytes, "user_messages", bytes(value));
            content_text(value, &mut result.tagged_text_bytes);
        }
    }
    let accounted: usize = result.parts_json_bytes.values().sum();
    add(
        &mut result.parts_json_bytes,
        "envelope_and_options",
        result.request_json_bytes - accounted,
    );
    result
}

pub(super) fn log(model: &str, body: &Value) {
    if tracing::enabled!(tracing::Level::INFO) {
        let size = profile(body);
        tracing::info!(model, input_profile = ?size, "provider input size breakdown (JSON bytes, not tokens; text tags are marker-based)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_accounting_works_across_provider_wire_shapes_without_recording_content() {
        let secret = "PRIVATE-PROMPT-DO-NOT-LOG";
        let bodies = [
            json!({"model":"test", "messages":[{"role":"system","content":secret},{"role":"user","content":"你是谁"},{"role":"tool","content":"result","tool_call_id":"1"}],"tools":[{"type":"function","function":{"name":"mcp__demo__read","parameters":{"type":"object"}}},{"type":"function","function":{"name":"read_file"}}]}),
            json!({"system":[{"type":"text","text":secret}],"messages":[{"role":"user","content":[{"type":"text","text":"你好"},{"type":"image","source":{"data":"not text"}}]}],"tools":[{"name":"mcp__demo__read","input_schema":{"type":"object"}}]}),
            json!({"instructions":secret,"input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]},{"type":"function_call_output","call_id":"1","output":"result"}],"tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}]}),
            json!({"messages":[{"role":"system","content":secret},{"role":"user","content":"hello"}],"stream":true,"options":{"num_ctx":1000}}),
        ];
        for body in bodies {
            let report = profile(&body);
            assert_eq!(
                report.request_json_bytes,
                serde_json::to_vec(&body).unwrap().len()
            );
            assert_eq!(
                report.parts_json_bytes.values().sum::<usize>(),
                report.request_json_bytes
            );
            assert!(report.parts_json_bytes["system"] >= secret.len());
            assert!(!serde_json::to_string(&report).unwrap().contains(secret));
            assert!(!format!("{report:?}").contains(secret));
        }
    }

    #[test]
    fn largest_core_tools_are_bounded_and_measure_final_wire_definitions_without_content() {
        let secret = "PRIVATE-DESCRIPTION-AND-PARAMETERS";
        for shape in ["openai", "anthropic", "responses"] {
            let make = |name: &str, padding: usize| {
                let schema = json!({"type":"object","properties":{"value":{"type":"string","description":secret}}});
                let description = format!("{secret}{}", "x".repeat(padding));
                match shape {
                    "openai" => {
                        json!({"type":"function","function":{"name":name,"description":description,"parameters":schema}})
                    }
                    "anthropic" => {
                        json!({"name":name,"description":description,"input_schema":schema})
                    }
                    _ => {
                        json!({"type":"function","name":name,"description":description,"parameters":schema})
                    }
                }
            };
            let mut tools = (0..15)
                .map(|i| make(&format!("native_{i:02}"), i * 20))
                .collect::<Vec<_>>();
            tools.push(make("mcp__demo__huge", 20_000));
            let report = profile(&json!({"tools":tools}));
            assert_eq!(report.core_tools, 15);
            assert_eq!(report.mcp_tools, 1);
            assert_eq!(report.largest_core_tools.len(), 10);
            for (size, original) in report
                .largest_core_tools
                .iter()
                .zip(tools[..15].iter().rev())
            {
                let original_name = original
                    .pointer("/function/name")
                    .or_else(|| original.get("name"))
                    .unwrap();
                assert_eq!(size.name, original_name.as_str().unwrap());
                assert_eq!(size.tool_json_bytes, bytes(original));
            }
            assert_eq!(report.largest_core_tools.last().unwrap().name, "native_05");
            assert!(!serde_json::to_string(&report).unwrap().contains(secret));
            assert!(!format!("{report:?}").contains(secret));
            assert_eq!(
                report.parts_json_bytes.values().sum::<usize>(),
                report.request_json_bytes
            );
        }
        let tied = profile(&json!({"tools":[{"name":"zz"},{"name":"aa"},{"name":"bb"}]}));
        assert_eq!(
            tied.largest_core_tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["aa", "bb", "zz"]
        );
        assert!(profile(&json!({"tools":[]})).largest_core_tools.is_empty());
    }

    #[test]
    fn anthropic_tool_results_are_not_counted_as_plain_user_input() {
        let tool_only = serde_json::json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"result"}]});
        let mixed = serde_json::json!({"role":"user","content":[{"type":"text","text":"follow-up"},{"type":"tool_result","tool_use_id":"a","content":"result"}]});
        let normal = serde_json::json!({"role":"user","content":[]});
        let report = profile(
            &serde_json::json!({"messages":[tool_only.clone(),mixed.clone(),normal.clone()]}),
        );
        assert_eq!(report.parts_json_bytes["tool_results"], bytes(&tool_only));
        assert_eq!(
            report.parts_json_bytes["mixed_user_tool_results"],
            bytes(&mixed)
        );
        assert_eq!(report.parts_json_bytes["user_messages"], bytes(&normal));
        assert_eq!(
            report.parts_json_bytes.values().sum::<usize>(),
            report.request_json_bytes
        );
    }

    #[test]
    fn tagged_text_is_a_separate_utf8_view_with_no_double_counting() {
        let project = "=== PROJECT INSTRUCTIONS (/repo/AGENTS.md) ===\n开发规则\n";
        let skills = "=== AVAILABLE SKILLS ===\n- demo: description\n";
        let other = "=== SESSION CONTEXT ===\nplatform\n";
        let memory = "=== MEMORY ===\n事实\n";
        let hook = "<system-reminder>\n{\"hookEventName\": \"SessionStart\",\"additionalContext\":\"private\"}";
        let body = json!({"messages":[
            {"role":"system","content":format!("{other}{project}{skills}")},
            {"role":"user","content":memory}, {"role":"user","content":hook}
        ],"tools":[]});
        let report = profile(&body);
        assert_eq!(
            report.tagged_text_bytes["project_and_user_instructions"],
            project.len()
        );
        assert_eq!(report.tagged_text_bytes["skills_catalog"], skills.len());
        assert_eq!(report.tagged_text_bytes["memory"], memory.len());
        assert_eq!(report.tagged_text_bytes["session_start_hook"], hook.len());
        assert_eq!(
            report.tagged_text_bytes.values().sum::<usize>(),
            other.len() + project.len() + skills.len() + memory.len() + hook.len()
        );
    }

    #[test]
    fn mcp_and_core_schema_sizes_are_split_after_projection() {
        let mcp = json!({"type":"function","function":{"name":"mcp__demo__read","description":"x".repeat(5000)}});
        let core = json!({"type":"function","function":{"name":"read_file"}});
        let eager = profile(&json!({"tools":[mcp.clone(),core.clone()]}));
        let lazy = profile(&json!({"tools":[core.clone()]}));
        assert_eq!(eager.mcp_tools, 1);
        assert_eq!(eager.core_tools, 1);
        assert_eq!(lazy.mcp_tools, 0);
        assert_eq!(eager.parts_json_bytes["tools_mcp"], bytes(&mcp));
        assert_eq!(
            eager.parts_json_bytes["tools_core"],
            lazy.parts_json_bytes["tools_core"]
        );
        assert!(eager.request_json_bytes > lazy.request_json_bytes + 5000);
    }
}
