# 一键迁移其他 AI Coding Agent 的资产 —— 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让从 Claude Code / Codex 迁来的用户，用一条命令看清「我的资产 AtomCode 已经能用哪些、哪些能带走、哪些带不走」，并能把 provider 账号逐项确认后搬进 `config.toml`。

**Architecture:** 新增 `atomcode-capabilities::migration` 模块（中立、只读、feature `migration`），把外部 Agent 的资产分成 `Active` / `Imported` / `Actionable` / `Blocked` 四态产出结构化报告；「是否真的生效」由**真加载器**（instructions / skills / mcp / permission rules）判定，id 与 endpoint 的归一规则与写入方**共用** `atomcode-config` 的同一条；文案走 `atomcode-i18n` 的 product 表。CLI 加 `atomcode import` 子命令（扫描 + `--apply` 写入），TUI 加 `/import` 命令，通过一个镜像 `/setup` 的端口 trait 向宿主要数据。

**Tech Stack:** Rust 1.94；`serde` / `serde_json` / `toml`；`atomcode-capabilities`（feature `migration`）、`atomcode-config`（`ProviderBook`）、`atomcode-i18n`（product 词表）、`atomcode-tui`（`CommandSet` + 端口）、`atomcode-cli`（clap 子命令）。

**设计依据：** `docs/plans/2026-11-17-external-agent-asset-migration-design.md`

**基线：** 分支 `release/v5.2.8`，基线 commit `7b37a2682`（设计文档提交）。开工前工作区有他人未提交改动（`atomcode-tui`、`atomcode-cli`、`webui` 等），**全部属于他人，本任务不得触碰**；每次委派前必须重新核对 `git status`，因为它会在本任务进行中变化。

**分工（AGENTS.md 约定）：** Claude Code 只写实现代码；Codex 写测试、跑测试与格式门、独立审查；AtomCode 调度、验收、提交。每轮任务必须给出独立完整的交接说明。

---

## 阶段、依赖顺序与验收

| 阶段 | Task | 目标 | 依赖 | 验收 |
|---|---|---|---|---|
| P1 骨架 | 1 | `migration` 模块与四态报告类型可用 | — | `cargo nextest run -p atomcode-capabilities --features migration` 全绿；`gates/layers.sh` 通过 |
| P2 探测 | 2, 3 | Claude / Codex 两个来源 | P1 | 样例目录快照判据全绿；坏文件不 panic |
| P2.5 探测 | 4 | OpenCode 来源 | P1, P2 | 样例目录快照判据全绿；坏文件不 panic；`.agents/skills` 标 `Active`、`.agents/commands` 标 `Actionable`（**我们不读 commands 那一半**） |
| P3 CLI | 5 | `atomcode import` 只读报告 | P2, P2.5 | 文本与 JSON 两种形态判据全绿 |
| P4 TUI | 6 | `/import` 面板 | P3 | 屏幕判据全绿；`gates/tui-i18n.sh` 通过 |
| P5 映射 | 7 | provider 中立草案 | P2, P2.5 | 凭据语义判据全绿（Claude=明文 / Codex=变量名 / OpenCode=明文） |
| P6 写入 | 8 | `--apply` 逐项确认写入 | P5 | 不确认不写、id 冲突不覆盖、非交互拒绝 |
| P7 验收 | 9 | 全部门 | P1–P6 | 见 Task 9 |

**依赖顺序是硬的**：P4 依赖 P3（屏幕显示的正是 CLI 那份报告），P6 依赖 P5（写入的对象正是草案），P7 依赖全部。

### 落地难度与工作量评估

开工前先读这一节。数字是**从本计划文本量出来的**，不是估的。

| Task | 步骤 | 代码行 | 判据 | 改动文件 | 难度 |
|---|---|---|---|---|---|
| 1 capabilities 骨架 | 7 | 287 | 3 | 4（2 新建） | 低 — 纯新模块，无外部依赖 |
| 2 Claude 来源 | 7 | **1106** | 11 | 1 | **高** — 最大的一个，8 类资产 × 4 个真加载器 |
| 3 Codex 来源 | 6 | 343 | 5 | 4 | 中 — TOML 解析 + 递归合并 |
| 4 OpenCode 来源 | 8 | ~430 | 5 | 5 | 中 — **第三种形状**：明文凭据、数组 command、npm 协议 |
| 5 CLI 子命令 | 9 | 422 | 5 | 4（1 新建） | 中 — 接 clap + 渲染 |
| 6 TUI `/import` | 10 | 437 | 6 | 11（2 新建） | 中 — **底部单子**（可滚动/可展开）+「多源才问」 |
| 7 provider 映射 | 5 | 434 | 8 | 5 | 中高 — 动到 `atomcode-config` 与 `atomcode-coding` |
| 8 写入 config.toml | 8 | 639 | 8 | 3 | 中 — **安全要求最高**（凭据逐项确认、明文不落盘） |
| 9 门与验收 | 6 | 0 | 0 | 0 | 低 — 跑门 |
| **合计** | **66** | **~4100** | **47** | **约 22 文件 / 6 crate** | |

**规模判断**：约 4100 行 Rust、47 条判据、22 个文件、6 个 crate（capabilities / coding / config / cli / tui / i18n）。另有 47 条新 i18n 词条、约 72 个新公开项。**这是一次中等规模的功能，不是大改** —— 但也不是「照着抄一遍」。

**加一个来源的成本（三个来源实测）**：Claude 约 1100 行、Codex 约 340 行、
OpenCode 约 430 行。**第二个来源最贵**（要摸清每个资产到底生不生效），第三个
明显便宜 —— 因为来源特有的知识已经被收回来源自己，`protocol_for_wire(wire_api,
default)` 只问「这条线怎么说」，不问「你是谁」。

**难度不在代码量，在这三处**：

1. **Task 2 是风险集中点。** 它的 1106 行里，最难的不是「找到文件」，而是**判定「到底生不生效」** —— 指令要看 `load_instructions` 的四个状态（`Loaded` / `Empty` / `Unreadable` / `Shadowed`），技能要看 `SkillRegistry` 的加载与诊断，MCP 要看 `parse_mcp_servers`，权限要看 `PermissionRules::parse`。**每一条判定都必须复用真加载器**，自己写一份就等于埋一个「说生效、其实不生效」的错。
2. **计划里的代码从未编译过。** 六轮复审修的是**计划文本**的编译错误 —— 每一轮都找出新的（游离字段赋值、`continue` 出循环、`Option` 收集类型、括号不匹配）。**文本对了不等于代码对**：实施时应当预期还会遇到同类编译错误，按「先让这一段过编译门，再往下写」的节奏走。
3. **共享 crate 的并发改动。** Task 7 要改 `atomcode-config/src/provider_book.rs` 与 `atomcode-coding/src/parts.rs` —— 而当前工作区里 `atomcode-kernel`、`atomcode-coding`、`atomcode-harness`、`atomcode-capabilities`、`atomcode-config`、`atomcode-cli` **都在被别人改**。落地前必须重新核对 `git status`，并按任务粒度提交，别把别人的改动卷进来（这条路我已经走过一次并出过事故，见「交接模板」里的提醒）。

**成本可预期的地方**：

- **不新增传递依赖。** `migration` feature 要的 `instructions` / `mcp` / `skills` / `tools` / `atomcode-config` / `atomcode-i18n` / `toml` 全都是 `atomcode-capabilities` **已有的 optional 依赖**；CLI 已启 `mcp`，TUI 经 `session`/`harness` 已启 `instructions` / `mcp` / `skills` / `tools`。所以**前端的依赖图不变**。
- **`Cargo.lock` 不该变。** 上面那些包都已在 lock 里。AGENTS.md 的污染判据（`hkdf` / `hmac` / `zeroize_derive` 三个 package 名为 0）当前是 0，实现后要复查仍是 0。
- **门禁不会被新代码触发红。** `gates/tui-i18n.sh` 的基线是 `hardcoded_cjk=3`（TUI/CLI 源码里不许有中文字面量 —— 本计划把所有文案都放在 i18n product 表，所以不增加）；`gates/tui-test-count.sh` 的基线 1976 是**棘轮**（只能升不能降），本任务只加判据；`gates/layers.sh` 禁的是 `atomcode-tui → atomcode-coding / atomcode`，而 capabilities→config 是**既有**的边，不新增违规。
- **`cargo-nextest` 已装**（0.9.149），AGENTS.md 要求的 runner 可用。

**按 AGENTS.md 的分工估**：Claude Code 只写实现（每个 Task 一次委派，给完整交接），Codex 写测试与跑门、返修复意见。**8 个 Task 就是 8 轮「实现 → 测试 → 审查 → 修复」**，不是一次写完。按每轮 1–2 小时的量级，**整轮走完大致是 1–2 个工作日**；其中 Task 2 与 Task 7 各可能多花一轮（前者判定逻辑多，后者要动共享 crate）。

**建议的开工顺序**（与依赖顺序一致，但有两处提前）：

1. **先 Task 1**，它把模块骨架与四态类型立起来 —— 后面的每个 Task 都在它上面长。
2. **Task 7 提前到 Task 3 之后**：它要改 `atomcode-config` 与 `atomcode-coding` 这两个共享 crate，而这两个 crate 现在正被别人改。**越早动，越早知道会不会撞**；撞上了还能改方案，晚了就是返工。
3. **Task 2 单独一轮**，不要与别的 Task 混 —— 它最重，混在一起出问题时说不清是谁的。
4. **Task 8 单独一轮**：安全要求最高（凭据逐项确认、明文不落盘、id 冲突不覆盖）。它值得一次专注的审查，而不是顺手写完。
5. **Task 6 不是「渲染一段文字」**：它是**底部单子**（`crate::sheet::List`），要有光标、筛选、展开看详情，并按 Codex 的做法**多源才问**。单子形态的判据比文本形态难写 —— 断言的是 `OpenSheet` 里的 `Page::List`，不是一段字符串。


**不属于本期，说明原因：**

- **会话历史转换**（第三期）—— 信息有损，且需要 kernel event 格式的写入契约；本期只在报告里计数并标 `Blocked`。
- **补运行时兼容面**（让 `~/.claude/CLAUDE.md` 等被直接读取）—— 那是改产品行为，不是迁移体验；本期只在报告里如实标注。
- **MCP 实际导入** —— MCP 配置含 `command` / `env`，搬过去等于搬一段可执行的东西，风险与凭据同级；本期只标 `Actionable` 并给目标路径。
- **插件与 subagent 定义迁移** —— 没有目标格式：我们的角色文件要 `permission` 与 `difficulty` 两个 frontmatter 键（`team.rs:279,403`），Claude 的 subagent 文件没有，会被解析器拒绝；插件清单格式是对方的。本期标 `Blocked`。

---

## 接口与状态所有权

**唯一状态所有者：磁盘。** 本功能不持有任何跨调用的运行时状态。

| 状态 | 所有者 | 谁读 | 谁写 |
|---|---|---|---|
| 外部资产 | 外部 Agent 的目录（`~/.claude`、`~/.codex`、`<project>/.claude`…） | `migration` 模块（**只读**） | 外部 Agent（不是我们） |
| 我们的 provider 账号 | `~/.atomcode/config.toml` | `ProviderBook` | **只有** `ProviderBook::create_account`（Task 8） |
| 我们的技能 / 指令 / 权限 | 产品用户树 / 项目树 | 既有的 `skills` / `instructions` / `permission_settings` 加载器 | 既有流程（不是本功能） |

**边界规则（不可绕过）：**

1. `capabilities::migration` **只读**。它不写、不删、不改任何文件，连 `config.toml` 都不碰。
2. **只有 `ProviderBook` 能写 `config.toml`**。本功能不自己拼 TOML、不自己造原子替换、不自己加锁 —— 复用既有写入路径（`ConfigStore` 文件锁 + `toml_edit` 逐键 patch + 原子替换）。
3. **凭据只在宿主侧处理**。`migration` 只产出 `ApiKeyRef::EnvVar` / `ApiKeyRef::Plaintext` 两种标记，**不解析、不展开、不落盘**密钥。
4. **不进 runtime、不进 `describe_self`**。这是 driver 层的本地诊断，不是一行能力。

---

## 失败与回滚语义

| 失败点 | 行为 | 回滚 |
|---|---|---|
| 外部目录不存在 | 该来源不出现 / 该项不报 | 不适用（无改动） |
| 外部文件损坏（坏 JSON / 坏 TOML） | 该项报 `Blocked`，其余项照常 | 不适用（只读） |
| 单项探测抛错 | 该项的 `detail` 说明原因，其余项照常 | 不适用 |
| 扫描期间文件被删 | 视同不存在 | 不适用 |
| `--apply` 时配置已被别的进程改过 | `ProviderBook` 在锁内读最新文档再决策（`provider_book.rs:165`），**合并**而不是拒绝；只有 id 冲突才拒绝 | 不适用（我们没写） |
| `--apply` 时 id 已被占用 | 报冲突，**不覆盖** | 不适用（我们没写） |
| `--apply` 写入中途失败 | `ConfigStore` 事务失败 → 整个文档不落盘 | `ProviderBook::create_account` 自带「账号与模型一次写」语义，失败不留半个账号 |
| 明文密钥未确认 | **什么都不写**（不是写一半） | 不适用（我们没写） |
| 非交互且无 `--yes` | 跳过需确认的项，退出码非 0 | 不适用（我们没写） |

**关键不变式：失败时 `config.toml` 要么完全没变，要么是完整的一次提交。** 绝不出现「账号写了、模型没写」的中间态 —— 这是 `ProviderBook::create_account` 存在的理由，本功能直接用它。

**没有一项资产可以凭空消失**：任何探测到的资产都必须出现在报告里（四态之一）。这是 Codex「残留兜底」在 AtomCode 的对应物。

---

## 交给 Codex 的验证要求

每个阶段结束后，Codex 独立跑并回报：

```bash
# 编译门（阻塞，AGENTS.md 规定）
bash gates/compile.sh

# 受影响 crate 的测试（用 nextest，不用 cargo test）
cargo nextest run -p atomcode-capabilities --features migration
cargo nextest run -p atomcode-cli
cargo nextest run -p atomcode-tui

# 分层与门禁
bash gates/layers.sh
bash gates/product-dir.sh
bash gates/tui-i18n.sh
bash gates/tui-layers.sh

# 格式（必须退出 0）
cargo fmt --all -- --check
```

**验证证据必须包含**：命令、退出状态、结果摘要、已验证的代码状态（commit SHA）、未覆盖范围。

**Codex 另行独立审查**：实际 diff、调用方是否都被覆盖（`standard_skill_dirs`、`permission_settings::load`、`instructions::load_instructions` 三处读取路径是否与报告的 `Active` 判定一致）、测试是否真的断言了行为而非实现。

**不适用项**：本功能不涉及会话日志、不涉及 runtime 生命周期，因此 `docs/adr/0024` 的会话事件日志判据与 runtime 生命周期不变量**不适用**。

---

## 交接模板（每次委派都用它）

外部 Agent 每次调用都是独立任务，**不会继承对话上下文**。每次委派必须给出：

1. **角色与禁止事项** —— CC 只写实现、不得跑测试或构建；Codex 写测试与验证、不得改业务实现。
2. **项目绝对路径、当前分支、基线 commit** —— `git rev-parse --short HEAD` 的实时值。
3. **用户需求与验收标准** —— 指向本文档与设计文档的**具体小节**，不要复制全文。
4. **相关文件与调用方** —— 具体路径与行号。
5. **必须遵守的架构约束** —— 本文「接口与状态所有权」与「分层约束」两节。
6. **工作区已有改动** —— `git status --porcelain` 的实时输出，以及「不得触碰他人改动」。
7. **本次允许修改的范围** —— 精确到文件。
8. **上一轮结果** —— 首次委派注明「尚无上一轮结果」；后续附审查问题与修复要求。
9. **本阶段要跑的验证命令** —— 从上面「交给 Codex 的验证要求」抄对应那几条。

---

## 文件结构

| 文件 | 职责 |
|---|---|
| `crates/atomcode-capabilities/src/migration/mod.rs` | 公共 API：`ScanContext`、`MigrationSource` trait、`builtin_sources()`、`scan()` |
| `crates/atomcode-capabilities/src/migration/report.rs` | 数据类型：`AssetKind` / `AssetStatus` / `AssetScope` / `AssetReport` / `SourceReport` / `MigrationReport` |
| `crates/atomcode-capabilities/src/migration/claude.rs` | `ClaudeSource`：探测 Claude Code 资产 |
| `crates/atomcode-capabilities/src/migration/codex.rs` | `CodexSource`：探测 Codex 资产 |
| `crates/atomcode-capabilities/src/migration/opencode.rs` | `OpenCodeSource`：探测 OpenCode 资产 |
| `crates/atomcode-capabilities/src/migration/text.rs` | 文案助手（`pub(crate)`，不进公共 API）—— 一件事一种说法，CLI 与 TUI 共用 |
| `crates/atomcode-capabilities/src/migration/provider.rs` | 中立 `ProviderAccountDraft` 类型 + **共享的归一包装**（`account_id_for` / `effective_endpoint` / `protocol_for_wire`）—— 与写入方共用同一条规则 |
| `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs` | 报告文案（product 表，三处同改） |
| `crates/atomcode-cli/src/import_cmd.rs` | `atomcode import` 子命令：扫描报告 / `--json` / `--apply` |
| `crates/atomcode-tui/src/migration.rs` | TUI 端口 trait + 视图类型（镜像 `setup.rs`） |
| `crates/atomcode-tui/src/commands.rs` | `/import` 命令集（新增 `ImportCommands`） |
| `crates/atomcode-cli/src/tui_migration.rs` | 端口实现：调 capabilities 扫描，转成 TUI 视图类型 |
| `crates/atomcode-coding/src/config.rs` | 宿主解析 OpenCode 的两个目录（XDG）—— capability 不读环境 |
| `crates/atomcode-coding/src/parts.rs` | 把技能根的解析公开出来，宿主按**运行时同一套规则**取技能根 |

**分层约束（`gates/layers.sh` 守）：** `atomcode-tui` **不得**引 `atomcode-coding` / `atomcode`（门禁的「UI 不认 Host,也不认 Product」）；`atomcode-i18n` 必须零 atomcode 依赖（「词表是叶子」）。

`migration` feature **要**引 `atomcode-config`：写入方会归一账号 id 与 endpoint，报告不跟着归一，回检就永远对不上 —— 规则必须**共用同一条**，不能各写一份。capabilities→config 这条边**本来就有**（`provider` / `mcp` / `instructions` / `session` 等 feature 都拉它），所以门禁并不因此改变；门禁禁的是 `atomcode-tui → atomcode-coding / atomcode`。

---

## Task 1: capabilities 的 migration 模块骨架

**Files:**
- Create: `crates/atomcode-capabilities/src/migration/mod.rs`
- Create: `crates/atomcode-capabilities/src/migration/report.rs`
- Create: `crates/atomcode-capabilities/src/migration/text.rs`（**最小版**，Task 2 再扩）
- Create: `crates/atomcode-capabilities/src/migration/claude.rs`（**空壳**，Task 2 填）
- Create: `crates/atomcode-capabilities/src/migration/codex.rs`（**空壳**，Task 3 填）
- Create: `crates/atomcode-capabilities/src/migration/provider.rs`（**两个类型 + 共享归一包装**，Task 7 只加来源草案）
- Modify: `crates/atomcode-capabilities/src/skills/registry.rs`（**记录它发现了什么** —— 报告只依据这份记录裁决，见下）
- Modify: `crates/atomcode-capabilities/src/skills/mod.rs`（导出 `DiscoveredTemplate` 和 `EntryKind`）
- Modify: `crates/atomcode-capabilities/Cargo.toml`
- Modify: `crates/atomcode-capabilities/src/lib.rs`
- Modify: `crates/atomcode-config/src/provider_book.rs`（供 provider 草案复用归一规则）
- Modify: `gates/compile.sh`（把 `migration` 加入 feature 编译门）
- Modify: `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs`（**只加 15 条**，Task 2 再加其余）

> **为什么注册表也要改**：「让真加载器说出来」的字面意思是**它说什么就是什么**，
> 报告不许自己再推断一遍。注册表**走过一遍**目录，它知道自己在哪一层停下了、
> 跳过了哪些目录、受什么深度上限约束、**哪一份同名定义赢了** —— 而报告若自己
> 再走一遍去推算「注册表会不会走到这个条目」，两遍遍历在符号链接、别名路径、
> 忽略目录上会给出**不同答案**。
>
> 所以注册表要**记录它发现了什么**（含被同名的另一份顶掉的那一份），报告先查这份记录，再查诊断。没有匹配的记录或诊断时，
> `Scanned` 候选按「没有生效，加载器没有给出原因」处理；`Foreign` 候选按解析结果审查复制建议。
>
> 这一步是**纯增量**：新增发现记录和入口形态类型、记录向量、两个按索引槽追踪胜者的映射及一个访问器，`insert_parsed` 的既有
> 行为（覆盖诊断只在定义不同时记、警告的记录、`diagnose` 的调用）一字不变。
> 但 `skills/registry.rs` 是**运行时也在用**的模块，所以验收要跑
> `cargo nextest run -p atomcode-capabilities`（**默认 feature**）。
>
> 规格见 `docs/plans/2026-11-19-task1-architecture-handoff.md`。


- [ ] **Step 1: 写失败的测试**

在 `crates/atomcode-capabilities/src/migration/mod.rs` 末尾加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_no_assets_is_empty_not_an_error() {
        let report = MigrationReport::default();

        assert!(report.sources.is_empty());
        assert_eq!(report.total_assets(), 0);
    }

    #[test]
    fn the_report_counts_assets_across_sources() {
        let report = MigrationReport {
            sources: vec![
                SourceReport {
                    source_id: "codex",
                    display_name: "Codex",
                    assets: vec![],
                },
                SourceReport {
                    source_id: "claude-code",
                    display_name: "Claude Code",
                    assets: vec![AssetReport {
                        kind: AssetKind::Skills,
                        name: "web-access".into(),
                        path: "/home/u/.claude/skills/web-access".into(),
                        scope: AssetScope::User,
                        status: AssetStatus::Active,
                        detail: "directly read".into(),
                    }],
                },
            ],
        };

        assert_eq!(report.total_assets(), 1);
        assert_eq!(report.assets_with_status(AssetStatus::Active), 1);
        assert_eq!(report.assets_with_status(AssetStatus::Blocked), 0);
    }

    #[test]
    fn scanning_with_no_sources_produces_an_empty_report() {
        let ctx = ScanContext {
            os_home: PathBuf::from("/home/u"),
            project: PathBuf::from("/work"),
            product: crate::ProductDirs::new("/home/u/.ours", ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let report = scan(&[], &ctx);

        assert!(report.sources.is_empty());
        assert_eq!(report.total_assets(), 0);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-capabilities --features migration`
Expected: FAIL —— `cannot find module migration` / `unresolved import`。

- [ ] **Step 3: 加 feature 与模块声明**

`crates/atomcode-capabilities/Cargo.toml` 的 `[features]` 段追加：

```toml
# Scan assets left behind by other AI coding agents (Claude Code / Codex) and
# report what we already read natively, what can be brought over, and what is
# blocked. Read-only; the provider-account draft it produces is written by the
# host, never here.
#
# It DOES pull `atomcode-config`, for two reasons: the id and the endpoint
# have to be normalised by the SAME rule the writer uses (otherwise the
# re-check never matches), and the real loaders' verdicts are what say
# "already in effect". `ProductDirs` lives in THIS crate (`product_dirs.rs`),
# so the trees still cost no dependency.
migration = [
    "tools",
    "instructions",
    "mcp",
    "skills",
    "dep:atomcode-i18n",
    "dep:atomcode-config",
    "dep:toml",
]
```

> **这些 feature 都是为了复用真实验证器**，不是为了拿能力。报告里「已经生效」这句话，
> 只能由**真的加载器**说出来 —— 自己再写一份校验，迟早与真加载器不一致，而
> 「说生效、其实不生效」正是这个报告最不能有的错：
>
> | feature | 复用什么 | 用来判什么 |
> |---|---|---|
> | `instructions` | `crate::instructions::load_instructions` | 指令文件是 `Loaded` / `Empty` / `Unreadable` / `Shadowed` 中的哪一个 |
> | `skills` | `crate::skills::SkillRegistry` | 这个技能**是不是**在生效的那一个（同名的另一个目录会覆盖它） |
> | `mcp` | `crate::mcp::parse_mcp_servers` | 整个 MCP 文件我们的加载器会不会拒绝 |
> | `tools` | `crate::tools::permission_rules::PermissionRules::parse` | 权限文件里的规则我们的加载器会不会拒绝 |
> | `dep:atomcode-config` | `provider_book::account_id_for` / `effective_endpoint` | 账号 id 与 endpoint **写成什么样**（写入方会归一，报告不归一就对不上） |
>
> **不新增传递依赖**：`tools` 在 default features 里；`instructions` / `mcp` /
> `session` 各自已经拉 `atomcode-config`，CLI 与 TUI 本来就有；
> `skills` 不拉任何东西。capabilities→config 这条边**本来就有**（`provider`、
> `notify`、`setup`、`mcp`、`instructions`、`session`、`offline` 都拉），
> 所以 `gates/layers.sh` 的分层规则并不因此改变 —— 那条门禁禁的是
> `atomcode-tui → atomcode-coding / atomcode`，不是 capabilities→config。
>
> 之前这里写着「migration 不引 atomcode-config」，那是基于一个**错误前提**：
> 以为 UI 不能碰 config。实际上 TUI 的依赖图里本来就有 config（经 `mcp` /
> `instructions` / `session`），门禁也没有这条限制。现在要引，是因为**写入方会
> 归一 id 与 endpoint**，报告不跟着归一，回检就永远对不上。

`toml` 与 `atomcode-i18n` 已是 optional 依赖（第 130 / 23 行），**不要**再往 `[dependencies]` 加。

`crates/atomcode-capabilities/src/lib.rs` 末尾追加：

```rust
/// Scan the assets other AI coding agents left behind, and report what we
/// already read natively, what can be brought over, and what is blocked.
///
/// Opt-in `migration` feature. Read-only: the provider-account draft this
/// produces is written by the host, not here. See
/// `docs/plans/2026-11-17-external-agent-asset-migration-design.md`.
#[cfg(feature = "migration")]
pub mod migration;
```

- [ ] **Step 4: 写 report.rs**

`crates/atomcode-capabilities/src/migration/report.rs`：

```rust
//! What a scan found: one row per asset, each carrying a status.
//!
//! Four statuses, not two. "Compatible" and "needs conversion" cannot say the
//! one thing a person most needs to know before deciding: whether the asset
//! can come over at all. A lost session history and a skill directory that
//! only needs copying are both "not native", but only one of them is
//! recoverable.

use serde::Serialize;
use std::path::PathBuf;

/// What kind of asset a row is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetKind {
    Instructions,
    Skills,
    Commands,
    McpServers,
    Permissions,
    Hooks,
    ProviderConfig,
    Agents,
    Sessions,
    Plugins,
}

/// Where an asset stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetStatus {
    /// AtomCode reads this exact path today. Nothing to do.
    Active,
    /// Its equivalent is already in OUR configuration — a previous import
    /// brought it over. This is the re-check half of the loop: without it,
    /// a second run would say "can be brought over" about something that is
    /// already there.
    Imported,
    /// There is a known way to bring it over.
    Actionable,
    /// It cannot come over (lossy, or this release does not do it).
    Blocked,
}

/// Which level of the tree an asset lives at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetScope {
    /// Under the person's home directory.
    User,
    /// Under the current project.
    Project,
}

/// One asset the scan found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetReport {
    pub kind: AssetKind,
    /// The asset's own name: a skill name, `settings.json`, a session file.
    pub name: String,
    /// Where it is on disk now.
    pub path: PathBuf,
    pub scope: AssetScope,
    pub status: AssetStatus,
    /// One sentence for a person: why this status, and what to do about it.
    ///
    /// Localized here so both front ends show the same words — see the design
    /// document's "文案只出一处". When the asset can be brought over, this
    /// sentence **names the exact path to copy it to**, so "copy it to the
    /// user tree" is not a thing the person has to work out.
    pub detail: String,
}

/// Everything one external agent had.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceReport {
    pub source_id: &'static str,
    pub display_name: &'static str,
    pub assets: Vec<AssetReport>,
}

/// Everything every external agent had.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    pub sources: Vec<SourceReport>,
}

impl MigrationReport {
    /// How many assets the whole scan found.
    pub fn total_assets(&self) -> usize {
        self.sources.iter().map(|s| s.assets.len()).sum()
    }

    /// How many assets are in one status.
    pub fn assets_with_status(&self, status: AssetStatus) -> usize {
        self.sources
            .iter()
            .flat_map(|s| s.assets.iter())
            .filter(|a| a.status == status)
            .count()
    }
}
```

- [ ] **Step 5: 写 mod.rs 的骨架**

Task 1 提供 `identity`、`same_file` 与 `file_id`，供 Task 2 的跨层指令去重使用。
`same_file` 依次比较原路径、规范路径、设备号与 inode。设备号和 inode 比较仅覆盖 Unix。

应用下方骨架时保留 Step 1 的测试模块。

`crates/atomcode-capabilities/src/migration/mod.rs`：

```rust
//! Scan the assets other AI coding agents left behind.
//!
//! A neutral, read-only capability: it is handed the paths to look at and
//! hands back a report. It resolves no environment and writes no file — the
//! host owns where the trees are (`AGENTS.md`: "解析环境只在宿主") and owns
//! every write.
//!
//! The shape follows Codex's `external-agent-migration`: detect, then report,
//! and never let an asset silently vanish — every asset found ends up in the
//! report with one of four statuses.
//!
//! Opt-in `migration` feature. See
//! `docs/plans/2026-11-17-external-agent-asset-migration-design.md`.

// The sources land one per task; each helper here is used by the source that
// comes next. The allow comes off when the last source is in.
#![allow(dead_code)]

mod claude;
mod codex;
mod provider;
pub mod report;
mod text;

use std::path::{Path, PathBuf};

// The FORM the registry's walk found an entry in — a skill directory or a
// flat file. The INDEX cannot say this: both forms go into the skill index.
use crate::skills::EntryKind;

// `endpoint_in_use` is the endpoint an account will actually talk to. It is
// re-exported because `provider` is a **private** module: the host needs it
// to name the endpoint in its confirmation prompt, and the host cannot reach
// into a private module.
pub use provider::{endpoint_in_use, ApiKeyRef, ProviderAccountDraft};
pub use report::{AssetKind, AssetReport, AssetScope, AssetStatus, MigrationReport, SourceReport};

/// Where to look. The host resolves all of it — this crate reads no environment.
///
/// **The two homes.** `os_home` is the person's home directory, where
/// `~/.claude` and `~/.codex` live. It is NOT the product's tree: the global
/// instruction tier reads `<product tree>/ATOMCODE.md` and the permission
/// loader reads `<product tree>` — see `instructions.rs:128` and
/// `permission_settings.rs:286`. The field is named `os_home` rather than
/// `home` because this crate already has another `home` meaning, and mixing
/// them up would make the report claim a file is read when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanContext {
    /// The person's home directory (`~/.claude` lives under it).
    pub os_home: PathBuf,
    /// The current project root.
    pub project: PathBuf,
    /// Where OUR trees are, so the report can name a target path instead of
    /// saying "copy it to the user tree" and leaving the person to guess.
    ///
    /// `ProductDirs` lives in this crate (`product_dirs.rs`), so carrying it
    /// costs no new dependency.
    pub product: crate::ProductDirs,
    /// The skill directories the runtime ACTUALLY uses, in the same LOW→HIGH
    /// order.
    ///
    /// **The host resolves these**, because only the host knows the user's
    /// configured `extra_skill_dirs` (`coding/src/parts.rs:558`
    /// `resolve_skill_dirs_at` expands `~`, makes relative paths
    /// project-relative, and puts the extras FIRST — before the standard
    /// list).
    ///
    /// The capability cannot recompute them: `standard_skill_dirs` alone
    /// would miss the configured roots, and the registry could then pick a
    /// DIFFERENT winner for a same-named skill than the runtime does — while
    /// the report claimed to be saying "the way the runtime does".
    ///
    /// **`None` and `Some(vec![])` mean different things.**
    ///
    /// - `None` = the host gave no list → the standard list
    ///   (`standard_skill_dirs`) is used.
    /// - `Some(vec![])` = the host EXPLICITLY asked for **no** skill roots —
    ///   the runtime has its skill tools turned off. The report must not
    ///   fall back to the standard list and then claim `~/.claude/skills/foo`
    ///   is `Active`: nothing is loading it.
    /// - `Some(dirs)` = exactly these roots, in this order.
    pub skill_dirs: Option<Vec<PathBuf>>,
    /// Where OpenCode keeps its configuration
    /// (`$XDG_CONFIG_HOME/opencode`, default `~/.config/opencode`).
    ///
    /// **The host resolves it** — the capability reads no environment
    /// (`AGENTS.md`: 「解析环境只在宿主」).
    pub opencode_config_dir: PathBuf,
    /// Where OpenCode keeps its data
    /// (`$XDG_DATA_HOME/opencode`, default `~/.local/share/opencode`).
    ///
    /// The host resolves it, for the same reason.
    pub opencode_data_dir: PathBuf,
}

/// One external agent's assets.
pub trait MigrationSource: Send + Sync {
    /// A stable id, e.g. `"claude-code"`.
    fn id(&self) -> &'static str;
    /// The name a person knows this agent by.
    fn display_name(&self) -> &'static str;
    /// Look, and report. Must not write anything.
    ///
    /// `truth` is the registry load `scan()` did — **the ONE load for the
    /// whole report**. A source that loaded its own would see a different
    /// moment of the same disk than the source next to it, and the report
    /// would contradict itself.
    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport>;
    /// The provider accounts this agent has configured, if any.
    fn provider_drafts(&self, _ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        Vec::new()
    }
    /// Why the provider accounts could NOT be read, when that is the case.
    ///
    /// **Absence and failure are different answers.** A source whose config
    /// file is broken has NOT "no provider accounts" — it has an unreadable
    /// file. Without this, `--apply` would print "No provider accounts were
    /// found" and exit successfully, and the person would never learn their
    /// config was not read at all.
    fn provider_drafts_error(&self, _ctx: &ScanContext) -> Option<String> {
        None
    }
}

/// The agents we know how to read.
pub fn builtin_sources() -> Vec<Box<dyn MigrationSource>> {
    vec![Box::new(claude::ClaudeSource), Box::new(codex::CodexSource)]
}

/// Scan every source. A source that finds nothing contributes no rows.
///
/// Only `detect()` is called here. The provider rows are part of what a
/// source detects — they are added by that source's own `detect()`, from the
/// same drafts the importer uses (Task 7). So this function needs no draft
/// machinery, and Task 1 stands on its own.
pub fn scan(sources: &[Box<dyn MigrationSource>], ctx: &ScanContext) -> MigrationReport {
    let mut report = MigrationReport::default();
    // **ONE registry load for the whole report.** The registry is the
    // authority on what is in effect; loading it per source would let two
    // sources in one report see two different moments of the same disk — and
    // the report would contradict itself.
    let truth = SkillTruth::load(ctx);
    for source in sources {
        let assets = source.detect(ctx, &truth);
        if assets.is_empty() {
            continue;
        }
        report.sources.push(SourceReport {
            source_id: source.id(),
            display_name: source.display_name(),
            assets,
        });
    }
    report
}

