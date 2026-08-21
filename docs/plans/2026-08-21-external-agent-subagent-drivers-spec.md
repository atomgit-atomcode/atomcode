# 外部 Agent 子代理驱动 —— 设计 Spec

> 让 atomcode 能像 deepseek-harness 一样，把 **Claude Code (CC)** 与 **Codex** 作为
> 可按需启用的"Profile Bundle"驱动为子代理；Codex 额外支持**非交互权限模式**与
> **多个命名实例**。外部 agent 以**子代理工具**形式暴露给主模型（每个命名实例 = 一个工具），
> 复用现有 Task 子代理机制。
>
> 状态：设计（未开工）。日期：2026-08-21。分支基线：`release/v5.0.9`。

---

## 1. 背景与参考

### 1.1 deepseek-harness 的机制（参考实现，思路借鉴，落地用中性描述）

- **统一子代理注册表**：一个 `SubagentProvider` 接口 `{ name, capabilities, inheritsParentContext, start(request)→run }`；in-process 的 `spawn`/`fork` 与外部的 CC/Codex **实现同一接口、平级共存**，按名注册。
- **CC 后端**：通过官方 Agent SDK 的 `query()` 驱动，自定义 spawn 回调把进程交给共享 subprocess owner；`permissionMode` ∈ `dontAsk/acceptEdits/auto/plan/bypassPermissions`。
- **Codex 后端**：spawn `codex app-server --stdio`，走 JSON-RPC（`initialize` → `thread/start`（ephemeral thread + 权限参数）→ `turn/start`）。
  - 非交互权限：`never` / `approve-for-me` / `dangerously-bypass-approvals-and-sandbox` → 映射 `approvalPolicy` + `sandbox`。
  - 多命名实例：同一 provider 插件以不同 `providerName`（`codex-primary` / `codex-secondary`）注册多次，各自暴露成独立工具。
- **Bundle 按需安装**：一个包声明 `bundle.patch`，安装时把 patch 叠进配置，插入对应 provider。本质是"往注册表插一行"的可分发单元。

### 1.2 atomcode 现状（落点与可复用件）

| 关注点 | atomcode 现状 | 文件锚点 |
|---|---|---|
| 子代理 | Task 工具跑 **in-process** 子 `Agent`，按角色/难度分层选 LLM provider | `crates/atomcode-capabilities/src/tools/task.rs`（`build_task_child`、`run_child_to_completion`、`Args`、provider 分层） |
| 子进程生命周期 | MCP `StdioClient` 已成熟：spawn / kill_on_drop / 超时 / 恢复 / 代际 / 请求串行化 | `crates/atomcode-capabilities/src/mcp/transport_stdio.rs` |
| 进程树终止 | Bash 工具已解决 Win Job Object + Unix killpg（防孤儿） | `crates/atomcode-capabilities/src/tools/bash.rs` |
| ACP | atomcode 是 **ACP server（agent）**，**无 client 侧** | `crates/atomcode-cli/src/acp/` |
| Provider | 仅 LLM endpoint 工厂，无"后端 agent"抽象 | `crates/atomcode-coding/src/provider_factory.rs` |
| 按需安装 | plugin installer：clone/记录/信任门/事件 | `crates/atomcode-capabilities/src/plugin/installer.rs` |
| 角色/persona | team 角色表 | `crates/atomcode-coding/src/team/` |

**核心洞察**：atomcode 的 Task 工具本身就是它的"子代理注册表"，目前只有一种后端（in-process）。
本特性 = 给它加一个**外部 agent 后端抽象** + **两个适配器** + **命名实例配置**，并复用 MCP 的子进程机制、
Bash 的进程树终止、plugin 的安装/信任模式。**不新建 crate/leaf**（并进 capabilities），遵循既有约束。

---

## 2. 目标 / 非目标

