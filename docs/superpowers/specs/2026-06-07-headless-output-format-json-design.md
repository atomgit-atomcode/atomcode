# Headless `--output-format json` / `stream-json`

## 背景与目标

AtomCode 的 headless(非交互)模式当前只把助手文本写到 stdout(Claude Code `-p` 风格),诊断信息走 stderr。本设计为 headless 模式新增机器可读的 JSON 输出,便于脚本、CI、上层 UI 消费 agent 运行过程与结果。

新增 CLI flag:

```
--output-format <text|json|stream-json>   # 默认 text
```

- `text`(默认):**现状不变** —— stdout 只出助手文本。
- `stream-json`:NDJSON,逐事件每行一个 JSON,首行 `init`、末行 `result`。
- `json`:同一事件流,但只缓冲,跑完输出**单个** `result` 对象(= stream-json 的终态折叠)。

**非目标 / YAGNI**:
- 不做交互(TUI)模式的 JSON 输出。
- 不做 `--input-format stream-json`(双向流式喂消息)。
- 不做 Claude Agent SDK 兼容垫片(`claude-stream-json`)—— 真有需求再说。

## 设计取向:为什么不 1:1 抄 Claude Code

没有跨厂商的 agent CLI JSON 流式标准;Claude Code、opencode 各自发明各自的。

- **Claude Code** 把内层 `message` 直接透传 Anthropic Messages API 原生结构,是因为它**只跑 Anthropic 模型**。1:1 抄它能换来的唯一好处是"输出可喂 `@anthropic-ai/claude-code` SDK"——而 AtomCode 用户基本不会这么用。
- **AtomCode 是多 provider**(Claude / OpenAI 风格 / DeepSeek-R1 等)。强行把所有东西塞进 Anthropic content-block 形状,会对非 Anthropic 模型做有损/别扭的归一化,并把 schema 绑死在 Anthropic API 形状上。
- **opencode**(同为多 provider 包装器)也没抄,走的是**扁平自定义事件**。AtomCode 定位上更像 opencode。

**结论(两轴拆分)**:
- 外层信封/分帧 —— **借 Claude Code 的成熟礼仪**:NDJSON、首行 `init`、末行 `result`、`is_error` 布尔约定。
- 内层 payload —— **扁平、provider 中立的 AtomCode 原生字段**,从 `AgentEvent` 1:1 映射,零有损转换。
- format 命名 —— 仍用 `text` / `json` / `stream-json`,对齐 Claude Code 降低认知成本。

## 架构与改动范围

只改 `crates/atomcode-cli/src/main.rs`。`atomcode-core` / provider / tui **零改动**。

挂载点:`run_headless`(`main.rs:1696`)已经在 `while let Some(event) = event_rx.recv().await` 里逐个消费 `AgentEvent`(`atomcode-core/src/agent/mod.rs:190`)——这正是把事件流投影成 JSON 的天然位置。

### 1. CLI flag

在 `Cli` struct(`main.rs:427`,`verbose` 字段之后)新增:

```rust
/// Output format for headless mode. `text` (default) prints only the
/// assistant reply (Claude Code -p style). `json` emits a single aggregate
/// result object at the end. `stream-json` emits one JSON event per line
/// (NDJSON) as the turn progresses.
#[arg(long, value_enum, default_value_t = OutputFormat::Text)]
output_format: OutputFormat,
```

枚举(放在 `Cli` 附近):

```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    StreamJson,
}
```

### 2. 透传

- 调用点 `main.rs:1531` 增参 `cli.output_format`。
- `run_headless` 签名(`main.rs:1696`)增参 `output_format: OutputFormat`。
- `session_id`:`AgentLoop.session_id`(`agent/mod.rs:507` 公开字段)在 `tokio::spawn(agent_loop.run())`(`main.rs:~1737`)**之前** `clone()` 出来,因为 `agent_loop` 随后被 move 进 spawn。

### 3. 事件投影

`run_headless` 现有的 `print!`/`eprintln!` 逻辑是 `Text` 模式专属。重构为:每个 `AgentEvent` 按 `output_format` 选择写法。

- `Text`:保持现状(stdout 助手文本 + verbose stderr 日志)。
- `StreamJson`:对每个事件调 `format_json_event`,有输出就 `println!` 到 stdout 并 flush。**stderr 的 verbose 诊断日志全部抑制**(stdout 只许出 JSON)。
- `Json`:同 `StreamJson` 的事件流,但不逐行输出;`text_delta` 累加进 buffer,`usage` 累计,在终态(`TurnComplete`/`Error`/`TurnCancelled`)输出**单个** `result` 对象。

## JSON 事件 schema(扁平、provider 中立、全集)

所有事件每行一个 JSON 对象,均含 `type` 字段。