/// The report row for a provider account.
///
/// **The row's NAME is the name the source wrote**, and the normalized
/// account id the importer will write is in `draft.id` — that id, not the
/// row's name, is what the re-check matches. The name stays the source's own
/// because two definitions can normalize to the same id (`"foo bar"` and
/// `"foo-bar"` both become `foo-bar`): only the original name tells them
/// apart, and it is the one the person recognises. Every source builds its
/// provider rows through this one function, so the rule lives in one place.
///
/// The `detail` is passed in rather than built here: this module has no
/// sentence of its own, and the source that found the account knows which
/// wording applies to it.
pub fn provider_row(
    draft: &ProviderAccountDraft,
    status: AssetStatus,
    detail: String,
) -> AssetReport {
    // A draft that cannot be imported is reported as `Blocked`, with the
    // reason it carries — not as "can be brought over".
    let (status, detail) = match &draft.blocked {
        Some(reason) => (AssetStatus::Blocked, reason.clone()),
        None => (status, detail),
    };
    AssetReport {
        kind: AssetKind::ProviderConfig,
        // The name the SOURCE wrote, not the normalized id: it is what the
        // person recognises, and it is what tells two colliding definitions
        // apart.
        name: draft.source_name.clone(),
        path: draft.source_path.clone(),
        scope: draft.scope,
        status,
        detail,
    }
}

/// The last component of a path, for a report row's name.
fn last_component(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default();
    name.to_string_lossy().into_owned()
}

/// A tree of skills: do we scan it, or is it someone else's?
///
/// **The tree decides where the CANDIDATES come from.**
///
/// - [`SkillTree::Scanned`] — a tree the REGISTRY scans. The candidates are
///   the registry's own discoveries; the report does not walk the disk a
///   second time. An asset the registry never found is **not in the report
///   at all**: the registry does not think it is an asset, and neither does
///   the report.
/// - [`SkillTree::Foreign`] — a tree NOBODY scans. The report walks it
///   itself, with the report's own limits and the report's own sentences
///   about them.
///
/// `Clone`, not `Copy`: `Foreign` owns a `PathBuf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillTree {
    /// We scan this tree — the registry does. Its discoveries are the
    /// candidates, and its verdicts decide whether each one is really in
    /// effect — or merely shadowed, or rejected.
    Scanned,
    /// We do NOT scan this tree. Nothing in it is in effect — unless the
    /// registry happens to load this tree anyway (a host that put it in the
    /// skill roots), in which case the registry's own word wins. Otherwise
    /// every row says where to bring it over.
    ///
    /// `target` is the directory to copy into (`skills/` or `commands/`)
    /// itself — no further `skills`/`commands` component is added. Each row
    /// joins its own name onto it, shaped like the asset: `<name>` for a
    /// skill directory, `<name>.md` for a flat command file.
    Foreign { target: PathBuf },
}

impl SkillTree {
    /// The depth-limit sentence — **WHO** stopped the walk.
    ///
    /// The registry has a depth limit of its own, and it says so in its own
    /// words — but only for a tree it scans. In a tree it never scanned, the
    /// **report's** walk is what stopped, and saying "the skill loader
    /// rejected it" would put words in the loader's mouth about a file the
    /// loader never looked at.
    fn depth_limit_detail(&self) -> String {
        match self {
            SkillTree::Scanned => text::skill_rejected(&text::skill_depth_limit()),
            SkillTree::Foreign { .. } => text::report_depth_limit(),
        }
    }

    /// The symlink-cycle sentence — **WHO** stopped the walk. The same
    /// split as [`Self::depth_limit_detail`].
    fn symlink_cycle_detail(&self) -> String {
        match self {
            SkillTree::Scanned => text::skill_rejected(&text::skill_symlink_cycle()),
            SkillTree::Foreign { .. } => text::report_symlink_cycle(),
        }
    }
    /// The status and sentence for one skill found in a FOREIGN tree.
    ///
    /// `path` is the skill directory or the flat command file; `name` is the
    /// registry's name for it (for a command file, already without `.md`).
    /// `parses` is whether the registry's own parser accepts the file.
    ///
    /// **Only for a tree the registry does NOT scan** — a tree it scans gets
    /// its candidates from the registry itself, and the registry's own
    /// verdict (see [`registry_rows`]).
    fn judge(
        &self,
        registry: &SkillTruth,
        name: &str,
        path: &Path,
        parses: bool,
    ) -> (AssetStatus, String) {
        match self {
            SkillTree::Scanned => registry.verdict(name, path, parses),
            SkillTree::Foreign { target } => {
                // A tree the registry does NOT scan can still hold an asset
                // the registry DID load: a host that put this tree in its
                // skill roots loaded it, and it is in effect right now. Then
                // "copy it over" is nonsense — the registry says what is
                // true, and we say that.
                let is_skill_dir = probe_dir(path).unwrap_or(false);
                if let Some(verdict) = registry.registry_says(name, path, is_skill_dir) {
                    return verdict;
                }
                // The registry found this asset and could not load it — with
                // its own reason. Copying it over would only move the
                // failure.
                if let Some((reason, rejected)) = registry.registry_failure(path, is_skill_dir) {
                    return if rejected {
                        (AssetStatus::Blocked, text::skill_rejected(&reason))
                    } else {
                        (AssetStatus::Blocked, text::report_entry_unusable(&reason))
                    };
                }
                // A file the loader cannot parse will not load there either:
                // copying it over only moves the failure. (The registry never
                // looked at this file, so this is not the loader's word — it
                // is ours.)
                if !parses {
                    return (AssetStatus::Blocked, text::foreign_unparseable());
                }
                // The registry loads flat commands as top-level `*.md` files
                // (`registry.rs:340`): a copy named `<name>` without the
                // extension would never be scanned, and the advice would not
                // work. A skill directory's name is its directory name.
                let is_command_file =
                    path.extension().and_then(|e| e.to_str()) == Some("md") && !is_skill_dir;
                let destination = if is_command_file {
                    target.join(format!("{name}.md"))
                } else {
                    target.join(name)
                };
                // "Bring it over" — an ACTION, not a promise: whether it wins
                // against a same-named higher-priority definition is not ours
                // to promise.
                (AssetStatus::Actionable, text::skill_copyable(&destination))
            }
        }
    }
}

// The skill helpers live HERE, not in one source: three sources report
// skills (Claude Code, OpenCode, and the shared `.agents` tree), and a
// helper that only one of them can reach is a helper the other two will
// re-implement — and then they will drift from the registry.
/// One row per skill directory (`<name>/SKILL.md`) or command file (`<name>.md`).
///
/// **Where the CANDIDATES come from depends on the tree.**
///
/// - [`SkillTree::Scanned`] — a tree the registry scans. The candidates are
///   **the registry's own discoveries**: it walked that tree, so it knows
///   what is under it. The report does **not** walk the disk a second time:
///   two walks of the same disk disagree (a symlink, an ignored directory, a
///   depth limit), and the report's answer must be the runtime's answer. So
///   an asset the registry never found is **not in the report at all** — it
///   is not ours, and listing it would mean explaining "why it is not in
///   effect", which is itself a guess.
/// - [`SkillTree::Foreign`] — a tree the registry does NOT scan. Nobody else
///   knows what is under it, so the report walks it itself. That walk is the
///   REPORT's own, with the report's own limits and the report's own
///   sentences about them.
///
/// **Whether the asset is really in effect is decided by the real registry**,
/// not by "the file exists". Two things make existence insufficient: a
/// `SKILL.md` that fails to parse is skipped, and a skill with the same name
/// in a **later, higher-priority** directory overrides this one — the
/// registry loads directories from low to high priority, so the last one
/// loaded wins. A report that says "already in the skill catalog" about an
/// overridden file is wrong.
///
/// **The row's name is the name the registry uses** — the `name:` in the
/// frontmatter, else the directory name or file stem, normalized — so the
/// report and the registry cannot disagree about what a skill is called.
fn skill_rows(
    kind: AssetKind,
    scope: AssetScope,
    root: &Path,
    registry: &SkillTruth,
    tree: SkillTree,
) -> Vec<AssetReport> {
    match tree {
        // A tree the registry scans: the registry's discoveries ARE the
        // candidates.
        SkillTree::Scanned => registry_rows(kind, scope, root, registry),
        // A tree nobody else scans: the report walks it itself.
        SkillTree::Foreign { .. } => foreign_rows(kind, scope, root, registry, &tree),
    }
}

/// The path the CALLER named, for an asset the registry found under a root.
///
/// The registry walks the LITERAL root it was handed, and the caller may
/// have named the same directory another way (the same path spelled two
/// ways — `/var` and `/private/var` on macOS). The report shows the path
/// the caller named, not the registry's spelling of it.
///
/// So: take the part of the asset's path that lies under the registry's
/// root, and re-attach it to the caller's root.
///
/// `None` when the asset is not under the registry's root at all — the
/// caller keeps its own path then.
fn re_spelled(root: &Path, registry_root: &Path, asset: &Path) -> Option<PathBuf> {
    let relative = asset.strip_prefix(registry_root).ok()?;
    Some(root.join(relative))
}

/// The rows for a tree the REGISTRY scans — its own discoveries.
///
/// **The candidates are the registry's**, filtered to the ones found under
/// THIS root: the root is what the caller named, and the registry says which
/// root found each entry.
///
/// **One row per DISCOVERY RECORD**, not per asset: the same directory
/// reached through two roots is two records, and the registry resolved them
/// against each other — the earlier one lost its name slot. Reporting the
/// loser as "in effect" would be the report's guess about which of two
/// records the runtime kept.
fn registry_rows(
    kind: AssetKind,
    scope: AssetScope,
    root: &Path,
    registry: &SkillTruth,
) -> Vec<AssetReport> {
    let mut out = Vec::new();
    let root_identity = identity(root);
    for found in &registry.discovered {
        // **Under THIS root?** The root is the one the caller named; the
        // registry may have spelled it another way (the same directory under
        // another name), so the identity settles it.
        if found.root != root_identity {
            continue;
        }
        // The row's PATH is the ASSET: the bundle directory for a skill
        // directory, the file for a flat template. **Spelled the way the
        // CALLER named it** — the registry may have spelled the same
        // directory another way, and the report shows the path the person
        // gave us.
        let path = re_spelled(root, &found.root_path, &found.asset_path)
            .unwrap_or_else(|| found.asset_path.clone());
        // **The registry's own verdict for THIS record**: it kept the record
        // that holds the name, and marked the others displaced. The flag was
        // set by the registry's slot resolution, not inferred here.
        let (status, detail) = if found.replaced {
            (AssetStatus::Blocked, text::skill_overridden(&found.name))
        } else {
            (AssetStatus::Active, text::skill_active())
        };
        out.push(AssetReport {
            kind,
            name: found.name.clone(),
            path,
            scope,
            status,
            detail,
        });
    }
    // **The assets the walk found but could NOT load.** They are assets too:
    // the file is there, the walk reached it, and the loader refused it.
    // Without a row they would vanish — and no asset may vanish.
    for failed in &registry.failed {
        if failed.root != root_identity {
            continue;
        }
        let (status, detail, path) =
            if matches!(failed.failure, crate::skills::LoadFailure::Rejected) {
                // The LOADER's own words: it read the file and refused it.
                // The row points at the ASSET — the bundle directory — the
                // same place a loaded skill's row points.
                let asset = re_spelled(root, &failed.root_path, &failed.asset_path)
                    .unwrap_or_else(|| failed.asset_path.clone());
                (
                    AssetStatus::Blocked,
                    text::skill_rejected(&failed.reason),
                    asset,
                )
            } else {
                // A fact about the ENTRY: the loader never treated it as a
                // skill at all, so this is not the loader's story to tell.
                // The row points at the ENTRY — that is the thing the person
                // has to look at.
                let entry = re_spelled(root, &failed.root_path, &failed.entry_path)
                    .unwrap_or_else(|| failed.entry_path.clone());
                (
                    AssetStatus::Blocked,
                    text::report_entry_unusable(&failed.reason),
                    entry,
                )
            };
        out.push(AssetReport {
            kind,
            name: failed.name.clone(),
            path,
            scope,
            status,
            detail,
        });
    }
    out
}

/// The rows for a tree NOBODY scans — the report walks it itself.
///
/// **The report's own walk**, with the report's own limits: the depth
/// bound, the ignored cache directories, the symlink-cycle guard. These are
/// not guesses about what the registry found — the registry never looked at
/// this tree — they are the boundaries of what the report can reach.
fn foreign_rows(
    kind: AssetKind,
    scope: AssetScope,
    root: &Path,
    registry: &SkillTruth,
    tree: &SkillTree,
) -> Vec<AssetReport> {
    let mut out = Vec::new();
    // Not `is_dir()`: it reports a directory we may not read as absent.
    match probe_dir(root) {
        Ok(true) => {}
        Ok(false) => {
            // "Not a directory" is not the same as "not there". A root that
            // IS there but is not a directory we can walk — a dangling
            // symlink, a regular file — is a row, not silence: the person
            // has to see that their tree is not what they think it is.
            if entry_exists(root) {
                out.push(AssetReport {
                    kind,
                    name: last_component(root),
                    path: root.to_path_buf(),
                    scope,
                    status: AssetStatus::Blocked,
                    detail: if is_broken_link(root) {
                        text::broken_link()
                    } else {
                        text::not_a_directory()
                    },
                });
            }
            return out;
        }
        Err(reason) => {
            out.push(AssetReport {
                kind,
                name: last_component(root),
                path: root.to_path_buf(),
                scope,
                status: AssetStatus::Blocked,
                detail: text::unreadable_file(&reason),
            });
            return out;
        }
    }
    // **Grouping directories are walked**: `skills/group/deploy/SKILL.md` is a
    // real skill, and a report that only looked at the immediate children
    // would drop it — a valid, active skill that vanishes from the report.
    const MAX_DEPTH: usize = 8;
    let mut ancestors: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    collect_skill_rows(
        kind,
        scope,
        root,
        0,
        MAX_DEPTH,
        &mut ancestors,
        registry,
        tree,
        &mut out,
    );
    out
}

/// Walk one FOREIGN skill directory — the REPORT's own walk.
///
/// **Only for a tree the registry does NOT scan.** A tree it scans gets its
/// candidates from the registry's own discoveries, so this walk would be a
/// second, disagreeing answer to the same question.
///
/// The limits here (the depth bound, the ignored cache directories, the
/// symlink-cycle guard) are **the report's own reach** — not a guess about
/// what the registry found, because the registry never looked at this tree.
#[allow(clippy::too_many_arguments)]
fn collect_skill_rows(
    kind: AssetKind,
    scope: AssetScope,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    ancestors: &mut std::collections::BTreeSet<PathBuf>,
    registry: &SkillTruth,
    tree: &SkillTree,
    out: &mut Vec<AssetReport>,
) {
    // A row about `dir` itself. The name matches the PATH this row reports
    // (`dir`), not the skills root it was found under.
    let blocked = |detail: String| AssetReport {
        kind,
        name: last_component(dir),
        path: dir.to_path_buf(),
        scope,
        status: AssetStatus::Blocked,
        detail,
    };
    if depth > max_depth {
        out.push(blocked(tree.depth_limit_detail()));
        return;
    }
    // The cycle guard: a symlink back to an ancestor would otherwise be
    // walked forever. The registry does the same (`registry.rs:320`).
    let identity = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(error) => {
            out.push(blocked(text::unreadable_file(&error.to_string())));
            return;
        }
    };
    if !ancestors.insert(identity.clone()) {
        out.push(blocked(tree.symlink_cycle_detail()));
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            out.push(blocked(text::unreadable_file(&error.to_string())));
            ancestors.remove(&identity);
            return;
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    // An entry that cannot be read is a row, not a silent skip: a skill we
    // could not look at is one we could not promise works.
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => out.push(blocked(text::unreadable_file(&error.to_string()))),
        }
    }
    paths.sort();
    // A row about one entry we could not tell the type of. The scan goes on
    // with the rest.
    let blocked_entry = |path: &Path, reason: String| AssetReport {
        kind,
        name: last_component(path),
        path: path.to_path_buf(),
        scope,
        status: AssetStatus::Blocked,
        detail: text::unreadable_file(&reason),
    };
    for path in paths {
        // Not `is_file()` / `is_dir()`: they report an entry we may not stat
        // as absent, and the entry would vanish from the report.
        let is_file = match probe_file(&path) {
            Ok(is_file) => is_file,
            Err(reason) => {
                out.push(blocked_entry(&path, reason));
                continue;
            }
        };
        let is_dir = if is_file {
            false
        } else {
            match probe_dir(&path) {
                Ok(is_dir) => is_dir,
                Err(reason) => {
                    out.push(blocked_entry(&path, reason));
                    continue;
                }
            }
        };
        if !is_file && !is_dir {
            // The entry is there, but it is neither a regular file nor a
            // directory. It is NOT absent — and an absent entry would be
            // skipped, but a present one that cannot be used must be a row:
            // a dangling symlink (the target is gone), a FIFO, a socket.
            //
            // `metadata` FOLLOWS the link, so a link whose target is gone
            // reads as "not there" — and the asset would vanish from the
            // report. `symlink_metadata` does not follow it, so the entry
            // itself is still visible.
            if !entry_exists(&path) {
                continue;
            }
            let name = if path.extension().and_then(|e| e.to_str()) == Some("md") {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                last_component(&path)
            };
            // Computed BEFORE `path` moves into the row: the link has to be
            // asked about while the path is still ours.
            let detail = if is_broken_link(&path) {
                text::broken_link()
            } else {
                text::not_a_regular_file()
            };
            out.push(AssetReport {
                kind,
                name,
                path,
                scope,
                status: AssetStatus::Blocked,
                detail,
            });
            continue;
        }
        if is_file {
            // Flat command templates are TOP-LEVEL only (`registry.rs:340`):
            // a skill's own `*.md` files are its resources, not commands.
            if depth == 0 && path.extension().and_then(|e| e.to_str()) == Some("md") {
                // The name the registry gives it (`registry.rs:342` parses
                // flat command files the same way). If it does not parse,
                // fall back to the stem; the registry's diagnostic for this
                // path then says why it is not in effect.
                let (name, parses) = match crate::skills::skill::parse_skill_file(&path, None) {
                    Ok(parsed) => (parsed.skill.name, true),
                    Err(_) => {
                        let stem = path.file_stem().unwrap_or_default();
                        (stem.to_string_lossy().into_owned(), false)
                    }
                };
                let (status, detail) = tree.judge(registry, &name, &path, parses);
                out.push(AssetReport {
                    kind,
                    name,
                    path,
                    scope,
                    status,
                    detail,
                });
            }
        } else if is_dir {
            // The registry skips dependency/cache directories
            // (`registry.rs:13`) — the report must skip them too, or it
            // would report thousands of vendored files as skills.
            if is_ignored_skill_directory(&path) {
                continue;
            }
            // Not `is_file()`: a `SKILL.md` we may not stat would look like no
            // `SKILL.md` at all, the directory would be walked as a grouping
            // directory, and the report would blame the depth limit for what
            // is really an unreadable file.
            let skill_md = path.join("SKILL.md");
            // "No `SKILL.md`" and "a `SKILL.md` that is not a regular file"
            // are different: only the first is a grouping directory. A
            // `SKILL.md` that is itself a directory would otherwise be walked
            // as a group, find nothing, and the asset would vanish from the
            // report without a word.
            match probe_dir(&skill_md) {
                Ok(false) => {}
                Ok(true) => {
                    // **The report's own finding** — the loader never
                    // treated this as a skill at all.
                    out.push(AssetReport {
                        kind,
                        name: last_component(&path),
                        path: skill_md,
                        scope,
                        status: AssetStatus::Blocked,
                        detail: text::report_entry_unusable("SKILL.md is a directory, not a file"),
                    });
                    continue;
                }
                Err(reason) => {
                    out.push(blocked_entry(&path, reason));
                    continue;
                }
            }
            let is_skill = match probe_file(&skill_md) {
                Ok(is_skill) => is_skill,
                Err(reason) => {
                    out.push(blocked_entry(&path, reason));
                    continue;
                }
            };
            if !is_skill && entry_exists(&skill_md) {
                // The `SKILL.md` ENTRY is there, but it is not a regular file:
                // a dangling symlink (the target is gone), a FIFO, a socket.
                // `metadata` FOLLOWS the link, so a link whose target is gone
                // reads as "not there" — and the whole skill would vanish
                // from the report, walked as a grouping directory that finds
                // nothing.
                //
                // Computed BEFORE `skill_md` moves into the row: the link has
                // to be asked about while the path is still ours.
                let detail = if is_broken_link(&skill_md) {
                    text::broken_link()
                } else {
                    text::report_entry_unusable("SKILL.md is not a regular file")
                };
                out.push(AssetReport {
                    kind,
                    name: last_component(&path),
                    path: skill_md,
                    scope,
                    status: AssetStatus::Blocked,
                    detail,
                });
                continue;
            }
            if is_skill {
                // The name the registry gives it (`registry.rs:354` parses
                // skill directories the same way, with no namespace for a
                // standard directory). If it does not parse, fall back to
                // the directory name; the registry's diagnostic for this
                // path then says why it is not in effect.
                let (name, parses) =
                    match crate::skills::skill::parse_skill_dir(&path, &skill_md, None) {
                        Ok(parsed) => (parsed.skill.name, true),
                        Err(_) => (last_component(&path), false),
                    };
                let (status, detail) = tree.judge(registry, &name, &path, parses);
                out.push(AssetReport {
                    kind,
                    name,
                    path,
                    scope,
                    status,
                    detail,
                });
            } else {
                // A grouping directory: keep walking.
                collect_skill_rows(
                    kind,
                    scope,
                    &path,
                    depth + 1,
                    max_depth,
                    ancestors,
                    registry,
                    tree,
                    out,
                );
            }
        }
    }
    ancestors.remove(&identity);
}

/// The dependency/cache directories the registry skips.
///
/// The same list as `registry.rs:13` — the report must not walk into
/// `node_modules` and report thousands of vendored files as skills.
fn is_ignored_skill_directory(path: &Path) -> bool {
    const IGNORED: &[&str] = &[
        ".venv",
        "venv",
        "node_modules",
        ".git",
        ".hg",
        ".svn",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".tox",
        ".nox",
    ];
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    IGNORED
        .iter()
        .any(|ignored| name.eq_ignore_ascii_case(ignored))
}

/// What the real skill registry makes of the skills we found.
///
/// The registry is loaded ONCE per scan, from the same directory list the
/// runtime uses (the host's list, or `standard_skill_dirs` when it gave
/// none), so it sees the same collisions the runtime will see.
///
/// **Every path here is an [`identity`]**, not the string the registry
/// stored: the registry keeps paths literally (`skill.rs:759`,
/// `registry.rs:225`), and the same file can be spelled two ways (`/var` and
/// `/private/var` on macOS). Comparisons use identities; the report still
/// shows the path WE scanned, never one of these.
///
/// **Public because it appears in a public trait's method.** Every source
/// gets the ONE registry load `scan()` did — two sources in one report must
/// not see two different moments of the same disk.
pub struct SkillTruth {
    /// Every entry the registry's walk found.
    ///
    /// **The walk's own record.** The report asks the registry what it saw
    /// rather than walking the disk a second time: two walks of the same
    /// disk can disagree (a symlink, an ignored directory, a depth limit),
    /// and a report that re-derives the answer is answering a different
    /// question than the runtime will.
    discovered: Vec<DiscoveredAsset>,
    /// Every asset the walk found but could NOT load.
    ///
    /// **Separate from `discovered`**: a failed asset has no registration,
    /// so it has no name in any index and no "was it displaced" question —
    /// but it IS an asset the walk found, and the report has to say so.
    failed: Vec<FailedTruthAsset>,
    /// The skills map (`registry.all()`): the name each skill was registered
    /// under, and the identity of the `SKILL.md` it came from.
    skills: Vec<(String, PathBuf)>,
    /// The flat command templates (`registry.slash_commands()`), the same
    /// way. A **separate index**, as in the registry (`registry.rs:56`): a
    /// skill and a command with the same name do not collide there, so they
    /// must not collide here either.
    commands: Vec<(String, PathBuf)>,
}

/// One asset the registry's walk found.
///
/// **The ASSET identity is the key.** A skill directory is its BUNDLE
/// directory, not its `SKILL.md`: two bundles may ship the same `SKILL.md`
/// (a symlink to one file), and they are two assets that happen to share a
/// body. Matching the file alone would report both as one.
struct DiscoveredAsset {
    /// The ASSET's identity — the bundle directory for a skill directory, the
    /// file itself for a flat template.
    asset: PathBuf,
    /// The ASSET's path as the REGISTRY spelled it. The report shows the
    /// path the CALLER named — the two can differ (the same directory under
    /// two names), and only the registry knows where the asset really is.
    asset_path: PathBuf,
    /// The name it was registered under.
    name: String,
    /// The FORM it was found in.
    entry: EntryKind,
    /// The scanned root that found it — the root EXACTLY as handed to the
    /// registry.
    root: PathBuf,
    /// The same root as the registry spelled it.
    root_path: PathBuf,
    /// Whether a later same-name entry displaced it.
    replaced: bool,
    /// The diagnostics the registry recorded for THIS asset: its reason and
    /// whether it is a warning.
    ///
    /// **On the asset, not on the file.** A flat candidate and a skill
    /// directory can sit at the same path; a diagnostic about one is not
    /// about the other.
    diagnostics: Vec<(String, bool)>,
}

/// One asset the registry's walk found but could NOT load.
///
/// **The ASSET's identity, not the file's** — a skill directory that fails
/// to parse is still the asset that directory is.
struct FailedTruthAsset {
    /// The ASSET's identity.
    asset: PathBuf,
    /// The ASSET's path as the registry spelled it.
    asset_path: PathBuf,
    /// The ENTRY's path as the registry spelled it — the `SKILL.md` for a
    /// skill directory, the file itself for a flat template.
    ///
    /// **The entry is where the problem is** when the entry itself is
    /// unusable (a dangling link, a FIFO): the row shows it, because that
    /// is the thing the person has to look at.
    entry_path: PathBuf,
    /// The name it would have been registered under.
    name: String,
    /// The FORM it was found in.
    entry: EntryKind,
    /// The scanned root that found it.
    root: PathBuf,
    /// The same root as the registry spelled it.
    root_path: PathBuf,
    /// Why it could not be loaded — and WHOSE words the reason is.
    failure: crate::skills::LoadFailure,
    /// The reason, in the registry's own words.
    reason: String,
}