### 目标
1. 定义 `SubagentBackend` 抽象，使外部 agent 与 in-process 子代理走同一 Task 分发路径。
2. Codex 适配器：非交互权限模式；MVP 用 `codex exec`。
3. Claude Code 适配器：headless CLI（`claude -p --output-format stream-json`）。
4. 多命名实例：配置声明一组 driver profile，每个注册成**命名子代理工具**。
5. "Profile Bundle" 按需启用：写配置 + 二进制探测 + 信任门（镜像 plugin 安装模式）。
6. 权限默认最严（fail-closed），提权显式声明；子进程受控（超时/取消/进程树终止）。

### 非目标（本期）
- 不做 ACP **client** 侧（CC/Codex 都用 CLI/子进程 JSON-RPC 驱动）。
- 不做"会话后端替换"（整会话切到外部 agent）——仅子代理工具委派。
- 不下载外部二进制（假定用户已装 `claude`/`codex`，PATH 可见）。
- 不做 Codex `app-server` 流式/多轮（列为进阶阶段 4）。
- 不改 webui（TUI + headless 优先；webui 后续）。

---

## 3. 架构总览

```
主模型
  │  调用命名子代理工具 (subagent_codex_primary / subagent_claude_review / ...)
  ▼
Task 分发层 (tools/task.rs)  ── 新增分叉 ──▶  ExternalSubagentTool
  │  role/难度 → in-process 子 Agent (现状)      │
  ▼                                              ▼
run_child_to_completion (现状)          SubagentBackend::run(SubagentRun)
                                               │
                          ┌────────────────────┴────────────────────┐
                          ▼                                          ▼
                  CodexBackend                              ClaudeCodeBackend
              spawn `codex exec` (MVP)                 spawn `claude -p --output-format
              /`codex app-server`(阶段4)                stream-json --permission-mode ...`
                          │                                          │
                          └──── 复用 ManagedChild（提炼自 StdioClient/bash）────┘
                                       spawn / 超时 / 取消 / 进程树 kill
```

### 3.1 新增/改动模块

- **新**：`crates/atomcode-capabilities/src/subagent/mod.rs` —— `SubagentBackend` trait + 公共类型（`SubagentRun` / `SubagentResult` / `PermissionMode` / `SubagentEvent`）。
- **新**：`crates/atomcode-capabilities/src/subagent/codex.rs` —— Codex 适配器。
- **新**：`crates/atomcode-capabilities/src/subagent/claude_code.rs` —— CC 适配器。
- **新**：`crates/atomcode-capabilities/src/subagent/proc.rs` —— `ManagedChild`（子进程生命周期，提炼 bash/StdioClient 的 spawn+超时+取消+进程树 kill）。
- **改**：`crates/atomcode-capabilities/src/tools/task.rs` —— Task 分发分叉出 `backend` 路径；或独立 `ExternalSubagentTool`（见 §6）。
- **改**：`crates/atomcode-coding/src/config.rs`（或 config crate）—— `[[subagent.external]]` profile 列表 + 反序列化。
- **改**：工具注册处（parts.rs / 工具装配）—— 按 profile 注册命名工具。
- **改**：persona —— 说明"可用命名外部子代理工具"（弱模型引导，参考既有 signposts）。

---

## 4. 接口契约

### 4.1 `SubagentBackend` trait

```rust
// crates/atomcode-capabilities/src/subagent/mod.rs（示意，非最终签名）
#[async_trait]
pub trait SubagentBackend: Send + Sync {
    /// 命名实例名，如 "codex-primary" / "claude-review"。也是工具名的来源。
    fn name(&self) -> &str;

    /// 静态能力位（是否支持工具过滤、结构化输出等），用于装配期校验。
    fn capabilities(&self) -> SubagentCapabilities;

    /// 一次性执行：把 prompt 交给外部 agent，在 cwd 下运行，返回最终结果。
    /// 流式进度通过 req.on_event 回调（复用 Task progress hook）。
    async fn run(&self, req: SubagentRun) -> Result<SubagentResult, SubagentError>;
}

pub struct SubagentRun {
    pub prompt: String,
    pub cwd: PathBuf,
    pub permission: PermissionMode,        // 见 §5
    pub tool_filter: Option<ToolFilter>,   // 能力位为真时才透传
    pub cancel: CancellationToken,         // 复用内核 cancel 语义
    pub on_event: EventSink,               // 进度/文本增量 → Task 面板
}

pub struct SubagentResult {
    pub output: String,                    // 汇总文本（回填 ToolResult）
    pub stop_reason: StopReason,           // completed / cancelled / error / permission_denied
}

pub enum SubagentError { SpawnFailed, Timeout, ProtocolError(String), NonZeroExit(i32), .. }
```

