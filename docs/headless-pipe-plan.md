# `atomcode --headless` 重构方案（pipe-friendly）

> 目标：让 `atomcode --headless` 在 CI / shell 管道 / 脚本调用场景下行为可预测、输出可解析、不会卡 stdin。
> 范围：仅 `crates/atomcode-cli/src/main.rs`，不动 agent / tool 接口。
> Team-lead 审核结论：直接重写 `--headless`，**不**新增 `--pipe` flag（atomcode 仍 pre-1.0，旧 headless 对 CI 本来就 broken，无兼容性需要保护）。

## 1. 现状回顾（`run_headless`，main.rs:232-340）

| 问题 | 触发位置 | 影响 |
|------|---------|------|
| stdout 同时承载 LLM 正文和 `[Tool: …]` / `[name: OK …]` 日志 | main.rs:253-273 | 下游脚本无法 `cmd \| jq` / `cmd > out.md` 提纯 |
| `[Done: …]` 摘要也写 stdout | main.rs:314-315 | 同上，摘要污染正文 |
| `ApprovalNeeded` 走 `io::stdin().read_line` | main.rs:282-294 | 无 TTY 环境（CI、`echo "" \| atomcode`）会卡死 |
| 进程退出码恒为 0（除非 anyhow 冒泡） | main.rs:340 | `Error` 事件后照样返回 0，CI 检测不到失败 |
| `Error` 分支不打断循环也不记账 | main.rs:325-327 | 永远等 `TurnComplete` 才退出，遇到 stream 错误可能挂住 |

## 2. 设计决策

### 2.1 直接重写 `--headless`，不引入新 flag

- `Cli` struct **不改字段**，沿用现有 `headless: bool` 和 `prompt: Option<String>`。
- `run_headless` 签名也**不加** `pipe_mode` 参数——整个函数就是新行为。
- 删除旧的 stdin 读取分支、删除旧的 stdout 日志混杂逻辑。
- 旧脚本（如果有）会感知到的差异：tool 日志和 `[Done: ...]` 摘要从 stdout 移到 stderr；ApprovalNeeded 不再读 stdin。这些都是修复 broken 行为，不属于"破坏兼容"。

### 2.2 Stream 路由：stdout 极简，stderr 全包

| AgentEvent | 当前去向 | 新去向 | 备注 |
|------------|----------|--------|------|
| `TextDelta(s)` | stdout | **stdout**（保持） | 唯一允许写 stdout 的事件 |
| `ToolCallStarted` | stdout | **stderr**，单行 `[tool→ name args=...]`，args 截断到 200 字节 | 不要换行包裹 |
| `ToolCallResult` | stdout | **stderr**，单行 `[tool← name OK 12ms]` 或 `FAILED`；output 超 500 截断后写第二行 | 失败 **不影响** 退出码（LLM 可恢复） |
| `ApprovalNeeded` | stdin 阻塞 | **stderr** `[approval-denied tool=… reason=…]`，立即 `cmd_tx.send(DenyTool)`，并把 `had_denial=true` | 见 §2.3 |
| `TokenUsage` | stderr | stderr（保持） | 不变 |
| `PhaseChange` | stderr | **完全静默** | headless 下 noisy 无用 |
| `TurnComplete` | stdout 摘要 + break | **stderr** 单行摘要 + 末尾给 stdout 补 `\n` + break | 修掉摘要污染 stdout 的 bug |
| `TurnCancelled` | stderr + break | stderr + `exit_code=130` + break | headless 下基本不会出现 |
| `Error(e)` | stderr | stderr + `exit_code=1` + **break**（避免挂住） | 见 §2.4 |
| `WorkingDirChanged` | stderr | stderr | 不变 |
| `ContextStats` | 静默 | 静默 | 不变 |
| `SubAgentProgress` | stderr | stderr | 不变 |

实现要点：
- 主循环里所有非正文输出直接 inline `eprintln!` —— 不要抽 helper 宏或闭包（CLAUDE.md：不为一次性操作创建 helper）。
- stdout 只 `print!` `TextDelta`，循环结束前若最后一段未以 `\n` 结尾再补一个换行，方便管道下游按行处理。