/// The path's identity — the single source of truth for "is this the same file".
///
/// `pathnorm::canonicalize` is the crate's one definition of path identity
/// (`pathnorm.rs:74`): on macOS `/var` and `/private/var` are the same
/// directory, and a caller may hand us either spelling. Comparing the
/// strings would say "different files"; the disk says otherwise.
///
/// A path that cannot be resolved falls back to itself: two identical
/// strings are the same file.
fn identity(path: &Path) -> PathBuf {
    crate::pathnorm::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether two paths name the same file.
///
/// **The disk is the only authority on this.** The same file can be spelled
/// two ways — `/var/folders/...` and `/private/var/folders/...` are the same
/// directory on macOS — and two *different* paths can be the same file too
/// (a hard link). Comparing the strings settles neither.
///
/// Three tiers, cheapest first:
/// 1. the literal paths — no syscall, and it settles the common case where
///    the caller handed us the same path twice;
/// 2. the [identities](identity) — the same path spelled two ways;
/// 3. the file itself — two different paths that are one inode.
///
/// A path we cannot resolve stays unproven: `None` is not "different", but
/// it is not "the same" either, and the caller must not treat it as proof.
fn same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if identity(left) == identity(right) {
        return true;
    }
    match (file_id(left), file_id(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// The file's identity on disk: its device and inode.
///
/// `None` when the platform has no such notion, or the path cannot be
/// stat'd — a path we could not read is not proven to be anything.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

impl SkillTruth {
    /// Load the registry the way the runtime does.
    fn load(ctx: &ScanContext) -> Self {
        // **The host's list**, not `standard_skill_dirs` — the runtime may
        // add the user's configured `extra_skill_dirs`, and the report has
        // to see the SAME directories the runtime sees, or it picks a
        // different winner than the runtime does.
        //
        // **The order is the runtime's order.** `resolve_skill_dirs_at`
        // (`coding/src/parts.rs`) puts the extras FIRST and the standard
        // roots after them; the registry loads a list front to back and the
        // LAST one wins a same-name collision — so the standard roots
        // outrank the extras. Passing the host's list verbatim keeps that
        // order; recomputing the standard list here would drop the extras
        // and change which skill wins.
        //
        // An empty list means "the standard list" — the host passes the real
        // list in production, and the fixtures use the default.
        //
        // **`None` and `Some(vec![])` are different.** `None` = the host gave
        // no list → the standard list. `Some(vec![])` = the host EXPLICITLY
        // asked for no skill roots (the runtime's skill tools are off) →
        // scan nothing. Falling back to the standard list there would make
        // the report claim `~/.claude/skills/foo` is `Active` while nothing
        // is loading it.
        let dirs = match ctx.skill_dirs.clone() {
            Some(dirs) => dirs,
            None => crate::skills::standard_skill_dirs(crate::skills::SkillRoots {
                home: &ctx.os_home,
                project: &ctx.project,
                dirs: &ctx.product,
            }),
        };
        let registry = crate::skills::SkillRegistry::load_with_explicit(&dirs, &[]);
        // **Both** maps, kept APART: `all()` is the SKILLS map, and the flat
        // command templates live in a separate one (`slash_commands()`,
        // `registry.rs:467`). Looking only at `all()` would make every
        // command file look overridden; merging them would make a skill
        // look overridden by a command of the same name — a collision the
        // registry never resolved, because the two never compete.
        let index = |templates: &[std::sync::Arc<crate::skills::Skill>]| -> Vec<(String, PathBuf)> {
            templates
                .iter()
                .map(|skill| (skill.name.clone(), identity(&skill.source_path)))
                .collect()
        };
        let skills = index(&registry.all());
        let commands = index(&registry.slash_commands());
        // **The walk's own record.** The registry says what it found: its
        // ASSET identity, under which name, in which FORM, under which root,
        // and which of those a later same-name entry displaced. The report
        // reads this instead of walking the disk again.
        let discovered = registry
            .discovered()
            .into_iter()
            .map(|entry| DiscoveredAsset {
                asset: identity(&entry.asset),
                asset_path: entry.asset,
                name: entry.name,
                entry: entry.entry,
                root: identity(&entry.root),
                root_path: entry.root,
                replaced: entry.replaced,
                diagnostics: entry.diagnostics,
            })
            .collect();
        // **The assets the walk found but could not load.** Their reason, and
        // WHOSE words it is: the loader's (a rejection) or a fact about the
        // ENTRY (a dangling link, an entry that is not a regular file).
        let failed = registry
            .failed()
            .into_iter()
            .map(|asset| FailedTruthAsset {
                asset: identity(&asset.asset),
                asset_path: asset.asset,
                entry_path: asset.path,
                name: asset.name,
                entry: asset.entry,
                root: identity(&asset.root),
                root_path: asset.root,
                failure: asset.failure,
                reason: asset.reason,
            })
            .collect();
        Self {
            discovered,
            failed,
            skills,
            commands,
        }
    }

    /// What the registry says about this asset, if it says anything.
    ///
    /// `Some` means the registry has a FACT about this path — it found it (or
    /// displaced it). `None` means the registry never walked it, so it has
    /// nothing to say.
    ///
    /// Matching is by **ASSET identity** (the bundle directory for a skill
    /// directory, the file itself for a flat template) **and by name** (the
    /// same asset registered under another name is not this asset) **and by
    /// FORM** (a flat `*.md` candidate is not the skill directory that
    /// happens to sit at the same path — both go into the skill index, so the
    /// index cannot tell them apart).
    fn registry_says(
        &self,
        name: &str,
        path: &Path,
        is_skill_dir: bool,
    ) -> Option<(AssetStatus, String)> {
        // The ASSET a candidate stands for: the bundle directory for a skill
        // directory, the file itself for a flat template. Two bundles may
        // ship the same `SKILL.md`; they are two assets that share a body,
        // and matching the file would report both as one.
        let asset = identity(path);
        let entry = if is_skill_dir {
            EntryKind::SkillDir
        } else {
            EntryKind::FlatFile
        };
        // Everything the registry found AS this asset, in this form, under
        // any name.
        let at_this_asset: Vec<&DiscoveredAsset> = self
            .discovered
            .iter()
            .filter(|found| found.asset == asset && found.entry == entry)
            .collect();
        if at_this_asset.is_empty() {
            // The registry never walked this asset. It has no fact about it,
            // and a second traversal is a different question.
            return None;
        }
        // The entries registered under OUR name.
        let ours: Vec<&DiscoveredAsset> = at_this_asset
            .into_iter()
            .filter(|found| found.name == name)
            .collect();
        if ours.is_empty() {
            // The registry found this asset, but under ANOTHER name: it is in
            // effect, but it is not in effect AS OUR ASSET. Saying "active"
            // here would be a claim about someone else's row.
            return None;
        }
        // One of our entries survived → it is the one in effect.
        if ours.iter().any(|found| !found.replaced) {
            return Some((AssetStatus::Active, text::skill_active()));
        }
        // All of them were displaced by a later same-name entry.
        Some((AssetStatus::Blocked, text::skill_overridden(name)))
    }

    /// The failure the registry recorded for this asset, if it recorded one.
    ///
    /// **By ASSET identity** — a flat candidate and a skill directory can
    /// sit at the same path, and a failure about one is not about the other.
    fn registry_failure(&self, path: &Path, is_skill_dir: bool) -> Option<(String, bool)> {
        let asset = identity(path);
        let entry = if is_skill_dir {
            EntryKind::SkillDir
        } else {
            EntryKind::FlatFile
        };
        self.failed
            .iter()
            .find(|failed| failed.asset == asset && failed.entry == entry)
            .map(|failed| {
                (
                    failed.reason.clone(),
                    matches!(failed.failure, crate::skills::LoadFailure::Rejected),
                )
            })
    }

    /// Whether this file is the one that is actually in effect.
    ///
    /// `name` is the name the registry gives this skill (see
    /// [`skill_rows`]). Paths are compared by [`identity`]. **The order
    /// matters**: what the registry SAID about our asset comes before what we
    /// infer from names, and every answer but the last rests on something the
    /// registry actually did:
    ///
    /// 1. The registry's own record of this asset says it is the one in
    ///    effect, or that a later same-name entry displaced it. **A fact, not
    ///    an inference** — the registry walked these paths; we did not walk
    ///    them a second time to find out. This covers the case the registry
    ///    records no diagnostic for: two IDENTICAL definitions, where it
    ///    keeps the later one and says nothing, because nothing is wrong.
    /// 2. The registry rejected it → the loader's own words, with the
    ///    loader's own reason. A definite cause; a same-name winner
    ///    elsewhere is only an inference and must not hide it — the fix is
    ///    the file, not the priority order.
    /// 3. Nothing says why → say exactly that. Claiming "overridden" here
    ///    would be a guess: the registry never found this asset, so it never
    ///    weighed it against a winner.
    fn verdict(&self, name: &str, path: &Path, parses: bool) -> (AssetStatus, String) {
        // If we cannot tell whether the path is a directory, treat it as the
        // file itself: the lookups below then simply find nothing for it, and
        // the verdict falls through to "not in effect" rather than inventing a
        // reason.
        let is_skill_dir = probe_dir(path).unwrap_or(false);
        // 1. What the registry ACTUALLY found. It walked these paths; we did
        //    not walk them a second time to find out.
        if let Some(verdict) = self.registry_says(name, path, is_skill_dir) {
            return verdict;
        }
        // 2. The registry said the loader could not use it — with the
        //    loader's own reason. Believed only when the file really does
        //    not parse: for a file that parses, a warning is a non-fatal
        //    one, not a rejection.
        if !parses {
            if let Some((reason, rejected)) = self.registry_failure(path, is_skill_dir) {
                if rejected {
                    return (AssetStatus::Blocked, text::skill_rejected(&reason));
                }
                // Not the loader's story: a fact about the ENTRY (a dangling
                // link, an entry that is not a regular file). The loader
                // never treated it as a skill at all.
                return (AssetStatus::Blocked, text::report_entry_unusable(&reason));
            }
        }
        // 3. Nothing says why → say exactly that. The registry never found
        //    this asset, so it never weighed it against a winner: claiming
        //    "overridden" here would be a guess.
        (AssetStatus::Blocked, text::skill_not_in_effect())
    }
}

/// Whether a protocol id names a wire format we do not speak.
///
/// `provider::unsupported_protocol` builds `unsupported:<wire_api>` for one, and
/// such an account must never be imported: `ProviderBook::create_account`
/// resolves an unknown preset to `openai-compatible`
/// (`config/provider_preset.rs:381`), so the import would silently succeed
/// with the WRONG wire protocol.
pub fn is_unsupported_protocol(protocol: &str) -> bool {
    protocol.starts_with("unsupported:")
}

/// Whether this path is a file we can use — and if not, WHY.
///
/// `Path::is_file()` collapses every error into `false`: a path we cannot
/// stat (a parent directory we may not traverse) looks exactly like a path
/// that is not there. The report would then say "not found" about something
/// that IS there but unreadable — and the person never learns to look.
///
/// `Ok(true)` = a regular file. `Ok(false)` = not there (or not a regular
/// file). `Err(reason)` = we could not tell.
fn probe_file(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether this path's ENTRY is there, whatever it is.
///
/// **`std::fs::metadata` follows the link**, so a dangling symlink reads as
/// "not there" — and the asset would vanish from the report, and no asset
/// may vanish. `symlink_metadata` does NOT follow it, so the entry itself is
/// still visible.
///
/// The mirror image of [`probe_file`]: that one says whether the path is
/// USABLE, this one says whether there is anything there at all.
fn entry_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Whether this path is a DANGLING symlink — the entry is there, but
/// following it does not reach anything.
///
/// A symlink whose target IS there is not broken — it resolves to a regular
/// file or a directory, and the probes already say so.
fn is_broken_link(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    // The link is there; is the target? `NotFound` is the dangling case —
    // any other error (a permission we may not read through) is not.
    matches!(
        std::fs::metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Whether this path is a directory we can use — and if not, WHY.
///
/// The directory counterpart of [`probe_file`].
fn probe_dir(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// What the real instruction loader makes of ONE instruction file.
///
/// The loader is the only thing that knows the order, what "shadowed" means,
/// and that an empty file is `Empty` rather than `Loaded` (`instructions.rs:173`).
/// So the report asks it, rather than re-deriving those rules — a
/// hand-rolled check is exactly how a report ends up saying "already in
/// effect" about a file that is not.
///
/// `None` when the loader does not know this file at all — it is not an
/// instruction file, or it is not in the project tier.
fn instruction_verdict(ctx: &ScanContext, path: &Path) -> Option<InstructionVerdict> {
    // The loader wants the product tree as the "home" (`instructions.rs:105`).
    let loaded = crate::instructions::load_instructions(ctx.product.user(), &ctx.project);
    // Project tier only (`instructions.rs:137`). The global file
    // (`<product tree>/ATOMCODE.md`, scope `"GLOBAL"`) would otherwise match
    // too and come back labelled `AssetScope::Project`.
    let source = loaded
        .sources
        .iter()
        .find(|source| source.scope == "PROJECT" && source.path == path)?;
    // The file that WINS in the project tier — the one the loader did not
    // shadow. A shadowed file's own name is NOT the explanation ("CLAUDE.md
    // takes precedence" about a shadowed CLAUDE.md says nothing); the
    // winner's name is.
    //
    // The loader marks the FIRST existing project file as the winner and
    // shadows the rest (`instructions.rs:131-139`: `selected |= path.is_file()`),
    // so the winner is the first PROJECT source that is neither shadowed nor
    // missing — even if it is empty, because an empty file still wins the
    // slot.
    let winner = loaded.sources.iter().find(|candidate| {
        candidate.scope == "PROJECT"
            && candidate.state != crate::instructions::InstructionState::Shadowed
            && candidate.state != crate::instructions::InstructionState::Missing
    });
    Some(InstructionVerdict {
        name: last_component(&source.path),
        path: source.path.clone(),
        scope: AssetScope::Project,
        state: source.state,
        shadowed_by: winner.map(|winner| last_component(&winner.path)),
    })
}

/// One instruction file, and what the loader said about it.
struct InstructionVerdict {
    name: String,
    path: PathBuf,
    scope: AssetScope,
    state: crate::instructions::InstructionState,
    /// The project file that wins over this one, when this one is shadowed.
    shadowed_by: Option<String>,
}
```

注册表记录成功解析的路径、注册名称、索引与覆盖状态。报告通过
`discovered()` 查询这些事实，通过 `diagnostics()` 补充诊断。报告自己的遍历仍决定
哪些路径会生成候选行，当前仍有自己的深度限制、忽略目录清单和循环检测。

`skills/registry.rs` 中的发现记录及索引字段：

```rust
/// How the walk found an entry — the FORM it was found in.
///
/// **Not the index.** A skill directory (`<dir>/SKILL.md`) and a flat `*.md`
/// file can BOTH go into the skill index (`registry.rs:341` and
/// `registry.rs:353`), so the index cannot tell them apart. A caller that
/// matched a flat-file candidate against a directory skill's record would be
/// borrowing a fact about a different asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A skill directory: found at its `SKILL.md`.
    SkillDir,
    /// A flat template file: found at the file itself.
    FlatFile,
}

/// Why an asset the walk found could not be loaded.
///
/// **Whose words the reason is.** A `Rejected` failure is the LOADER's own
/// words: it read the file and refused it. The other two are facts about the
/// ENTRY — the loader never treated it as a skill at all, so putting its
/// words in its mouth would send the person to fix the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadFailure {
    /// The loader read it and refused it.
    Rejected,
    /// The entry is a symlink whose target is gone.
    BrokenLink,
    /// The entry is there, but it is not a regular file (a FIFO, a socket).
    NotARegularFile,
}

/// One entry the registry's walk found.
///
/// **The walk's own record.** A caller that needs to know what the registry
/// actually saw — not what a second traversal would find — reads this. Two
/// traversals of the same disk can disagree (a symlink, an ignored
/// directory, a depth limit), so a caller that re-derives the answer is
/// answering a different question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredTemplate {
    /// The path the walk found it at: the `SKILL.md` for a skill directory,
    /// the file itself for a flat command template.
    pub path: PathBuf,
    /// The ASSET this entry is — the thing a person would call "this skill".
    ///
    /// **A skill directory is its BUNDLE directory, not its `SKILL.md`.** Two
    /// bundles may ship the same `SKILL.md` (a symlink to one file); they are
    /// two assets that happen to share a body. Matching the file alone would
    /// report both as one.
    ///
    /// For a flat template file this is the file itself.
    pub asset: PathBuf,
    /// The name it was registered under.
    pub name: String,
    /// Which index it went into.
    pub kind: TemplateKind,
    /// The FORM it was found in — a skill directory or a flat file. The
    /// index cannot say this: both forms go into the skill index.
    pub entry: EntryKind,
    /// The scanned root this entry was found under — the root EXACTLY as
    /// handed to the registry.
    ///
    /// A caller enumerates a tree by its root; this says which root found
    /// the entry, so "the assets under THIS root" is a question the registry
    /// answers.
    pub root: PathBuf,
    /// Whether a later same-name entry displaced it.
    ///
    /// **A same-name entry displaces the earlier one even when the two
    /// definitions are identical** — the registry keeps the later one and
    /// says nothing, because nothing is wrong. A caller reporting "which one
    /// is in effect" needs to know the earlier one lost, and only the
    /// registry knows.
    pub replaced: bool,
    /// The diagnostics the registry recorded for THIS asset: its reason and
    /// whether it is a warning.
    ///
    /// **On the asset, not on the file.** A flat candidate and a skill
    /// directory can sit at the same path; a diagnostic about one is not
    /// about the other.
    pub diagnostics: Vec<(String, bool)>,
}

/// An asset the walk found but could NOT load.
///
/// **The asset's identity, not the file's.** A skill directory that fails to
/// parse is still the asset that directory is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedAsset {
    /// The path the walk found it at: the `SKILL.md` for a skill directory,
    /// the file itself for a flat template.
    pub path: PathBuf,
    /// The ASSET this is — the bundle directory for a skill directory, the
    /// file itself for a flat template.
    pub asset: PathBuf,
    /// The name it would have been registered under.
    pub name: String,
    /// The FORM it was found in.
    pub entry: EntryKind,
    /// The scanned root it was found under — the root EXACTLY as handed to
    /// the registry.
    pub root: PathBuf,
    /// Why it could not be loaded.
    pub failure: LoadFailure,
    /// The reason, in the registry's own words. Meaningful for
    /// [`LoadFailure::Rejected`].
    pub reason: String,
}
```

在 `impl SkillRegistry` 中添加访问器，并更新 `insert_parsed`：

```rust
    /// Every entry the walk found, in the order it found them.
    ///
    /// **The walk's own record.** A caller that needs to know what the registry
    /// actually saw reads this, rather than walking the disk a second time and
    /// hoping the two walks agree.
    pub fn discovered(&self) -> Vec<DiscoveredTemplate> {
        self.read().discovered.clone()
    }
```

用下列实现替换原有 `insert_parsed`：

```rust
    fn insert_parsed(
        &self,
        parsed: Result<Parsed, String>,
        path: &Path,
        kind: TemplateKind,
        entry: EntryKind,
    ) {
        match parsed {
            Ok(parsed) => {
                let mut index = self.write();
                let skill = parsed.skill;
                let name = skill.name.clone();
                let source = skill.source_path.clone();
                // A bad `args:` value is a warning, not a load failure: the
                // skill still loads with its default, so the typo is reported
                // without dropping the command from the menu.
                for warning in parsed.warnings {
                    index.diagnostics.push(SkillDiagnostic {
                        path: path.to_path_buf(),
                        reason: warning,
                        warning: true,
                    });
                }
                // Compare loaded definitions, not current disk contents: a file
                // may have changed since the earlier source was scanned.
                let same_definition = match kind {
                    TemplateKind::Skill => index.skills.get(&name),
                    TemplateKind::Command => index.commands.get(&name),
                }
                .is_some_and(|previous| {
                    previous.display_name == skill.display_name
                        && previous.description == skill.description
                        && previous.template == skill.template
                        && previous.allowed_tools == skill.allowed_tools
                        && previous.user_invocable == skill.user_invocable
                        && previous.args_requirement == skill.args_requirement
                });
                // The SLOT that holds this name now — and, before it, the one
                // that used to. The slot is what says which record actually
                // won: the same file can be found twice (the same root listed
                // twice, one directory reached through two roots), and the
                // same path can sit in BOTH indexes — so looking the loser up
                // by path would find the wrong record.
                //
                // Read BEFORE the slot is moved below.
                let displaced = match kind {
                    TemplateKind::Skill => index.skill_records.get(&name).copied(),
                    TemplateKind::Command => index.command_records.get(&name).copied(),
                };
                let previous = match kind {
                    TemplateKind::Skill => index.skills.insert(name.clone(), Arc::new(skill)),
                    TemplateKind::Command => index.commands.insert(name.clone(), Arc::new(skill)),
                };
                // The walk's own record of what it found. The report reads it
                // instead of walking the disk a second time.
                let record = index.discovered.len();
                index.discovered.push(DiscoveredTemplate {
                    path: path.to_path_buf(),
                    name: name.clone(),
                    kind,
                    entry,
                    replaced: false,
                });
                match kind {
                    TemplateKind::Skill => {
                        index.skill_records.insert(name.clone(), record);
                    }
                    TemplateKind::Command => {
                        index.command_records.insert(name.clone(), record);
                    }
                }
                // The slot MOVED, so the record that used to hold this name
                // lost it — even when the new record is the SAME file (the
                // same root listed twice) and even when the two definitions
                // are identical. Both are displacements: the earlier record
                // is not the one in effect any more.
                //
                // The registry keeps the later one and says nothing, because
                // nothing is wrong — but a caller reporting "which one is in
                // effect" needs to know the earlier one lost, and only the
                // registry knows.
                if let Some(slot) = displaced {
                    if let Some(displaced_entry) = index.discovered.get_mut(slot) {
                        displaced_entry.replaced = true;
                    }
                }
                if let Some(previous) = previous {
                    // The OVERRIDE DIAGNOSTIC's condition is unchanged: an
                    // identical definition is not a problem, so it is not
                    // reported. The same path re-loaded is not an override
                    // either.
                    if previous.source_path != source && !same_definition {
                        index.diagnostics.push(SkillDiagnostic {
                            path: previous.source_path.clone(),
                            reason: format!("overridden by {}", source.display()),
                            warning: false,
                        });
                    }
                }
            }
            Err(error) => self.diagnose(path, error, true),
        }
    }
```

`skills/mod.rs` 导出发现记录：

```rust
    skill_install_dirs, standard_skill_dirs, DiscoveredTemplate, EntryKind, SkillDiagnostic,
    SkillRegistry, SkillRoots, TemplateDirectory, TemplateKind,
```

注册表遍历的两个插入点要分别传入对应的入口形态：

```rust
        for p in entries {
            if p.is_file() {
                if depth == 0 && p.extension().and_then(|e| e.to_str()) == Some("md") {
                    self.insert_parsed(
                        parse_skill_file(&p, namespace),
                        &p,
                        kind,
                        EntryKind::FlatFile,
                    );
                }
            } else if p.is_dir() {
                // Explicit roots remain usable, including .system bundles and
                // symlinked installs. Only incidental dependency/cache children
                // are skipped; do not blanket-ignore hidden grouping directories.
                if ignored_discovery_directory(&p) {
                    continue;
                }
                let skill_md = p.join("SKILL.md");
                if skill_md.is_file() {
                    self.insert_parsed(
                        parse_skill_dir(&p, &skill_md, namespace),
                        &skill_md,
                        TemplateKind::Skill,
                        EntryKind::SkillDir,
                    );
                } else {
                    self.scan_skill_dir(&p, namespace, depth + 1, kind, ancestors);
                }
            }
        }
```

本 Task 创建 `claude.rs` 和 `codex.rs` 的占位实现，并定义 `provider.rs` 的共享类型
与助手。Task 2、3、7 再补各来源的资产检测和 provider 草案。

`crates/atomcode-capabilities/src/migration/claude.rs`：

```rust
//! Claude Code assets. Filled in by Task 2.

use super::report::AssetReport;
use super::{MigrationSource, ScanContext, SkillTruth};

pub struct ClaudeSource;

impl MigrationSource for ClaudeSource {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn detect(&self, _ctx: &ScanContext, _truth: &SkillTruth) -> Vec<AssetReport> {
        Vec::new()
    }
}
```

`crates/atomcode-capabilities/src/migration/codex.rs`：

```rust
//! Codex assets. Filled in by Task 3.

use super::report::AssetReport;
use super::{MigrationSource, ScanContext, SkillTruth};

pub struct CodexSource;

impl MigrationSource for CodexSource {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    fn detect(&self, _ctx: &ScanContext, _truth: &SkillTruth) -> Vec<AssetReport> {
        Vec::new()
    }
}
```

`crates/atomcode-capabilities/src/migration/provider.rs`：

```rust
//! Provider accounts found in another agent's configuration. Filled in by Task 7.

use std::path::Path;

use super::report::AssetScope;

/// How a credential is referenced.
///
/// `Debug` is written by hand: a derived one would print a literal key in
/// every `{:?}`, `dbg!` and `unwrap()` panic message.
#[derive(Clone, PartialEq, Eq)]
pub enum ApiKeyRef {
    /// The NAME of an environment variable, e.g. `"MY_KEY"`. The secret stays
    /// where it is and is resolved at run time.
    EnvVar(String),
    /// A literal secret found in the source configuration. Written only after
    /// the person confirms that one item.
    Plaintext(String),
}

impl ApiKeyRef {
    /// How the credential is written, for the report to say. The VALUE is
    /// never part of this — only its kind.
    pub fn kind(&self) -> &'static str {
        match self {
            ApiKeyRef::EnvVar(_) => "environment variable",
            ApiKeyRef::Plaintext(_) => "a literal key",
        }
    }
}

impl std::fmt::Debug for ApiKeyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The variable's NAME, not a secret.
            ApiKeyRef::EnvVar(name) => f.debug_tuple("EnvVar").field(name).finish(),
            // The value IS the secret: only its kind is shown.
            ApiKeyRef::Plaintext(_) => f.debug_tuple("Plaintext").field(&"<redacted>").finish(),
        }
    }
}

/// A provider account an external agent has configured.
///
/// The derived `Debug` is safe: `api_key` goes through `ApiKeyRef`'s own
/// redacting `Debug`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountDraft {
    /// **The id the account WILL get** — already normalised by
    /// `account_id_for`, the same rule the writer uses. It is not the raw
    /// source id: `create_account` sanitises the name and may prefix it, so
    /// a draft built on the raw name would never be found again.
    pub id: String,
    /// The definition's name **as the source wrote it**.
    ///
    /// Two definitions in one file can normalise to the same id (`"foo bar"`
    /// and `"foo-bar"` both become `foo-bar`), so the id alone cannot say
    /// which definition a row is about. The original name can — and it is
    /// what the person recognises, so the report shows it.
    pub source_name: String,
    /// A preset or protocol id (`openai-compatible`, `anthropic-compatible`,
    /// `openai-responses`).
    pub protocol: String,
    /// The endpoint as it will be **stored** — normalised by
    /// `effective_endpoint`, so it is `None` when the endpoint is the
    /// protocol's own default (the writer stores nothing in that case).
    /// The endpoint in USE is `endpoint_in_use(protocol, base_url)`.
    pub base_url: Option<String>,
    /// The credential, when the source named one.
    pub api_key: Option<ApiKeyRef>,
    /// The models the source was using.
    pub models: Vec<String>,
    /// Where this account was configured — the file it was read from. The
    /// report shows it, so a person can see WHICH file to look at.
    pub source_path: std::path::PathBuf,
    /// Whether the account came from the person's own tree or from the
    /// current project. A project provider is not the user's provider.
    pub scope: AssetScope,
    /// Why this account CANNOT be imported, when that is the case.
    ///
    /// `None` means it can. A `Some` reason means the account is still
    /// **reported** — it is configured, the person should see it — but the
    /// importer refuses it.
    ///
    /// Two things land here:
    /// - the name cannot become an account id at all (`account_id_for`
    ///   returns `None`), so there is nothing to write it under;
    /// - the endpoint speaks a wire protocol we do not support.
    ///
    /// Both used to be either silently dropped or reported as importable,
    /// which is the one thing this report must never do.
    pub blocked: Option<String>,
}
```

**这个 Task 还要带上共享的归一包装** —— 它们不属于任何一个来源，所以放在
`provider.rs`（共享模块）里，**每个来源都能用**：

```rust
/// The protocol an external agent's endpoint speaks, mapped to ours.
///
/// **The SOURCE says its own default**, so this function does not ask who is
/// asking: a new source passes a different default, and the shared module
/// stays unchanged. That is what makes adding a source cheap.
///
/// **A missing `wire_api` means Responses, not chat/completions.** Codex's own
/// `WireApi` has `#[default] Responses`
/// (`codex-rs/model-provider-info/src/lib.rs:117`), so a config that names a
/// provider but no protocol speaks Responses. Mapping that to
/// `openai-compatible` (chat/completions) would send the request to an
/// endpoint that does not speak it.
///
/// A protocol we do not know is **not** guessed at — it becomes
/// `unsupported:…` and the report says so, instead of the import failing
/// later against an endpoint that speaks something else.
pub fn protocol_for_wire(wire_api: Option<&str>, default: &str) -> String {
    match wire_api {
        Some("responses") => "openai-responses".into(),
        Some("anthropic") => "anthropic-compatible".into(),
        Some("chat") | Some("openai") => "openai-compatible".into(),
        Some(other) => unsupported_protocol(other),
        None => default.into(),
    }
}

/// The protocol id for a wire family we do not support.
///
/// The report says the endpoint speaks something we cannot, so the person
/// sees it instead of the import silently failing later.
pub fn unsupported_protocol(wire_api: &str) -> String {
    format!("unsupported:{wire_api}")
}

/// The id the importer will give this account.
///
/// **The writer normalizes the name**, so the report must use the SAME id:
/// `ProviderBook::create_account` runs the name through `base_id`
/// (`provider_book.rs:700`), which sanitises it and prefixes `custom-` when
/// it belongs to CodingPlan. A report built on the raw source id would never
/// match the account on the next run — the re-check would keep saying
/// "can be brought over" about an account that is already there.
///
/// `base_id` is exposed for this (`provider_book::account_id_for`), so there
/// is **one** rule, not two that can drift.
pub fn account_id_for(name: &str) -> Option<String> {
    match atomcode_config::provider_book::account_id_for(name) {
        Ok(id) => Some(id),
        // An unusable name cannot become an account at all.
        Err(_) => None,
    }
}

/// The endpoint as it will be **stored**.
///
/// `endpoint_override` (`provider_book.rs:785`) trims the endpoint and stores
/// NOTHING when it equals the protocol's own default — so a file does not pin
/// a URL that should follow the build. Comparing the raw source endpoint
/// against the stored one would never match: the account ends up with
/// `base_url = None` while the draft still says the default URL.
///
/// `endpoint_override` is exposed for this
/// (`provider_book::effective_endpoint`).
///
/// **This delegates to the writer's rule** rather than re-implementing it. A
/// copy would be correct today and wrong the day the writer changes — and the
/// symptom would be a re-check that never matches.
pub fn effective_endpoint(protocol: &str, endpoint: Option<&str>) -> Option<String> {
    atomcode_config::provider_book::effective_endpoint(protocol, endpoint.unwrap_or_default())
}

/// Whether this protocol REQUIRES an endpoint.
///
/// `ProviderBook::create_account` refuses a custom-compatible protocol with no
/// endpoint (`provider_book.rs:198`: `ProtocolNeedsEndpoint`) — only a preset
/// that has its own default can do without one.
///
/// The report has to know this **before** the write, or it would advertise an
/// account as importable and then fail to write it.
pub fn protocol_needs_endpoint(protocol: &str) -> bool {
    atomcode_config::config::provider_preset::preset(protocol)
        .map(|preset| preset.default_base_url.is_none())
        .unwrap_or(true)
}

/// The endpoint the account will actually talk to.
///
/// The stored form may be `None` — that means "follow the protocol's
/// default" — but the person still needs to see WHICH endpoint that is.
pub fn endpoint_in_use(protocol: &str, endpoint: Option<&str>) -> Option<String> {
    if let Some(stored) = effective_endpoint(protocol, endpoint) {
        return Some(stored);
    }
    atomcode_config::config::provider_preset::preset(protocol)
        .and_then(|preset| preset.default_base_url)
        .map(str::to_string)
}

/// Read a JSON file that may carry comments.
///
/// **Comments are stripped first**, the way our own MCP loader does
/// (`mcp/config.rs:624`: `serde_json::from_str(&crate::jsonc::strip_comments(content))`).
/// A hand-edited file with a comment in it is a perfectly good file; calling
/// it broken would be wrong.
///
/// `None` when the file cannot be read or parsed — the caller reports that
/// as blocked, and does NOT treat it as "there is nothing here".
pub fn read_jsonc(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&crate::jsonc::strip_comments(&raw)).ok()
}
```

> **为什么这些在 Task 1 而不是 Task 7**：它们**不是**某个来源的知识，而是
> 「我们的写入方会怎么归一」——`account_id_for` / `effective_endpoint` 是
> `atomcode-config` 那个写入方的规则，`protocol_for_wire` 只问「这条线怎么说」。
> Task 4 的 OpenCode 扫描已经要用 `account_id_for` 与 `effective_endpoint`
> （它要报 provider 行），Task 1 也要 re-export `endpoint_in_use` 给宿主 ——
> 所以它们必须在 Task 1 就到位，**Task 1 才能单独编译并通过**。
> Task 7 只做**每个来源怎么读出自己的草案**（Claude 读 `env` 块、Codex 合并两层
> TOML），不再动这些共享规则。

`crates/atomcode-capabilities/src/migration/text.rs`（**最小版** —— Task 1 的
`mod.rs` 里的技能助手要用这 15 条；Task 2 再把其余的补进来）：

```rust
//! The sentences a report shows.
//!
//! One place, so the CLI and the screen cannot say the same thing two ways.
//! Every function here has an entry in the product message table.

use atomcode_i18n::product::{t, Msg};

pub fn skill_active() -> String {
    t(Msg::MigrationSkillActive).into_owned()
}
pub fn skill_rejected(reason: &str) -> String {
    t(Msg::MigrationSkillRejected { reason }).into_owned()
}
pub fn skill_overridden(name: &str) -> String {
    t(Msg::MigrationSkillOverridden { name }).into_owned()
}
pub fn unreadable_file(reason: &str) -> String {
    t(Msg::MigrationUnreadableFile { reason }).into_owned()
}
pub fn skill_depth_limit() -> String {
    t(Msg::MigrationSkillDepthLimit).into_owned()
}
pub fn skill_symlink_cycle() -> String {
    t(Msg::MigrationSkillSymlinkCycle).into_owned()
}
/// A `SKILL.md` that exists but is not a regular file (a directory, say).
pub fn skill_md_not_a_file() -> String {
    t(Msg::MigrationSkillMdNotAFile).into_owned()
}
/// The asset is in someone else's tree: it is not in effect, and this is
/// where to put it to bring it over.
///
/// **The wording promises the ACTION, not the outcome** — whether the copy
/// wins against a same-named higher-priority definition is not ours to
/// promise.
pub fn skill_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationSkillCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}
/// The asset is not in effect and nothing says why.
pub fn skill_not_in_effect() -> String {
    t(Msg::MigrationSkillNotInEffect).into_owned()
}
/// The file cannot be parsed, so copying it over would only move the
/// failure. The registry never looked at this file — this is our own
/// reading of it.
pub fn foreign_unparseable() -> String {
    t(Msg::MigrationForeignUnparseable).into_owned()
}
/// The entry is a symlink whose target is gone.
pub fn broken_link() -> String {
    t(Msg::MigrationBrokenLink).into_owned()
}
/// The entry is there, but it is not a regular file — a FIFO, a socket.
pub fn not_a_regular_file() -> String {
    t(Msg::MigrationNotARegularFile).into_owned()
}
/// The tree root is there, but it is not a directory we can walk.
pub fn not_a_directory() -> String {
    t(Msg::MigrationNotADirectory).into_owned()
}
/// **The report's own walk** stopped at the depth limit — not the loader's.
///
/// The loader has a depth limit of its own and says so in its own words,
/// but only about a tree it scans. In a tree it never scanned, the REPORT's
/// walk is what stopped, and "the skill loader rejected it" would put words
/// in the loader's mouth about a file the loader never looked at.
pub fn report_depth_limit() -> String {
    t(Msg::MigrationReportDepthLimit).into_owned()
}
/// **The report's own walk** found a symlink cycle — not the loader's.
pub fn report_symlink_cycle() -> String {
    t(Msg::MigrationReportSymlinkCycle).into_owned()
}
/// The ENTRY is there, but it is not something the loader can use — a
/// dangling symlink, a FIFO, a socket.
///
/// **The report's own finding, not the loader's.** The loader never treated
/// this entry as a skill at all, so putting its words in its mouth would
/// send the person to fix the wrong thing.
pub fn report_entry_unusable(reason: &str) -> String {
    t(Msg::MigrationReportEntryUnusable { reason }).into_owned()
}
```

`crates/atomcode-i18n/src/product/messages.rs` 的 `pub enum Msg<'a>` 末尾追加
（**Task 1 只加这 15 条**，Task 2 再加其余的；`en.rs` 与 `zh_cn.rs` 同步加）：

```rust
    // External-agent asset migration (see
    // docs/plans/2026-11-17-external-agent-asset-migration-design.md).
    MigrationSkillActive,
    MigrationSkillRejected {
        reason: &'a str,
    },
    MigrationSkillOverridden {
        name: &'a str,
    },
    MigrationUnreadableFile {
        reason: &'a str,
    },
```

`crates/atomcode-i18n/src/product/en.rs` 的 `match` 里追加：

```rust
        Msg::MigrationSkillActive => "已经生效：直接读取，无需迁移。".into(),
        Msg::MigrationSkillRejected { reason } => format!("技能加载器拒绝了它：{reason}").into(),
        Msg::MigrationSkillOverridden { name } => {
            format!("已被覆盖：另一个目录里同名的「{name}」优先生效。").into()
        }
        Msg::MigrationUnreadableFile { reason } => format!("文件读取失败：{reason}").into(),
        Msg::MigrationSkillDepthLimit => "分组目录层级超出上限".into(),
        Msg::MigrationSkillSymlinkCycle => "检测到符号链接环".into(),
        Msg::MigrationSkillMdNotAFile => "这里的 SKILL.md 不是普通文件，该技能无法加载。".into(),
        Msg::MigrationSkillCopyable { target } => {
            format!("尚未生效。复制到 {target} 就能搬过来。").into()
        }
        Msg::MigrationSkillNotInEffect => "没有生效，技能加载器也没有给出原因。".into(),
        Msg::MigrationForeignUnparseable => {
            "这个文件本身无法解析，即使复制过去也加载不了。".into()
        }
        Msg::MigrationBrokenLink => "这是一个断链：目标已经不存在，任何东西都无法加载它。".into(),
        Msg::MigrationNotARegularFile => "这不是普通文件，无法作为技能加载。".into(),
        Msg::MigrationNotADirectory => "这不是目录，无法在其中扫描技能。".into(),
        Msg::MigrationReportDepthLimit => "扫描在此停止：分组目录层级超出上限".into(),
        Msg::MigrationReportSymlinkCycle => "扫描在此停止：检测到符号链接环".into(),
        Msg::MigrationReportEntryUnusable { reason } => {
            format!("这个条目无法作为技能使用：{reason}").into()
        }
```

`crates/atomcode-i18n/src/product/zh_cn.rs` 的 `match` 里追加：

```rust
        Msg::MigrationSkillActive => "已经生效：直接读取，无需迁移。".into(),
        Msg::MigrationSkillRejected { reason } => format!("技能加载器拒绝了它：{reason}").into(),
        Msg::MigrationSkillOverridden { name } => {
            format!("已被覆盖：另一个目录里同名的「{name}」优先生效。").into()
        }
        Msg::MigrationUnreadableFile { reason } => format!("文件读取失败：{reason}").into(),
        Msg::MigrationSkillDepthLimit => "分组目录层级超出上限".into(),
        Msg::MigrationSkillSymlinkCycle => "检测到符号链接环".into(),
        Msg::MigrationSkillMdNotAFile => "这里的 SKILL.md 不是普通文件，该技能无法加载。".into(),
        Msg::MigrationSkillCopyable { target } => {
            format!("尚未生效。复制到 {target} 就能搬过来。").into()
        }
        Msg::MigrationSkillNotInEffect => "没有生效，技能加载器也没有给出原因。".into(),
        Msg::MigrationForeignUnparseable => {
            "这个文件本身无法解析，即使复制过去也加载不了。".into()
        }
        Msg::MigrationBrokenLink => "这是一个断链：目标已经不存在，任何东西都无法加载它。".into(),
        Msg::MigrationNotARegularFile => "这不是普通文件，无法作为技能加载。".into(),
        Msg::MigrationNotADirectory => "这不是目录，无法在其中扫描技能。".into(),
        Msg::MigrationReportDepthLimit => "扫描在此停止：分组目录层级超出上限".into(),
        Msg::MigrationReportSymlinkCycle => "扫描在此停止：检测到符号链接环".into(),
```

> **为什么 Task 1 就要建 `text.rs`**：Task 1 的 `mod.rs` 里已经有共享的技能助手
> （`skill_rows` / `SkillTruth`）。这 15 条文案覆盖加载结果、目录遍历错误和复制目标，
> 均由 `text::*` 从 product 表读取。Task 1 必须能**单独编译并通过**，因此枚举、
> 中英文 match 臂和 helper 都在此任务一起加入。Task 2 保留这 15 个 helper 和词条。

从同一根扫描注册表和报告时，第 8 层分组下的技能可产生技能行，第 9 层仅产生
分组目录的深度超限行。从第 9 层单独生成某个技能行时，保留原注册表根列表，
其判定为「没有生效，技能加载器也没有给出原因」。

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-capabilities --features migration -E 'test(migration::tests::)'`
Expected: PASS —— Task 1 的三条（报告计数 / 空报告 / 无来源扫描）。

- [ ] **Step 7: 提交**

```bash
git add crates/atomcode-capabilities/Cargo.toml crates/atomcode-capabilities/src/lib.rs crates/atomcode-capabilities/src/migration/ crates/atomcode-capabilities/src/skills/{mod,registry}.rs crates/atomcode-config/src/provider_book.rs crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs gates/compile.sh
git commit -m "feat(capabilities): add migration module skeleton

The report carries four statuses, not two: Active (we read it today),
Imported (an earlier import already brought it over), Actionable (there is a
known way to bring it over), and Blocked (it cannot come over). \"Compatible\" and \"needs conversion\" cannot say whether a
lost session history is recoverable, and that is the thing a person needs
to know before deciding.