**设计原则**
- `run` 是**一次性委派**（MVP）。多轮/续聊留给阶段 4（Codex app-server thread）。
- 后端**不感知** atomcode 内核 Conversation；只吃 prompt、吐结果 + 事件流。
- 事件流复用 Task 现有 progress hook（marker 前缀活动行），TUI 不需要新渲染。

### 4.2 权限模式（统一枚举 → 各 agent 映射，见 §5）

```rust
pub enum PermissionMode { ReadOnly, AcceptEdits, Auto, Bypass }
```

---

## 5. 非交互权限映射（fail-closed）

atomcode 统一枚举 → 各后端旗标。**默认 `ReadOnly`**；`Auto`/`Bypass` 必须在 profile 显式声明。

| atomcode | Claude Code (`claude -p --permission-mode`) | Codex (`codex exec`) |
|---|---|---|
| `ReadOnly`（默认） | `plan`（禁写）+ `--disallowedTools` 写类 | `-a never --sandbox read-only` |
| `AcceptEdits` | `acceptEdits` | `-a on-request --sandbox workspace-write` |
| `Auto` | `acceptEdits`（+放开工具集） | `--full-auto`（workspace-write + on-failure 自动） |
| `Bypass`（危险，需显式） | `bypassPermissions`（`--dangerously-skip-permissions`） | `--dangerously-bypass-approvals-and-sandbox` |

**约束**（对齐 scheduled-task 的 approver 原则 `[[project_local_scheduled_tasks_phase1]]`）：
- scheduled / headless / 非交互上下文下，`Bypass` 一律拒绝（严格 approver 封顶）。
- profile 未声明 permission → 落 `ReadOnly`。
- `Bypass` 在配置解析处打警告 + 需要独立开关（如 `allow_dangerous = true`）双重确认。

---

## 6. 工具暴露（子代理工具，每命名实例一个工具）

### 6.1 配置（TOML）

```toml
[[subagent.external]]
name       = "codex-primary"       # 工具名派生：subagent_codex_primary
kind       = "codex"               # codex | claude-code
permission = "accept-edits"
model      = "gpt-5-codex"          # 可选，透传给外部 agent
extra_args = ["--search"]           # 可选逃生舱
enabled    = true

[[subagent.external]]
name       = "claude-review"
kind       = "claude-code"
permission = "read-only"
enabled    = true
```

### 6.2 注册与命名

- 装配期读取 `subagent.external`，对每个 `enabled` profile：
  1. 探测二进制在 PATH（`codex` / `claude`），缺失 → 跳过 + 一次性 warning（不 fail 启动）。
  2. 构造对应 `SubagentBackend`（Codex/CC）。
  3. 注册一个命名工具 `subagent_<name(下划线化)>`，desc 含 kind + 权限 + 用途提示。
- 工具 execute → 组 `SubagentRun` → `backend.run()` → 回填 `ToolResult`。
- **复用 Task 机制**：进度面板、cancel、sensitive-path 拒绝（`DenySensitivePaths`）、worker scope（若声明）。

### 6.3 与内置 Task 的关系

- 优先**独立 `ExternalSubagentTool`**（每 profile 一个实例），而非塞进 `task` 的 args——命名工具更贴合 deepseek 语义、对弱模型更好用、隔离清晰。
- 复用 task.rs 的子部件（progress hook、scope gate、result 汇总）而非复制。

---

## 7. 子进程管理（复用/提炼）

