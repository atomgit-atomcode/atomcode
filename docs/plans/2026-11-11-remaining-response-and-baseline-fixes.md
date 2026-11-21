# Remaining Response and Baseline Fixes Implementation Plan

**Goal:** 修复已确认的 8 项基线失败，查清并纠正无工具时的错误能力声明，为首段长期无正文提供准确且有限的客户端保护。

**Architecture:** 保持 CodingRuntime 作为状态 owner，复用 harness 的 request/stream seam 和 native 会话事件日志。恢复/审批不从正文猜测工具调用；本地截止不得伪装为服务端 length，亦不自动改变用户模型或 effort。

**Tech Stack:** Rust, Tokio, native event sessions, plexus harness, nextest.

## Baseline and scope

- Branch release/v5.2.7, HEAD 685ef57512bb9d7dca00ef83ef830c01f55f6259。
- 保留前几轮尚未提交的恢复保护、会话隔离与相关测试修改。
- 公共边界先核对请求参数、stream watchdog、SnapshotHook 和 runtime session 生命周期及相关 Git 历史。
- 工具标记是 API 的 content 文本时，不把它猜测转换为可执行工具；优先消除 no-tools 与技能目录提示之间的矛盾。

## Task 1: 基线中的两项状态缺陷

Files: crates/atomcode-cli/src/host.rs, crates/atomcode-cli/src/background.rs, crates/atomcode-coding/src/runtime.rs, crates/atomcode-coding/src/on_harness.rs, crates/atomcode-cli/tests/host.rs, crates/atomcode-cli/tests/tui_bg.rs.

1. 使用既有失败用例确认取消回合和缺失目录路径。
2. 根据 native 日志/已有 checkpoint seam 修复共同 owner；不得新增快照 writer 或 fallback。
3. 缺失项目在候选准备/切换前拒绝，保留旧 foreground/binding。
4. nextest 单项验证，再运行相关 crate/driver 测试。

## Task 2: 其余六项基线断言

Files: crates/atomcode-coding/tests/recovery.rs, crates/atomcode-coding/src/host_rows.rs, crates/atomcode-coding/tests/loop_policy.rs.

1. 对照已有 jitter 实现和近期历史，验证等待范围、共享预算、Retry-After 下限与次数，不关闭生产随机抖动。
2. todo 测试先创建真实有效的待办，再验证重复更新相同状态的守卫行为。
3. 单项确认后跑相关测试集，确保不是删断言换绿。

## Task 3: 无工具能力声明与正文工具标记

Files: crates/atomcode-coding/src/parts.rs, crates/atomcode-coding/src/on_harness.rs, crates/atomcode-cli/src/main.rs, crates/atomcode-coding/tests/mount_wiring.rs, crates/atomcode-cli/tests/headless.rs (按实际调用点调整).

1. 核对真实请求的 system/tools，确认技能目录究竟来自哪个挂载行。
2. tools=false 时关闭模型可见的技能声明/auto-load，保留用户明确提供的项目指令。
3. 用网关/fixture 原始 SSE 确认 content 中的标记来源；不执行正文 XML。
4. 验证 no-tools 请求没有工具 schema，也没有承诺可调用技能的目录。

## Task 4: 首段长期无正文保护

Files: existing provider StreamWatchdog, kernel ChatOptions/config, harness streaming/request recovery, coding config and frontend notices (确定 seam 后补精确字段).

1. 区分 prefill、持续推理、可用正文/工具进展；复用现有 watchdog，避免重叠 timer/retry owner。
2. 截止必须有独立准确原因、可配置且可关闭，不能冒充模型 length 或静默降档；部分内容和取消保持既有日志语义。
3. 使用 paused Tokio 时间覆盖持续 reasoning、正常正文/工具、取消和恢复预算。
4. 隔离认证构建实测；区分“保护在限定时间生效”与“模型终于答对”。

## Delivery checks

- 相关 crate nextest，最终 bash gates/compile.sh，cargo fmt --all 及 --check，diff 检查。
- Cargo.lock 的 hkdf/hmac/zeroize_derive 均为 0，保护其他未提交修改。
- 分别报告三个问题的实现、真实验证范围及仍由模型/网关决定的部分。

## Implemented boundary (2026-11-11)