The feature pulls atomcode-config, which the screen already has transitively."
```

---

## Task 2: Claude Code 来源

**Files:**
- Modify: `crates/atomcode-capabilities/src/migration/claude.rs`
- Modify: `crates/atomcode-capabilities/src/migration/text.rs`（**扩成完整版**：保留 Task 1 的 11 个 helper）
- Modify: `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs`（**加其余的词条**）
- Test: `crates/atomcode-capabilities/src/migration/claude.rs`（内联 `mod tests`）

前置条件是 Task 1 Step 5 的 `same_file` 和 `file_id` 已落地。用户级探测失败时，
仅在已有路径信息足以证明同一文件时去重。若两个不同路径都无法解析身份，保留
用户级 `Blocked` 行及原始错误。

- [ ] **Step 1: 写失败的测试**

在 `claude.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::report::{AssetKind, AssetScope, AssetStatus};
    // `PathBuf` is not in scope through `super::*` — the module under test
    // imports only `Path`.
    use std::path::PathBuf;

    fn ctx(home: &std::path::Path, project: &std::path::Path) -> ScanContext {
        ScanContext {
            os_home: home.to_path_buf(),
            project: project.to_path_buf(),
            product: crate::ProductDirs::new(home.join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        }
    }

    /// The registry's facts, loaded the way `scan()` loads them.
    ///
    /// **The same load the report uses** — so a fixture sees what the report
    /// sees. A fixture that loaded its own registry could disagree with the
    /// report for reasons that have nothing to do with the behaviour under
    /// test.
    fn truth(home: &std::path::Path, project: &std::path::Path) -> SkillTruth {
        super::super::SkillTruth::load(&ctx(home, project))
    }

    fn skill_at(root: &std::path::Path, name: &str) {
        let dir = root.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# skill").unwrap();
    }

    /// Write a file, creating its parent directory first.
    ///
    /// A fixture that writes `<home>/.claude/settings.json` into a directory
    /// that does not exist yet fails at the write, not at the assertion — and
    /// the failure says nothing about the behaviour being checked.
    fn write_file(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn a_user_skill_is_active_because_we_read_that_path_today() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        skill_at(&home.path().join(".claude"), "web-access");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let skill = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills && a.name == "web-access")
            .expect("the skill is reported");
        assert_eq!(skill.status, AssetStatus::Active);
        assert_eq!(skill.scope, AssetScope::User);
    }

    #[test]
    fn a_project_instruction_file_is_active() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&project.path().join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert_eq!(instructions.status, AssetStatus::Active);
        assert_eq!(instructions.scope, AssetScope::Project);
    }

    #[test]
    fn a_user_instruction_file_is_actionable_because_the_global_layer_cannot_see_it() {
        // The global instruction tier reads <product user tree>/ATOMCODE.md, not
        // ~/.claude/CLAUDE.md — so this one has to be copied, not merely read.
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert_eq!(instructions.status, AssetStatus::Actionable);
        assert_eq!(instructions.scope, AssetScope::User);
    }

    #[test]
    fn a_project_instruction_file_shadowed_by_agents_md_is_not_active() {
        // The project tier takes the FIRST existing name in order, so a
        // CLAUDE.md next to an AGENTS.md is never loaded. Calling it "already
        // in effect" would be a lie.
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&project.path().join("AGENTS.md"), "# rules");
        write_file(&project.path().join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert_eq!(instructions.status, AssetStatus::Blocked);
        assert!(
            instructions.detail.contains("AGENTS.md"),
            "it says what shadows it: {}",
            instructions.detail
        );
    }

    #[test]
    fn a_user_settings_file_is_actionable_and_the_project_one_is_active() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("settings.json"), "{}");
        write_file(&project.path().join(".claude").join("settings.json"), "{}");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let permissions: Vec<_> = assets
            .iter()
            .filter(|a| a.kind == AssetKind::Permissions)
            .collect();
        assert_eq!(permissions.len(), 2);
        assert!(permissions.iter().any(|p| p.scope == AssetScope::User
            && p.status == AssetStatus::Actionable));
        assert!(permissions.iter().any(|p| p.scope == AssetScope::Project
            && p.status == AssetStatus::Active));
    }

    #[test]
    fn a_broken_settings_file_is_blocked_not_active() {
        // Our own loader refuses a broken policy (`permission_settings.rs:296`),
        // so a broken file must not be reported as already in effect.
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&project.path().join(".claude").join("settings.json"), "{not json");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let permissions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Permissions)
            .expect("the broken file is reported");
        assert_eq!(permissions.status, AssetStatus::Blocked);
    }

    #[test]
    fn a_project_mcp_file_is_active_and_the_global_one_is_actionable() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &project.path().join(".mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"mcp-fs"}}}"#,
        );
        write_file(
            &home.path().join(".claude.json"),
            r#"{"mcpServers":{"gh":{"command":"mcp-gh"}}}"#,
        );

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let mcp: Vec<_> = assets.iter().filter(|a| a.kind == AssetKind::McpServers).collect();
        assert_eq!(mcp.len(), 2);
        assert!(mcp.iter().any(|m| m.scope == AssetScope::Project
            && m.status == AssetStatus::Active));
        assert!(mcp.iter().any(|m| m.scope == AssetScope::User
            && m.status == AssetStatus::Actionable));
    }

    #[test]
    fn an_actionable_row_names_the_path_to_copy_it_to() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert!(
            instructions.detail.contains("ATOMCODE.md"),
            "the row says where to put it: {}",
            instructions.detail
        );
    }

    #[test]
    fn a_subagent_definition_is_blocked_because_the_formats_differ() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reviewer.md"), "---").unwrap();

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let agent = assets
            .iter()
            .find(|a| a.kind == AssetKind::Agents)
            .expect("the agent definition is reported");
        assert_eq!(agent.status, AssetStatus::Blocked);
    }

    #[test]
    fn a_broken_settings_file_does_not_stop_the_scan() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("settings.json"), "{not json");
        skill_at(&home.path().join(".claude"), "web-access");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        // The skill is still found, and the broken file is reported as blocked
        // rather than taking the whole scan down.
        assert!(assets.iter().any(|a| a.kind == AssetKind::Skills));
        assert!(assets
            .iter()
            .any(|a| a.kind == AssetKind::Permissions && a.status == AssetStatus::Blocked));
    }

    #[test]
    fn no_claude_directory_means_no_rows() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        assert!(assets.is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-capabilities --features migration migration::claude`
Expected: FAIL —— 找不到资产（`detect` 返回空）。

- [ ] **Step 3: 实现 ClaudeSource**

把 `claude.rs` 整体替换为：

> **保留 Step 1 写的测试。** 下面这个替换块**不含** `#[cfg(test)] mod tests` ——
> 整体替换会把 Step 1 的判据一起删掉，于是 Step 5 的「跑测试确认通过」跑的是
> **零条**判据，看起来全绿，其实什么都没验。替换时把 Step 1 的测试模块**追加到
> 文件末尾**（或把替换块的末尾与测试模块合并成一次写入）。

```rust
//! Claude Code's assets.
//!
//! What AtomCode reads from a Claude Code tree today, and what it does not:
//!
//! | asset                          | user level      | project level |
//! |--------------------------------|-----------------|---------------|
//! | instructions (`CLAUDE.md`)     | not read        | read          |
//! | skills / commands              | read            | read          |
//! | permissions (`settings.json`)  | not read        | read          |
//! | MCP (`~/.claude.json`)         | not read        | read          |
//!
//! The global instruction tier reads `<product user tree>/ATOMCODE.md` and the
//! permission loader reads `<product user tree>` — neither is `~/.claude`. So
//! the user-level ones are `Actionable` (copy them over), not `Active`.
//!
//! Read-only: this looks and reports, it never writes.

use super::report::{AssetKind, AssetReport, AssetScope, AssetStatus};
// `probe_file` / `probe_dir` live in the parent module. A child module may
// reach a parent's private items, but it has to **name** them — an unqualified
// `probe_file(...)` would not resolve.
use super::{probe_dir, probe_file, MigrationSource, ScanContext};
use std::path::Path;

pub struct ClaudeSource;

impl MigrationSource for ClaudeSource {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport> {
        let mut out = Vec::new();
        let user = ctx.os_home.join(".claude");
        let project_claude = ctx.project.join(".claude");
        // The registry's facts come from the ONE load `scan()` did — not a
        // load of our own. Two sources in one report must not see two
        // different moments of the same disk.
        let skills = truth;

        // Instructions. The verdict comes from the **real loader** —
        // `crate::instructions::load_instructions` — not from a hand-rolled
        // "is it empty / is it shadowed" check. The loader knows the whole
        // order (`.atomcode.md`, `ATOMCODE.md`, `AGENTS.md`, `CLAUDE.md`,
        // `claude.md` — `instructions.rs:17`), what "shadowed" means, and that
        // an empty file loads as `Empty` rather than `Loaded`
        // (`instructions.rs:173`). Re-implementing those rules is how a report
        // ends up saying "already in effect" about a file that is not.
        // **Both spellings.** The loader's project tier knows five names
        // (`instructions.rs:17`), and two of them are Claude's: `CLAUDE.md`
        // and `claude.md`. On a case-sensitive filesystem only the lowercase
        // one may exist — and the loader reads it. Asking for one spelling
        // would leave the other unmentioned: the file is there, it IS in
        // effect, and the report would not say so.
        //
        // The loader deduplicates the two when they name the same file
        // (`instructions.rs:132`), so the report must too — otherwise one
        // file would get two rows.
        for name in ["CLAUDE.md", "claude.md"] {
            let project_instructions = ctx.project.join(name);
            if let Some(source) = super::instruction_verdict(ctx, &project_instructions) {
                // Only report instruction files the loader found.
                if source.state != crate::instructions::InstructionState::Missing {
                    let (status, detail) = match source.state {
                        crate::instructions::InstructionState::Loaded => {
                            (AssetStatus::Active, super::text::instructions_active())
                        }
                        // The message names the file that WINS, not this one.
                        crate::instructions::InstructionState::Shadowed => (
                            AssetStatus::Blocked,
                            super::text::instructions_shadowed(
                                source.shadowed_by.as_deref().unwrap_or(&source.name),
                            ),
                        ),
                        crate::instructions::InstructionState::Empty => {
                            (AssetStatus::Blocked, super::text::instructions_empty())
                        }
                        crate::instructions::InstructionState::Unreadable => (
                            AssetStatus::Blocked,
                            super::text::unreadable_file("the file could not be read"),
                        ),
                        crate::instructions::InstructionState::Missing => {
                            unreachable!("handled above")
                        }
                    };
                    out.push(AssetReport {
                        kind: AssetKind::Instructions,
                        name: source.name,
                        path: source.path,
                        scope: source.scope,
                        status,
                        detail,
                    });
                }
            }
        }
        let user_instructions = user.join("CLAUDE.md");
        // **Fallible**: a file we cannot stat is not the same as a file that
        // is not there.
        //
        // **The same file can be both.** `ctx.project` may BE `~/.claude`
        // (a project that happens to live there) — then the user-level path
        // and the project-level path are the same file, and reporting it
        // twice would say both "already in effect" and "can be brought
        // over" about one file.
        //
        // **One file, one row** — and the check covers BOTH branches below,
        // because the two can fail differently: the project tier may have
        // read the file fine while the user-level probe hits a permission
        // error (or the file changed between the two probes). Comparing by
        // **file identity** (the same path spelled two ways, or a hard link
        // under a different name) settles it — a string comparison would
        // call a hard link two files.
        let already_reported = |path: &std::path::Path| {
            out.iter().any(|row: &AssetReport| {
                row.kind == AssetKind::Instructions && super::same_file(&row.path, path)
            })
        };
        match probe_file(&user_instructions) {
            Ok(true) => {
                // Already reported as the project-level file: it is one file,
                // and it already has a row.
                if !already_reported(&user_instructions) {
                    out.push(AssetReport {
                        kind: AssetKind::Instructions,
                        name: "CLAUDE.md".into(),
                        path: user_instructions,
                        scope: AssetScope::User,
                        status: AssetStatus::Actionable,
                        detail: super::text::instructions_copyable(
                            &ctx.product.user().join("ATOMCODE.md"),
                        ),
                    });
                }
            }
            Ok(false) => {}
            Err(reason) => {
                // The file could not be read. If the project tier already
                // reported it, that row stands — one file, one row. If not,
                // this is the only thing we know about it, and the error is
                // the reason it is not in effect.
                if !already_reported(&user_instructions) {
                    out.push(AssetReport {
                        kind: AssetKind::Instructions,
                        name: "CLAUDE.md".into(),
                        path: user_instructions,
                        scope: AssetScope::User,
                        status: AssetStatus::Blocked,
                        detail: super::text::unreadable_file(&reason),
                    });
                }
            }
        }

        // Skills and commands: the standard paths are in our own scan list.
        for (kind, dir) in [
            (AssetKind::Skills, "skills"),
            (AssetKind::Commands, "commands"),
        ] {
            for (scope, root) in [
                (AssetScope::User, user.join(dir)),
                (AssetScope::Project, project_claude.join(dir)),
            ] {
                // These paths ARE ours — the registry scans them, so its
                // verdict says whether each one is really in effect.
                out.extend(super::skill_rows(
                    kind,
                    scope,
                    &root,
                    &skills,
                    super::SkillTree::Scanned,
                ));
            }
        }

        // Permissions. A file that cannot be parsed is NOT "in effect" — our
        // own loader refuses a broken policy (`permission_settings.rs:296`), so
        // it must not be reported as active either.
        for (scope, root) in [
            (AssetScope::User, user.clone()),
            (AssetScope::Project, project_claude.clone()),
        ] {
            for name in ["settings.json", "settings.local.json"] {
                let path = root.join(name);
                match probe_file(&path) {
                    Ok(true) => {}
                    // Not there: nothing to report.
                    Ok(false) => continue,
                    // There, but we cannot read it: that IS an asset, and it
                    // is blocked.
                    Err(reason) => {
                        out.push(AssetReport {
                            kind: AssetKind::Permissions,
                            name: name.into(),
                            path,
                            scope,
                            status: AssetStatus::Blocked,
                            detail: super::text::unreadable_file(&reason),
                        });
                        continue;
                    }
                }
                let parsed = parse_json(&path);
                let (status, detail) = match parsed {
                    Err(reason) => (AssetStatus::Blocked, super::text::unreadable_file(&reason)),
                    // A file that parses but whose rules our loader would
                    // REJECT is not "in effect" either — the loader fails the
                    // whole file (`permission_settings.rs:345-353`), so none
                    // of its rules apply.
                    Ok(value) if !permission_rules_are_valid(&value) => (
                        AssetStatus::Blocked,
                        super::text::rules_rejected(),
                    ),
                    Ok(_) => match scope {
                        // The project one is merged into our own permission rules.
                        AssetScope::Project => {
                            (AssetStatus::Active, super::text::permissions_active())
                        }
                        // The user one is not read: the loader reads the product tree.
                        AssetScope::User => (
                            AssetStatus::Actionable,
                            super::text::permissions_copyable(
                                &ctx.product.user().join("settings.json"),
                            ),
                        ),
                    },
                };
                out.push(AssetReport {
                    kind: AssetKind::Permissions,
                    name: name.into(),
                    path,
                    scope,
                    status,
                    detail,
                });
            }
        }

        // MCP servers. The project's own `.mcp.json` is read; the global
        // `~/.claude.json` is not, so it can only be brought over.
        let project_mcp = ctx.project.join(".mcp.json");
        if probe_file(&project_mcp).unwrap_or(false) {
            // **One parse.** The loader's verdict and the count come from
            // the same parse, so they cannot disagree about the same file.
            match mcp_servers(&project_mcp) {
                Ok(counts) if counts.total > 0 => {
                    if counts.enabled > 0 {
                        out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: "mcpServers".into(),
                            path: project_mcp.clone(),
                            scope: AssetScope::Project,
                            status: AssetStatus::Active,
                            detail: super::text::mcp_active(counts.enabled),
                        });
                    } else {
                        // Configured, but every server is disabled: the
                        // definitions are there, none of them run.
                        out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: "mcpServers".into(),
                            path: project_mcp.clone(),
                            scope: AssetScope::Project,
                            status: AssetStatus::Blocked,
                            detail: super::text::mcp_all_disabled(counts.total),
                        });
                    }
                }
                // A file that parses and declares nothing is not an asset.
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::McpServers,
                    name: ".mcp.json".into(),
                    path: project_mcp,
                    scope: AssetScope::Project,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }
        let global_state = ctx.os_home.join(".claude.json");
        if probe_file(&global_state).unwrap_or(false) {
            // The same helper as the project's file — one parse, and the
            // counts come from the loader.
            match mcp_servers(&global_state) {
                // The definitions can be brought over, so the count is the
                // TOTAL: a disabled server is still a definition to move.
                Ok(counts) if counts.total > 0 => out.push(AssetReport {
                    kind: AssetKind::McpServers,
                    name: "mcpServers".into(),
                    path: global_state,
                    scope: AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: super::text::mcp_copyable(
                        counts.total,
                        &ctx.product.user().join("mcp.json"),
                    ),
                }),
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::McpServers,
                    name: ".claude.json".into(),
                    path: global_state,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        // Hooks live in the user settings file.
        let user_settings = user.join("settings.json");
        if probe_file(&user_settings).unwrap_or(false) {
            match settings_hooks(&user_settings) {
                Ok(true) => out.push(AssetReport {
                    kind: AssetKind::Hooks,
                    name: "hooks".into(),
                    path: user_settings,
                    scope: AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: super::text::hooks_copyable(
                        &ctx.product.user().join("hooks.json"),
                    ),
                }),
                Ok(false) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::Hooks,
                    name: "settings.json".into(),
                    path: user_settings,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        // The provider rows. They come from the drafts, so the row's NAME is
        // the account id the importer will write — a row named "provider"
        // could never be matched by the re-check.
        //
        // In THIS task `provider_drafts` is still the default (empty), so no
        // provider row appears yet; Task 7 fills the drafts in and the rows
        // appear with it.
        for draft in self.provider_drafts(ctx) {
            out.push(super::provider_row(
                &draft,
                AssetStatus::Actionable,
                super::text::provider_copyable(
                    draft.base_url.as_deref(),
                    draft.api_key.as_ref().map(|k| k.kind()),
                ),
            ));
        }

        // Subagent definitions: our role files need frontmatter Claude's
        // subagent files do not have, so they cannot come over.
        //
        // A directory that exists but cannot be read is a `Blocked` row, not
        // silence — "we found nothing" and "we could not look" are different
        // answers, and only one of them is true.
        let agents = user.join("agents");
        if probe_dir(&agents).unwrap_or(false) {
            match std::fs::read_dir(&agents) {
                Ok(entries) => {
                    for entry in entries {
                        // An entry that cannot be read is a row, not a silent
                        // skip: a subagent file we could not look at is one we
                        // could not say anything true about.
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(error) => {
                                out.push(AssetReport {
                                    kind: AssetKind::Agents,
                                    name: "agents".into(),
                                    path: agents.clone(),
                                    scope: AssetScope::User,
                                    status: AssetStatus::Blocked,
                                    detail: super::text::unreadable_file(&error.to_string()),
                                });
                                continue;
                            }
                        };
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("md") {
                            continue;
                        }
                        out.push(AssetReport {
                            kind: AssetKind::Agents,
                            name: entry.file_name().to_string_lossy().into_owned(),
                            path,
                            scope: AssetScope::User,
                            status: AssetStatus::Blocked,
                            detail: super::text::agents_blocked(),
                        });
                    }
                }
                Err(error) => out.push(AssetReport {
                    kind: AssetKind::Agents,
                    name: "agents".into(),
                    path: agents,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&error.to_string()),
                }),
            }
        }

        // Plugins: the manifest format is theirs, not ours.
        let plugins = user.join("plugins");
        if plugins.is_dir() {
            out.push(AssetReport {
                kind: AssetKind::Plugins,
                name: "plugins".into(),
                path: plugins,
                scope: AssetScope::User,
                status: AssetStatus::Blocked,
                detail: super::text::plugins_blocked(),
            });
        }

        // Sessions: the history is lossy to convert, and this release does not.
        let projects = user.join("projects");
        if projects.is_dir() {
            match session_file_count(&projects) {
                Ok(count) if count > 0 => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: projects,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::sessions_blocked(count),
                }),
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: projects,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        out
    }
}


/// How many `mcpServers` a JSON file declares.
///
/// `Ok(0)` means the file parsed and declares none. `Err` means the file could
/// not be read or parsed — which is **not** the same as "there are none", and
/// the report says so instead of quietly showing nothing.
/// Whether a settings file declares hooks. `Err` means it could not be read
/// or parsed — again, not the same as "there are none".
fn settings_hooks(path: &Path) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    Ok(value.get("hooks").map(|h| !h.is_null()).unwrap_or(false))
}

/// Read a JSON file. `Err` carries the reason, because "this file is broken"
/// and "this file says nothing" are different reports.
fn parse_json(path: &Path) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&raw).map_err(|e| e.to_string())
}

/// Whether our own loader would ACCEPT this permissions file.
///
/// It rejects the WHOLE file when any rule is malformed
/// (`permission_settings.rs:345-353`: `PermissionRules::parse` returns the
/// invalid ones, and a non-empty list fails the load). So a file that parses
/// as JSON but has a bad rule in it applies NONE of its rules — reporting it
/// as "already in effect" would be wrong.
///
/// The check reuses the same rule parser our loader uses, so the two cannot
/// drift.
fn permission_rules_are_valid(value: &serde_json::Value) -> bool {
    // The top level must be an object (`permission_settings.rs:328`: "expected
    // a JSON object"). A `[]`, `null` or a scalar has no `permissions` key,
    // so without this check it would sail through as valid.
    if !value.is_object() {
        return false;
    }
    let rules = value.get("permissions");
    let Some(rules) = rules else {
        // No `permissions` key at all: nothing to reject.
        return true;
    };
    let Some(rules) = rules.as_object() else {
        // `permissions` must be an object (`permission_settings.rs:228`).
        return false;
    };
    let list = |key: &str| -> Vec<String> {
        rules
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    // `allow` and `deny` must be arrays of strings when present.
    for key in ["allow", "deny"] {
        match rules.get(key) {
            None => {}
            Some(value) => {
                let Some(items) = value.as_array() else {
                    return false;
                };
                if items.iter().any(|v| !v.is_string()) {
                    return false;
                }
            }
        }
    }
    let allow = list("allow");
    let deny = list("deny");
    let (_, invalid) = crate::tools::permission_rules::PermissionRules::parse(&allow, &deny);
    invalid.is_empty()
}

/// What an MCP file declares: how many servers, and how many of them are
/// ENABLED.
///
/// **This calls the real loader** — `crate::mcp::parse_mcp_servers` — rather
/// than re-checking the shape by hand. The loader does typed deserialization,
/// `requirements::validate` for experimental capabilities, and HTTP auth
/// validation; a hand-rolled check that only looked for `command`/`url` would
/// call a file "in effect" that the loader rejects. It also reads the legacy
/// `servers` key as an alias for `mcpServers` (`mcp/config.rs:140`), so
/// counting is left to it too.
///
/// A disabled server is configured but not running, so it must not be counted
/// as "in effect" (`mcp/config.rs:320` filters `!c.disabled`) — but it is
/// still an asset the person configured, and hiding it would make a
/// configured-but-disabled server vanish from the report entirely.
fn mcp_servers(path: &Path) -> Result<McpCounts, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let servers = crate::mcp::parse_mcp_servers(&raw, crate::mcp::McpConfigSource::Project)
        .map_err(|e| e.to_string())?;
    Ok(McpCounts {
        total: servers.len(),
        enabled: servers.iter().filter(|server| !server.disabled).count(),
    })
}

/// How many MCP servers a file declares, and how many run.
struct McpCounts {
    total: usize,
    enabled: usize,
}

/// The provider Claude was pointed at, and how its credential is written.
///
/// `Ok(None)` = the settings file says nothing about a provider. `Err` = the
/// file could not be parsed.
fn claude_provider(user: &Path) -> Option<(Option<String>, Option<String>)> {
    let raw = std::fs::read_to_string(user.join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let env = value.get("env")?.as_object()?;
    let base_url = env
        .get("ANTHROPIC_BASE_URL")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // The value in Claude's `env` block IS the secret; it is never a reference.
    let key = env
        .get("ANTHROPIC_API_KEY")
        .or_else(|| env.get("ANTHROPIC_AUTH_TOKEN"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .map(|_| "plaintext".to_string());
    if base_url.is_none() && key.is_none() {
        None
    } else {
        Some((base_url, key))
    }
}

/// How many session files there are under `projects/`.
///
/// `Err` means a directory could not be read — which is **not** the same as
/// "there are none", and the caller reports it as such.
fn session_file_count(projects: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    // **The visited set is what stops a symlink cycle.** `is_dir()` follows
    // links, so a directory that links back to an ancestor would otherwise
    // be walked forever and the read-only report would never finish.
    let mut visited: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    let mut stack = vec![projects.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let identity = match std::fs::canonicalize(&dir) {
            Ok(path) => path,
            Err(error) => return Err(error.to_string()),
        };
        if !visited.insert(identity) {
            continue;
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            // **Fallible**: a directory we cannot stat is not the same as a
            // file that is not there.
            match probe_dir(&path) {
                Ok(true) => stack.push(path),
                Ok(false) => {
                    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        count += 1;
                    }
                }
                Err(_) => {}
            }
        }
    }
    Ok(count)
}
```

**导入**：`use std::path::Path;`（只用 `Path`，不用 `PathBuf`）。

**注意：** 上面引用的 `super::text::*` 是 Task 1 建的文案辅助模块（最小版），
Task 2 把它**整个替换**成完整版 —— **不要在最小版后面追加**，否则同一个函数
会定义两次（编译不过）：

`crates/atomcode-capabilities/src/migration/text.rs`（**替换 Task 1 的最小版**）：

```rust
//! The sentences a report shows.
//!
//! One place, so the CLI and the screen cannot say the same thing two ways.
//! Every function here has an entry in the product message table.

use atomcode_i18n::product::{t, Msg};

pub fn skill_active() -> String {
    t(Msg::MigrationSkillActive).into_owned()
}
pub fn skill_rejected(reason: &str) -> String {
    t(Msg::MigrationSkillRejected { reason }).into_owned()
}
pub fn skill_overridden(name: &str) -> String {
    t(Msg::MigrationSkillOverridden { name }).into_owned()
}
pub fn unreadable_file(reason: &str) -> String {
    t(Msg::MigrationUnreadableFile { reason }).into_owned()
}
pub fn skill_depth_limit() -> String {
    t(Msg::MigrationSkillDepthLimit).into_owned()
}
pub fn skill_symlink_cycle() -> String {
    t(Msg::MigrationSkillSymlinkCycle).into_owned()
}
/// A `SKILL.md` that exists but is not a regular file (a directory, say).
pub fn skill_md_not_a_file() -> String {
    t(Msg::MigrationSkillMdNotAFile).into_owned()
}
/// The asset is in someone else's tree: it is not in effect, and this is
/// where to put it to bring it over.
///
/// **The wording promises the ACTION, not the outcome** — whether the copy
/// wins against a same-named higher-priority definition is not ours to
/// promise.
pub fn skill_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationSkillCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}
/// The asset is not in effect and nothing says why.
pub fn skill_not_in_effect() -> String {
    t(Msg::MigrationSkillNotInEffect).into_owned()
}
/// The file cannot be parsed, so copying it over would only move the
/// failure. The registry never looked at this file — this is our own
/// reading of it.
pub fn foreign_unparseable() -> String {
    t(Msg::MigrationForeignUnparseable).into_owned()
}
/// The entry is a symlink whose target is gone.
pub fn broken_link() -> String {
    t(Msg::MigrationBrokenLink).into_owned()
}

pub fn instructions_active() -> String {
    t(Msg::MigrationInstructionsActive).into_owned()
}
pub fn instructions_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationInstructionsCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}
pub fn instructions_shadowed(by: &str) -> String {
    t(Msg::MigrationInstructionsShadowed { by }).into_owned()
}
pub fn instructions_empty() -> String {
    t(Msg::MigrationInstructionsEmpty).into_owned()
}
pub fn permissions_active() -> String {
    t(Msg::MigrationPermissionsActive).into_owned()
}
pub fn permissions_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationPermissionsCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}
pub fn mcp_copyable(count: usize, target: &std::path::Path) -> String {
    t(Msg::MigrationMcpCopyable {
        count,
        target: &target.display().to_string(),
    })
    .into_owned()
}
pub fn mcp_active(count: usize) -> String {
    t(Msg::MigrationMcpActive { count }).into_owned()
}
pub fn mcp_all_disabled(count: usize) -> String {
    t(Msg::MigrationMcpAllDisabled { count }).into_owned()
}
pub fn rules_rejected() -> String {
    t(Msg::MigrationRulesRejected).into_owned()
}
pub fn opencode_plaintext_credentials() -> String {
    t(Msg::MigrationOpencodePlaintextCredentials).into_owned()
}
pub fn hooks_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationHooksCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}
pub fn agents_blocked() -> String {
    t(Msg::MigrationAgentsBlocked).into_owned()
}
pub fn plugins_blocked() -> String {
    t(Msg::MigrationPluginsBlocked).into_owned()
}
pub fn sessions_blocked(count: usize) -> String {
    t(Msg::MigrationSessionsBlocked { count }).into_owned()
}
pub fn provider_copyable(base_url: Option<&str>, key_kind: Option<&str>) -> String {
    t(Msg::MigrationProviderCopyable {
        base_url: base_url.unwrap_or("-"),
        key: key_kind.unwrap_or("-"),
    })
    .into_owned()
}
pub fn provider_name_unusable(name: &str) -> String {
    t(Msg::MigrationProviderNameUnusable { name }).into_owned()
}
pub fn provider_protocol_unsupported(protocol: &str) -> String {
    t(Msg::MigrationProviderProtocolUnsupported { protocol }).into_owned()
}
pub fn provider_nothing_to_connect_to() -> String {
    t(Msg::MigrationProviderNothingToConnectTo).into_owned()
}
```

> **共享的技能助手在 `mod.rs` 里，不在这里。** `SkillTruth` 与 `skill_rows`
> 三个来源都要用（Claude Code、OpenCode、以及共享的 `.agents` 树），所以它们
> 定义在 **`mod.rs`**（Task 1 就建好了），`claude.rs` 与 `opencode.rs` 都用
> `super::skill_rows(…)` 拿 —— 子模块**不会**自动拿到父模块的名字，必须写
> `super::`。
>
> **`SkillTruth` 由 `scan()` 加载一次并传进来**（`detect` 的第二个参数），
> **不要**在来源里自己 `SkillTruth::load(ctx)` —— 同一份报告里两个来源看到两个
> 不同时刻的加载状态，报告就会自相矛盾。
>
> `provider_copyable` 放在**这里**而不是 Task 3：Task 2 的 Claude 扫描已经要报
> provider 那一行，而 Task 2 必须能**单独编译并通过**（阶段 P2 的验收就是
> `cargo nextest run -p atomcode-capabilities --features migration` 全绿）。
>
> `text.rs` 在 Task 1 已经有了**最小版**（11 条：技能、复制目标、读取失败与断链），本 Step 把它
> **扩成完整版** —— `mod text;` 的声明在 Task 1 的 `mod.rs` 里已经在了。

- [ ] **Step 4: 在 i18n product 表加这些条目**

`crates/atomcode-i18n/src/product/messages.rs` 的 `pub enum Msg<'a>` 末尾追加
（保留 Task 1 的 11 个变体及中英文 match 臂，包括
`MigrationUnreadableFile`、`MigrationForeignUnparseable` 和 `MigrationBrokenLink`）：

```rust
    // External-agent asset migration (see
    // docs/plans/2026-11-17-external-agent-asset-migration-design.md).
    MigrationInstructionsActive,
    MigrationInstructionsCopyable {
        target: &'a str,
    },
    MigrationInstructionsShadowed {
        by: &'a str,
    },
    MigrationInstructionsEmpty,
    MigrationPermissionsActive,
    MigrationPermissionsCopyable {
        target: &'a str,
    },
    MigrationMcpCopyable {
        count: usize,
        target: &'a str,
    },
    MigrationMcpActive {
        count: usize,
    },
    MigrationMcpAllDisabled {
        count: usize,
    },
    MigrationRulesRejected,
    MigrationHooksCopyable {
        target: &'a str,
    },
    MigrationProviderCopyable {
        base_url: &'a str,
        key: &'a str,
    },
    MigrationProviderNameUnusable {
        name: &'a str,
    },
    MigrationProviderProtocolUnsupported {
        protocol: &'a str,
    },
    MigrationProviderNothingToConnectTo,
    MigrationCredentialHasDollar,
    MigrationCredentialAmbiguous,
    MigrationOpencodePlaintextCredentials,
    MigrationAgentsBlocked,
    MigrationPluginsBlocked,
    MigrationSessionsBlocked {
        count: usize,
    },
```

`crates/atomcode-i18n/src/product/en.rs` 的 `match` 里追加：

```rust
        Msg::MigrationInstructionsActive => {
            "Read directly from this project — already in effect".into()
        }
        Msg::MigrationInstructionsCopyable { target } => {
            format!("Copy it to {target} to make it apply to every project").into()
        }
        Msg::MigrationInstructionsShadowed { by } => {
            format!("Not read: {by} takes precedence in this project").into()
        }
        Msg::MigrationInstructionsEmpty => {
            "The file is empty, so nothing is loaded from it".into()
        }
        Msg::MigrationPermissionsActive => {
            "Merged into this project's permission rules — already in effect".into()
        }
        Msg::MigrationPermissionsCopyable { target } => {
            format!("Copy it to {target} to make it a global permission rule").into()
        }
        Msg::MigrationMcpCopyable { count, target } => {
            format!("{count} MCP server(s) — copy them into {target}").into()
        }
        Msg::MigrationMcpActive { count } => {
            format!("{count} MCP server(s) — read directly from this project").into()
        }
        Msg::MigrationMcpAllDisabled { count } => {
            format!("{count} MCP server(s) are configured but disabled — none of them run").into()
        }
        Msg::MigrationRulesRejected => {
            "Our loader would reject this file, so none of its rules apply".into()
        }
        Msg::MigrationHooksCopyable { target } => {
            format!("Copy them into {target}").into()
        }
        Msg::MigrationProviderCopyable { base_url, key } => {
            format!(
                "Provider settings (endpoint {base_url}, credential {key}) can be imported — \
                 review them before writing"
            )
            .into()
        }
        Msg::MigrationProviderNameUnusable { name } => {
            format!("{name}: this name cannot be an account id — not imported").into()
        }
        Msg::MigrationProviderProtocolUnsupported { protocol } => {
            format!("This endpoint speaks {protocol}, which we do not support — not imported")
                .into()
        }
        Msg::MigrationProviderNothingToConnectTo => {
            "This provider names no endpoint and no credential — there is nothing to connect to"
                .into()
        }
        Msg::MigrationCredentialHasDollar => {
            "This key contains `$`, which the configuration would expand — not imported".into()
        }
        Msg::MigrationCredentialAmbiguous => {
            "An environment variable of the same name is set, so this key cannot be written \
             unambiguously — not imported"
                .into()
        }
        Msg::MigrationOpencodePlaintextCredentials => {
            "This file holds plaintext keys — they are not moved, only reported".into()
        }
        Msg::MigrationAgentsBlocked => {
            "Our role files need `permission` and `difficulty` frontmatter, which these do not \
             have — not migrated"
                .into()
        }
        Msg::MigrationPluginsBlocked => {
            "The plugin manifest format is theirs, not ours — not migrated".into()
        }
        Msg::MigrationSessionsBlocked { count } => {
            format!("{count} session file(s) — converting history is lossy, not done yet").into()
        }
```

`crates/atomcode-i18n/src/product/zh_cn.rs` 的 `match` 里追加：

```rust
        Msg::MigrationInstructionsActive => "直接读取本项目的这个文件 —— 已经生效".into(),
        Msg::MigrationInstructionsCopyable { target } => {
            format!("复制到 {target} 后对所有项目生效").into()
        }
        Msg::MigrationInstructionsShadowed { by } => {
            format!("不会被读取：本项目中 {by} 的优先级更高").into()
        }
        Msg::MigrationInstructionsEmpty => "这个文件是空的，因此没有加载任何内容".into(),
        Msg::MigrationPermissionsActive => "已并入本项目的权限规则 —— 已经生效".into(),
        Msg::MigrationPermissionsCopyable { target } => {
            format!("复制到 {target} 后即成为全局权限规则").into()
        }
        Msg::MigrationMcpCopyable { count, target } => {
            format!("{count} 个 MCP 服务 —— 可复制到 {target}").into()
        }
        Msg::MigrationMcpActive { count } => {
            format!("{count} 个 MCP 服务 —— 直接读取本项目的这个文件").into()
        }
        Msg::MigrationMcpAllDisabled { count } => {
            format!("{count} 个 MCP 服务已配置但被禁用 —— 都没有在运行").into()
        }
        Msg::MigrationRulesRejected => "我们的加载器会拒绝这个文件，因此其中规则都不生效".into(),
        Msg::MigrationHooksCopyable { target } => {
            format!("可复制到 {target}").into()
        }
        Msg::MigrationProviderCopyable { base_url, key } => {
            format!("其中的 provider 设置可以导入（endpoint {base_url}，凭据 {key}）—— 写入前请先确认")
                .into()
        }
        Msg::MigrationProviderNameUnusable { name } => {
            format!("{name}：这个名字不能作为账号 id —— 未导入").into()
        }
        Msg::MigrationProviderProtocolUnsupported { protocol } => {
            format!("这个端点说的是 {protocol}，我们不支持 —— 未导入").into()
        }
        Msg::MigrationProviderNothingToConnectTo => {
            "这个 provider 没有写端点也没有写凭据 —— 没有可连接的对象".into()
        }
        Msg::MigrationCredentialHasDollar => {
            "这个密钥里含有 `$`，配置会把它展开 —— 未导入".into()
        }
        Msg::MigrationCredentialAmbiguous => {
            "同名环境变量已设置，这个密钥无法唯一确定 —— 未导入".into()
        }
        Msg::MigrationOpencodePlaintextCredentials => {
            "这个文件里是明文密钥 —— 只报告，不搬运".into()
        }
        Msg::MigrationAgentsBlocked => {
            "我们的角色文件需要 permission 与 difficulty 前置元数据，Claude 的 subagent 文件没有 \
             —— 未迁移"
                .into()
        }
        Msg::MigrationPluginsBlocked => "插件清单格式是对方的 —— 未迁移".into(),
        Msg::MigrationSessionsBlocked { count } => {
            format!("{count} 个会话文件 —— 历史转换有损，本期未做").into()
        }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-capabilities --features migration migration::claude`
Expected: PASS —— 当前 Step 1 有 11 条判据，覆盖指令、技能、权限、MCP、子 agent 与空来源。后续增加判据时，以实际执行条数为准。

- [ ] **Step 6: 跑 i18n 编译检查**

Run: `cargo check -p atomcode-i18n`
Expected: 退出 0（漏一个 match arm 就是编译错误）。

- [ ] **Step 7: 提交**

```bash
git add crates/atomcode-capabilities/src/migration/ crates/atomcode-i18n/src/product/
git commit -m "feat(capabilities): detect Claude Code assets

Project-level instructions, skills, commands and permissions are read
directly; the user-level ones are not, because the global instruction tier
reads the product tree rather than ~/.claude. The report says which is
which instead of implying everything already works."
```

---

## Task 3: Codex 来源

**Files:**
- Modify: `crates/atomcode-capabilities/src/migration/codex.rs`
- Modify: `crates/atomcode-capabilities/src/migration/text.rs`
- Modify: `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs`
- Modify: `crates/atomcode-capabilities/Cargo.toml`（`migration` feature 已含 `dep:toml`，无需改）

- [ ] **Step 1: 写失败的测试**

在 `codex.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::report::{AssetKind, AssetScope, AssetStatus};

    fn ctx(home: &std::path::Path, project: &std::path::Path) -> ScanContext {
        ScanContext {
            os_home: home.to_path_buf(),
            project: project.to_path_buf(),
            product: crate::ProductDirs::new(home.join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        }
    }

    /// The registry's facts, loaded the way `scan()` loads them.
    fn truth(home: &std::path::Path, project: &std::path::Path) -> SkillTruth {
        super::super::SkillTruth::load(&ctx(home, project))
    }

    /// Write a file, creating its parent directory first.
    ///
    /// A fixture that writes `<home>/.codex/config.toml` into a directory that
    /// does not exist yet fails at the write, not at the assertion — and the
    /// failure says nothing about the behaviour being checked.
    fn write_file(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn the_global_agents_file_is_actionable() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".codex").join("AGENTS.md"), "# rules");

        let assets = CodexSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert_eq!(instructions.status, AssetStatus::Actionable);
        assert_eq!(instructions.scope, AssetScope::User);
    }

    #[test]
    fn codex_skills_are_actionable_because_we_do_not_scan_that_path() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex").join("skills").join("deploy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# skill").unwrap();

        let assets = CodexSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let skill = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills)
            .expect("the skill is reported");
        assert_eq!(skill.status, AssetStatus::Actionable);
        assert_eq!(skill.name, "deploy");
    }

    #[test]
    fn a_broken_config_toml_is_reported_without_taking_the_scan_down() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &home.path().join(".codex").join("config.toml"),
            "this is not = = toml",
        );
        let dir = home.path().join(".codex").join("skills").join("deploy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# skill").unwrap();

        let assets = CodexSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        assert!(assets.iter().any(|a| a.kind == AssetKind::Skills));
        assert!(assets
            .iter()
            .any(|a| a.kind == AssetKind::ProviderConfig && a.status == AssetStatus::Blocked));
    }

    #[test]
    fn mcp_servers_in_the_config_are_actionable() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &home.path().join(".codex").join("config.toml"),
            "[mcp_servers.fs]\ncommand = \"mcp-fs\"\n",
        );

        let assets = CodexSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let mcp = assets
            .iter()
            .find(|a| a.kind == AssetKind::McpServers)
            .expect("the MCP servers are reported");
        assert_eq!(mcp.status, AssetStatus::Actionable);
    }

    #[test]
    fn no_codex_directory_means_no_rows() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let assets = CodexSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        assert!(assets.is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-capabilities --features migration migration::codex`
Expected: FAIL —— 找不到资产。

- [ ] **Step 3: 实现 CodexSource**

把 `codex.rs` 整体替换为：

```rust
//! Codex's assets.
//!
//! None of them are read by AtomCode today: our skill scan list has no
//! `.codex` entry, our instruction tiers do not look at `~/.codex/AGENTS.md`,
//! and our MCP config is JSON while Codex's is TOML. So every row here is
//! `Actionable` (there is a way to bring it over) except sessions, which are
//! lossy to convert.
//!
//! Read-only: this looks and reports, it never writes.

use super::report::{AssetKind, AssetReport, AssetScope, AssetStatus};
use super::{MigrationSource, ScanContext, SkillTruth};
use std::path::{Path, PathBuf};

pub struct CodexSource;

impl MigrationSource for CodexSource {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport> {
        let mut out = Vec::new();
        let user = ctx.os_home.join(".codex");
        let project_codex = ctx.project.join(".codex");

        // Global instructions.
        let agents = user.join("AGENTS.md");
        if agents.is_file() {
            out.push(AssetReport {
                kind: AssetKind::Instructions,
                name: "AGENTS.md".into(),
                path: agents,
                scope: AssetScope::User,
                status: AssetStatus::Actionable,
                detail: super::text::instructions_copyable(
                    &ctx.product.user().join("ATOMCODE.md"),
                ),
            });
        }

        // Project-level instructions: the open standard, so we read them —
        // but the SAME real loader decides, so an `AGENTS.md` that is empty,
        // unreadable, or shadowed by a higher-priority file is not called
        // "already in effect". The verdict comes from the loader, not from a
        // hand-rolled check that could drift from it.
        let project_agents = ctx.project.join("AGENTS.md");
        if let Some(source) = super::instruction_verdict(ctx, &project_agents) {
            // A `Missing` file produces NO row — and it cannot be a
            // `continue`, because this is not a loop.
            if source.state != crate::instructions::InstructionState::Missing {
                let (status, detail) = match source.state {
                    crate::instructions::InstructionState::Loaded => {
                        (AssetStatus::Active, super::text::instructions_active())
                    }
                    // The message names the file that WINS, not this one.
                    crate::instructions::InstructionState::Shadowed => (
                        AssetStatus::Blocked,
                        super::text::instructions_shadowed(
                            source.shadowed_by.as_deref().unwrap_or(&source.name),
                        ),
                    ),
                    crate::instructions::InstructionState::Empty => {
                        (AssetStatus::Blocked, super::text::instructions_empty())
                    }
                    crate::instructions::InstructionState::Unreadable => {
                        (AssetStatus::Blocked, super::text::unreadable_file(
                            "the file could not be read",
                        ))
                    }
                    crate::instructions::InstructionState::Missing => {
                        unreachable!("handled above")
                    }
                };
                out.push(AssetReport {
                    kind: AssetKind::Instructions,
                    name: source.name,
                    path: source.path,
                    scope: source.scope,
                    status,
                    detail,
                });
            }
        }

        // Skills. Codex installs them in GROUPING directories
        // (`~/.codex/skills/.system/<name>/SKILL.md`), so only the immediate
        // children would miss the installed ones — a real skill vanishing
        // from the report.
        //
        // **Through the shared helper, not a walk of our own.** The helper
        // knows how to descend grouping directories, and — more importantly —
        // it asks the registry first: a host that put `~/.codex/skills` in its
        // skill roots has those skills ALREADY in effect, and "copy it over"
        // would be nonsense. A walk of our own that answered `Actionable`
        // unconditionally would say that even about a skill that is running
        // right now.
        for (scope, root) in [
            (AssetScope::User, user.join("skills")),
            (AssetScope::Project, project_codex.join("skills")),
        ] {
            out.extend(super::skill_rows(
                AssetKind::Skills,
                scope,
                &root,
                truth,
                super::SkillTree::Foreign {
                    target: ctx.product.user().join("skills"),
                },
            ));
        }

        // The TOML config: MCP servers and the provider setup.
        for (scope, path) in [
            (AssetScope::User, user.join("config.toml")),
            (AssetScope::Project, project_codex.join("config.toml")),
        ] {
            if !path.is_file() {
                continue;
            }
            match std::fs::read_to_string(&path).ok().and_then(|raw| {
                raw.parse::<toml::Value>().ok()
            }) {
                Some(value) => {
                    if let Some(count) = mcp_server_count(&value) {
                        out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: "mcp_servers".into(),
                            path: path.clone(),
                            scope,
                            status: AssetStatus::Actionable,
                            detail: super::text::mcp_copyable(
                                count,
                                &ctx.product.user().join("mcp.json"),
                            ),
                        });
                    }
                    // The provider rows are added below, from the drafts —
                    // one row per account, named by its id.
                }
                None => {
                    out.push(AssetReport {
                        kind: AssetKind::ProviderConfig,
                        name: "config.toml".into(),
                        path: path.clone(),
                        scope,
                        status: AssetStatus::Blocked,
                        detail: super::text::unreadable_config(),
                    });
                }
            }
        }

        // The provider rows. They come from the drafts, so the row's NAME is
        // the account id the importer will write — a row named "config.toml"
        // could never be matched by the re-check.
        //
        // In THIS task `provider_drafts` is still the default (empty), so no
        // provider row appears yet; Task 7 fills the drafts in and the rows
        // appear with it.
        for draft in self.provider_drafts(ctx) {
            out.push(super::provider_row(
                &draft,
                AssetStatus::Actionable,
                super::text::provider_copyable(
                    draft.base_url.as_deref(),
                    draft.api_key.as_ref().map(|k| k.kind()),
                ),
            ));
        }

        // Sessions.
        let sessions = user.join("sessions");
        if sessions.is_dir() {
            match session_file_count(&sessions) {
                Ok(count) if count > 0 => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: sessions,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::sessions_blocked(count),
                }),
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: sessions,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        out
    }
}

/// The bound on how deep the Codex skill walk goes.
///
/// The registry's own bound is 8 (`registry.rs:296`); the same number here
/// keeps the two from disagreeing about what exists.
const MAX_SKILL_DEPTH: usize = 8;

/// Find every Codex skill under `root`, including the ones in grouping
/// directories (`~/.codex/skills/.system/<name>/SKILL.md`).
///
/// The walk is **bounded** and **cycle-guarded**: a symlink back to an
/// ancestor would otherwise be walked forever, and the read-only report would
/// never finish.
fn collect_codex_skills(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    ancestors: &mut std::collections::BTreeSet<std::path::PathBuf>,
    found: &mut Vec<(String, std::path::PathBuf)>,
) {
    if depth > max_depth {
        return;
    }
    let identity = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(_) => return,
    };
    if !ancestors.insert(identity.clone()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries.flatten() {
        paths.push(entry.path());
    }
    paths.sort();
    for path in paths {
        if path.is_dir() {
            if path.join("SKILL.md").is_file() {
                found.push((
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    path,
                ));
            } else {
                collect_codex_skills(&path, depth + 1, max_depth, ancestors, found);
            }
        }
    }
    ancestors.remove(&identity);
}

/// How many `[mcp_servers.*]` tables the config declares.
fn mcp_server_count(value: &toml::Value) -> Option<usize> {
    let servers = value.get("mcp_servers")?.as_table()?;
    (!servers.is_empty()).then_some(servers.len())
}

/// How many session files there are under `sessions/`.
fn session_file_count(sessions: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    let mut stack = vec![sessions.to_path_buf()];
    // **The visited set stops a symlink cycle.** `is_dir()` follows links, so
    // a directory that links back to an ancestor would be walked forever and
    // the read-only report would never finish.
    let mut visited: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    while let Some(dir) = stack.pop() {
        let identity = match std::fs::canonicalize(&dir) {
            Ok(path) => path,
            Err(error) => return Err(error.to_string()),
        };
        if !visited.insert(identity) {
            continue;
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            // **Fallible**: a directory we cannot stat is not the same as a
            // file that is not there.
            match probe_dir(&path) {
                Ok(true) => stack.push(path),
                Ok(false) => {
                    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        count += 1;
                    }
                }
                Err(_) => {}
            }
        }
    }
    Ok(count)
}
```

- [ ] **Step 4: 补文案**

`text.rs` 追加：

```rust
pub fn unreadable_config() -> String {
    t(Msg::MigrationUnreadableConfig).into_owned()
}
```

`messages.rs` 追加：

```rust
    MigrationUnreadableConfig,
```

`en.rs` 追加：

```rust
        Msg::MigrationUnreadableConfig => {
            "This config file could not be parsed, so nothing could be read from it".into()
        }
```

`zh_cn.rs` 追加：

```rust
        Msg::MigrationUnreadableConfig => "这个配置文件无法解析，因此其中内容都没读到".into(),
```

> **`MigrationSkillCopyable` 不在这里加** —— Task 1 已经加过这条臂（它服务的是
> 共享的技能助手）。重复的枚举变体会触发编译错误，重复的 match 臂会触发不可达模式告警。
> `text.rs` 里 `skill_copyable()` 也已在 Task 1 定义，**不要重复定义**（E0428）。
>
> `MigrationProviderCopyable` **不在这里加** —— Task 2 已经加过这条臂（它服务的是
> Claude 与 Codex 两个来源的 provider 行）。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-capabilities --features migration`
Expected: PASS —— 本 Task 定义的 claude / codex 判据全部通过（**以实际条数为准**）。

- [ ] **Step 6: 提交**

```bash
git add crates/atomcode-capabilities/src/migration/ crates/atomcode-i18n/src/product/
git commit -m "feat(capabilities): detect Codex assets

Codex keeps its skills, instructions, MCP servers and provider setup where
we do not look, so every row is actionable rather than active. A config
file we cannot parse is reported as blocked instead of silently yielding
nothing."
```

---

## Task 4: OpenCode 来源

**为什么单独一个 Task，而不是并进 Task 3**：OpenCode 是**第三个**来源，也是
**第一种新形状**的来源。它的每一样资产都与前两个不同 —— 而这正是检验抽象的
样本。若加它需要改共享模块（`provider.rs`），说明抽象没做对；所以本 Task
顺带把「来源特有的知识」收回来源自己（见 Step 4）。

**Files:**
- Create: `crates/atomcode-capabilities/src/migration/opencode.rs`
- Modify: `crates/atomcode-capabilities/src/migration/mod.rs`（声明模块并在 `builtin_sources` 注册）
- Modify: `crates/atomcode-coding/src/config.rs`（**宿主解析 OpenCode 的目录**，Step 5）

### OpenCode 的资产长什么样（本机实测，`/Users/theo/.config/opencode`）

| 资产 | 路径 | 形状 |
|---|---|---|
| 技能 | `~/.config/opencode/skills/<name>/SKILL.md` | frontmatter `name` / `description` / `version` —— **与我们的技能格式相同** |
| 命令 | `~/.config/opencode/commands/<name>.md` | 平铺的 `.md` |
| 共享技能 | `~/.agents/skills/<name>/SKILL.md` | 同上（跨 agent 约定） |
| provider | `~/.config/opencode/opencode.json` | `provider.<id>.{npm, name, options:{baseURL, apiKey}, models:{...}}` |
| MCP | `~/.config/opencode/opencode.jsonc` | `mcp.<id>.{type:"local", command:[...], enabled, timeout}` |
| 凭据 | `~/.local/share/opencode/auth.json` | `<vendor>.{type:"api", key:"sk-…"}` —— **明文** |

三处与前两个来源**不同**的地方，构成本 Task 的技术难点：

1. **凭据在 `options.apiKey`**，是**明文字符串**，不是变量名 —— 与 Claude 的
   `env` 块同族（都是明文），与 Codex 的 `env_key`（变量名）不同。
2. **MCP 的 `command` 是数组**（`["node", "…/mcp-server.mjs"]`），我们的
   `McpServerEntry` 是 `command: Option<String>` + `args: Option<Vec<String>>`
   （`mcp/config.rs:145`）—— 要**拆开**成 command + args。
3. **协议不靠 `wire_api` 声明**，靠 `npm` 包名（`@ai-sdk/openai-compatible`）。

### 目录解析

OpenCode 遵循 XDG（本机实测：二进制里有 `XDG_CONFIG_HOME` / `XDG_DATA_HOME`）：

- 配置：`$XDG_CONFIG_HOME/opencode/`（默认 `~/.config/opencode/`）
- 数据：`$XDG_DATA_HOME/opencode/`（默认 `~/.local/share/opencode/`）

**路径由宿主解析**（`AGENTS.md`：「解析环境只在宿主」）—— capability 不读环境变量。
所以 `ScanContext` 要多两个字段，或者宿主把解析好的路径塞进 `ProductDirs` 之外的
地方。**本 Task 取前者**：加 `opencode_config_dir` 与 `opencode_data_dir` 两个
`PathBuf`，宿主填。

- [ ] **Step 1: 写失败的测试**

在 `crates/atomcode-capabilities/src/migration/opencode.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::report::{AssetKind, AssetScope, AssetStatus};

    fn ctx(home: &std::path::Path, project: &std::path::Path) -> ScanContext {
        ScanContext {
            os_home: home.to_path_buf(),
            project: project.to_path_buf(),
            product: crate::ProductDirs::new(home.join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: home.join(".config/opencode"),
            opencode_data_dir: home.join(".local/share/opencode"),
        }
    }

    /// The registry's facts, loaded the way `scan()` loads them.
    fn truth(home: &std::path::Path, project: &std::path::Path) -> SkillTruth {
        super::super::SkillTruth::load(&ctx(home, project))
    }

    /// A skill in OpenCode's own tree: same format as ours, so it can be
    /// copied.
    #[test]
    fn an_opencode_skill_is_actionable_because_we_do_not_read_that_tree() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let skill = home.path().join(".config/opencode/skills/huawei-apig/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "---\nname: huawei-apig\ndescription: x\n---\n\nbody\n").unwrap();

        let assets = OpenCodeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills)
            .expect("the skill is reported");
        assert_eq!(row.status, AssetStatus::Actionable);
    }

    /// A shared `.agents/skills` skill is ALREADY ours — we read that tree
    /// (`registry.rs:585`). The report must say so, not claim it is movable.
    #[test]
    fn a_shared_agents_skill_is_active_because_we_read_that_tree() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let skill = home.path().join(".agents/skills/huawei-vpc/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "---\nname: huawei-vpc\ndescription: x\n---\n\nbody\n").unwrap();

        let assets = OpenCodeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills && a.name == "huawei-vpc")
            .expect("the skill is reported");
        assert_eq!(row.status, AssetStatus::Active);
    }

    /// The provider's credential is a LITERAL — like Claude's, unlike Codex's.
    #[test]
    fn an_opencode_provider_key_is_a_literal_not_a_reference() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/opencode/opencode.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"{"provider":{"atomgit":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"http://127.0.0.1:8001/v1","apiKey":"sk-literal"}}}}"#,
        )
        .unwrap();

        let drafts = OpenCodeSource.provider_drafts(&ctx(home.path(), project.path()));
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].api_key,
            Some(ApiKeyRef::Plaintext("sk-literal".into())),
            "a literal, so it needs the person's confirmation"
        );
    }

    /// The MCP `command` is an ARRAY — split into command + args.
    #[test]
    fn an_opencode_mcp_command_array_is_split_into_command_and_args() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/opencode/opencode.jsonc");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "{\n  // a comment, because this file is jsonc\n  \"mcp\":{\"x\":{\"type\":\"local\",\"command\":[\"node\",\"/tmp/s.mjs\"],\"enabled\":true}}\n}",
        )
        .unwrap();

        let assets = OpenCodeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::McpServers)
            .expect("the server is reported");
        assert_eq!(row.status, AssetStatus::Actionable);
    }

    /// `auth.json` holds PLAINTEXT keys. We report that they are there and
    /// never offer to move them.
    #[test]
    fn the_auth_file_is_reported_but_never_offered_for_import() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let auth = home.path().join(".local/share/opencode/auth.json");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, r#"{"deepseek":{"type":"api","key":"sk-x"}}"#).unwrap();

        let assets = OpenCodeSource.detect(&ctx(home.path(), project.path()), &truth(home.path(), project.path()));
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::ProviderConfig)
            .expect("the credentials are reported");
        assert_eq!(row.status, AssetStatus::Blocked);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-capabilities --features migration migration::opencode`
Expected: FAIL —— `OpenCodeSource` 不存在。

- [ ] **Step 3: 实现 OpenCodeSource**

`mod.rs` 加 `mod opencode;`，并在 `builtin_sources()` 的列表末尾追加
`Box::new(opencode::OpenCodeSource)`。将 Step 1 的测试模块保留在 `opencode.rs` 末尾。

`crates/atomcode-capabilities/src/migration/opencode.rs`（新建）：

```rust
//! OpenCode's assets.
//!
//! The THIRD source, and the first with a shape of its own: its credential
//! is a literal in `options.apiKey`, its MCP `command` is an ARRAY, and its
//! protocol comes from an `npm` package name rather than a `wire_api` field.
//!
//! If supporting it needed a change in the shared `provider.rs`, the
//! abstraction would be wrong — so the source-specific knowledge stays HERE.