`subagent/proc.rs::ManagedChild` 提炼自 `mcp/transport_stdio.rs` + `tools/bash.rs`：
- spawn（`tokio::process::Command`，`kill_on_drop`）。
- 空闲/总超时（参考 bash 的 60/300s + 空闲 90s；外部 agent 用更宽松值，可配）。
- 取消：cancel token → 进程树终止（**复用 bash 的 Win Job Object + Unix killpg**，见 `[[project_windows_bash_process_tree_orphan_job_object]]`，防 codex/claude 派生的孤儿）。
- stdout/stderr 流式读取（Codex `exec` 逐行；CC stream-json 逐 JSON 事件）。
- 退出码 → `SubagentError::NonZeroExit`。

---

## 8. "Profile Bundle" 按需安装

atomcode 版务实：bundle 实质是**一份可分发的 driver 配置 profile**（外部二进制用户自装）。

- **install** = 写/合并 `subagent.external` 一行 + 探测二进制 + 走信任门（镜像 `plugin/installer.rs` 流程与事件）。
- 可选：随 bundle 附带 persona / 工具过滤 / 默认权限，作为 preset 分发。
- 信任：新增外部 driver 视为可执行外部程序，首次启用需信任确认（对齐 MCP/plugin 信任门 `[[project_mcp_disable_and_jsonc]]` / `[[project_cc_plugin_hooks_trust_gate]]`）。
- 阶段 3 才做；MVP 手写 TOML 即可。

---

## 9. 复用清单（避免重复造轮子）

| 需求 | 复用 | 位置 |
|---|---|---|
| 子进程 spawn/超时/取消 | bash + StdioClient 模式 → 提炼 `ManagedChild` | bash.rs / transport_stdio.rs |
| 进程树终止（防孤儿） | Job Object / killpg | bash.rs |
| 进度面板/取消/结果汇总 | Task 子部件 | tools/task.rs |
| 敏感路径拒绝 | `DenySensitivePaths` | tools/task.rs |
| worker 范围限制 | `WorkerScopeGate` | tools/task.rs |
| 安装/信任/事件 | plugin installer | plugin/installer.rs |
| 弱模型工具引导 | persona signposts | coding/persona.rs |

---

## 10. 任务拆解（SDD，按阶段）

### 阶段 1 —— MVP（单实例，Codex + CC）
- **T1.1** `subagent/mod.rs`：trait + 公共类型（`SubagentRun/Result/PermissionMode/Error/Capabilities`）。纯类型 + 单测。
- **T1.2** `subagent/proc.rs`：`ManagedChild`（spawn + 超时 + cancel + 进程树 kill）。四象限测试（Win/Unix × execute/kill），交叉编译验证。
- **T1.3** `subagent/codex.rs`：`codex exec` 适配器（权限映射 + 逐行输出解析 + 退出码）。纯映射函数单测 + 假二进制（stub script）集成测试。
- **T1.4** `subagent/claude_code.rs`：`claude -p --output-format stream-json` 适配器（权限映射 + stream-json 解析）。同上测试策略。
- **T1.5** `ExternalSubagentTool` + 装配：读单个 profile、探测二进制、注册命名工具、execute 走 backend、回填 ToolResult。
- **T1.6** 配置：`[[subagent.external]]` 反序列化 + 默认 `ReadOnly` + `Bypass` 双确认门。config 单测。
- **T1.7** persona：一句话引导"可用命名外部子代理工具"。
- **验收**：手配一个 codex + 一个 claude profile，主模型能委派一段子任务、拿回结果、可取消、非交互不卡审批、敏感路径被拒。

### 阶段 2 —— 多命名实例
- **T2.1** 装配循环支持多 profile → 多命名工具，工具名去重/规范化。
- **T2.2** 并发：每次调用各自 spawn（无共享状态）；并发上限与 Task 一致。
- **T2.3** 事件/进度按实例名打标（面板区分）。
- **验收**：codex-primary / codex-fast / claude-review 三工具并存，可并行委派互不串台。