- Native conversation undo delegates directly to `undo_to_turn`; a missing workspace checkpoint no longer rejects an accepted/cancelled log turn. Missing targets still return `RewindPointNotFound`.
- Shared project navigation validates the retained runtime's current directory before replacing foreground. Failure keeps the old foreground.
- Retry tests retain exact attempt and budget identity checks, use the existing 80–100% jitter range, and allow at most one millisecond of timer rounding per wait. Server Retry-After is never shortened. The todo test seeds a valid completed item before asserting a repeated no-op update.
- No-tool mounts disable the skills/catalog/skill-first rows. The host-tools row is present even for an empty host list, matching the existing unconditional configuration patch; tool schema remains empty and project instructions remain present.
- `CodingAgentConfig::reasoning_timeout` defaults to 120s. `ATOMCODE_REASONING_TIMEOUT_SECS` can override it; 0 disables it. The existing agent-loop row passes this value into the existing `StreamWatchdog`; no second timer owner or lifecycle hook is added.
- Each request starts its reasoning deadline on first reasoning/signature content. Prefill is excluded. Non-whitespace text or a tool fragment disarms it. Content-idle and first-content budgets still operate independently.
- Deadline errors use `reasoning_output_timeout`, not provider length. The existing stream-recovery row writes the partial assistant message once, preserving signed blocks. Headless stops without automatic retries. Interactive drivers use the distinct `reasoning_timeout_checkpoint` kind and ask continue/lower once/stop. Missing/malformed/unsupported choices stop. Temporary effort belongs to the request stack and is restored after the next resumed request; cancellation drops that stack under the existing runtime owner.
- Existing per-turn provider attempt allowance still applies to requests resumed by the person. No credentials enter row config or evidence summaries. No text marker is converted into executable calls.

Validation results are recorded after the final checks below; earlier partial runs are not a completed release verification.

## Final verification

- harness/TUI/i18n: 2373 passed. The later expanded reasoning-deadline binary: 6 passed (including cancellation while its question is open and signed-block preservation).
- coding full default suite: 1325 passed, 11 intentionally skipped. Watchdog: 6 passed. Host-feature differential + mount tests (`atomgit,grape`): 90 passed.
- The complete CLI/daemon/coding run passed all CLI and daemon tests. Its four coding differential failures exposed overly broad skill suppression; moving suppression to the explicit `prepare.tools=false` boundary fixed them. Default and host-feature differential suites subsequently passed. The original eight baseline failures all pass.
- Final `bash gates/compile.sh`, `cargo fmt --all`, `cargo fmt --all -- --check`, and `git diff --check` passed. Root Cargo.lock remains clean, index flag H; hkdf/hmac/zeroize_derive package counts are all 0.
- Authenticated release binary built with the existing private signer in an isolated worktree. Root crypto stub and Cargo.lock were not overlaid. The tested binary and `target/release/atomcode` have SHA-256 `b8ab56e89dc61cea2dfae77463b120c730058a395a8652363311ca16bd00332c`.
- Real lumen-alpha/high, default 32768 output cap, shortened 3-second guard: first reasoning at 15.645s; local stop 3.005s later; 1869 reasoning characters, no body, one provider attempt, `reasoning_output_timeout`, retryable=false. No final usage was reported by the interrupted stream; zero reported tokens must not be interpreted as zero cost. The default 120-second wall-clock wait was not repeated.
- A normal real request returned exactly `OK` and stopped normally, with no native tool markup.
- Real TUI: two distinct `reasoning waiting limit` questions; first choice lower once produced actual provider effort high → medium while the footer remained high. The second choice stop produced no third request; TUI exited with status 0. Signed/partial preservation, cancellation and restoration on the next request/turn are also covered by deterministic tests.
- No-tool prompt/catalog contradiction is fixed, and no native tool markup appeared in these runs. This is not a guarantee against future malformed model/gateway content; text is never guessed into executable calls.
- Concurrent commit `75e75e752` (computer-use followup) was preserved. The isolated release build snapshot contains this task's exact production source changes; that concurrent commit's later CLI changes were outside its build snapshot.
- Temporary credential copies and the owned isolated private build sources were removed after preserving sanitized evidence.

Evidence directory: `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-final-response-check-_gtgsbir` (live-summary.json, tui-summary.json, provider-evidence.log, release-build.log, verification-summary.json, test logs).