use super::report::{AssetKind, AssetReport, AssetScope, AssetStatus};
use super::provider::{ApiKeyRef, ProviderAccountDraft};
use super::{MigrationSource, ScanContext, SkillTruth};

pub struct OpenCodeSource;

impl MigrationSource for OpenCodeSource {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport> {
        let mut out = Vec::new();
        let config = &ctx.opencode_config_dir;
        // The registry's facts come from the ONE load `scan()` did — not a
        // load of our own. Two sources in one report must not see two
        // different moments of the same disk.
        let skills = truth;

        // Skills and commands. Prefer discoveries already in the registry;
        // otherwise check whether the candidate can be brought over.
        for (kind, dir) in [
            (AssetKind::Skills, "skills"),
            (AssetKind::Commands, "commands"),
        ] {
            for (scope, root) in [
                (AssetScope::User, config.join(dir)),
                (AssetScope::Project, ctx.project.join(".opencode").join(dir)),
            ] {
                out.extend(super::skill_rows(
                    kind,
                    scope,
                    &root,
                    &skills,
                    super::SkillTree::Foreign {
                        target: ctx.product.user().join(dir),
                    },
                ));
            }
        }
        // Shared skills are standard roots. Shared commands may also be
        // configured by the host, so consult the same registry snapshot
        // before offering a copy.
        for (scope, root) in [
            (AssetScope::User, ctx.os_home.join(".agents").join("skills")),
            (AssetScope::Project, ctx.project.join(".agents").join("skills")),
        ] {
            out.extend(super::skill_rows(
                AssetKind::Skills,
                scope,
                &root,
                &skills,
                super::SkillTree::Scanned,
            ));
        }
        for (scope, root) in [
            (AssetScope::User, ctx.os_home.join(".agents").join("commands")),
            (AssetScope::Project, ctx.project.join(".agents").join("commands")),
        ] {
            out.extend(super::skill_rows(
                AssetKind::Commands,
                scope,
                &root,
                &skills,
                super::SkillTree::Foreign {
                    target: ctx.product.user().join("commands"),
                },
            ));
        }

        // The provider configuration.
        out.extend(provider_rows(ctx));

        // The MCP servers.
        out.extend(mcp_rows(ctx));

        // `auth.json`: plaintext credentials. REPORTED, never offered.
        let auth = ctx.opencode_data_dir.join("auth.json");
        if auth.is_file() {
            out.push(AssetReport {
                kind: AssetKind::ProviderConfig,
                name: "auth.json".into(),
                path: auth,
                scope: AssetScope::User,
                status: AssetStatus::Blocked,
                detail: super::text::opencode_plaintext_credentials(),
            });
        }
        out
    }

    /// The provider accounts OpenCode has configured.
    ///
    /// **The credential is a LITERAL**, like Claude's — not a variable name
    /// like Codex's. So it needs the person's confirmation before it is
    /// written.
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        let Some(value) = super::provider::read_jsonc(&ctx.opencode_config_dir.join("opencode.json"))
        else {
            return Vec::new();
        };
        let Some(providers) = value.get("provider").and_then(|p| p.as_object()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, provider) in providers {
            let Some(options) = provider.get("options").and_then(|o| o.as_object()) else {
                continue;
            };
            let base_url = options
                .get("baseURL")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let api_key = options
                .get("apiKey")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|secret| ApiKeyRef::Plaintext(secret.to_string()));
            if base_url.is_none() && api_key.is_none() {
                continue;
            }
            let protocol = protocol_for_opencode(
                provider.get("npm").and_then(|v| v.as_str()),
            );
            out.push(ProviderAccountDraft {
                id: super::provider::account_id_for(id).unwrap_or_else(|| id.clone()),
                source_name: id.clone(),
                protocol: protocol.clone(),
                base_url: super::provider::effective_endpoint(&protocol, base_url.as_deref()),
                api_key,
                models: Vec::new(),
                source_path: ctx.opencode_config_dir.join("opencode.json"),
                scope: AssetScope::User,
                blocked: None,
            });
        }
        out
    }
}

/// The provider accounts OpenCode has configured.
///
/// **The rows come from the drafts**, so the row's name is the account id
/// the importer will write — the same rule the other two sources follow.
fn provider_rows(ctx: &ScanContext) -> Vec<AssetReport> {
    // **The rows come from the drafts** — the same shape the other two
    // sources use. Re-deriving the values here would give two places that
    // could disagree about what an account is.
    let mut out = Vec::new();
    for draft in OpenCodeSource.provider_drafts(ctx) {
        out.push(super::provider_row(
            &draft,
            AssetStatus::Actionable,
            super::text::provider_copyable(
                draft.base_url.as_deref(),
                draft.api_key.as_ref().map(|k| k.kind()),
            ),
        ));
    }
    out
}

/// The MCP servers OpenCode has configured.
///
/// **OpenCode's `command` is an ARRAY** (`["node", "…/mcp-server.mjs"]`),
/// while ours is `command: Option<String>` + `args: Option<Vec<String>>`
/// (`mcp/config.rs:145`) — so it is split here.
fn mcp_rows(ctx: &ScanContext) -> Vec<AssetReport> {
    let Some(value) = super::provider::read_jsonc(&ctx.opencode_config_dir.join("opencode.jsonc"))
    else {
        return Vec::new();
    };
    let Some(servers) = value.get("mcp").and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, server) in servers {
        let Some(server) = server.as_object() else {
            continue;
        };
        let command = server.get("command");
        // A command may be a string or an array — both shapes occur.
        let (command, args): (Option<String>, Vec<String>) = match command {
            Some(serde_json::Value::String(text)) => (Some(text.clone()), Vec::new()),
            Some(serde_json::Value::Array(items)) => {
                let mut items = items.iter();
                let first = items.next().and_then(|v| v.as_str()).map(str::to_string);
                (first, items.filter_map(|v| v.as_str().map(str::to_string)).collect())
            }
            _ => (None, Vec::new()),
        };
        if command.is_none() {
            // Neither `command` nor `url`: our loader would reject the whole
            // file (`mcp/config.rs:670-673`).
            out.push(AssetReport {
                kind: AssetKind::McpServers,
                name: name.clone(),
                path: ctx.opencode_config_dir.join("opencode.jsonc"),
                scope: AssetScope::User,
                status: AssetStatus::Blocked,
                detail: super::text::rules_rejected(),
            });
            continue;
        }
        let _ = args;
        out.push(AssetReport {
            kind: AssetKind::McpServers,
            name: name.clone(),
            path: ctx.opencode_config_dir.join("opencode.jsonc"),
            scope: AssetScope::User,
            status: AssetStatus::Actionable,
            detail: super::text::mcp_copyable(
                1,
                &ctx.product.user().join("mcp.json"),
            ),
        });
    }
    out
}
```

- [ ] **Step 4: 把来源特有的知识收回来源自己**

这一步是本 Task 的**要点**。OpenCode 加进来之后，`provider.rs` 里**不该**再多出
任何 `opencode` 的知识。

**共享的 `protocol_for_wire` 已经在 Task 1 里了**（它只问「这条线怎么说」，
不问「你是谁」）。本 Task 要做的是**把 OpenCode 的那份知识放进 OpenCode 自己
的文件里**：

```rust
/// The protocol for an OpenCode provider, from its `npm` package name.
///
/// **OpenCode does not say `wire_api`** — it says which AI SDK package it
/// speaks through, and that is what determines the wire format.
///
/// **This lives HERE, not in the shared `provider.rs`.** The shared module
/// knows the wire families; OpenCode knows how IT names one. A new source
/// adds a function here, and the shared module stays untouched.
fn protocol_for_opencode(npm: Option<&str>) -> String {
    match npm {
        Some("@ai-sdk/openai-compatible") => "openai-compatible".into(),
        Some("@ai-sdk/anthropic") => "anthropic-compatible".into(),
        Some("@ai-sdk/openai") => "openai-responses".into(),
        Some(other) => super::provider::unsupported_protocol(&format!("opencode:{other}")),
        None => "openai-compatible".into(),
    }
}
```

各来源**自己**说默认值 —— 共享的 `protocol_for_wire` 从不问「你是谁」：

- Codex：`protocol_for_wire(wire_api, "openai-responses")` —— Codex 的
  `WireApi` 默认是 Responses（`codex-rs/model-provider-info/src/lib.rs:117`）。
- Claude：`protocol_for_wire(None, "anthropic-compatible")`。
- OpenCode：按 `npm` 包名 —— `@ai-sdk/openai-compatible` → `openai-compatible`；
  `@ai-sdk/anthropic` → `anthropic-compatible`；不认识 → `unsupported:opencode:<npm>`。

> **Step 3 的代码已经用了来源本地的 `protocol_for_opencode`**（不是
> `super::provider::protocol_for_opencode`）—— 所以本 Step 只是把那个函数
> **定义**补上，两个 Step 合起来才是完整实现。共享模块里**没有** OpenCode 的
> 知识：它只有 `protocol_for_wire`（问「这条线怎么说」）与 `unsupported_protocol`
> （造一个「我们不认」的 id）。

**复用 Task 1 已声明的 `ScanContext` 字段**（宿主解析，capability 不读环境）：

```rust
    /// Where OpenCode keeps its configuration
    /// (`$XDG_CONFIG_HOME/opencode`, default `~/.config/opencode`).
    pub opencode_config_dir: PathBuf,
    /// Where OpenCode keeps its data
    /// (`$XDG_DATA_HOME/opencode`, default `~/.local/share/opencode`).
    pub opencode_data_dir: PathBuf,
```

- [ ] **Step 5: 宿主解析 OpenCode 的目录**

`atomcode-coding/src/config.rs` 加（与 `product_dirs_from_env` 同处）：

```rust
/// Where OpenCode keeps its configuration and data.
///
/// **The host resolves these** — the capability reads no environment
/// (`AGENTS.md`: 「解析环境只在宿主」). OpenCode follows XDG
/// (`XDG_CONFIG_HOME` / `XDG_DATA_HOME`), with the usual defaults.
pub fn opencode_dirs() -> (PathBuf, PathBuf) {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("opencode");
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("opencode");
    (config, data)
}
```

`scan_context` 加两个参数，CLI 与 TUI 两处调用都补上。

- [ ] **Step 6: 核对文案**

复用 Task 2 已添加的 `text::opencode_plaintext_credentials()` 与
`MigrationOpencodePlaintextCredentials` 的中英文文案。

- [ ] **Step 7: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-capabilities --features migration migration::opencode`
Expected: PASS —— 本 Task 定义的判据全部通过。

- [ ] **Step 8: 提交**

```bash
git add crates/atomcode-capabilities/src/migration/ crates/atomcode-coding/src/config.rs crates/atomcode-i18n/src/product/
git commit -m "feat(capabilities): detect OpenCode assets

The third source, and the first with a shape of its own: a literal
credential in options.apiKey, an MCP command that is an array, and a
protocol that comes from an npm package name rather than a wire_api field.

The source-specific knowledge stays inside the source: protocol_for now
takes the source's own default instead of asking which source it is, so
adding a source does not change the shared module."
```

---

## Task 5: CLI 子命令 `atomcode import`（扫描报告）

**Files:**
- Create: `crates/atomcode-cli/src/import_cmd.rs`
- Modify: `crates/atomcode-cli/src/main.rs`
- Modify: `crates/atomcode-cli/src/lib.rs`
- Modify: `crates/atomcode-cli/Cargo.toml`（capabilities feature 加 `migration`）

- [ ] **Step 1: 写失败的测试**

在 `crates/atomcode-cli/src/import_cmd.rs` 里先建文件与测试：