### 阶段 3 —— Bundle 安装
- **T3.1** install/enable 命令：写配置 + 二进制探测 + 信任门 + 事件（镜像 plugin）。
- **T3.2** bundle 可携带 persona/权限/工具过滤 preset。
- **T3.3** 缺二进制/未信任的友好提示。

### 阶段 4 —— 进阶（可选）
- **T4.1** Codex 升 `app-server --stdio`（JSON-RPC：initialize/thread.start/turn.start），拿多轮 + 流式。
- **T4.2** CC 可选 ACP client（需新建 ACP client 侧，重，评估后再定）。
- **T4.3** webui 暴露。

---

## 11. 测试计划

- **纯函数单测**（无外部依赖，CI 稳）：权限映射（4×2 矩阵）、命令行组装、stream-json / codex-exec 输出解析、profile 反序列化、工具名规范化、`Bypass` 门控。
- **假二进制集成测试**：用 stub 脚本冒充 `codex`/`claude`（吐固定 stream-json / 逐行 + 指定退出码），验证 spawn→解析→结果→取消→超时→非零退出全链路，**不依赖真 agent、不联网**（对标 `acp_smoke.py` 的隔离思路）。
- **进程树终止测试**：四象限（Win/Unix × 正常/取消），验证派生子进程被清（对齐 bash 既有测试）。
- **安全测试**：非交互下 `Bypass` 被拒；敏感路径委派被 `DenySensitivePaths` 拦；未声明权限落 `ReadOnly`。
- **不做**：真机跑 codex/claude（留给手工验收）。

---

## 12. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 外部 CLI 旗标/协议漂移（codex/claude 升级改参数） | 旗标集中在适配器一处 + 版本探测 + 假二进制测试锁行为；漂移只改一处 |
| 非交互权限误放行（危险操作自动执行） | 默认最严 + `Bypass` 双确认 + 非交互上下文硬拒 + 敏感路径拦截 |
| 孤儿进程（外部 agent 自身派生子进程） | 复用 Job Object/killpg 进程树终止 |
| 弱模型滥用/循环调用外部 agent | 复用内核跨轮熔断 + max_rounds（`[[project_runaway_toolcall_loop_repetition_fuse]]`） |
| 二进制缺失致启动失败 | 探测缺失只跳过+warning，不 fail 启动 |
| 与并行 WIP/合并冲突 | 新代码集中在新 `subagent/` 模块，Task 侧改动最小化 |
| 改 config 结构牵连全工作区 | 加字段要编全 workspace（呼应 `[[project_local_scheduled_tasks_phase1]]` 教训） |

---

## 13. 开放问题（开工前定）

1. Codex MVP 用 `codex exec`（一次性）还是直接上 `app-server`（多轮/流式）？—— 建议 exec 起步。
2. 工具命名规则：`subagent_<name>` 还是 `<kind>_<name>`？对弱模型哪个更清晰？
3. profile 配置放哪层：项目级 `.atomcode` vs 全局 config vs 两者？信任粒度如何？
4. 是否需要"工具过滤透传"（限制外部 agent 只用某些工具）——CC 支持 `--allowedTools`，Codex 支持度需核实。
5. 结果回填：只回汇总文本，还是也把外部 agent 的文件改动 diff 摘要带回？

---

## 附：关键文件锚点（实现时对照）

- Task 子代理：`crates/atomcode-capabilities/src/tools/task.rs`（`build_task_child` / `run_child_to_completion` / `Args` / provider 分层 / `DenySensitivePaths` / `WorkerScopeGate`）
- MCP 子进程：`crates/atomcode-capabilities/src/mcp/transport_stdio.rs`
- Bash 进程树终止：`crates/atomcode-capabilities/src/tools/bash.rs`
- Plugin 安装/信任：`crates/atomcode-capabilities/src/plugin/installer.rs`、`mod.rs`
- Persona 引导：`crates/atomcode-coding/src/persona.rs`
- 工具装配：`crates/atomcode-coding/src/parts.rs`
- 配置：`crates/atomcode-coding/src/config.rs`
