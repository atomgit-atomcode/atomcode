# atomcode-review(L2)

一个只读的**代码评审 agent**,由中立内核([`atomcode-kernel`](../atomcode-kernel))+ 能力层
([`atomcode-capabilities`](../atomcode-capabilities))组装而成 —— 不依赖 `atomcode-core`。
结构对标 [`atomcode-coding`](../atomcode-coding),但面向评审。

终端用户的 CLI 见 [`atomcode-clix`](../atomcode-clix)(`atomcodex review …`)。本 README 面向
**直接使用本库的嵌入者**。

---

## 提供什么

`build_review_agent(cfg)` 返回一个内核 `Agent`,**外加**一个 `ReportFindingTool` 句柄。你把 diff
作为任务注入、运行 agent,然后从句柄读取结构化 findings:

```rust
use atomcode_review::{build_review_agent, ReviewAgentConfig};
use atomcode_kernel::agent::AutoRespond;

# async fn demo() -> Result<(), String> {
let (agent, report) = build_review_agent(ReviewAgentConfig::new(
    "sk-...",                       // api_key(keyless 网关可留空)
    "https://api.deepseek.com/v1",  // base_url
    "deepseek-v4",                  // model
    ".",                            // 只读工具作用的仓库 working_dir
))?;

let task = "Review this diff:\n```diff\n<你的 diff>\n```";
let outcome = agent.run_to_completion(task, AutoRespond::AllowAll).await;

for f in report.findings() {
    println!("[{} {:.2}] {}:{}-{}  {}", f.priority, f.confidence, f.file_path, f.line_start, f.line_end, f.title);
}
let _ = outcome;
# Ok(()) }
```

agent 只挂载**只读工具集** —— `read_file`、`grep`、`glob`、`list_directory`、`ast_grep`、
`web_search`、`report_finding`,以及代码智能(`list_symbols` / `read_symbol` /
`find_references` / `trace_callers` / `trace_callees` / `trace_chain` / `blast_radius` /
`file_dependencies`)。它无法写入/编辑/运行任何东西。diff 由你作为任务提供,所以 agent 不需要 shell。

> 想要实时轨迹 / 事件流而非 `run_to_completion`?用 `agent.spawn()` 消费 `AgentEvent`
> (`atomcode-clix` 就是这样打印逐工具进度的)。

---

## 传入自定义 system prompt(全量覆盖)

reviewer 提示词是内置的([`review_persona`])。要**完全替换**它,设置
`ReviewAgentConfig::persona`(例如用 `with_persona`):

```rust
use atomcode_review::ReviewAgentConfig;

let cfg = ReviewAgentConfig::new("sk-...", base_url, model, repo)
    .with_persona(std::fs::read_to_string("reviewer.md")?);
let (agent, report) = atomcode_review::build_review_agent(cfg)?;
```

`persona = None`(默认)使用内置 [`review_persona`];`Some(text)` 则**替换**它 —— 内置指令**不会**
被追加。

> ⚠️ 全量覆盖会丢弃内置对工具集的介绍以及 `report_finding` 上报协议。你的提示词必须告诉模型有哪些
> 工具、并要求逐条用 `report_finding` 上报,否则 `report.findings()` 会返回空。

若只想保留内置 reviewer 再**追加**指导,自己拼接即可:
`format!("{}\n\n{}", review_persona(model), 你的补充)`。

---

## `ReviewAgentConfig`

| 字段 | 默认 | 说明 |
|---|---|---|
| `api_key` / `base_url` / `model` | — | provider 凭据(OpenAI 兼容) |
| `working_dir` | — | 只读工具作用的仓库根目录(固定,不用进程 cwd) |
| `context_window` | `128_000` | 转发给 provider |
| `stream_timeout` | `120s` | 单流事件存活上限 |
| `request_timeout` | `300s` | 驱动方响应上限 |
| `persona` | `None` | **全量** system-prompt 覆盖(`with_persona`) |

## Findings

`report.findings()` / `report.take_findings()` 返回 `Vec<Finding>`;`Finding` 实现了
`Serialize`:

```rust
pub struct Finding {
    pub title: String,
    pub body: String,
    pub priority: String,   // "P0".."P3"
    pub confidence: f32,    // 0.0..=1.0
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
}
```

## Cargo features

以 `["provider", "tools", "codeintel", "web"]` 引入 `atomcode-capabilities` —— 即完整的只读评审
工具集。