```rust
//! `atomcode import` — look at the assets another agent left behind.
//!
//! Read-only by default: it reports. `--apply` is the second phase, and it is
//! the only path that writes.

use atomcode_capabilities::migration::{
    self, ApiKeyRef, AssetKind, AssetStatus, MigrationReport, ProviderAccountDraft, ScanContext,
};
use std::path::{Path, PathBuf};

/// The report, as text or as JSON.
pub fn render(report: &MigrationReport, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(report).unwrap_or_else(|e| e.to_string());
    }
    render_text(report)
}

/// The text form: one section per source, one line per asset.
///
/// Every sentence comes from the product message table, so the CLI and the
/// screen cannot end up saying the same thing two ways.
fn render_text(report: &MigrationReport) -> String {
    use atomcode_i18n::product::{t, Msg};
    if report.sources.is_empty() {
        return t(Msg::MigrationReportEmpty).into_owned();
    }
    let mut out = String::new();
    for source in &report.sources {
        out.push_str(&format!("{}\n", source.display_name));
        for asset in &source.assets {
            out.push_str(&format!(
                "  [{}] {} — {}\n",
                status_word(asset.status),
                asset.name,
                asset.detail
            ));
        }
    }
    out.push_str(&t(Msg::MigrationSummary {
        total: report.total_assets(),
        active: report.assets_with_status(AssetStatus::Active),
        imported: report.assets_with_status(AssetStatus::Imported),
        actionable: report.assets_with_status(AssetStatus::Actionable),
        blocked: report.assets_with_status(AssetStatus::Blocked),
    }));
    out
}

/// The word a status shows as. Localized: the report is read by a person.
fn status_word(status: AssetStatus) -> String {
    use atomcode_i18n::product::{t, Msg};
    match status {
        AssetStatus::Active => t(Msg::MigrationStatusActive).into_owned(),
        AssetStatus::Imported => t(Msg::MigrationStatusImported).into_owned(),
        AssetStatus::Actionable => t(Msg::MigrationStatusActionable).into_owned(),
        AssetStatus::Blocked => t(Msg::MigrationStatusBlocked).into_owned(),
    }
}

/// The re-check: mark the drafts whose equivalent is already in OUR config.
///
/// This is the "回检" half of Codex's loop. Without it a second run says
/// "can be brought over" about an account that is already there, and the
/// person has no way to tell "not yet imported" from "already imported but
/// different".
///
/// Compared: protocol, endpoint, model list, and the credential's KIND.
///
/// The credential is compared by KIND (a reference to a variable against a
/// reference to a variable), never by value: reading our own credential to
/// compare it would put a secret in memory for no reason, and the file's
/// credential may legitimately have been rotated since.
pub fn mark_already_imported(
    report: &mut MigrationReport,
    drafts: &[ProviderAccountDraft],
    config_path: &Path,
) {
    use atomcode_config::provider_book::ProviderBook;
    let config = ProviderBook::new(config_path.to_path_buf()).load();
    if config.provider_accounts.is_empty() {
        return;
    }
    let accounts = &config.provider_accounts;
    let models = &config.models;
    for source in &mut report.sources {
        for asset in &mut source.assets {
            if asset.kind != AssetKind::ProviderConfig {
                continue;
            }
            // **The draft must come from THIS source.** Matching on the id
            // alone would let one source's row be judged by another
            // source's account: Claude's account and a Codex provider that
            // both land on the id `claude` would be confused, and one
            // source could be reported as "already imported" because of the
            // other's configuration.
            // **A row that is already `Blocked` is never upgraded.** The
            // blocked reason is the more specific truth: "we will not import
            // this" must not become "this is done".
            if asset.status == AssetStatus::Blocked {
                continue;
            }
            // **The draft must come from THIS source, and the draft must be
            // THE ONE this row is about.**
            //
            // Matching on the id alone would let one source's row be judged
            // by another source's account. But even id + source path is not
            // enough: two definitions in ONE file can normalize to the same
            // id (`"foo bar"` and `"foo-bar"` both become `foo-bar`), so
            // `.find()` would hand both rows the first draft — and a blocked
            // second definition would be judged by an unblocked first one.
            //
            // The row keeps the definition's ORIGINAL name, so the drafts
            // are matched on that too.
            let Some(draft) = drafts.iter().find(|d| {
                d.source_name == asset.name && d.source_path == asset.path
            }) else {
                continue;
            };
            let Some(account) = accounts.get(&draft.id) else {
                continue;
            };
            if same_account(account, models, draft) {
                asset.status = AssetStatus::Imported;
                asset.detail =
                    atomcode_i18n::product::t(atomcode_i18n::product::Msg::MigrationAlreadyImported)
                        .into_owned();
            }
        }
    }
}

/// Whether OUR account is the same account as the draft.
///
/// Compared: protocol, endpoint, model list, and the credential's KIND.
///
/// The credential is never compared by value: the file's credential may
/// legitimately have been rotated since, and reading it to compare it would
/// put a secret in memory for no reason.
///
/// **The protocol is compared exactly.** `openai-compatible` is a real preset
/// id, not a wildcard: an account speaking the Responses API is a different
/// account from one speaking chat/completions, even at the same endpoint.
fn same_account(
    account: &atomcode_config::config::ProviderAccountConfig,
    models: &std::collections::HashMap<String, atomcode_config::config::ModelProfileConfig>,
    draft: &ProviderAccountDraft,
) -> bool {
    // **A blocked draft is never "already imported".** A blocked draft is one
    // we REFUSE to import — an unsupported protocol, a name that cannot
    // become an account id, a definition with nothing to connect to. If our
    // config happened to contain a matching entry (someone made it by hand),
    // the row must stay `Blocked`: the re-check must not turn "we will not
    // import this" into "this is done".
    if draft.blocked.is_some() {
        return false;
    }
    // Both sides are the STORED form: the account's `base_url` is `None`
    // when it follows the protocol's default, and the draft's is `None` for
    // the same reason (`effective_endpoint`).
    let same_endpoint = account.base_url.as_deref() == draft.base_url.as_deref();
    let same_protocol = account.provider == draft.protocol;
    // **The model lists are compared as SETS, in both directions.** A
    // one-way check ("every draft model is in ours") is a CONTAINMENT test,
    // not an equivalence test: an account with the draft's models PLUS three
    // more would pass, and the report would call it "already imported" while
    // our config has models the source never had. The documents promise
    // equivalence, so the check must be equivalence.
    //
    // Model names are compared TRIMMED, because the writer trims them
    // (`plan_models`, `provider_book.rs:623`).
    let ours: std::collections::BTreeSet<String> = models
        .values()
        .filter(|m| m.account == draft.id)
        .map(|m| m.model.trim().to_string())
        .collect();
    let theirs: std::collections::BTreeSet<String> = draft
        .models
        .iter()
        .map(|model| model.trim().to_string())
        .collect();
    let same_models = ours == theirs;
    // The credential is compared by **KIND**, never by value: a reference to
    // a variable is the same kind of credential as another reference, and a
    // literal is the same kind as another literal — but the value itself is
    // never read, because the file's key may legitimately have been rotated
    // since, and reading it would put a secret in memory for no reason.
    let same_credential = same_credential_kind(account.api_key.as_deref(), &draft.api_key);
    same_endpoint && same_protocol && same_models && same_credential
}

/// Whether our stored credential is the same KIND as the draft's.
///
/// Our stored form is `${NAME}` for a reference and the secret itself for a
/// literal, so the kind is readable from the shape — without touching a
/// secret's value beyond looking at its first character.
fn same_credential_kind(stored: Option<&str>, draft: &Option<ApiKeyRef>) -> bool {
    let stored_is_reference = stored.is_some_and(|key| key.starts_with("${"));
    match draft {
        None => stored.is_none(),
        Some(ApiKeyRef::EnvVar(_)) => stored_is_reference,
        Some(ApiKeyRef::Plaintext(_)) => stored.is_some() && !stored_is_reference,
    }
}

/// Where to look. The host resolves these; the capability resolves nothing.
///
/// The product's trees come from the host too: the report needs to name a
/// target path, and only the host knows where our trees are.
pub fn scan_context(
    os_home: &Path,
    project: &Path,
    product: atomcode_capabilities::ProductDirs,
    skill_dirs: Option<Vec<std::path::PathBuf>>,
    opencode_config_dir: std::path::PathBuf,
    opencode_data_dir: std::path::PathBuf,
) -> ScanContext {
    ScanContext {
        os_home: os_home.to_path_buf(),
        project: project.to_path_buf(),
        product,
        skill_dirs,
        opencode_config_dir,
        opencode_data_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::migration::{
        AssetKind, AssetReport, AssetScope, AssetStatus, SourceReport,
    };

    fn sample() -> MigrationReport {
        MigrationReport {
            sources: vec![SourceReport {
                source_id: "claude-code",
                display_name: "Claude Code",
                assets: vec![
                    AssetReport {
                        kind: AssetKind::Skills,
                        name: "web-access".into(),
                        path: "/home/u/.claude/skills/web-access".into(),
                        scope: AssetScope::User,
                        status: AssetStatus::Active,
                        detail: "Read directly".into(),
                    },
                    AssetReport {
                        kind: AssetKind::Sessions,
                        name: "sessions".into(),
                        path: "/home/u/.claude/projects".into(),
                        scope: AssetScope::User,
                        status: AssetStatus::Blocked,
                        detail: "Lossy".into(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn the_text_report_names_the_asset_and_its_state() {
        let _guard = atomcode_i18n::product::test_lock();
        atomcode_i18n::product::set_locale(atomcode_i18n::locale::Locale::En);
        let text = render(&sample(), false);
        assert!(text.contains("Claude Code"), "{text}");
        assert!(text.contains("web-access"), "{text}");
        // The state words come from the message table, so they are compared
        // against the table rather than against a literal in this file.
        assert!(
            text.contains(&atomcode_i18n::product::t(
                atomcode_i18n::product::Msg::MigrationStatusActive
            )),
            "{text}"
        );
        assert!(
            text.contains(&atomcode_i18n::product::t(
                atomcode_i18n::product::Msg::MigrationStatusBlocked
            )),
            "{text}"
        );
    }

    #[test]
    fn the_json_report_is_parseable_and_carries_the_same_rows() {
        let json = render(&sample(), true);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["sources"][0]["display_name"], "Claude Code");
        assert_eq!(value["sources"][0]["assets"][0]["name"], "web-access");
        assert_eq!(value["sources"][0]["assets"][0]["status"], "active");
    }

    #[test]
    fn an_empty_report_says_so_instead_of_printing_nothing() {
        let _guard = atomcode_i18n::product::test_lock();
        atomcode_i18n::product::set_locale(atomcode_i18n::locale::Locale::En);
        let text = render(&MigrationReport::default(), false);
        assert!(!text.trim().is_empty());
        assert!(
            text.contains(&atomcode_i18n::product::t(
                atomcode_i18n::product::Msg::MigrationReportEmpty
            )),
            "{text}"
        );
    }

    #[test]
    fn a_draft_already_in_our_config_is_marked_imported() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[provider_accounts.gateway]\nprovider = \"openai-compatible\"\n\
             base_url = \"https://gw.invalid/v1\"\n",
        )
        .unwrap();

        let draft = ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: None,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        };
        let mut report = MigrationReport {
            sources: vec![atomcode_capabilities::migration::SourceReport {
                source_id: "codex",
                display_name: "Codex",
                assets: vec![atomcode_capabilities::migration::AssetReport {
                    kind: AssetKind::ProviderConfig,
                    name: "gateway".into(),
                    path: "/home/u/.codex/config.toml".into(),
                    scope: atomcode_capabilities::migration::AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: "can be imported".into(),
                }],
            }],
        };
        super::mark_already_imported(&mut report, &[draft], &config);
        let asset = &report.sources[0].assets[0];
        assert_eq!(
            asset.status,
            AssetStatus::Imported,
            "it is already there: {asset:?}"
        );
    }

    #[test]
    fn a_draft_that_differs_from_ours_stays_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[provider_accounts.gateway]\nprovider = \"openai-compatible\"\n\
             base_url = \"https://other.invalid/v1\"\n",
        )
        .unwrap();
        let draft = ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: None,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        };
        let mut report = MigrationReport {
            sources: vec![atomcode_capabilities::migration::SourceReport {
                source_id: "codex",
                display_name: "Codex",
                assets: vec![atomcode_capabilities::migration::AssetReport {
                    kind: AssetKind::ProviderConfig,
                    name: "gateway".into(),
                    path: "/home/u/.codex/config.toml".into(),
                    scope: atomcode_capabilities::migration::AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: "can be imported".into(),
                }],
            }],
        };
        super::mark_already_imported(&mut report, &[draft], &config);
        assert_eq!(
            report.sources[0].assets[0].status,
            AssetStatus::Actionable,
            "a different endpoint is not the same account"
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-cli import_cmd`
Expected: FAIL —— 模块不存在（`import_cmd` 未在 `lib.rs` 声明）。

- [ ] **Step 3: 接上 capabilities 的 migration feature**

`crates/atomcode-cli/Cargo.toml` 第 65 行：

```toml
atomcode-capabilities = { path = "../atomcode-capabilities", features = ["notify", "setup", "plugin", "mcp", "atomgit", "cc-hooks", "migration"] }
```

- [ ] **Step 4: 声明模块**

`crates/atomcode-cli/src/lib.rs` 的模块声明区加：

```rust
pub mod import_cmd;
```

- [ ] **Step 5: 加报告文案的 i18n 条目**

`render_text` 与 `status_word` 用的词条（`MigrationReportEmpty` / `MigrationSummary` / `MigrationStatusActive` / `MigrationStatusActionable` / `MigrationStatusBlocked`）还没有 —— Task 2 只加了「每一项资产」的说明词。这几个是**报告的外壳**。

`crates/atomcode-i18n/src/product/messages.rs` 的 `pub enum Msg<'a>` 末尾追加：

```rust
    MigrationReportEmpty,
    MigrationSummary {
        total: usize,
        active: usize,
        imported: usize,
        actionable: usize,
        blocked: usize,
    },
    MigrationStatusActive,
    MigrationStatusImported,
    MigrationStatusActionable,
    MigrationStatusBlocked,
    MigrationAlreadyImported,
```

`crates/atomcode-i18n/src/product/en.rs` 的 `match` 里追加：

```rust
        Msg::MigrationReportEmpty => {
            "No assets from another AI coding agent were found.".into()
        }
        Msg::MigrationSummary {
            total,
            active,
            imported,
            actionable,
            blocked,
        } => format!(
            "\n{total} assets: {active} already in effect, {imported} already imported, \
             {actionable} can be brought over, {blocked} cannot.\n"
        )
        .into(),
        Msg::MigrationStatusActive => "in effect".into(),
        Msg::MigrationStatusImported => "already imported".into(),
        Msg::MigrationStatusActionable => "can move".into(),
        Msg::MigrationStatusBlocked => "cannot move".into(),
        Msg::MigrationAlreadyImported => {
            "Already in your configuration — nothing left to bring over".into()
        }
```

`crates/atomcode-i18n/src/product/zh_cn.rs` 的 `match` 里追加：

```rust
        Msg::MigrationReportEmpty => "没有发现其他 AI 编码 Agent 留下的资产。".into(),
        Msg::MigrationSummary {
            total,
            active,
            imported,
            actionable,
            blocked,
        } => format!(
            "\n共 {total} 项资产：{active} 项已经生效，{imported} 项已导入，\
             {actionable} 项可以带走，{blocked} 项带不走。\n"
        )
        .into(),
        Msg::MigrationStatusActive => "已经生效".into(),
        Msg::MigrationStatusImported => "已导入".into(),
        Msg::MigrationStatusActionable => "可以带走".into(),
        Msg::MigrationStatusBlocked => "带不走".into(),
        Msg::MigrationAlreadyImported => "已在你的配置里 —— 没有需要带过来的了".into(),
```

> **中文标点按 `docs/i18n-style.md`**：全角逗号与句号，数字与中文之间留半角空格。

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-cli import_cmd`
Expected: PASS —— 本 Task 定义的判据全部通过（**以实际条数为准**）。

- [ ] **Step 7: 加 CLI 子命令**

`crates/atomcode-cli/src/main.rs` 的 `enum Commands` 里加（在 `Setup { .. }` 附近）：

```rust
    /// Look at the assets another AI coding agent left behind: instructions,
    /// skills, MCP servers, provider settings. Read-only unless --apply.
    Import {
        /// Print the report as JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Import the provider accounts, asking before each one.
        #[arg(long)]
        apply: bool,
        /// Do not ask; import every provider account that needs no confirmation.
        #[arg(long)]
        yes: bool,
    },
```

在 `main.rs` 的 `match cli.command` 分发里加（与 `Commands::Setup { .. }` 同级）：

```rust
            Commands::Import { json, apply, yes } => {
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                let exit_code =
                    run_import_command(json, apply, yes, cli.config.as_deref()).await;
                telemetry.shutdown(std::time::Duration::from_millis(500)).await;
                return Ok(exit_code);
            }
```

并在 `main.rs` 里加实现（放在 `run_setup_command` 旁边）：

```rust
/// Where our configuration lives.
///
/// The host owns this path. `--config` wins when it is given, the same way it
/// does everywhere else in this binary (`main.rs:1801`); otherwise it is the
/// default location — the same file `atomcode login` writes to.
fn cli_config_path(cli_config: Option<&std::path::Path>) -> std::path::PathBuf {
    match cli_config {
        Some(path) => path.to_path_buf(),
        None => atomcode_config::config::Config::default_path(),
    }
}

async fn run_import_command(
    json: bool,
    apply: bool,
    yes: bool,
    cli_config: Option<&std::path::Path>,
) -> i32 {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let project = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let dirs = atomcode_coding::config::product_dirs_from_env();
    // The configuration we are importing INTO — it also says where our
    // skills live (`[skills].extra_dirs`).
    let config = atomcode_config::config::Config::load(&cli_config_path(cli_config))
        .unwrap_or_default();
    // The skill roots the RUNTIME uses — the user's configured `extra_dirs`
    // come first, and a skill there can win a same-name collision. Resolving
    // them here (rather than letting the capability fall back to the
    // standard list) is what makes the report agree with the runtime.
    let skill_dirs = atomcode_coding::parts::resolve_skill_dirs_from(
        &config.skills.extra_dirs,
        &project,
        &dirs,
        home.as_deref(),
        None,
    )
    .0;
    let ctx =
        let (opencode_config, opencode_data) = atomcode_coding::config::opencode_dirs();
        atomcode::import_cmd::scan_context(
            &home,
            &project,
            dirs.clone(),
            skill_dirs,
            opencode_config,
            opencode_data,
        );
    let sources = atomcode_capabilities::migration::builtin_sources();
    let mut report = atomcode_capabilities::migration::scan(&sources, &ctx);

    // The re-check: what is already in OUR configuration is not "to be
    // brought over" — it is already there.
    let drafts: Vec<_> = sources
        .iter()
        .flat_map(|source| source.provider_drafts(&ctx))
        .collect();
    let config_path = cli_config_path(cli_config);
    atomcode::import_cmd::mark_already_imported(&mut report, &drafts, &config_path);

    if !apply {
        println!("{}", atomcode::import_cmd::render(&report, json));
        return 0;
    }

    // Phase two: provider accounts. `--apply` without `--yes` asks per item.
    match atomcode::import_cmd::apply_provider_accounts(
        &sources,
        &ctx,
        yes,
        &config_path,
        std::io::IsTerminal::is_terminal(&std::io::stdin()),
    ) {
        Ok((text, skipped)) => {
            println!("{text}");
            // Something was left out on purpose: that is not a success.
            if skipped > 0 {
                1
            } else {
                0
            }
        }
        Err(error) => {
            eprintln!("import: {error}");
            1
        }
    }
}
```

（`apply_provider_accounts` 的完整实现与**最终签名**在 Task 8。**Task 5 先写同签名的占位**让编译通过 —— 签名从一开始就定死，Task 8 只换实现不改签名：）

```rust
/// Import the provider accounts an external agent has configured.
///
/// Placeholder in Task 5; Task 8 fills it in. The signature is final.
pub fn apply_provider_accounts(
    _sources: &[Box<dyn atomcode_capabilities::migration::MigrationSource>],
    _ctx: &ScanContext,
    _yes: bool,
    _config_path: &std::path::Path,
    _interactive: bool,
) -> Result<(String, usize), String> {
    Err("provider import is not wired up yet".into())
}
```

**返回值的第二个数是「被跳过的项数」** —— 调用方据此决定退出码：有东西没导入就不是成功。

- [ ] **Step 8: 跑测试与编译检查**

Run: `cargo nextest run -p atomcode-cli import_cmd && cargo check -p atomcode-cli`
Expected: PASS + 退出 0。

- [ ] **Step 9: 提交**

```bash
git add crates/atomcode-cli/src/import_cmd.rs crates/atomcode-cli/src/main.rs crates/atomcode-cli/src/lib.rs crates/atomcode-cli/Cargo.toml crates/atomcode-i18n/src/product/
git commit -m "feat(cli): add atomcode import, a read-only asset report

Prints what another agent left behind and where each item stands: read
directly, can be brought over, or cannot. The JSON form carries the same
rows with stable status names."
```

---

## Task 6: TUI `/import` 命令

**Files:**
- Create: `crates/atomcode-tui/src/migration.rs`
- Modify: `crates/atomcode-tui/src/commands.rs`
- Modify: `crates/atomcode-tui/src/lib.rs`
- Modify: `crates/atomcode-tui/src/plugin.rs`（端口声明 `MigrationSvc`）
- Modify: `crates/atomcode-tui/src/rows.rs`（`commands!` 宏 + 行清单）
- Modify: `crates/atomcode-tui/src/launch.rs`（`Ports` 字段 + 端口注册）
- Modify: `crates/atomcode-tui/tests/language.rs`（内置命令集清单）
- Modify: `crates/atomcode-tui/tests/e2e.rs`（`launch::Ports` 构造点）
- Create: `crates/atomcode-cli/src/tui_migration.rs`
- Modify: `crates/atomcode-cli/src/lib.rs`（模块声明 + `Ports` 装配）
- Modify: `crates/atomcode-i18n/src/screen/{messages,en,zh_cn}.rs`

- [ ] **Step 1: 写失败的测试**

`crates/atomcode-tui/src/migration.rs`（新建，含端口 trait 与视图类型）：

```rust
//! The migration port: what the screen asks the host about another agent's
//! assets.
//!
//! The screen owns the list; the host owns the disk. Same shape as
//! [`crate::setup`] — the screen asks, the host answers with a view type the
//! screen can lay out.

use async_trait::async_trait;

/// What the host found, in the shape the screen draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReportView {
    pub sources: Vec<MigrationSourceView>,
}

/// One external agent's assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationSourceView {
    pub source_id: String,
    pub display_name: String,
    pub assets: Vec<MigrationAssetView>,
}

/// One asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationAssetView {
    pub name: String,
    /// The status, as the four-state vocabulary. The screen renders it with
    /// the SAME shared product message the CLI uses, so the two cannot say
    /// the same state two ways.
    pub status: AssetStatusView,
    pub detail: String,
}

/// The status of one asset, as the screen sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetStatusView {
    Active,
    Imported,
    Actionable,
    Blocked,
}

/// The word a status shows as.
///
/// The **product** table, not the screen table: the CLI says the same state
/// with the same words, and one thing must not be said two ways.
///
/// `pub(crate)` because `commands.rs` — a sibling module — calls it.
pub(crate) fn status_word(status: AssetStatusView) -> String {
    use crate::i18n::product::{t, Msg};
    match status {
        AssetStatusView::Active => t(Msg::MigrationStatusActive).into_owned(),
        AssetStatusView::Imported => t(Msg::MigrationStatusImported).into_owned(),
        AssetStatusView::Actionable => t(Msg::MigrationStatusActionable).into_owned(),
        AssetStatusView::Blocked => t(Msg::MigrationStatusBlocked).into_owned(),
    }
}

/// Read the assets another agent left behind.
#[async_trait]
pub trait Migration: Send + Sync {
    /// The report. Read-only.
    async fn report(&self) -> Result<MigrationReportView, String>;
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-tui migration`

Expected: FAIL —— `/import` 还没注册，判据找不到这个命令。

> **不是** `cargo check` 失败：一个还没被声明、也没被引用的源文件**不会**让
> `cargo check` 失败 —— 它根本没进编译。所以这条判据必须是**跑起来的**：
> 它断言 `/import` 出现在 `/help` 里、且渲染出报告，命令没注册就必然失败。

- [ ] **Step 3: 声明模块**

`crates/atomcode-tui/src/lib.rs` 的模块声明区加：

```rust
pub mod migration;
```

- [ ] **Step 4: 加命令集**

`crates/atomcode-tui/src/commands.rs` 加：

```rust
/// The `/import` command set: another agent's assets, and where each stands.
pub struct ImportCommands;

fn import_catalogue() -> Vec<Command> {
    vec![Command::said("import", t(Msg::CmdAboutImport))]
}

/// `CommandSet` is an `#[async_trait]` trait (`command.rs:296`), so the impl
/// needs the attribute too — without it the `async fn` in the trait does not
/// match the desugared signature. `async_trait` is already imported in this
/// file (`commands.rs:14`), so nothing else is needed.
#[async_trait]
impl CommandSet for ImportCommands {
    fn id(&self) -> &'static str {
        "cmd-import"
    }
    fn commands(&self) -> Vec<Command> {
        import_catalogue()
    }
    async fn run(&self, _name: &str, _args: &str, ctx: &Context) -> Outcome {
        let Some(port) = ctx.service::<crate::plugin::MigrationSvc>() else {
            return Outcome::Refused(t(Msg::NoMigrationPort).into_owned());
        };
        let report = match port.report().await {
            Ok(report) => report,
            Err(error) => return Outcome::Refused(error),
        };

        // **多源才问**（Codex 的做法，`codex-rs/tui/src/external_agent_config_
        // migration/flow.rs:300`）：先检测**全部**来源，只把**检测到东西的**
        // 来源放进选择屏；只有一个来源时**不打扰**用户，直接进面板。
        //
        // 问「你要导入 Claude Code 还是 Codex」而用户只装了一个，是白问一遍
        // —— Codex 的 `detected_sources.len() == 1` 分支正是为此。
        let found: Vec<_> = report
            .sources
            .iter()
            .filter(|source| !source.assets.is_empty())
            .collect();
        if found.is_empty() {
            // The **product** table: the CLI says the same sentence, and one
            // thing must not be said two ways.
            use crate::i18n::product::{t as pt, Msg as PMsg};
            return Outcome::Said(pt(PMsg::MigrationReportEmpty).into_owned());
        }
        if found.len() > 1 {
            let rows: Vec<crate::sheet::Row> = found
                .iter()
                .map(|source| {
                    crate::sheet::Row::new(
                        format!("/import {}", source.source_id),
                        source.display_name.clone(),
                    )
                    .about(format!("{} {}", source.assets.len(), t(Msg::MigrationAssetsCount)))
                })
                .collect();
            return Outcome::Do(crate::keymap::Action::OpenSheet(
                crate::sheet::Sheet::list(
                    crate::sheet::List::new("import-source", t(Msg::MigrationPickSource), rows),
                ),
            ));
        }

        // One source: its assets are the list.
        Outcome::Do(crate::keymap::Action::OpenSheet(crate::sheet::Sheet::list(
            import_list(&found[0]),
        )))
    }
}

/// The assets of one source, as a list the person can walk.
///
/// **This is a bottom sheet (`crate::sheet`), not an overlay.** `overlay.rs`'s
/// own module note says it: a modal is "a flow rather than a list", and a
/// list to pick from is the bottom sheet, which rises from the foot of the
/// screen like every other working panel instead of covering the
/// conversation.
///
/// The sheet gives us, for free: a cursor (↑/↓/PgUp/PgDn/Home/End), a filter
/// box, and — the reason we want a sheet rather than a text dump —
/// **`Row::preview`**: the lines shown under the list when the cursor rests on
/// a row (`sheet.rs:127`). So "expand one item to read what it says" is the
/// existing shape, not something to invent.
fn import_list(source: &crate::migration::MigrationSourceView) -> crate::sheet::List {
    let mut rows: Vec<crate::sheet::Row> = Vec::new();
    for asset in &source.assets {
        let status = crate::migration::status_word(asset.status);
        rows.push(
            crate::sheet::Row::new(
                format!("/import {}", asset.name),
                asset.name.clone(),
            )
            .about(format!("[{status}] {}", asset.detail))
            // The preview is what the row's own sentence says, in full —
            // the `about` is the short form.
            .preview(vec![asset.detail.clone()]),
        );
    }
    crate::sheet::List::new("import", t(Msg::MigrationReportTitle), rows)
        .summary(vec![crate::sheet::Piece::new(
            t(Msg::MigrationSummaryLine {
                source: &source.display_name,
                count: source.assets.len(),
            }),
            crate::sheet::Tone::Plain,
        )])
        .empty(t(Msg::MigrationNoAssets))
}
```

在 `crates/atomcode-tui/src/plugin.rs` 加端口声明（与 `SetupSvc` 同处）：

```rust
plexus_service!(MigrationSvc => dyn crate::migration::Migration, "tui-migration", Seam, "Read the assets another AI coding agent left behind");
```

**注册命令集**：内置命令集不是直接 `add`，而是 `crates/atomcode-tui/src/rows.rs` 里的 `commands!` 宏（它把命令集挂成一行 plexus row，行被移除时命令集也跟着撤下）。照抄 `SetupCommandsRow` 的形状：

`crates/atomcode-tui/src/rows.rs` 里 `commands!(...)` 调用处追加：

```rust
commands!(
    ImportCommandsRow,
    "tui-commands-import",
    crate::commands::ImportCommands,
    "/import: what another AI coding agent left behind, and where each item stands"
);
```

并把该行加进 `rows.rs` 的行清单（`Arc::new(SetupCommandsRow),` 那一段）：

```rust
        Arc::new(ImportCommandsRow),
```

**注意**：`/import` 的名字若与别的命令集冲突，`Commands::add` 会返回 `Err("`/` and `/` both define /import; disable one")` —— 那是**运行期**错误，注册后必须跑一次判据确认 `/import` 出现在 `/help` 里。

`crates/atomcode-tui/tests/language.rs` 里列了全部内置命令集（`Box::new(atomcode_tui::commands::SessionCommands)` 等），**把 `ImportCommands` 也加进去**，否则那条语言判据不会覆盖新命令。

- [ ] **Step 5: 加 screen 表文案**

**只有屏幕自己的话进 screen 表。** 「没有发现其他 AI 编码 Agent 留下的资产」这句话 CLI 也会说（`product` 表的 `MigrationReportEmpty`），所以屏幕**复用 product 那一条**，不在 screen 表里写第二遍。

**面板自己的话进 screen 表**；CLI 也会说的那句（「没有发现其他 AI 编码 Agent
留下的资产」）**复用 product 表的 `MigrationReportEmpty`**，不在 screen 表里写
第二遍。

`crates/atomcode-i18n/src/screen/messages.rs` 的 `pub enum Msg` 加：

```rust
    CmdAboutImport,
    NoMigrationPort,
    /// The sheet's title: the assets another agent left behind.
    MigrationReportTitle,
    /// The sheet when the chosen source has no assets.
    MigrationNoAssets,
    /// Picking which source to look at, when more than one has assets.
    MigrationPickSource,
    /// How many assets a source has.
    MigrationAssetsCount,
    /// The sheet's summary line: which source, how many assets.
    MigrationSummaryLine {
        source: &'a str,
        count: usize,
    },
```

`crates/atomcode-i18n/src/screen/en.rs` 加：

```rust
        Msg::CmdAboutImport => "Look at the assets another AI coding agent left behind".into(),
        Msg::NoMigrationPort => "No migration service is available".into(),
        Msg::MigrationReportTitle => "Assets another agent left behind".into(),
        Msg::MigrationNoAssets => "This source has no assets".into(),
        Msg::MigrationPickSource => "Which agent's assets do you want to see?".into(),
        Msg::MigrationAssetsCount => "assets".into(),
        Msg::MigrationSummaryLine { source, count } => {
            format!("{source}: {count} assets").into()
        }
```

`crates/atomcode-i18n/src/screen/zh_cn.rs` 加：

```rust
        Msg::CmdAboutImport => "查看其他 AI 编码 Agent 留下的资产".into(),
        Msg::NoMigrationPort => "迁移服务不可用".into(),
        Msg::MigrationReportTitle => "其他 Agent 留下的资产".into(),
        Msg::MigrationNoAssets => "这个来源没有资产".into(),
        Msg::MigrationPickSource => "要看哪个 Agent 的资产？".into(),
        Msg::MigrationAssetsCount => "项资产".into(),
        Msg::MigrationSummaryLine { source, count } => {
            format!("{source}：{count} 项资产").into()
        }
```

- [ ] **Step 6: 写端口实现**

`crates/atomcode-cli/src/tui_migration.rs`（新建）：

> **别忘了声明模块**：`crates/atomcode-cli/src/lib.rs` 的模块区加 `pub mod tui_migration;`（只建文件不声明模块，代码不会进编译）。

```rust
//! The host side of the migration port: it scans the disk and hands the screen
//! a view it can lay out.

use async_trait::async_trait;
use atomcode_tui::migration::{
    AssetStatusView, Migration, MigrationAssetView, MigrationReportView, MigrationSourceView,
};
use std::path::PathBuf;

pub struct DiskMigration {
    home: PathBuf,
    project: PathBuf,
    product: atomcode_capabilities::ProductDirs,
    /// The skill roots the RUNTIME uses, resolved by the host.
    ///
    /// **The host must supply them**: the screen cannot resolve them itself —
    /// it may not depend on `atomcode-coding` (the layering gate forbids it),
    /// and the resolution rule lives there. `None` means "the host did not
    /// say" and the standard list is used; `Some(dirs)` means exactly these.
    /// An empty list would fall back to the standard list, and then the
    /// screen would disagree with the CLI about which same-named skill wins.
    skill_dirs: Option<Vec<PathBuf>>,
    /// Where OUR configuration lives — the screen's re-check compares against
    /// it, so an account that was imported shows as "already imported" in the
    /// screen too, not just in the CLI.
    config_path: PathBuf,
}

impl DiskMigration {
    pub fn new(
        home: PathBuf,
        project: PathBuf,
        product: atomcode_capabilities::ProductDirs,
        config_path: PathBuf,
        skill_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self {
            home,
            project,
            product,
            config_path,
            skill_dirs,
        }
    }
}

#[async_trait]
impl Migration for DiskMigration {
    async fn report(&self) -> Result<MigrationReportView, String> {
        let ctx = atomcode_capabilities::migration::ScanContext {
            os_home: self.home.clone(),
            project: self.project.clone(),
            product: self.product.clone(),
            skill_dirs: self.skill_dirs.clone(),
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let sources = atomcode_capabilities::migration::builtin_sources();
        let mut report = atomcode_capabilities::migration::scan(&sources, &ctx);
        // The same re-check the CLI does — the screen must not say "can be
        // brought over" about an account that is already in the config.
        let drafts: Vec<_> = sources
            .iter()
            .flat_map(|source| source.provider_drafts(&ctx))
            .collect();
        // `crate::` — this file IS the `atomcode` library, so `atomcode::`
        // does not resolve here.
        crate::import_cmd::mark_already_imported(&mut report, &drafts, &self.config_path);
        Ok(MigrationReportView {
            sources: report
                .sources
                .into_iter()
                .map(|source| MigrationSourceView {
                    source_id: source.source_id.to_string(),
                    display_name: source.display_name.to_string(),
                    assets: source
                        .assets
                        .into_iter()
                        .map(|asset| MigrationAssetView {
                            name: asset.name,
                            status: match asset.status {
                                atomcode_capabilities::migration::AssetStatus::Active => {
                                    AssetStatusView::Active
                                }
                                atomcode_capabilities::migration::AssetStatus::Imported => {
                                    AssetStatusView::Imported
                                }
                                atomcode_capabilities::migration::AssetStatus::Actionable => {
                                    AssetStatusView::Actionable
                                }
                                atomcode_capabilities::migration::AssetStatus::Blocked => {
                                    AssetStatusView::Blocked
                                }
                            },
                            detail: asset.detail,
                        })
                        .collect(),
                })
                .collect(),
        })
    }
}
```

- [ ] **Step 7: 装配端口**

`crates/atomcode-cli/src/lib.rs` 的 `launch::Ports { ... }` 加一项。

**先算出技能根**（就在 `launch::Ports` 之前，`config` 还在手上时）：

```rust
        // The skill roots the RUNTIME uses. The screen may not depend on
        // `atomcode-coding` (the layering gate forbids it), so the HOST
        // resolves them and hands them over — the same list the CLI's own
        // report uses, so the two front ends agree about which same-named
        // skill wins.
        let skill_dirs = atomcode_coding::parts::resolve_skill_dirs_from(
            &config.extra_skill_dirs,
            &working_dir,
            &dirs,
            dirs::home_dir().as_deref(),
            None,
        )
        .0;
```

然后加一项：

```rust
                migration: Some(Arc::new(crate::tui_migration::DiskMigration::new(
                    dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(".")),
                    working_dir.clone(),
                    dirs.clone(),
                    config_path.clone(),
                    skill_dirs.clone(),
                ))),