### 2.3 审批策略：默认拒绝

- headless 下检测到 `ApprovalNeeded`：
  1. `eprintln!("[approval-denied] tool={} reason={}", tool_name, reason);`
  2. `cmd_tx.send(AgentCommand::DenyTool)?;`
  3. 设置 `had_denial = true`（不立即 break，让 LLM 自己决定后续）。
- 退出码：`had_denial=true` 仍 **正常完成 turn** 的情况下，最终退出码 = `2`（区分"完成但有受限工具被拒"，对齐 grep/diff 的 soft-warning 惯例）。
- **不**新增 `--yes` / `--allow-all` 自动批准开关——破坏 CLAUDE.md §3 的安全沙箱铁律，有需要再单独立项。

### 2.4 退出码规范

| 场景 | exit code |
|------|-----------|
| `TurnComplete` 正常结束，无 Error/Denial | `0` |
| 任意 `AgentEvent::Error` 出现过 | `1` |
| `ApprovalNeeded` 被自动拒绝（即使 turn 最终完成） | `2` |
| `TurnCancelled` | `130`（POSIX `SIGINT` 习惯值） |
| infra 错误（anyhow 冒泡到 `main`） | `1`（已有，沿用 main.rs:98） |

实现：
- `run_headless` 改成返回 `Result<i32>`。
- `run()` 也跟着改成 `Result<i32>`：非 headless 路径直接 `Ok(0)`。
- `main()` 里 `match run().await { Ok(code) => process::exit(code), Err(e) => { … process::exit(1); } }`，两个出口合并。
- 优先级覆盖规则：`Error(1) > Denial(2) > 0`；`TurnCancelled` 单独覆写为 `130`。

### 2.5 其它需要触碰的分支

- `Error`：不仅记账还要 **break**。当前会继续 loop 等 `TurnComplete`，但部分错误路径不会再发 `TurnComplete`，会挂住。
- `TurnComplete` 摘要从 `println!` 改 `eprintln!`。
- `ToolCallResult` 文本里的换行 trim，避免单行日志被切成多行难解析。
- `print!` / `eprintln!` 前后注意 flush：只在 `TextDelta` 后 flush stdout；stderr 默认 line-buffered，不需要手动 flush。

## 3. 不做的事

- 不引入 JSON 行格式（`--headless-json`）—— 留作后续 task，避免本次膨胀。
- 不动 `AgentEvent` / `AgentCommand` 枚举。
- 不动 TUI / agent loop。
- 不增加自动批准开关。
- 不抽 `log_err!` 宏 / helper 闭包。

## 4. 影响面

- **改动文件**：仅 `crates/atomcode-cli/src/main.rs`。
- **改动函数**：
  - `main()`（exit code 适配，约 5 行）
  - `run()`（返回类型 `Result<i32>`，约 3 行）
  - `run_headless()`（核心改写，约 60 行内）
- **依赖**：无新增 crate。
- **测试**：QA 需要在 `tests/` 或 `scripts/` 加 smoke：
  - 简单 prompt → 校验 stdout 不含 `[tool` / `[Done`，stderr 含 `[Done`
  - 触发危险 bash → 校验 stderr 含 `approval-denied` 且 exit code = 2
  - 故意 prompt 失败 → 校验 exit code = 1
  - 全量 `./scripts/test-all.sh` 必须通过（CLAUDE.md 铁律）

## 5. 实施步骤（Task #2）

1. `main()` 改 `run()` 返回 `Result<i32>`，统一 `process::exit(code)`，保留 `Err` 路径 `exit(1)`。
2. `run()` 非 headless 分支末尾 `Ok(0)`。
3. 重写 `run_headless` 主循环：分流 stdout/stderr，自动拒批，记账 exit code，`Error` 分支 break，`TurnComplete` 摘要走 stderr。
4. `cargo build -p atomcode-cli` + `cargo clippy -p atomcode-cli -- -D warnings`。
5. `./scripts/test-all.sh` 全绿。
6. SendMessage 给 qa，触发 Task #3 / #4。