| AgentEvent | JSON 行 |
|---|---|
| (循环开始,发一次) | `{"type":"init","session_id":<id>,"model":<model>,"cwd":<path>,"tools":[<name>...]}` |
| `TextDelta(t)` | `{"type":"text_delta","text":t}` |
| `ReasoningDelta(t)` | `{"type":"reasoning_delta","text":t}` |
| `ToolCallStarted{id,name,arguments}` | `{"type":"tool_use","id":id,"name":name,"input":<parsed>}` 或 `"input_raw":arguments`(解析失败) |
| `ToolCallResult{call_id,name,output,success,duration}` | `{"type":"tool_result","tool_use_id":call_id,"name":name,"content":output,"is_error":!success,"duration_ms":<ms>}` |
| `TokenUsage(u)` | `{"type":"usage","input_tokens":u.prompt_tokens,"output_tokens":u.completion_tokens}` |
| `SubAgentDispatchStart{tasks}` | `{"type":"sub_agent","subtype":"start","tasks":[...]}` |
| `SubAgentTaskDone{index,elapsed_ms,turns,summary}` | `{"type":"sub_agent","subtype":"done","index":...,"elapsed_ms":...,"turns":...,"summary":...}` |
| `SubAgentTaskFailed{index,elapsed_ms,turns,reason}` | `{"type":"sub_agent","subtype":"failed","index":...,"elapsed_ms":...,"turns":...,"reason":...}` |
| `TurnComplete{...}` | `result`(success,见下) |
| `Error{error}` | `result`(error) |
| `TurnCancelled` | `result`(cancelled) |

未列出的 `AgentEvent` 变体(`PhaseChange`、`ContextStats`、`ToolCallStreaming`、`ToolBatchStarted/Completed`、`ToolOutputChunk`、`ApprovalNeeded`、sync/echo 类等)在 JSON 模式下**不进流**(返回 `None`)。`ApprovalNeeded` 的决策逻辑不变(bash 自动批准、其余拒绝),但 JSON 模式下不发对应日志行;后续如需可补一条 `{"type":"approval",...}`(本期不做)。

### `result` 对象(精简元数据)

`json` 档的唯一输出 / `stream-json` 的末行共用:

```jsonc
{
  "type": "result",
  "subtype": "success" | "error" | "cancelled",
  "is_error": false,
  "result": "<累加的最终助手文本>",
  "session_id": "<id>",
  "num_turns": <turn_count>,
  "tool_calls": <tool_call_count>,
  "duration_ms": <duration as ms>,
  "total_tokens": <total_tokens>,
  "stop_reason": "<TurnStopReason::as_tag()>"   // success 时
}
```

- `error` 子型:`is_error:true` + `"error":"<msg>"`,无 `result`/统计(Error 事件不带这些)。
- `cancelled` 子型:`is_error:true`。
- **不含** 完整 `messages` 数组(轻量)。

## 实现细节

- 用 `serde_json::json!` 或 `#[derive(Serialize)]` 结构体,**不手拼字符串**(thinking 文本含引号/换行)。
- `ToolCallStarted.arguments` 是 `String`(已字符串化的 JSON):`serde_json::from_str::<Value>(&arguments)` 成功 → 嵌为 `input` 对象;失败 → 退化成 `"input_raw": arguments` 字符串,**不 panic**。
- `Duration` → ms:`duration.as_millis()`。
- 核心提取一个纯函数便于单测:

```rust
/// Map a single AgentEvent to one NDJSON line (no trailing newline).
/// Returns None for events that don't surface in JSON output.
fn format_json_event(event: &AgentEvent) -> Option<String>
```

  `result` 的构造依赖循环里累计的 buffer/计数,可单独走一个 `build_result_json(...)` 辅助函数,同样可单测。

- JSON 模式下现有 `last_text_ended_with_newline`、`thinking_line_open`、`close_thinking_line` 等 Text 专属逻辑全部跳过 —— 每行 JSON 自带换行。
- `capture`(fixissue 回灌)与 JSON 模式正交:JSON 模式照常累加 `captured`,不冲突。
- 退出码语义不变:`0` 自然完成 / `1` 错误 / `130` 取消 / 拒绝路径维持现状。

## 测试

对齐现有 `close_thinking_chunk` / `format_thinking_chunk` 的纯函数单测风格(`main.rs` 的 `#[cfg(test)]`):

1. `format_json_event` 对每类事件产出预期 JSON(`text_delta` / `tool_use` 含 parsed input / `tool_use` 退化 input_raw / `tool_result` is_error 取反 success / `usage` / sub_agent 三态)。
2. 不进流的事件返回 `None`(抽样 `PhaseChange`、`ContextStats`、`ToolCallStreaming`)。
3. `build_result_json`:success / error / cancelled 三种子型字段正确,success 含 stop_reason,error 含 error 字段无统计。
4. `arguments` 非法 JSON → 走 `input_raw` 不 panic。
5. 解析每行输出确认是合法 JSON 且单行(无内嵌裸换行破坏 NDJSON)。

手动验证:`atomcode -p "list files" --output-format stream-json` 看逐行事件;`--output-format json` 看单对象;`--output-format text`(默认)确认现状未回归。

## 风险

- **stdout 纯净**:任何漏网的 stderr→stdout 或 print 都会破坏 NDJSON 解析。重构时确保 JSON 分支彻底接管输出路径。
- **provider 字段差异**:`TokenUsage` 字段名(prompt/completion)以 `crate::stream::TokenUsage` 实际定义为准,实现时核对。