```

> 字段类型是 `Option<Arc<dyn Migration>>`，所以要 `Arc::new(...)` 包起来 —— 直接传 `DiskMigration` 类型对不上。

> **这一项要放在 `setup:` 与 `dirs:` 之前。** 结构体是逐字段书写的，而
> `setup: Some(crate::tui_setup::DiskSetup::new(working_dir, dirs.clone(), config_path))`
> **移走**了 `working_dir` 与 `config_path`，`dirs: Some(dirs)` 又移走了 `dirs`。
> 迁移这一项排在其后就用不了这些值了 —— 报「use of moved value」。
> 放在前面，它 `clone()` 到的就是还没被移走的那一份。

`crates/atomcode-tui/src/launch.rs` 的 `pub struct Ports` 加字段：

```rust
    pub migration: Option<Arc<dyn crate::migration::Migration>>,
```

并在 `launch.rs` 的端口注册处（`if let Some(setup) = ports.setup { ... }` 旁边）加：

```rust
    if let Some(migration) = ports.migration {
        let _ = ctx
            .provide::<crate::plugin::MigrationSvc>(migration)
            .map_err(|e| e.to_string())?;
    }
```

**`Ports` 的两个构造点都要补这个字段**（结构体是逐字段书写的，加字段会让它们编译不过）：

- `crates/atomcode-cli/src/lib.rs:386`（见 Step 7）
- `crates/atomcode-tui/tests/e2e.rs:586`：

```rust
        launch::Ports {
            settings,
            providers: None,
            plugins,
            setup: None,
            migration: None,
            dirs: None,
        },
```

> `Ports` 虽然 `#[derive(Default)]`，但这两个构造点没有用 `..Default::default()`，所以**必须**逐个补字段。

- [ ] **Step 8: 写判据**

在 `crates/atomcode-tui/src/commands.rs` 的测试区加：

```rust
    #[test]
    fn the_import_list_carries_each_asset_of_the_source() {
        let source = crate::migration::MigrationSourceView {
            source_id: "claude-code".into(),
            display_name: "Claude Code".into(),
            assets: vec![crate::migration::MigrationAssetView {
                name: "web-access".into(),
                status: crate::migration::AssetStatusView::Active,
                detail: "Read directly".into(),
            }],
        };
        let list = super::import_list(&source);
        let rows = list.rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "web-access");
        // The status word is part of what the row means.
        assert!(rows[0].about.contains("in effect"), "{:?}", rows[0].about);
        // The preview is the row's own sentence, in full — that is what
        // "expand one item to read it" shows.
        assert_eq!(rows[0].preview, vec!["Read directly".to_string()]);
    }

    #[test]
    fn an_import_list_with_no_assets_says_so() {
        let source = crate::migration::MigrationSourceView {
            source_id: "codex".into(),
            display_name: "Codex".into(),
            assets: vec![],
        };
        let list = super::import_list(&source);
        assert!(!list.empty.trim().is_empty());
    }
```

**还要一条判据，证明 `/import` 真的注册了、而且真的能跑** —— 上面两条只测渲染
函数：命令没进行清单、或者端口取不到，它们照样通过。

这条判据放在 **`crates/atomcode-tui/src/commands.rs` 的测试区**（不是
`tests/e2e.rs`）—— 因为它要断言的是**命令集与端口**，而 `commands.rs` 的测试
区已经有这两样：`bare()` 造一个空 `App`、`app.context()` 拿到 `Context`、
`ctx.provide::<Svc>(…)` 装一个假端口、`ctx.service::<Svc>()` 取它
（`plexus/src/context.rs:110` 与 `:125`）。`tests/e2e.rs` 那套 harness 走的是
整屏挂载，与这条判据要证明的事无关。

```rust
    /// A host that answers with one known row.
    struct FakeMigration {
        report: crate::migration::MigrationReportView,
    }

    #[async_trait::async_trait]
    impl crate::migration::Migration for FakeMigration {
        async fn report(&self) -> Result<crate::migration::MigrationReportView, String> {
            Ok(self.report.clone())
        }
    }

    /// The `/import` command is REGISTERED (it is in the command inventory)
    /// and it RUNS (it reaches the port and renders what the host said).
    ///
    /// The two rendering criteria above would pass even if `/import` were
    /// never registered, or if the port could not be reached — they call the
    /// renderer directly. This one goes through the command set.
    #[tokio::test]
    async fn the_import_command_is_registered_and_renders_the_host_report() {
        let app = bare();
        // The port the command reaches.
        let _ = app.context().provide::<crate::plugin::MigrationSvc>(Arc::new(FakeMigration {
            report: crate::migration::MigrationReportView {
                sources: vec![crate::migration::MigrationSourceView {
                    source_id: "claude-code".into(),
                    display_name: "Claude Code".into(),
                    assets: vec![crate::migration::MigrationAssetView {
                        name: "web-access".into(),
                        status: crate::migration::AssetStatusView::Active,
                        detail: "Read directly".into(),
                    }],
                }],
            },
        }));

        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(ImportCommands));

        // It is REGISTERED: the command is in the inventory.
        let names: Vec<String> = all
            .matching("import")
            .into_iter()
            .map(|command| command.name.to_string())
            .collect();
        assert!(
            names.iter().any(|name| name == "import"),
            "/import is not registered: {names:?}"
        );

        // It RUNS: the port is reached and its report becomes a SHEET.
        let Outcome::Do(crate::keymap::Action::OpenSheet(sheet)) =
            all.dispatch("/import", &app.context()).await
        else {
            panic!("/import did not open a sheet");
        };
        let crate::sheet::Page::List(list) = sheet.page else {
            panic!("a list");
        };
        assert_eq!(list.rows.len(), 1);
        assert_eq!(list.rows[0].label, "web-access");
    }

    /// **多源才问**（Codex 的做法，`flow.rs:300`）：一个来源有资产时**不打扰**
    /// 用户，直接进面板。
    #[tokio::test]
    async fn one_source_with_assets_goes_straight_to_the_list() {
        let app = bare();
        let _ = app.context().provide::<crate::plugin::MigrationSvc>(Arc::new(FakeMigration {
            report: crate::migration::MigrationReportView {
                sources: vec![crate::migration::MigrationSourceView {
                    source_id: "claude-code".into(),
                    display_name: "Claude Code".into(),
                    assets: vec![crate::migration::MigrationAssetView {
                        name: "web-access".into(),
                        status: crate::migration::AssetStatusView::Active,
                        detail: "Read directly".into(),
                    }],
                }],
            },
        }));
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(ImportCommands));
        let Outcome::Do(crate::keymap::Action::OpenSheet(sheet)) =
            all.dispatch("/import", &app.context()).await
        else {
            panic!("/import did not open a sheet");
        };
        // The list of the SOURCE's assets — not a picker of sources.
        let crate::sheet::Page::List(list) = sheet.page else {
            panic!("a list");
        };
        assert_eq!(list.id, "import");
    }

    /// Two sources with assets → ask which one.
    #[tokio::test]
    async fn two_sources_with_assets_ask_which_one() {
        let app = bare();
        let _ = app.context().provide::<crate::plugin::MigrationSvc>(Arc::new(FakeMigration {
            report: crate::migration::MigrationReportView {
                sources: vec![
                    crate::migration::MigrationSourceView {
                        source_id: "claude-code".into(),
                        display_name: "Claude Code".into(),
                        assets: vec![crate::migration::MigrationAssetView {
                            name: "web-access".into(),
                            status: crate::migration::AssetStatusView::Active,
                            detail: "Read directly".into(),
                        }],
                    },
                    crate::migration::MigrationSourceView {
                        source_id: "codex".into(),
                        display_name: "Codex".into(),
                        assets: vec![crate::migration::MigrationAssetView {
                            name: "agents".into(),
                            status: crate::migration::AssetStatusView::Actionable,
                            detail: "Copy it somewhere".into(),
                        }],
                    },
                ],
            },
        }));
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(ImportCommands));
        let Outcome::Do(crate::keymap::Action::OpenSheet(sheet)) =
            all.dispatch("/import", &app.context()).await
        else {
            panic!("/import did not open a sheet");
        };
        let crate::sheet::Page::List(list) = sheet.page else {
            panic!("a list");
        };
        assert_eq!(list.id, "import-source", "a picker of sources");
        assert_eq!(list.rows.len(), 2);
    }

    /// A source with NO assets is not offered — asking "which agent?" when
    /// only one of them has anything is a question with an obvious answer.
    #[tokio::test]
    async fn a_source_with_no_assets_is_not_offered() {
        let app = bare();
        let _ = app.context().provide::<crate::plugin::MigrationSvc>(Arc::new(FakeMigration {
            report: crate::migration::MigrationReportView {
                sources: vec![
                    crate::migration::MigrationSourceView {
                        source_id: "claude-code".into(),
                        display_name: "Claude Code".into(),
                        assets: vec![crate::migration::MigrationAssetView {
                            name: "web-access".into(),
                            status: crate::migration::AssetStatusView::Active,
                            detail: "Read directly".into(),
                        }],
                    },
                    crate::migration::MigrationSourceView {
                        source_id: "codex".into(),
                        display_name: "Codex".into(),
                        assets: vec![],
                    },
                ],
            },
        }));
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(ImportCommands));
        let Outcome::Do(crate::keymap::Action::OpenSheet(sheet)) =
            all.dispatch("/import", &app.context()).await
        else {
            panic!("/import did not open a sheet");
        };
        let crate::sheet::Page::List(list) = sheet.page else {
            panic!("a list");
        };
        assert_eq!(
            list.id, "import",
            "only the source WITH assets is offered, so no picker"
        );
    }
```

> **命令名不带斜杠**：`Command::said("import", …)`（`command.rs:148`）存的是
> `import`，`/help` 与 `matching` 看到的就是这个名字；斜杠只是**调用时**的前缀
> （`dispatch("/import", …)`）。所以断言的是 `"import"`，不是 `"/import"`。
>
> 判据的**名字**里都有 `import`，所以 `cargo nextest run -p atomcode-tui import`
> 这一条过滤能同时跑到它们全部。
>
> **这些判据断言的是「底部单子」，不是一段文本。** `/import` 走
> `Outcome::Do(Action::OpenSheet(…))`（`keymap.rs:321`），所以断言的是
> `OpenSheet` 里的 `Page::List` —— 单子的 id、行数、每行的 label 与 preview。
> 一段文本（`Outcome::Said`）在一屏放不下时会被截断，而清单被截断等于清单没了。

- [ ] **Step 9: 跑测试**

Run: `cargo nextest run -p atomcode-tui import`
Expected: PASS —— 本 Task 定义的判据全部通过（**以实际条数为准**：两条列表判据 +
一条注册与执行 + 三条「多源才问」）。

Run: `cargo check -p atomcode-cli`
Expected: 退出 0。

- [ ] **Step 10: 提交**

```bash
git add crates/atomcode-tui/src/migration.rs crates/atomcode-tui/src/commands.rs crates/atomcode-tui/src/lib.rs crates/atomcode-tui/src/plugin.rs crates/atomcode-tui/src/launch.rs crates/atomcode-tui/src/rows.rs crates/atomcode-tui/tests/language.rs crates/atomcode-tui/tests/e2e.rs crates/atomcode-cli/src/tui_migration.rs crates/atomcode-cli/src/lib.rs crates/atomcode-i18n/src/screen/
git commit -m "feat(tui): add /import, another agent's assets on screen

The screen asks the host for the report and lays it out; the host owns the
disk. Same shape as /setup."
```

---

## Task 7: provider 映射（中立草案）

**Files:**
- Modify: `crates/atomcode-config/src/provider_book.rs`（把归一规则公开出来）
- Modify: `crates/atomcode-coding/src/parts.rs`（把技能根的解析公开出来，宿主要靠它）
- Modify: `crates/atomcode-capabilities/src/migration/provider.rs`
- Modify: `crates/atomcode-capabilities/src/migration/claude.rs`
- Modify: `crates/atomcode-capabilities/src/migration/codex.rs`
- Modify: `crates/atomcode-capabilities/src/migration/mod.rs`

**先做 coding 那一处**（宿主技能根要靠它）：把技能目录的解析公开出来，让宿主
按**运行时同一套规则**取得技能根，而不是自己重算一份。

`crates/atomcode-coding/src/parts.rs` —— 宿主（CLI / TUI）要拿到**运行时实际
用的**技能根，就需要一个公开入口。但**不能**让宿主自己重算一份规则：重算的那份
迟早与运行时不一致，而「同名技能谁生效」正是报告必须说对的事。

所以做法是**一个实现、两个入口**：`resolve_skill_dirs_at` 的函数体不动，只是把
它**要读的那几个输入**显式化，再加一个公开包装。

```rust
/// The skill roots the runtime actually uses, in LOW→HIGH priority order.
///
/// **Public because a host that reports on skills must see the SAME roots the
/// runtime sees** — the user's configured `extra_dirs` come FIRST, and a skill
/// there can win a same-name collision. A host that recomputed the standard
/// list alone would report a different winner than the one that runs.
///
/// The inputs are spelled out rather than taken as a whole
/// `CodingAgentConfig`, so a host can call this without assembling one.
/// The body is the runtime's own — there is ONE rule, not two.
pub fn resolve_skill_dirs_from(
    extra_dirs: &[String],
    working_dir: &Path,
    dirs: &ProductDirs,
    home: Option<&Path>,
    explicit: Option<&[PathBuf]>,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<String>) {
    // The same body `resolve_skill_dirs_at` has today, reading these five
    // inputs instead of `cfg` / `opts`.
    if let Some(dirs) = explicit {
        return (dirs.to_vec(), dirs.to_vec(), Vec::new());
    }
    let mut warnings = Vec::new();
    let mut extra = Vec::new();
    for value in extra_dirs {
        // …the same expansion `~`, the same project-relative rule, the same
        // warnings, the same canonicalization…
    }
    // …the same `dirs = extra ++ standard_skill_dirs(…)` and the same
    // alias dedup…
    (dirs, explicit_list, warnings)
}
```

> **实现要求**：`resolve_skill_dirs_at` 与 `resolve_skill_dirs_from` **共用同一份
> 函数体** —— 把 `resolve_skill_dirs_at` 改成转发给后者，`prepare` 走的还是同
> 一条规则。这样「宿主看到的」与「运行时用的」不可能分叉。
>
> `explicit` 是驱动的显式覆盖（`PrepareOptions::skill_dirs`）：给了它就**只**用它，
> 连标准目录都不扫 —— 那是覆盖的语义，宿主也要照做。

**再�� config 那一处**（其余步骤都依赖它）：把写入方的两条归一规则公开出来，
让报告和写入方**共用同一条**，而不是各写一份必然漂移的：

`crates/atomcode-config/src/provider_book.rs` —— `base_id` 与
`endpoint_override` 现在是私有的（`:700` 与 `:785`）。各加一个公开包装：

```rust
/// The id an account with this name would get.
///
/// The same rule `create_account` applies, so a caller that needs to refer
/// to an account before it exists (a migration report, say) names it the
/// way it will be written.
///
/// `None` when the name cannot become an account at all.
pub fn account_id_for(name: &str) -> Result<String, BookError> {
    base_id(name)
}

/// The endpoint worth storing for this protocol.
///
/// `None` when the endpoint is empty or is the protocol's own default — the
/// same rule `create_account` applies.
pub fn effective_endpoint(protocol: &str, endpoint: &str) -> Option<String> {
    endpoint_override(endpoint, protocol).map(str::to_string)
}
```

> `base_id` 与 `endpoint_override` **本身不动** —— 只加包装，所以写入方的行为
> 一字不变，报告这边却能拿到同一条规则。
>
> `base_id` 返回 `Result`，因为名字为空或全是无法归一的字符时会
> `BookError::NameRules`（`:703`）—— 那种名字**根本不能成为账号**，报告要把它
> 标成 `Blocked`，不是硬造一个 id。


- [ ] **Step 1: 写失败的测试**

在 `provider.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // The tests call `provider_drafts`, a method on the trait — so the trait
    // has to be in scope, not just the types. `AssetKind` / `AssetStatus` are
    // used by the criteria that drive the public `scan()`.
    use crate::migration::{
        AssetKind, AssetScope, AssetStatus, MigrationSource, ScanContext,
    };
    use std::path::Path;

    /// Claude's settings.json with a provider configured by environment
    /// variable name — the shape that can be carried over without copying a
    /// secret.
    fn claude_settings(home: &Path, body: &str) {
        let dir = home.join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), body).unwrap();
    }

    #[test]
    fn a_claude_env_value_is_a_literal_secret_not_a_reference() {
        // Claude's `env` block maps a NAME to a VALUE — the value is the secret
        // itself. Codex's `env_key` is the other way round: it names the
        // variable to read. Treating them alike would either copy a secret or
        // write a name that means nothing.
        let home = tempfile::tempdir().unwrap();
        claude_settings(
            home.path(),
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://example.invalid","ANTHROPIC_API_KEY":"sk-secret-value"}}"#,
        );
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: std::path::PathBuf::from("/work"),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let drafts = super::super::claude::ClaudeSource.provider_drafts(&ctx);
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(draft.base_url.as_deref(), Some("https://example.invalid"));
        assert_eq!(
            draft.api_key,
            Some(ApiKeyRef::Plaintext("sk-secret-value".into())),
            "the value in Claude's env block IS the secret, so it is marked as one"
        );
    }

    #[test]
    fn a_codex_env_key_becomes_an_env_named_key() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[model_providers.gateway]\nbase_url = \"https://gw.invalid/v1\"\nenv_key = \"GW_TOKEN\"\n",
        )
        .unwrap();
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: std::path::PathBuf::from("/work"),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let drafts = super::super::codex::CodexSource.provider_drafts(&ctx);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].api_key, Some(ApiKeyRef::EnvVar("GW_TOKEN".into())));
        assert_eq!(drafts[0].base_url.as_deref(), Some("https://gw.invalid/v1"));
    }

    #[test]
    fn a_plaintext_key_is_marked_plaintext() {
        let home = tempfile::tempdir().unwrap();
        claude_settings(
            home.path(),
            r#"{"env":{"ANTHROPIC_API_KEY":"sk-secret-value"}}"#,
        );
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: std::path::PathBuf::from("/work"),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let drafts = super::super::claude::ClaudeSource.provider_drafts(&ctx);
        assert_eq!(
            drafts[0].api_key,
            Some(ApiKeyRef::Plaintext("sk-secret-value".into())),
            "a literal secret is marked as one, so the host can ask before writing it"
        );
    }

    #[test]
    fn a_provider_in_the_user_settings_is_reported_as_a_row_named_by_its_id() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let settings = home.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            &settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://example.invalid","ANTHROPIC_API_KEY":"sk-x"}}"#,
        )
        .unwrap();
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: project.path().to_path_buf(),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };

        // The row comes from the draft, so the row's NAME is the account id —
        // that is what the re-check matches on.
        let report = super::super::scan(
            &[Box::new(super::super::claude::ClaudeSource)],
            &ctx,
        );
        let row = report.sources[0]
            .assets
            .iter()
            .find(|a| a.kind == AssetKind::ProviderConfig)
            .expect("the provider is reported");
        assert_eq!(row.name, "claude", "the row is named by the account id");
        assert_eq!(row.status, AssetStatus::Actionable);
    }

    #[test]
    fn no_provider_configuration_means_no_drafts() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: std::path::PathBuf::from("/work"),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let drafts = super::super::claude::ClaudeSource.provider_drafts(&ctx);
        assert!(drafts.is_empty());
    }

    /// A project config that overrides ONE field under a provider id the user
    /// config also declares.
    ///
    /// Codex merges its layers recursively
    /// (`codex-rs/config/src/merge.rs:59`), so the inherited `env_key` and
    /// `wire_api` survive. Replacing the whole table would drop the
    /// credential — the one thing the person most needs to keep.
    #[test]
    fn a_project_override_keeps_the_inherited_fields() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let user_config = home.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        std::fs::write(
            &user_config,
            "[model_providers.gateway]\nbase_url = \"https://user.invalid/v1\"\n\
             env_key = \"GW_TOKEN\"\nwire_api = \"responses\"\n",
        )
        .unwrap();
        let project_config = project.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(project_config.parent().unwrap()).unwrap();
        std::fs::write(
            &project_config,
            "[model_providers.gateway]\nbase_url = \"https://project.invalid/v1\"\n",
        )
        .unwrap();

        // Asserted through the PUBLIC surface: `merge_codex_configs` and the
        // merged struct are private to `codex.rs`, and a sibling module's
        // test cannot reach them.
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: project.path().to_path_buf(),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let drafts = super::super::codex::CodexSource.provider_drafts(&ctx);
        let draft = drafts
            .iter()
            .find(|d| d.id == "gateway")
            .expect("the provider is merged into one draft");
        assert_eq!(
            draft.base_url.as_deref(),
            Some("https://project.invalid/v1"),
            "the project's value wins"
        );
        assert_eq!(
            draft.api_key,
            Some(ApiKeyRef::EnvVar("GW_TOKEN".into())),
            "the inherited credential is NOT dropped"
        );
        assert_eq!(
            draft.protocol, "openai-responses",
            "the inherited protocol is NOT dropped"
        );
    }

    #[test]
    fn a_missing_wire_api_means_responses_not_chat_completions() {
        // Codex's `WireApi` has `#[default] Responses`
        // (`codex-rs/model-provider-info/src/lib.rs:117`).
        assert_eq!(
            super::protocol_for_wire(None, "openai-responses"),
            "openai-responses",
            "a config with no wire_api speaks Responses"
        );
        assert_eq!(
            super::protocol_for_wire(Some("responses"), "openai-responses"),
            "openai-responses"
        );
        assert_eq!(
            super::protocol_for_wire(Some("chat"), "openai-responses"),
            "openai-compatible"
        );
        // The SOURCE says its own default — the shared function does not know
        // which source it is.
        assert_eq!(
            super::protocol_for_wire(None, "anthropic-compatible"),
            "anthropic-compatible"
        );
    }

    #[test]
    fn the_model_goes_to_the_provider_that_was_selected() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let config = home.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "model = \"gpt-x\"\nmodel_provider = \"second\"\n\
             [model_providers.first]\nbase_url = \"https://a.invalid/v1\"\n\
             [model_providers.second]\nbase_url = \"https://b.invalid/v1\"\n",
        )
        .unwrap();
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: project.path().to_path_buf(),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let drafts = super::super::codex::CodexSource.provider_drafts(&ctx);
        let first = drafts.iter().find(|d| d.id == "first").expect("first exists");
        let second = drafts.iter().find(|d| d.id == "second").expect("second exists");
        assert!(
            first.models.is_empty(),
            "the model does NOT go to the first account: {:?}",
            first.models
        );
        assert_eq!(
            second.models,
            vec!["gpt-x".to_string()],
            "the model goes to the provider that was selected"
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-capabilities --features migration migration::provider`
Expected: FAIL —— `provider_drafts` 不存在。

- [ ] **Step 3: 实现映射**

`provider.rs` 的共享规则（`protocol_for_wire` / `unsupported_protocol` /
`account_id_for` / `effective_endpoint` / `protocol_needs_endpoint` /
`endpoint_in_use` / `read_jsonc`）**已经在 Task 1 里了** —— 它们不是某个来源的
知识，而是「我们的写入方会怎么归一」，所以放在共享模块，**每个来源都能用**。
本 Task **不再改它们**，只做**每个来源怎么读出自己的草案**。

> **为什么这些在 Task 1 而不是本 Task**：Task 4 的 OpenCode 扫描已经要用
> `account_id_for` 与 `effective_endpoint`（它要报 provider 行），Task 1 也要
> re-export `endpoint_in_use` 给宿主 —— 所以它们必须在 Task 1 就到位，
> **Task 1 与 Task 4 才能各自单独编译并通过**。

**`claude_env` 从 `provider.rs` 移到 `claude.rs`** —— 它是 **Claude 专属**的
读法（`settings.json` 的 `env` 块），共享模块不该知道某个来源的配置长什么样。
它落到 `claude.rs`：

```rust
/// Read Claude's `env` block out of its settings file.
///
/// **Claude's, not ours.** The `env` block maps a NAME to a VALUE, and the
/// value IS the secret — so a key read from here is a literal, never a
/// reference. That is the opposite of Codex's `env_key`, and the difference
/// is why this function lives with Claude rather than in the shared module.
pub fn claude_env(home: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = std::fs::read_to_string(home.join(".claude").join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("env")?.as_object().cloned()
}
```

调用点跟着改（`super::provider::claude_env` → 本模块的 `claude_env`）。

**为什么没有启发式**：两个来源的语义都是已知的，不需要靠字符串形状猜。

- Claude 的 `env` 块是 **名字 → 值**，值**就是**密钥本身 → 永远是 `Plaintext`。
- Codex 的 `env_key` 是 **变量的名字**，值在环境里 → 永远是 `EnvVar`。

一个「长得像变量名就当引用」的启发式会把 Claude 的 `ANTHROPIC_API_KEY=MY_KEY`（值恰好像名字）误判成引用，把一个真密钥当名字存进我们的文件 —— 那样运行时读不到密钥，而文件里还留着一个看似无害的字符串。**按来源分别处理，不猜。**

**Claude 的草案**（加进 `claude.rs` 的 `impl MigrationSource`）：

```rust
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        // Claude's own reader — it lives with Claude now.
        let Some(env) = claude_env(&ctx.os_home) else {
            return Vec::new();
        };
        let base_url = env
            .get("ANTHROPIC_BASE_URL")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Claude's `env` block maps a NAME to a VALUE: the value IS the secret.
        // It is never a reference, so it is always a plaintext credential — and
        // a plaintext credential needs the person's confirmation before it is
        // written. Codex's `env_key` is the opposite (a name to read), and that
        // one becomes an `EnvVar`.
        let key = env
            .get("ANTHROPIC_API_KEY")
            .or_else(|| env.get("ANTHROPIC_AUTH_TOKEN"))
            .and_then(|v| v.as_str())
            .filter(|v| !v.trim().is_empty())
            .map(|secret| ApiKeyRef::Plaintext(secret.to_string()));
        if base_url.is_none() && key.is_none() {
            return Vec::new();
        }
        // **A BLANK endpoint is an ABSENT endpoint.** Claude's settings can
        // carry `ANTHROPIC_BASE_URL: ""`; treating that as "it has an
        // endpoint" would pick the custom protocol and then fail to write it
        // with `ProtocolNeedsEndpoint`. So the endpoint is trimmed and
        // emptied ones are treated as absent.
        let base_url = base_url
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty());
        // **Claude with only a key and no endpoint was talking to Anthropic
        // itself.** It gets the `anthropic` preset, which HAS a default
        // endpoint (`config/provider_preset.rs:352`). With the
        // `anthropic-compatible` preset — which has NO default endpoint
        // (`:86`) — the write would fail with `ProtocolNeedsEndpoint` for a
        // perfectly ordinary configuration.
        //
        // With an explicit `ANTHROPIC_BASE_URL` it is a custom endpoint, so
        // `anthropic-compatible` is right.
        let protocol = if base_url.is_some() {
            super::provider::protocol_for_wire(None, "anthropic-compatible")
        } else {
            "anthropic".to_string()
        };
        vec![ProviderAccountDraft {
            // The id the account will get, not the raw name.
            id: super::provider::account_id_for("claude")
                .unwrap_or_else(|| "claude".into()),
            // The name the source wrote it under.
            source_name: "claude".into(),
            protocol: protocol.clone(),
            // The endpoint as it will be stored.
            base_url: super::provider::effective_endpoint(&protocol, base_url.as_deref()),
            api_key: key,
            models: Vec::new(),
            source_path: ctx.os_home.join(".claude").join("settings.json"),
            scope: AssetScope::User,
            blocked: None,
        }]
    }
```

（`claude.rs` 顶部加 `use super::provider::{ApiKeyRef, ProviderAccountDraft};`）

> **共享的技能助手在 `mod.rs` 里**（Task 1 就建好了）：`SkillTruth` 与
> `skill_rows` 三个来源都要用，所以它们在父模块，Claude 这边用
> `super::skill_rows(…)` 拿；`SkillTruth` 由 `scan()` 加载一次传进来
> （`detect` 的第二个参数），**不要**在来源里自己 `SkillTruth::load(ctx)`。
> `text.rs` 也在 Task 1 有了最小版，本 Task 把它扩成完整版。

```rust
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        // Codex layers its config: the project's `.codex/config.toml` wins over
        // the user's `~/.codex/config.toml`. So the two are **merged by id**,
        // project over user — otherwise the same provider id appears twice and
        // the second one is a conflict instead of an override.
        let merged = merge_codex_configs(&[
            (
                ctx.os_home.join(".codex").join("config.toml"),
                super::report::AssetScope::User,
            ),
            (
                ctx.project.join(".codex").join("config.toml"),
                super::report::AssetScope::Project,
            ),
        ]);

        let mut drafts: Vec<ProviderAccountDraft> = merged
            .providers
            .into_iter()
            .map(|(name, (provider, source_path, scope))| {
                let base_url = provider
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                // `env_key` is the NAME of the variable to read, never a secret.
                let key = provider
                    .get("env_key")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                    .map(|name| ApiKeyRef::EnvVar(name.to_string()));
                // Codex says its own default: its `WireApi` defaults to
                // Responses (`codex-rs/model-provider-info/src/lib.rs:117`).
                let protocol = super::provider::protocol_for_wire(
                    provider.get("wire_api").and_then(|v| v.as_str()),
                    "openai-responses",
                );
                // **Nothing is dropped here.** Every provider table the
                // source declares becomes a row — a definition that cannot
                // be imported is `Blocked` with the reason, not silence.
                //
                // Normalizing can turn a valid definition into an
                // empty-looking one — a provider that uses the Responses
                // default endpoint has its endpoint stored as `None` — so
                // the decision is made on the RAW definition, never on the
                // normalized one.
                //
                // A name that cannot become an account id at all is a real
                // problem the person should see, not something to paper
                // over with the raw name.
                let id = super::provider::account_id_for(&name);
                // A wire protocol we do not speak is refused before the
                // write (`create_account` would silently downgrade it).
                let blocked = if id.is_none() {
                    Some(super::text::provider_name_unusable(&name))
                } else if super::is_unsupported_protocol(&protocol) {
                    Some(super::text::provider_protocol_unsupported(&protocol))
                } else if base_url.is_none() && key.is_none() {
                    // No endpoint and no credential: there is nothing to
                    // connect to. It is still a definition the person wrote,
                    // so it is reported — as blocked.
                    Some(super::text::provider_nothing_to_connect_to())
                } else {
                    None
                };
                ProviderAccountDraft {
                    // The id the account will get, not the raw name. When the
                    // name cannot become an id, the raw name is kept ONLY so
                    // the row can name the definition — `blocked` says it
                    // cannot be imported.
                    id: id.clone().unwrap_or_else(|| name.clone()),
                    // The name the source wrote it under — the table's key.
                    source_name: name.clone(),
                    protocol: protocol.clone(),
                    // The endpoint as it will be stored.
                    base_url: super::provider::effective_endpoint(
                        &protocol,
                        base_url.as_deref(),
                    ),
                    api_key: key,
                    source_path: source_path.clone(),
                    // The model belongs to the provider that was actually
                    // selected, not to whichever came first in the table.
                    models: if merged.model_provider.as_deref() == Some(name.as_str()) {
                        merged.model.iter().cloned().collect()
                    } else {
                        Vec::new()
                    },
                    // The scope the config was read at: a provider from the
                    // project's config is not the person's own provider.
                    scope,
                    blocked,
                }
            })
            .collect();

        // A model with NO named provider needs no special handling here: the
        // per-draft `models` above already gives the model to the provider
        // Codex actually selected, and to nobody else.
        //
        // When `model_provider` is absent, Codex was on its built-in `openai`
        // provider (`codex-rs/core/src/config/mod.rs:4133` falls back to
        // `"openai"`). That provider has no entry in `model_providers` and is
        // not ours to import — so the model is left unattached rather than
        // pinned onto "the first account", which would put it on an account
        // the person never selected.
        drafts
    }
```

**合并的实现**（放进 `codex.rs`）：

```rust
/// Codex's layered config, merged the way Codex itself layers it: the
/// project's file wins over the user's, key by key.
struct MergedCodexConfig {
    /// Provider id -> the provider's table, the file it was read from, and
    /// the scope it was read at — so the report can show which file to look
    /// at, and a project provider is not reported as the person's own.
    providers: std::collections::BTreeMap<
        String,
        (toml::Value, std::path::PathBuf, super::report::AssetScope),
    >,
    /// The provider the person selected, if the config names one.
    model_provider: Option<String>,
    /// The model the person was using.
    model: Option<String>,
}

/// Merge the config files in order — later files win.
///
/// Each file comes with the scope it was read at, so a provider that came
/// from the project's config is not reported as the person's own.
///
/// **Tables are merged key by key, not replaced.** Codex merges its config
/// layers recursively (`codex-rs/config/src/merge.rs:59`, `merge_toml_values`
/// walks into nested tables), so a project file that overrides only
/// `base_url` under a provider id keeps the `env_key` and `wire_api` the user
/// file set. Replacing the whole table would drop the credential — the very
/// thing the person most needs to keep.
fn merge_codex_configs(
    files: &[(
        std::path::PathBuf,
        super::report::AssetScope,
    )],
) -> MergedCodexConfig {
    // The type is spelled out: `and_modify` below needs to know what the
    // value is.
    let mut providers: std::collections::BTreeMap<
        String,
        (toml::Value, std::path::PathBuf, super::report::AssetScope),
    > = std::collections::BTreeMap::new();
    let mut model_provider = None;
    let mut model = None;
    for (path, scope) in files {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = raw.parse::<toml::Value>() else {
            continue;
        };
        if let Some(table) = value.get("model_providers").and_then(|p| p.as_table()) {
            for (name, provider) in table {
                providers
                    .entry(name.clone())
                    .and_modify(|existing| {
                        merge_table(&mut existing.0, provider);
                        // The overlay file is now the one that decides this
                        // provider's values — so it is also the file the report
                        // should point at.
                        existing.1 = path.clone();
                        existing.2 = *scope;
                    })
                    .or_insert_with(|| (provider.clone(), path.clone(), *scope));
            }
        }
        if let Some(selected) = value.get("model_provider").and_then(|v| v.as_str()) {
            model_provider = Some(selected.to_string());
        }
        if let Some(name) = value.get("model").and_then(|v| v.as_str()) {
            model = Some(name.to_string());
        }
    }
    MergedCodexConfig {
        providers,
        model_provider,
        model,
    }
}

/// Merge `overlay` into `base`, key by key, the way Codex merges its layers.
///
/// A scalar in the overlay replaces the base's; a table in the overlay is
/// merged into the base's table rather than replacing it.
fn merge_table(base: &mut toml::Value, overlay: &toml::Value) {
    // Reborrow, so `base` is still usable in the `else` arm below — matching
    // on `(base, overlay)` would MOVE the reference and leave it unusable.
    let (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) =
        (&mut *base, overlay)
    else {
        // Not a table: the overlay wins outright.
        *base = overlay.clone();
        return;
    };
    for (key, value) in overlay_table {
        match base_table.get_mut(key) {
            Some(existing) => merge_table(existing, value),
            None => {
                base_table.insert(key.clone(), value.clone());
            }
        }
    }
}
```

（`codex.rs` 顶部加 `use super::provider::{ApiKeyRef, ProviderAccountDraft};`）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-capabilities --features migration`
Expected: PASS —— 全部绿。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-config/src/provider_book.rs crates/atomcode-capabilities/src/migration/
git commit -m "feat(capabilities): map external provider accounts to drafts

A credential that names an environment variable stays a name, so the
secret never lands in our files. A literal secret is marked as one, so the
host can ask about that single item before writing it."
```

---

## Task 8: 第二期 —— 写入 `config.toml`（逐项确认）

**Files:**
- Modify: `crates/atomcode-cli/src/import_cmd.rs`
- Modify: `crates/atomcode-cli/src/main.rs`
- Modify: `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs`

- [ ] **Step 1: 写失败的测试**

在 `import_cmd.rs` 的测试区加：

```rust
    #[test]
    fn a_draft_with_an_env_named_key_is_written_as_a_reference() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: Some(atomcode_capabilities::migration::ApiKeyRef::EnvVar(
                "GW_TOKEN".into(),
            )),
            models: vec!["gpt-x".into()],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        };
        let written =
            super::write_account(&config, &draft, true).expect("the account is written");
        let text = std::fs::read_to_string(&config).unwrap();
        // The reference form, not the bare name: a bare name falls through to
        // `resolved_api_key`'s literal case and would be used AS the key.
        assert!(text.contains("${GW_TOKEN}"), "{text}");
        assert!(
            !text.contains("sk-"),
            "no secret is written: {text}"
        );
        assert_eq!(written, "gateway");
    }

    #[test]
    fn a_literal_that_would_be_expanded_is_refused() {
        // `resolved_api_key` expands any `$` in the value, so a key with a `$`
        // in it would mean something else at run time.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-$ecret".into(),
            )),
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        };
        let error = super::write_account(&config, &draft, true)
            .expect_err("a key with a dollar sign cannot be written");
        assert!(error.contains('$'), "{error}");
        assert!(
            !config.exists(),
            "nothing is written when the key cannot be written unambiguously"
        );
    }

    #[test]
    fn a_plaintext_key_is_not_written_without_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-secret".into(),
            )),
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        };
        let error =
            super::write_account(&config, &draft, false).expect_err("a secret needs a yes");
        assert!(error.contains("confirm"), "{error}");
        assert!(
            !config.exists(),
            "nothing is written when the confirmation is missing"
        );
    }

    #[test]
    fn an_existing_account_id_is_a_conflict_and_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[provider_accounts.gateway]\nprovider = \"openai-compatible\"\n",
        )
        .unwrap();
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: None,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        };
        let error =
            super::write_account(&config, &draft, true).expect_err("the id is taken");
        assert!(error.contains("gateway"), "{error}");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(
            !text.contains("gw.invalid"),
            "the existing account is untouched: {text}"
        );
    }

    /// A source that hands back fixed drafts, so the `--apply` flow can be
    /// driven without a real `~/.claude`.
    struct FixedDrafts(Vec<atomcode_capabilities::migration::ProviderAccountDraft>);

    impl atomcode_capabilities::migration::MigrationSource for FixedDrafts {
        fn id(&self) -> &'static str {
            "fixed"
        }
        fn display_name(&self) -> &'static str {
            "Fixed drafts"
        }
        fn detect(
            &self,
            _ctx: &atomcode_capabilities::migration::ScanContext,
            _truth: &atomcode_capabilities::migration::SkillTruth,
        ) -> Vec<atomcode_capabilities::migration::AssetReport> {
            Vec::new()
        }
        fn provider_drafts(
            &self,
            _ctx: &atomcode_capabilities::migration::ScanContext,
        ) -> Vec<atomcode_capabilities::migration::ProviderAccountDraft> {
            self.0.clone()
        }
    }

    fn draft(
        id: &str,
        key: Option<atomcode_capabilities::migration::ApiKeyRef>,
    ) -> atomcode_capabilities::migration::ProviderAccountDraft {
        atomcode_capabilities::migration::ProviderAccountDraft {
            id: id.into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: key,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        }
        source_name: id.into(),
    }

    fn ctx_for(dir: &std::path::Path) -> atomcode_capabilities::migration::ScanContext {
        atomcode_capabilities::migration::ScanContext {
            os_home: dir.to_path_buf(),
            project: dir.to_path_buf(),
            product: atomcode_capabilities::ProductDirs::new(dir.join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        }
    }

    fn sources(
        drafts: Vec<atomcode_capabilities::migration::ProviderAccountDraft>,
    ) -> Vec<Box<dyn atomcode_capabilities::migration::MigrationSource>> {
        vec![Box::new(FixedDrafts(drafts))]
    }

    #[test]
    fn yes_does_not_authorize_writing_a_plaintext_secret() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let sources = sources(vec![draft(
            "gateway",
            Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-secret".into(),
            )),
        )]);
        // `--yes`, but no terminal to ask in: the secret is not written.
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 1, "the account that holds a secret was left out");
        assert!(
            !config.exists(),
            "the config file was not written at all"
        );
    }

    #[test]
    fn an_account_that_only_names_a_variable_is_written_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let sources = sources(vec![draft(
            "gateway",
            Some(atomcode_capabilities::migration::ApiKeyRef::EnvVar(
                "GW_TOKEN".into(),
            )),
        )]);
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 0, "nothing was left out");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("${GW_TOKEN}"), "{text}");
        assert!(!text.contains("sk-"), "no secret was written: {text}");
    }

    #[test]
    fn a_batch_with_one_secret_writes_only_the_account_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let sources = sources(vec![
            draft(
                "with-secret",
                Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                    "sk-secret".into(),
                )),
            ),
            draft(
                "without-secret",
                Some(atomcode_capabilities::migration::ApiKeyRef::EnvVar(
                    "GW_TOKEN".into(),
                )),
            ),
        ]);
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 1, "the one that holds a secret was left out");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("without-secret"), "{text}");
        assert!(
            !text.contains("with-secret"),
            "the account that holds a secret is not written: {text}"
        );
        assert!(!text.contains("sk-secret"), "no secret was written: {text}");
    }

    #[test]
    fn a_config_with_no_eligible_account_is_left_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let before = b"[provider_accounts.existing]\nprovider = \"openai-compatible\"\n";
        std::fs::write(&config, before).unwrap();
        let sources = sources(vec![draft(
            "gateway",
            Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-secret".into(),
            )),
        )]);
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 1);
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "nothing was eligible, so the file is byte for byte what it was"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-cli import_cmd`
Expected: FAIL —— `write_account` 不存在。

- [ ] **Step 3: 实现写入**

`import_cmd.rs` 加：

```rust
use atomcode_config::provider_book::{AccountInput, BookError, ModelInput, ProviderBook};
```

**`ApiKeyRef` 与 `ProviderAccountDraft` 已经在 Task 5 的那一条 import 里** ——
不要在这里再写一条同名 import（同名 import 编译不过，E0252）。Task 5 那一条
import 因此是：

```rust
use atomcode_capabilities::migration::{
    self, ApiKeyRef, AssetKind, AssetStatus, MigrationReport, ProviderAccountDraft, ScanContext,
};
```

```rust

/// Why this credential cannot be written, if it cannot.
///
/// The rules are the writer's, and they are **stated here once** so the report
/// and the writer cannot disagree:
///
/// - a literal containing `$` would be **expanded** by `resolved_api_key`
///   (`config/provider.rs:360`), so the file would mean something else;
/// - a literal whose **trimmed** value names a live environment variable
///   would resolve to that variable instead (`provider.rs:365`), so the file
///   would mean something else.
///
/// The trimmed value is what matters, because that is what the writer stores
/// (`written_key`, `provider_book.rs:753`).
///
/// The **value is never printed** — the reason names the account, not the key.
fn unwriteable_credential(draft: &ProviderAccountDraft) -> Option<String> {
    let Some(ApiKeyRef::Plaintext(secret)) = &draft.api_key else {
        // A reference is always writable: the file holds a NAME.
        return None;
    };
    let stored = secret.trim();
    if stored.contains('$') {
        return Some(atomcode_i18n::product::t(
            atomcode_i18n::product::Msg::MigrationCredentialHasDollar,
        )
        .into_owned());
    }
    if std::env::var(stored).is_ok() {
        return Some(atomcode_i18n::product::t(
            atomcode_i18n::product::Msg::MigrationCredentialAmbiguous,
        )
        .into_owned());
    }
    None
}

/// Write one provider account.
///
/// A literal secret needs `confirmed`; without it nothing is written at all —
/// not the account, not the models, not a partial file.
///
/// **How the credential is written.**
///
/// `resolved_api_key` (`config/provider.rs:356`) has three cases:
/// a value containing `$` is expanded as an environment reference; a value
/// whose NAME matches a live environment variable resolves to that variable;
/// anything else is the key itself. So the same string can mean two things,
/// and the written form has to pick one:
///
/// - A **reference** is written `${NAME}`. Expansion is unambiguous: an unset
///   variable expands to nothing, so the account ends up with NO key rather
///   than with the key "NAME". The bare name would instead fall through to
///   the literal case and be used as the key.
/// - A **literal** is written as itself, but only when it cannot be read as
///   something else: no `$` in it (the config would expand it), and no live
///   environment variable of the same name (that would win over the
///   literal). A literal that fails either test is refused rather than
///   written, because the file would then mean two things at once.
pub fn write_account(
    config_path: &Path,
    draft: &ProviderAccountDraft,
    confirmed: bool,
) -> Result<String, String> {
    // An owned string, because the reference form has to be BUILT rather than
    // borrowed from the draft.
    let key: Option<String> = match &draft.api_key {
        None => None,
        // `${NAME}` is the only unambiguous way to write a reference:
        // `resolved_api_key` (`config/provider.rs:360`) expands a value
        // containing `$`, and an unset variable expands to nothing — so the
        // string "NAME" is never used AS the key. Writing the bare name
        // would instead fall through to the literal branch
        // (`provider.rs:370`) and use the string "NAME" as the key.
        //
        // It does NOT mean "no key": if the expansion comes out empty,
        // `resolved_api_key` keeps looking and finds a platform credential
        // (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `ATOMCODE_API_KEY`,
        // `provider.rs:375-396`). So the migrated endpoint can end up
        // authenticated with a credential that is not the one this account
        // was configured for. `${NAME}` is still the right form — it is the
        // only one that does not put the NAME in as a key — but the report
        // must not promise "no key" when the variable is unset.
        Some(ApiKeyRef::EnvVar(name)) => Some(format!("${{{name}}}")),
        Some(ApiKeyRef::Plaintext(secret)) => {
            if !confirmed {
                return Err(
                    "this account has a plaintext key — confirm it to write it".into(),
                );
            }
            // A literal that could be read as something else is refused
            // rather than written, because the file would then mean two
            // things: `resolved_api_key` expands any `$` in it, and a live
            // environment variable of the same name wins over the literal.
            if secret.trim().contains('$') {
                return Err(format!(
                    "{}: this key contains `$`, which the config would expand — set it by hand \
                     instead",
                    draft.id
                ));
            }
            // The error names the ACCOUNT, never the secret: the message is
            // printed to the terminal and into the report, and a secret in
            // it is a secret in the scrollback.
            //
            // The check is "is the variable SET", not "is it set and
            // non-empty": `resolved_api_key` looks the name up, and a lookup
            // that succeeds with an empty value falls THROUGH to the
            // platform fallbacks (`provider.rs:375`) instead of treating the
            // configured string as the literal it is. So an empty variable of
            // the same name still makes the value ambiguous.
            //
            // The check uses the **trimmed** value, because that is what gets
            // stored: `written_key` trims it (`provider_book.rs:753`). A value
            // of `" GW_TOKEN "` would be stored as `GW_TOKEN` — so checking
            // the padded string would miss that `GW_TOKEN` is a live
            // variable, and the file would end up holding a reference where a
            // literal was meant.
            let stored = secret.trim();
            if std::env::var(stored).is_ok() {
                return Err(format!(
                    "{}: an environment variable of the same name is set, so this key cannot be \
                     written unambiguously — rename the variable or set the key by hand",
                    draft.id
                ));
            }
            Some(stored.to_string())
        }
    };
    let base_url = draft.base_url.as_deref().unwrap_or_default();
    if base_url.is_empty() && draft.protocol == "openai-compatible" {
        return Err(format!("{}: no endpoint was found to import", draft.id));
    }
    let models: Vec<ModelInput<'_>> = draft
        .models
        .iter()
        .map(|model| ModelInput {
            account: "",
            model,
            id: None,
            display_name: None,
            window: None,
            max_tokens: None,
            vision: None,
            effort: None,
            levels: None,
            key: None,
            default: false,
        })
        .collect();
    let book = ProviderBook::new(config_path.to_path_buf());
    let input = AccountInput {
        headers: None,
        name: &draft.id,
        protocol: &draft.protocol,
        display_name: None,
        endpoint: base_url,
        key: key.as_deref(),
        exact_id: true,
    };
    // A draft that cannot be imported is refused **before** the write.
    // The report already says so; the writer must not try anyway.
    if let Some(reason) = &draft.blocked {
        return Err(format!("{}: {reason}", draft.id));
    }
    // An unsupported wire protocol is refused **before** the write.
    // `create_account` resolves an unknown preset to `openai-compatible`
    // (`config/provider_preset.rs:381`), so the import would otherwise
    // succeed — against the wrong wire format, and with the file now
    // claiming a protocol it does not speak.
    if atomcode_capabilities::migration::is_unsupported_protocol(&draft.protocol) {
        return Err(format!(
            "{}: this endpoint speaks a protocol we do not support ({}) — not imported",
            draft.id, draft.protocol
        ));
    }
    match book.create_account(&input, &models) {
        Ok((id, _)) => Ok(id),
        Err(BookError::IdTaken(name)) => {
            Err(format!("{name}: an account with that id already exists — not overwritten"))
        }
        // `BookError` has no `Display` impl (each screen words its own), so it is
        // rendered by shape rather than by `to_string()`.
        Err(other) => Err(format!("{other:?}")),
    }
}
```

- [ ] **Step 4: 实现 `--apply` 流程**

`import_cmd.rs` 加：

```rust
/// Import the provider accounts an external agent has configured.
///
/// **What `yes` means.** It answers "yes, import these accounts" — it does
/// **not** authorize writing a secret the person has not seen. An account
/// whose credential is a literal is always asked about, one at a time, even
/// with `--yes`: the question is about the secret, not about the import.
///
/// Without a terminal there is nobody to ask, so an account that needs an
/// answer is **not written at all** — and if that is the only kind of
/// account there is, nothing is written and the config file stays byte for
/// byte as it was.
///
/// Returns the text to show and how many items were left out. A non-zero
/// count means the caller must not report success.
pub fn apply_provider_accounts(
    sources: &[Box<dyn atomcode_capabilities::migration::MigrationSource>],
    ctx: &ScanContext,
    yes: bool,
    config_path: &Path,
    interactive: bool,
) -> Result<(String, usize), String> {
    use atomcode_i18n::product::{t, Msg};
    let mut drafts: Vec<ProviderAccountDraft> = Vec::new();
    // **A source that could not be read is reported, not swallowed.**
    //
    // "No provider accounts" and "I could not read your provider config" are
    // different answers, and the second one must not exit successfully: a
    // script would read the exit code as "nothing to import" and move on.
    let mut unreadable: Vec<String> = Vec::new();
    for source in sources {
        drafts.extend(source.provider_drafts(ctx));
        if let Some(reason) = source.provider_drafts_error(ctx) {
            unreadable.push(format!("{}: {reason}", source.display_name()));
        }
    }
    // **The credential is preflighted HERE, in the host** — before the
    // re-check and before the write.
    //
    // Why the host and not the capability: the check is "is there an
    // environment variable of this name right now", and that is a question
    // about the PROCESS, not about the file. The capability is a read-only
    // view of the disk; it must not ask the environment anything.
    //
    // Why before the re-check: a literal with a `$` in it, or one whose
    // trimmed value names a live environment variable, is a credential the
    // writer will REFUSE. If the re-check ran first, it could call such an
    // account "already imported" — and the report would advertise an account
    // that cannot be written.
    for draft in &mut drafts {
        if let Some(reason) = unwriteable_credential(draft) {
            draft.blocked = Some(reason);
        }
    }
    if drafts.is_empty() && unreadable.is_empty() {
        return Ok((t(Msg::MigrationImportNone).into_owned(), 0));
    }
    if drafts.is_empty() {
        // Something could not be read, and there is nothing else to say.
        return Err(t(Msg::MigrationProvidersUnreadable {
            reasons: &unreadable.join("; "),
        })
        .into_owned());
    }
    }
    // What is already in OUR configuration. The re-check and the apply share
    // this: an account that is already there is a **successful no-op**, not a
    // conflict — otherwise a second run would try to write it again, get
    // "the id is taken", and exit non-zero for something that is already done.
    let existing = atomcode_config::provider_book::ProviderBook::new(config_path.to_path_buf())
        .load();
    let mut written = Vec::new();
    let mut already = Vec::new();
    let mut skipped = Vec::new();
    for draft in &drafts {
        // A draft we REFUSE to import is refused FIRST — before the
        // existing-account check. Otherwise a hand-made account that happens
        // to match would be reported as "already there", and the real reason
        // (we cannot import this) would never be said.
        if let Some(reason) = &draft.blocked {
            skipped.push(format!("{} ({reason})", draft.id));
            continue;
        }
        if let Some(account) = existing.provider_accounts.get(&draft.id) {
            if same_account(account, &existing.models, draft) {
                already.push(draft.id.clone());
                continue;
            }
            // Same id, different account: a real conflict, not a no-op.
            skipped.push(format!(
                "{} ({})",
                draft.id,
                t(Msg::MigrationImportConflict { id: &draft.id })
            ));
            continue;
        }
        // A literal secret is always the person's own decision — `--yes` does
        // not reach it. An account that only names a variable holds no secret,
        // so `--yes` answers for it; without `--yes` it is asked too.
        let holds_a_secret = matches!(draft.api_key, Some(ApiKeyRef::Plaintext(_)));
        let confirmed = if holds_a_secret {
            if !interactive {
                skipped.push(draft.id.clone());
                continue;
            }
            ask_to_confirm(draft, config_path)?
        } else if yes {
            true
        } else {
            if !interactive {
                skipped.push(draft.id.clone());
                continue;
            }
            ask_to_confirm(draft, config_path)?
        };
        if !confirmed {
            skipped.push(draft.id.clone());
            continue;
        }
        match write_account(config_path, draft, confirmed) {
            Ok(id) => written.push(id),
            Err(error) => skipped.push(format!("{} ({error})", draft.id)),
        }
    }
    let mut out = String::new();
    if !written.is_empty() {
        out.push_str(&t(Msg::MigrationImportDone {
            imported: &written.join(", "),
        }));
        out.push('\n');
    }
    if !already.is_empty() {
        out.push_str(&t(Msg::MigrationImportAlreadyThere {
            already: &already.join(", "),
        }));
        out.push('\n');
    }
    if !skipped.is_empty() {
        out.push_str(&t(Msg::MigrationImportSkipped {
            skipped: &skipped.join(", "),
        }));
        out.push('\n');
    }
    Ok((out, skipped.len()))
}

/// Ask about one account, showing what would be written.
///
/// The question names the account, the endpoint, the protocol and the models,
/// and says that the key is a literal found in the other agent's config — so
/// the answer is about a specific thing, not a blanket yes.
fn ask_to_confirm(draft: &ProviderAccountDraft, config_path: &Path) -> Result<bool, String> {
    use atomcode_i18n::product::{t, Msg};
    use std::io::Write;
    print!(
        "{}",
        t(Msg::MigrationImportConfirm {
            id: &draft.id,
            protocol: &draft.protocol,
            // The endpoint IN USE — a protocol's own default is shown, not a
            // dash, because that is the endpoint this account will talk to.
            endpoint: &atomcode_capabilities::migration::endpoint_in_use(
                &draft.protocol,
                draft.base_url.as_deref(),
            )
            .unwrap_or_else(|| "-".into()),
            models: &draft.models.join(", "),
            // What will happen to the credential. For a literal it says the
            // secret will be COPIED INTO the file, and names the file — the
            // person is being asked to agree to that, not to "import the key
            // found somewhere".
            credential: &match &draft.api_key {
                Some(ApiKeyRef::Plaintext(_)) => "plaintext".into(),
                Some(ApiKeyRef::EnvVar(name)) => format!("env:{name}"),
                None => "none".into(),
            },
            destination: &config_path.display().to_string(),
        })
    );
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read the answer: {e}"))?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
```

- [ ] **Step 5: 接上 CLI**

`main.rs` 的 `run_import_command` 改成：

```rust
async fn run_import_command(
    json: bool,
    apply: bool,
    yes: bool,
    cli_config: Option<&std::path::Path>,
) -> i32 {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let project = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let dirs = atomcode_coding::config::product_dirs_from_env();
    // The configuration we are importing INTO — it also says where our
    // skills live (`[skills].extra_dirs`).
    let config = atomcode_config::config::Config::load(&cli_config_path(cli_config))
        .unwrap_or_default();
    // The skill roots the RUNTIME uses — the user's configured `extra_dirs`
    // come first, and a skill there can win a same-name collision. Resolving
    // them here (rather than letting the capability fall back to the
    // standard list) is what makes the report agree with the runtime.
    let skill_dirs = atomcode_coding::parts::resolve_skill_dirs_from(
        &config.skills.extra_dirs,
        &project,
        &dirs,
        home.as_deref(),
        None,
    )
    .0;
    let ctx =
        let (opencode_config, opencode_data) = atomcode_coding::config::opencode_dirs();
        atomcode::import_cmd::scan_context(
            &home,
            &project,
            dirs.clone(),
            skill_dirs,
            opencode_config,
            opencode_data,
        );
    let sources = atomcode_capabilities::migration::builtin_sources();
    let mut report = atomcode_capabilities::migration::scan(&sources, &ctx);

    // The re-check: what is already in OUR configuration is not "to be
    // brought over" — it is already there.
    let drafts: Vec<_> = sources
        .iter()
        .flat_map(|source| source.provider_drafts(&ctx))
        .collect();
    let config_path = cli_config_path(cli_config);
    atomcode::import_cmd::mark_already_imported(&mut report, &drafts, &config_path);

    if !apply {
        println!("{}", atomcode::import_cmd::render(&report, json));
        return 0;
    }

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    match atomcode::import_cmd::apply_provider_accounts(
        &sources,
        &ctx,
        yes,
        &config_path,
        interactive,
    ) {
        // The second number is how many items were left out. Something left
        // out is not a success: a script that reads the exit code must not be
        // told everything went in when it did not.
        Ok((text, skipped)) => {
            println!("{text}");
            if skipped > 0 {
                1
            } else {
                0
            }
        }
        Err(error) => {
            eprintln!("import: {error}");
            1
        }
    }
}
```

> **退出码**：有东西没导入 → 非 0。Task 5 的占位实现与这里的最终实现**签名一致**（都返回 `Result<(String, usize), String>`），Task 8 只换实现、不改签名，所以 Task 5 的调用方不用改。

> `cli_config_path()` 已在 Task 5 定义（同一个 `main.rs`），**不要在这里再定义一遍**。它接受 `--config` 传下来的路径，所以验收时可以把配置写到临时目录。

- [ ] **Step 6: 加文案**

`product/messages.rs` 加：

```rust
    MigrationImportDone {
        imported: &'a str,
    },
    MigrationImportSkipped {
        skipped: &'a str,
    },
    MigrationImportNone,
    MigrationProvidersUnreadable {
        reasons: &'a str,
    },
    MigrationImportAlreadyThere {
        already: &'a str,
    },
    MigrationImportConflict {
        id: &'a str,
    },
    MigrationImportConfirm {
        id: &'a str,
        protocol: &'a str,
        endpoint: &'a str,
        models: &'a str,
        /// What will happen to the credential: `plaintext`, `env:NAME`, or
        /// `none`. The prompt must say which — the person is agreeing to
        /// copy a literal secret into a file, not to "import a key".
        credential: &'a str,
        /// The file the account will be written into.
        destination: &'a str,
    },
```

`product/en.rs` 加：

```rust
        Msg::MigrationImportDone { imported } => format!("Imported: {imported}.").into(),
        Msg::MigrationImportSkipped { skipped } => format!("Not imported: {skipped}.").into(),
        Msg::MigrationImportNone => "No provider accounts were found to import.".into(),
        Msg::MigrationProvidersUnreadable { reasons } => {
            format!("The provider configuration could not be read: {reasons}").into()
        }
        Msg::MigrationImportAlreadyThere { already } => {
            format!("Already there: {already}.").into()
        }
        Msg::MigrationImportConflict { id } => {
            format!("{id}: an account with that id exists but differs — not overwritten").into()
        }
        Msg::MigrationImportConfirm {
            id,
            protocol,
            endpoint,
            models,
            credential,
            destination,
        } => {
            let models = if models.is_empty() {
                String::new()
            } else {
                format!(" with models {models}")
            };
            // **The credential's fate is spelled out.** For a literal it
            // says the secret is COPIED INTO the file, and names the file.
            let credential = match credential.strip_prefix("plaintext") {
                Some(_) => format!(
                    " and copy its PLAINTEXT KEY into {destination}"
                ),
                None => match credential.strip_prefix("env:") {
                    Some(name) => {
                        format!(" and use the environment variable {name} (the secret stays put)")
                    }
                    None => String::new(),
                },
            };
            format!(
                "Import account `{id}` ({protocol} -> {endpoint}){models}{credential}? [y/N] "
            )
            .into()
        }
```

`product/zh_cn.rs` 加：

```rust
        Msg::MigrationImportDone { imported } => format!("已导入：{imported}。").into(),
        Msg::MigrationImportSkipped { skipped } => format!("未导入：{skipped}。").into(),
        Msg::MigrationImportNone => "没有发现可导入的 provider 账号。".into(),
        Msg::MigrationProvidersUnreadable { reasons } => {
            format!("无法读取 provider 配置：{reasons}").into()
        }
        Msg::MigrationImportAlreadyThere { already } => {
            format!("已经在配置里：{already}。").into()
        }
        Msg::MigrationImportConflict { id } => {
            format!("{id}：已存在同名但不同的账号 —— 没有覆盖。").into()
        }
        Msg::MigrationImportConfirm {
            id,
            protocol,
            endpoint,
            models,
            credential,
            destination,
        } => {
            let models = if models.is_empty() {
                String::new()
            } else {
                format!("，模型 {models}")
            };
            // **凭据的下场要说清楚。** 明文密钥要说明「会被**复制进**这个文件」，
            // 并写出是哪个文件。
            let credential = match credential.strip_prefix("plaintext") {
                Some(_) => format!("，并把其中的**明文密钥**复制进 {destination}"),
                None => match credential.strip_prefix("env:") {
                    Some(name) => {
                        format!("，并使用环境变量 {name}（密钥留在原地）")
                    }
                    None => String::new(),
                },
            };
            format!(
                "要导入账号 `{id}`（{protocol} -> {endpoint}）{models}{credential}吗？[y/N] "
            )
            .into()
        }
```

> **`.into()` 不能漏**：这些 arm 返回 `Cow<'static, str>`，`format!` 给出的是 `String`，所以要 `.into()` 才匹配得上（既有 arm 也是这么写的，见 `product/en.rs:1044`）。

- [ ] **Step 7: 跑测试**

Run: `cargo nextest run -p atomcode-cli import_cmd`
Expected: PASS —— 本 Task 定义的判据全部通过（**以实际条数为准**）。

- [ ] **Step 8: 提交**

```bash
git add crates/atomcode-cli/src/import_cmd.rs crates/atomcode-cli/src/main.rs crates/atomcode-i18n/src/product/
git commit -m "feat(cli): import provider accounts one item at a time

A credential that names an environment variable is written as that name, so
no secret reaches our files. A literal secret is written only after the
person confirms that one account, and an id that is taken is a conflict
rather than an overwrite."
```

---

## Task 9: 门与验收

**Files:** 无新增文件；只跑门。

- [ ] **Step 1: 编译门**

Run: `bash gates/compile.sh`
Expected: 退出 0。

- [ ] **Step 2: 受影响 crate 的测试**

Run: `cargo nextest run -p atomcode-capabilities --features migration`
Expected: 全绿。

Run: `cargo nextest run -p atomcode-config`
Expected: 全绿 —— **Task 7 改了 `provider_book.rs`**（公开了两个归一规则的包装），所以 config crate 也在受影响范围内，它的测试也要跑。

Run: `cargo nextest run -p atomcode-cli`
Expected: 全绿。

Run: `cargo nextest run -p atomcode-tui`
Expected: 全绿。

- [ ] **Step 3: 分层与门禁**

Run: `bash gates/layers.sh`
Expected: 通过。

Run: `bash gates/product-dir.sh`
Expected: 通过（本次没有在库里写 `.atomcode` 字面量；`distribution.rs` 是唯一允许处）。

Run: `bash gates/tui-i18n.sh`
Expected: 通过（本次没有在 `atomcode-tui/src` 或 `atomcode-cli/src` 写中文字面量）。

Run: `bash gates/tui-layers.sh`
Expected: 通过。

- [ ] **Step 4: 格式**

Run: `cargo fmt --all`
Run: `cargo fmt --all -- --check`
Expected: 退出 0。

- [ ] **Step 5: 手工验收（本机）**

**先编出要验的那个二进制** —— 这一步在**每一条手工命令之前**，因为
`PATH` 上的 `atomcode` 可能是装好的旧版，而编译门与 nextest 都不保证
`target/debug/atomcode` 是刚链出来的（它们可能只跑了 check，或者命中了旧的
增量产物）。手工验收要看的，就是**这个构建**的行为。

```bash
cargo build -p atomcode-cli
BIN="$(pwd)/target/debug/atomcode"
```

**只读的那两条**（用刚编出来的 `$BIN`）：

```bash
"$BIN" import
"$BIN" import --json | head -40
```

Expected: 报告列出本机 `~/.claude` 与 `~/.codex` 的资产；项目级 `CLAUDE.md`（本仓库没有）与 `AGENTS.md`（本仓库有）状态正确；`~/.claude/CLAUDE.md` 标 `can move`；`~/.claude/skills/*` 标 `in effect`。

> **不是每个 `~/.claude/skills/*` 都是 `in effect`** —— 同名的另一个目录会覆盖它，
> 解析失败的会被加载器拒绝（`registry.rs:262`）。这两种都仍然是 `Blocked`，
> 那是**正确**的，不是缺陷。

**OpenCode 也要看**（第三个来源，本期新增）—— 它的目录由宿主按 XDG 解析，
所以先确认宿主解析到了哪里：

```bash
# OpenCode 的两个目录（宿主解析，capability 不读环境）
"$BIN" import --json | grep -i opencode
```

Expected: 若本机装了 OpenCode，报告里有 `opencode` 这一组；`~/.config/opencode/skills/*`
标 `can move`（**我们不读那棵树**），`~/.local/share/opencode/auth.json` 标
`blocked`（明文密钥，只报不搬）。

> **`.agents/commands` 与 `.agents/skills` 的状态不同** —— 前者标 `can move`
> （注册表的标准目录表里**没有** `.agents/commands`，`registry.rs:585` 只有
> `.agents/skills`），后者标 `in effect`。两者都出现在报告里，但含义相反：
> 把前者当成「已经在生效」，用户就会以为自己的共享命令已经在跑了。

若本机**没装** OpenCode：报告里**不该**有 `opencode` 这一组 —— 「没发现」
不是错误（§3.7 的失败语义）。

**不**用真实的 `~/.claude` 跑 `--apply`：那会把**本机真实的密钥**写进文件。而且只改 `ATOMCODE_HOME` 并不隔离**源** —— 扫描的是 OS 家目录，与 `ATOMCODE_HOME` 无关。

所以 `--apply` 的验收用**人工夹具**：一个临时的源目录 + 一个临时的目标配置，两个都不碰真实家目录。

**在临时目录里跑**，这样当前仓库的 `.codex/config.toml`（本仓库有 `AGENTS.md`，
也可能有自己的配置）不会混进「隔离」的夹具里：

```bash
# 1. 造一个假的源：一个只有变量名（没有明文密钥）的 provider
src=$(mktemp -d)
mkdir -p "$src/.codex"
cat > "$src/.codex/config.toml" <<'TOML'
[model_providers.gateway]
base_url = "https://gw.invalid/v1"
env_key = "ATOMCODE_TEST_GW_TOKEN"
TOML

# 2. 造一个假的目标配置
dst=$(mktemp -d)
printf '[provider_accounts.existing]\nprovider = "openai-compatible"\n' > "$dst/config.toml"

# 3. 在一个**空的项目目录**里跑，HOME 指过去，配置指向临时文件
#    注意：`--config` 是**顶层**参数，不带 `global = true`（`main.rs:646`），
#    所以它必须写在子命令**前面**。
work=$(mktemp -d)
( cd "$work" && HOME="$src" "$BIN" --config "$dst/config.toml" import --apply --yes )

# 4. 看结果：只有变量名，没有密钥值
cat "$dst/config.toml"
```

Expected:
- 退出码 0。
- `$dst/config.toml` 里有 `[provider_accounts.gateway]`，`api_key` 是 `${ATOMCODE_TEST_GW_TOKEN}`（**变量引用**，不是密钥值）。
- 原有的 `[provider_accounts.existing]` **还在** —— 导入是追加，不是覆盖。

**再跑一次同一条命令**（重复导入 = 幂等）：Expected: 退出码仍为 0，输出说
`Already there`（**不是**报错），配置**逐字节不变**：

```bash
# **用 `cp` 存一份，不用 `$(cat …)`** —— 命令替换会**去掉结尾的换行**，
# 于是「文件没变」也会被 `diff` 报成变了。
cp "$dst/config.toml" "$dst/config.before"
( cd "$work" && HOME="$src" "$BIN" --config "$dst/config.toml" import --apply --yes )
cmp "$dst/config.before" "$dst/config.toml" && echo "unchanged"
```

**再单独跑一次只读报告**（这才是回检那一态）：Expected: `gateway` 标为
`already imported`，而不是 `can be brought over`。

```bash
( cd "$work" && HOME="$src" "$BIN" --config "$dst/config.toml" import )
```

> **为什么报告与 `--apply` 的输出不一样**：重复 `--apply` 说的是「这个账号已经
> 在配置里了」（因为它**已经写进去了**）；只读报告说的是「这项资产已经导入」
> （因为**回检**发现我们的配置里已有等价的账号）。两件事，措辞不同是对的。

- [ ] **Step 6: 提交门的结果（若有格式改动）**

**只暂存本任务改过的文件**，不要用 `git add -A` —— 工作区里有他人的改动：

```bash
git add crates/atomcode-capabilities/Cargo.toml crates/atomcode-capabilities/src/lib.rs crates/atomcode-capabilities/src/migration/ crates/atomcode-cli/Cargo.toml crates/atomcode-cli/src/import_cmd.rs crates/atomcode-cli/src/lib.rs crates/atomcode-cli/src/main.rs crates/atomcode-cli/src/tui_migration.rs crates/atomcode-tui/src/commands.rs crates/atomcode-tui/src/lib.rs crates/atomcode-tui/src/launch.rs crates/atomcode-tui/src/migration.rs crates/atomcode-tui/src/plugin.rs crates/atomcode-tui/src/rows.rs crates/atomcode-i18n/src/product/ crates/atomcode-i18n/src/screen/
git commit -m "style: rustfmt"
```

---

## 交付说明（写给验收的人）

- **行为变化**：新增 `atomcode import`（只读报告）、`atomcode import --json`、`atomcode import --apply`、TUI `/import`。
- **风险**：第二期会写 `config.toml`。已封住三条：密钥走环境变量名、密钥需逐项确认、id 冲突不覆盖。
- **已验证**：Task 9 的每条命令与其输出。
- **未验证**：真实 Claude/Codex 会话文件的转换（本期不做）；`~/.claude.json` 里 `projects.*.mcpServers` 的逐项目解析（本期只取顶层 `mcpServers`）。
