# 一键迁移其他 AI Coding Agent 的资产 —— 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让从 Claude Code / Codex 迁来的用户，用一条命令看清「我的资产 AtomCode 已经能用哪些、哪些能带走、哪些带不走」，并能把 provider 账号逐项确认后搬进 `config.toml`。

**Architecture:** 新增 `atomcode-capabilities::migration` 模块（中立、只读、feature `migration`、**不依赖 `atomcode-config`**），把外部 Agent 的资产分成 `Active` / `Imported` / `Actionable` / `Blocked` 四态产出结构化报告；文案走 `atomcode-i18n` 的 product 表。CLI 加 `atomcode import` 子命令（扫描 + `--apply` 写入），TUI 加 `/import` 命令，通过一个镜像 `/setup` 的端口 trait 向宿主要数据。

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
| P3 CLI | 4 | `atomcode import` 只读报告 | P2 | 文本与 JSON 两种形态判据全绿 |
| P4 TUI | 5 | `/import` 面板 | P3 | 屏幕判据全绿；`gates/tui-i18n.sh` 通过 |
| P5 映射 | 6 | provider 中立草案 | P2 | 凭据语义判据全绿（Claude=明文 / Codex=变量名） |
| P6 写入 | 7 | `--apply` 逐项确认写入 | P5 | 不确认不写、id 冲突不覆盖、非交互拒绝 |
| P7 验收 | 8 | 全部门 | P1–P6 | 见 Task 8 |

**依赖顺序是硬的**：P4 依赖 P3（屏幕显示的正是 CLI 那份报告），P6 依赖 P5（写入的对象正是草案），P7 依赖全部。

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
| 我们的 provider 账号 | `~/.atomcode/config.toml` | `ProviderBook` | **只有** `ProviderBook::create_account`（Task 7） |
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
| `crates/atomcode-capabilities/src/migration/provider.rs` | 第二期：外部 provider 配置 → 中立 `ProviderAccountDraft`（**不写盘**） |
| `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs` | 报告文案（product 表，三处同改） |
| `crates/atomcode-cli/src/import_cmd.rs` | `atomcode import` 子命令：扫描报告 / `--json` / `--apply` |
| `crates/atomcode-tui/src/migration.rs` | TUI 端口 trait + 视图类型（镜像 `setup.rs`） |
| `crates/atomcode-tui/src/commands.rs` | `/import` 命令集（新增 `ImportCommands`） |
| `crates/atomcode-cli/src/tui_migration.rs` | 端口实现：调 capabilities 扫描，转成 TUI 视图类型 |

**分层约束（`gates/layers.sh` 守）：** `migration` feature **不得**引 `atomcode-config`（用不到：报告只需要 `ProductDirs`，而它就在 capabilities 本 crate 里；config 是**写入**那一侧，只归宿主）；`atomcode-tui` **不得**引 `atomcode-coding` / `atomcode`（门禁的「UI 不认 Host,也不认 Product」）；`atomcode-i18n` 必须零 atomcode 依赖（「词表是叶子」）。

---

## Task 1: capabilities 的 migration 模块骨架

**Files:**
- Create: `crates/atomcode-capabilities/src/migration/mod.rs`
- Create: `crates/atomcode-capabilities/src/migration/report.rs`
- Modify: `crates/atomcode-capabilities/Cargo.toml`
- Modify: `crates/atomcode-capabilities/src/lib.rs`

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
                SourceReport {
                    source_id: "codex",
                    display_name: "Codex",
                    assets: vec![],
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
            os_home: std::path::PathBuf::from("/home/u"),
            project: std::path::PathBuf::from("/work"),
            product: crate::ProductDirs::new("/home/u/.ours", ".ours"),
        };
        let report = scan(&[], &ctx);
        assert!(report.sources.is_empty());
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
# Deliberately does NOT pull `atomcode-config`: the report needs the product's
# trees, and `ProductDirs` already lives in THIS crate (`product_dirs.rs`) —
# the host passes it in. Keeping the edge out means this module stays usable
# without the config leaf.
migration = ["tools", "dep:atomcode-i18n", "dep:toml"]
```

> **`tools` 是为了复用真实验证器**，不是为了拿工具能力：判定「这个权限文件我们的加载器会不会拒绝」要用**同一个**规则解析器
> （`crate::tools::permission_rules::PermissionRules::parse`，
> `tools/permission_rules.rs:370`）。自己再写一份校验，迟早与真加载器不一致 ——
> 而「说生效、其实不生效」正是这个报告最不能有的错。
>
> `tools` 已在 default features 里（`Cargo.toml`：`default = ["provider", "tools"]`），
> CLI 与 TUI 本来就有，**不新增传递依赖**；而且 `tools` **不引 `atomcode-config`**，
> 所以 capabilities→config 的边仍然没有。

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
//! Three statuses, not two. "Compatible" and "needs conversion" cannot say the
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
//! report with one of three statuses.
//!
//! Opt-in `migration` feature. See
//! `docs/plans/2026-11-17-external-agent-asset-migration-design.md`.

mod claude;
mod codex;
mod provider;
pub mod report;

use std::path::PathBuf;

pub use provider::{ApiKeyRef, ProviderAccountDraft};
pub use report::{
    AssetKind, AssetReport, AssetScope, AssetStatus, MigrationReport, SourceReport,
};

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
}

/// One external agent's assets.
pub trait MigrationSource: Send + Sync {
    /// A stable id, e.g. `"claude-code"`.
    fn id(&self) -> &'static str;
    /// The name a person knows this agent by.
    fn display_name(&self) -> &'static str;
    /// Look, and report. Must not write anything.
    fn detect(&self, ctx: &ScanContext) -> Vec<AssetReport>;
    /// The provider accounts this agent has configured, if any.
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        Vec::new()
    }
}

/// The agents we know how to read.
pub fn builtin_sources() -> Vec<Box<dyn MigrationSource>> {
    vec![
        Box::new(claude::ClaudeSource),
        Box::new(codex::CodexSource),
    ]
}

/// Scan every source. A source that finds nothing contributes no rows.
///
/// **Provider rows come from the drafts, not from the scan.** The re-check
/// compares a row against what is in OUR config by account id, so the row's
/// name has to BE that id. A row named "provider" (Claude) or "config.toml"
/// (Codex) could never match, and the re-check would never fire — so the
/// provider rows are added here, from the same drafts the importer uses.
pub fn scan(sources: &[Box<dyn MigrationSource>], ctx: &ScanContext) -> MigrationReport {
    let mut report = MigrationReport::default();
    for source in sources {
        let assets = source.detect(ctx);
        let drafts = source.provider_drafts(ctx);
        if assets.is_empty() && drafts.is_empty() {
            continue;
        }
        let mut rows = assets;
        // One row per provider draft. The row's NAME is the account id — the
        // same id the importer will write — so the re-check can match it.
        for draft in drafts {
            rows.push(AssetReport {
                kind: AssetKind::ProviderConfig,
                name: draft.id.clone(),
                path: draft.source_path.clone(),
                scope: AssetScope::User,
                status: AssetStatus::Actionable,
                detail: super::text::provider_copyable(
                    draft.base_url.as_deref(),
                    draft.api_key.as_ref().map(|k| k.kind()),
                ),
            });
        }
        report.sources.push(SourceReport {
            source_id: source.id(),
            display_name: source.display_name(),
            assets: rows,
        });
    }
    report
}
```

（`claude.rs` / `codex.rs` / `provider.rs` 在 Task 2 / 3 / 6 建；本 Task 先建三个空壳文件让编译通过：`claude.rs` 与 `codex.rs` 各自放一个 `pub struct ClaudeSource;` 之类，Task 2/3 填内容；`provider.rs` 在 Task 6 建。**为让本 Task 编译通过，Step 5 之后先建两个空壳：**）

`crates/atomcode-capabilities/src/migration/claude.rs`：

```rust
//! Claude Code assets. Filled in by Task 2.

use super::{MigrationSource, ScanContext};
use super::report::AssetReport;

pub struct ClaudeSource;

impl MigrationSource for ClaudeSource {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn detect(&self, _ctx: &ScanContext) -> Vec<AssetReport> {
        Vec::new()
    }
}
```

`crates/atomcode-capabilities/src/migration/codex.rs`：

```rust
//! Codex assets. Filled in by Task 3.

use super::report::AssetReport;
use super::{MigrationSource, ScanContext};

pub struct CodexSource;

impl MigrationSource for CodexSource {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    fn detect(&self, _ctx: &ScanContext) -> Vec<AssetReport> {
        Vec::new()
    }
}
```

`crates/atomcode-capabilities/src/migration/provider.rs`：

```rust
//! Provider accounts found in another agent's configuration. Filled in by Task 6.

/// How a credential is referenced.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// A provider account an external agent has configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountDraft {
    /// The id the account would get.
    pub id: String,
    /// A preset or protocol id (`openai-compatible`, `anthropic-compatible`,
    /// `openai-responses`).
    pub protocol: String,
    /// The endpoint, when the source named one.
    pub base_url: Option<String>,
    /// The credential, when the source named one.
    pub api_key: Option<ApiKeyRef>,
    /// The models the source was using.
    pub models: Vec<String>,
    /// Where this account was configured — the file it was read from. The
    /// report shows it, so a person can see WHICH file to look at.
    pub source_path: std::path::PathBuf,
}
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-capabilities --features migration`
Expected: PASS —— 3 tests。

- [ ] **Step 7: 提交**

```bash
git add crates/atomcode-capabilities/Cargo.toml crates/atomcode-capabilities/src/lib.rs crates/atomcode-capabilities/src/migration/
git commit -m "feat(capabilities): add migration module skeleton

The report carries three statuses, not two: Active (we read it today),
Actionable (there is a known way to bring it over), Blocked (it cannot
come over). \"Compatible\" and \"needs conversion\" cannot say whether a
lost session history is recoverable, and that is the thing a person needs
to know before deciding.

The feature does not pull atomcode-config, so the screen may use it."
```

---

## Task 2: Claude Code 来源

**Files:**
- Modify: `crates/atomcode-capabilities/src/migration/claude.rs`
- Test: `crates/atomcode-capabilities/src/migration/claude.rs`（内联 `mod tests`）

- [ ] **Step 1: 写失败的测试**

在 `claude.rs` 末尾追加：

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
        }
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
        let mcp: Vec<_> = assets.iter().filter(|a| a.kind == AssetKind::McpServers).collect();
        assert_eq!(mcp.len(), 2);
        assert!(mcp.iter().any(|m| m.scope == AssetScope::Project
            && m.status == AssetStatus::Active));
        assert!(mcp.iter().any(|m| m.scope == AssetScope::User
            && m.status == AssetStatus::Actionable));
    }

    #[test]
    fn a_provider_in_the_user_settings_is_reported() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &home.path().join(".claude").join("settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://example.invalid","ANTHROPIC_API_KEY":"sk-x"}}"#,
        );

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
        let provider = assets
            .iter()
            .find(|a| a.kind == AssetKind::ProviderConfig)
            .expect("the provider is reported");
        assert_eq!(provider.status, AssetStatus::Actionable);
    }

    #[test]
    fn an_actionable_row_names_the_path_to_copy_it_to() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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

        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
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
        let assets = ClaudeSource.detect(&ctx(home.path(), project.path()));
        assert!(assets.is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo nextest run -p atomcode-capabilities --features migration migration::claude`
Expected: FAIL —— 找不到资产（`detect` 返回空）。

- [ ] **Step 3: 实现 ClaudeSource**

把 `claude.rs` 整体替换为：

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
use super::{MigrationSource, ScanContext};
use std::path::{Path, PathBuf};

pub struct ClaudeSource;

impl MigrationSource for ClaudeSource {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn detect(&self, ctx: &ScanContext) -> Vec<AssetReport> {
        let mut out = Vec::new();
        let user = ctx.os_home.join(".claude");
        let project_claude = ctx.project.join(".claude");

        // Instructions. The project one is read as-is — but only if it is the
        // one that WINS: the project tier takes the first existing name in
        // order (`.atomcode.md`, `ATOMCODE.md`, `AGENTS.md`, `CLAUDE.md`,
        // `claude.md` — `instructions.rs:17`), so a `CLAUDE.md` next to an
        // `AGENTS.md` is never loaded. Reporting it as "already in effect"
        // would be a lie, so a shadowed file says so instead.
        let project_instructions = ctx.project.join("CLAUDE.md");
        if project_instructions.is_file() {
            let shadowed_by = higher_precedence_instruction(&ctx.project);
            let (status, detail) = match shadowed_by {
                Some(other) => (
                    AssetStatus::Blocked,
                    super::text::instructions_shadowed(&other),
                ),
                // An EMPTY file loads as `Empty`, not `Loaded`
                // (`instructions.rs:173`), so nothing is applied from it.
                None if is_empty_file(&project_instructions) => (
                    AssetStatus::Blocked,
                    super::text::instructions_empty(),
                ),
                None => (AssetStatus::Active, super::text::instructions_active()),
            };
            out.push(AssetReport {
                kind: AssetKind::Instructions,
                name: "CLAUDE.md".into(),
                path: project_instructions,
                scope: AssetScope::Project,
                status,
                detail,
            });
        }
        let user_instructions = user.join("CLAUDE.md");
        if user_instructions.is_file() {
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

        // Skills and commands: the standard paths are in our own scan list.
        for (kind, dir) in [
            (AssetKind::Skills, "skills"),
            (AssetKind::Commands, "commands"),
        ] {
            for (scope, root) in [
                (AssetScope::User, user.join(dir)),
                (AssetScope::Project, project_claude.join(dir)),
            ] {
                out.extend(skill_rows(kind, scope, &root));
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
                if !path.is_file() {
                    continue;
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
        if project_mcp.is_file() {
            match mcp_server_count(&project_mcp) {
                // A server with neither `command` nor `url` fails the WHOLE
                // file (`mcp/config.rs:670-673`), so none of its servers run.
                Ok(count) if count > 0 => {
                    match mcp_servers_are_valid(&project_mcp) {
                        Ok(true) => out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: "mcpServers".into(),
                            path: project_mcp,
                            scope: AssetScope::Project,
                            status: AssetStatus::Active,
                            detail: super::text::mcp_active(count),
                        }),
                        Ok(false) => out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: ".mcp.json".into(),
                            path: project_mcp,
                            scope: AssetScope::Project,
                            status: AssetStatus::Blocked,
                            detail: super::text::rules_rejected(),
                        }),
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
        if global_state.is_file() {
            match mcp_server_count(&global_state) {
                Ok(count) if count > 0 => out.push(AssetReport {
                    kind: AssetKind::McpServers,
                    name: "mcpServers".into(),
                    path: global_state,
                    scope: AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: super::text::mcp_copyable(
                        count,
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
        if user_settings.is_file() {
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

        // The provider row is NOT added here: `scan()` adds one row per
        // provider draft, so the row's name is the account id the importer
        // will write. A row named "provider" could never be matched by the
        // re-check.

        // Subagent definitions: our role files need frontmatter Claude's
        // subagent files do not have, so they cannot come over.
        //
        // A directory that exists but cannot be read is a `Blocked` row, not
        // silence — "we found nothing" and "we could not look" are different
        // answers, and only one of them is true.
        let agents = user.join("agents");
        if agents.is_dir() {
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

/// One row per skill directory (`<name>/SKILL.md`) or command file (`<name>.md`).
///
/// A directory that does not exist is simply empty. A directory that exists
/// but cannot be read is NOT empty — it is a problem the person should see,
/// so it becomes a `Blocked` row rather than silence.
fn skill_rows(kind: AssetKind, scope: AssetScope, root: &Path) -> Vec<AssetReport> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            out.push(AssetReport {
                kind,
                name: root.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                path: root.to_path_buf(),
                scope,
                status: AssetStatus::Blocked,
                detail: super::text::unreadable_file(&error.to_string()),
            });
            return out;
        }
    };
    // An entry that cannot be read is a row, not a silent skip: a skill we
    // could not look at is one we could not promise works.
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                out.push(AssetReport {
                    kind,
                    name: root.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    path: root.to_path_buf(),
                    scope,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&error.to_string()),
                });
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_skill = path.is_dir() && path.join("SKILL.md").is_file();
        let is_command = path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md");
        if !is_skill && !is_command {
            continue;
        }
        out.push(AssetReport {
            kind,
            name,
            path,
            scope,
            status: AssetStatus::Active,
            detail: super::text::skill_active(),
        });
    }
    out
}

/// Whether a file has no content to apply.
///
/// An empty instruction file loads as `Empty`, not `Loaded`
/// (`instructions.rs:173`), so it contributes nothing — "already in effect"
/// would overstate it.
fn is_empty_file(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(body) => body.trim().is_empty(),
        // Unreadable is not empty; the caller reports it as unreadable.
        Err(_) => false,
    }
}

/// The instruction file that wins over `CLAUDE.md` in the project tier, if any.
///
/// The project tier takes the **first existing** name in order
/// (`.atomcode.md`, `ATOMCODE.md`, `AGENTS.md`, `CLAUDE.md`, `claude.md` —
/// `instructions.rs:17`), and only that one is loaded. So a `CLAUDE.md` that
/// sits next to an `AGENTS.md` is never read, and the report has to say so
/// rather than call it "already in effect".
fn higher_precedence_instruction(project: &Path) -> Option<String> {
    for name in [".atomcode.md", "ATOMCODE.md", "AGENTS.md"] {
        if project.join(name).is_file() {
            return Some(name.to_string());
        }
    }
    None
}

/// How many `mcpServers` a JSON file declares.
///
/// `Ok(0)` means the file parsed and declares none. `Err` means the file could
/// not be read or parsed — which is **not** the same as "there are none", and
/// the report says so instead of quietly showing nothing.
fn mcp_server_count(path: &Path) -> Result<usize, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    // Our loader strips comments before parsing (`mcp/config.rs:624`:
    // `serde_json::from_str(&crate::jsonc::strip_comments(content))`), because
    // these files are hand-edited. Parsing them WITHOUT stripping would call
    // a perfectly good file broken — and report it as `Blocked`.
    let value: serde_json::Value = serde_json::from_str(&crate::jsonc::strip_comments(&raw))
        .map_err(|e| e.to_string())?;
    Ok(value
        .get("mcpServers")
        .and_then(|s| s.as_object())
        .map(|o| o.len())
        .unwrap_or(0))
}

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

/// Whether our own loader would ACCEPT this MCP file.
///
/// A server with neither `command` nor `url` fails the whole file
/// (`mcp/config.rs:670-673`), so none of its servers would run.
fn mcp_servers_are_valid(path: &Path) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&crate::jsonc::strip_comments(&raw))
        .map_err(|e| e.to_string())?;
    let Some(servers) = value.get("mcpServers").and_then(|s| s.as_object()) else {
        return Ok(true);
    };
    for (name, entry) in servers {
        let Some(entry) = entry.as_object() else {
            return Ok(false);
        };
        let has_command = entry.get("command").and_then(|v| v.as_str()).is_some();
        let has_url = entry.get("url").and_then(|v| v.as_str()).is_some();
        if !has_command && !has_url {
            // "MCP server '{name}' must have either 'command' (stdio) or
            // 'url' (http)" — the loader's own message.
            return Ok(false);
        }
    }
    Ok(true)
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
    let mut stack = vec![projects.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                count += 1;
            }
        }
    }
    Ok(count)
}
```

**导入**：`use std::path::Path;`（只用 `Path`，不用 `PathBuf`）。

**注意：** 上面引用的 `super::text::*` 是 Task 3 建立的文案辅助模块。为了让 Task 2 单独可编译，**先在 Task 2 里建 `text.rs` 的最小版本**：

`crates/atomcode-capabilities/src/migration/text.rs`：

```rust
//! The sentences a report shows.
//!
//! One place, so the CLI and the screen cannot say the same thing two ways.
//! Every function here has an entry in the product message table.

use atomcode_i18n::product::{t, Msg};

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
pub fn skill_active() -> String {
    t(Msg::MigrationSkillActive).into_owned()
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
pub fn unreadable_file(reason: &str) -> String {
    t(Msg::MigrationUnreadableFile { reason }).into_owned()
}
pub fn rules_rejected() -> String {
    t(Msg::MigrationRulesRejected).into_owned()
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
```

> `provider_copyable` 放在**这里**而不是 Task 3：Task 2 的 Claude 扫描已经要报
> provider 那一行，而 Task 2 必须能**单独编译并通过**（阶段 P2 的验收就是
> `cargo nextest run -p atomcode-capabilities --features migration` 全绿）。

并在 `mod.rs` 加 `mod text;`（**注意：Task 2 就要加**，因为 `text.rs` 是 `pub(crate)` 可见的辅助模块；它不导出到公共 API）。

- [ ] **Step 4: 在 i18n product 表加这些条目**

`crates/atomcode-i18n/src/product/messages.rs` 的 `pub enum Msg<'a>` 末尾追加：

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
    MigrationSkillActive,
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
    MigrationUnreadableFile {
        reason: &'a str,
    },
    MigrationRulesRejected,
    MigrationHooksCopyable {
        target: &'a str,
    },
    MigrationProviderCopyable {
        base_url: &'a str,
        key: &'a str,
    },
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
        Msg::MigrationSkillActive => "Read directly — already in the skill catalog".into(),
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
        Msg::MigrationUnreadableFile { reason } => {
            format!("This file could not be read: {reason}").into()
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
        Msg::MigrationSkillActive => "直接读取 —— 已在技能目录里".into(),
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
        Msg::MigrationUnreadableFile { reason } => {
            format!("这个文件无法读取：{reason}").into()
        }
        Msg::MigrationRulesRejected => "我们的加载器会拒绝这个文件，因此其中规则都不生效".into(),
        Msg::MigrationHooksCopyable { target } => {
            format!("可复制到 {target}").into()
        }
        Msg::MigrationProviderCopyable { base_url, key } => {
            format!("其中的 provider 设置可以导入（endpoint {base_url}，凭据 {key}）—— 写入前请先确认")
                .into()
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
Expected: PASS —— 7 tests。

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
        }
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

        let assets = CodexSource.detect(&ctx(home.path(), project.path()));
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

        let assets = CodexSource.detect(&ctx(home.path(), project.path()));
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

        let assets = CodexSource.detect(&ctx(home.path(), project.path()));
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

        let assets = CodexSource.detect(&ctx(home.path(), project.path()));
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
        let assets = CodexSource.detect(&ctx(home.path(), project.path()));
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
use super::{MigrationSource, ScanContext};
use std::path::{Path, PathBuf};

pub struct CodexSource;

impl MigrationSource for CodexSource {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn detect(&self, ctx: &ScanContext) -> Vec<AssetReport> {
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

        // Project-level instructions: the open standard, so we read them.
        let project_agents = ctx.project.join("AGENTS.md");
        if project_agents.is_file() {
            out.push(AssetReport {
                kind: AssetKind::Instructions,
                name: "AGENTS.md".into(),
                path: project_agents,
                scope: AssetScope::Project,
                status: AssetStatus::Active,
                detail: super::text::instructions_active(),
            });
        }

        // Skills.
        for (scope, root) in [
            (AssetScope::User, user.join("skills")),
            (AssetScope::Project, project_codex.join("skills")),
        ] {
            let mut names: Vec<String> = Vec::new();
            if root.is_dir() {
                match std::fs::read_dir(&root) {
                    Ok(entries) => {
                        for entry in entries {
                            let entry = match entry {
                                Ok(entry) => entry,
                                Err(error) => {
                                    out.push(AssetReport {
                                        kind: AssetKind::Skills,
                                        name: "skills".into(),
                                        path: root.clone(),
                                        scope,
                                        status: AssetStatus::Blocked,
                                        detail: super::text::unreadable_file(&error.to_string()),
                                    });
                                    continue;
                                }
                            };
                            let path = entry.path();
                            let is_skill = path.is_dir() && path.join("SKILL.md").is_file();
                            if is_skill {
                                names.push(entry.file_name().to_string_lossy().into_owned());
                            }
                        }
                    }
                    Err(error) => out.push(AssetReport {
                        kind: AssetKind::Skills,
                        name: "skills".into(),
                        path: root.clone(),
                        scope,
                        status: AssetStatus::Blocked,
                        detail: super::text::unreadable_file(&error.to_string()),
                    }),
                }
            }
            names.sort();
            for name in names {
                out.push(AssetReport {
                    kind: AssetKind::Skills,
                    name: name.clone(),
                    path: root.join(&name),
                    scope,
                    status: AssetStatus::Actionable,
                    detail: super::text::skill_copyable(
                        &ctx.product.user().join("skills").join(&name),
                    ),
                });
            }
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
                    // The provider rows are added by `scan()` from the
                    // drafts — one row per account, named by its id.
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

/// How many `[mcp_servers.*]` tables the config declares.
fn mcp_server_count(value: &toml::Value) -> Option<usize> {
    let servers = value.get("mcp_servers")?.as_table()?;
    (!servers.is_empty()).then_some(servers.len())
}

/// How many session files there are under `sessions/`.
fn session_file_count(sessions: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    let mut stack = vec![sessions.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                count += 1;
            }
        }
    }
    Ok(count)
}
```

- [ ] **Step 4: 补文案**

`text.rs` 追加：

```rust
pub fn skill_copyable(target: &std::path::Path) -> String {
    t(Msg::MigrationSkillCopyable {
        target: &target.display().to_string(),
    })
    .into_owned()
}
pub fn unreadable_config() -> String {
    t(Msg::MigrationUnreadableConfig).into_owned()
}
```

`messages.rs` 追加：

```rust
    MigrationSkillCopyable {
        target: &'a str,
    },
    MigrationUnreadableConfig,
```

`en.rs` 追加：

```rust
        Msg::MigrationSkillCopyable { target } => {
            format!("Copy it to {target} to make it load").into()
        }
        Msg::MigrationUnreadableConfig => {
            "This config file could not be parsed, so nothing could be read from it".into()
        }
```

`zh_cn.rs` 追加：

```rust
        Msg::MigrationSkillCopyable { target } => {
            format!("复制到 {target} 后即可加载").into()
        },
        Msg::MigrationProviderCopyable { base_url, key } => {
            format!("其中的 provider 设置可以导入（endpoint {base_url}，凭据 {key}）—— 写入前请先确认")
                .into()
        }
        Msg::MigrationUnreadableConfig => "这个配置文件无法解析，因此其中内容都没读到".into(),
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo nextest run -p atomcode-capabilities --features migration`
Expected: PASS —— 12 tests（3 + 7 + 5 中的 claude/codex 部分；数字以实际为准，关键是全绿）。

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

## Task 4: CLI 子命令 `atomcode import`（扫描报告）

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
    self, AssetKind, AssetStatus, MigrationReport, ProviderAccountDraft, ScanContext,
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
/// Only the **non-secret** parts are compared — protocol, endpoint and the
/// model list. A credential is compared by KIND (a reference to a variable
/// against a reference to a variable), never by value: reading our own
/// credential to compare it would put a secret in memory for no reason, and
/// the file's credential may legitimately have been rotated since.
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
            let Some(draft) = drafts.iter().find(|d| d.id == asset.name) else {
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
/// Only the **non-secret** parts are compared — protocol, endpoint and the
/// model list. A credential is never compared by value: the file's credential
/// may legitimately have been rotated since, and reading it to compare it
/// would put a secret in memory for no reason.
///
/// **The protocol is compared exactly.** `openai-compatible` is a real preset
/// id, not a wildcard: an account speaking the Responses API is a different
/// account from one speaking chat/completions, even at the same endpoint.
fn same_account(
    account: &atomcode_config::config::ProviderAccountConfig,
    models: &std::collections::HashMap<String, atomcode_config::config::ModelProfileConfig>,
    draft: &ProviderAccountDraft,
) -> bool {
    let same_endpoint = account.base_url.as_deref() == draft.base_url.as_deref();
    let same_protocol = account.provider == draft.protocol;
    let same_models = draft.models.iter().all(|model| {
        models
            .values()
            .any(|m| m.model == *model && m.account == draft.id)
    });
    same_endpoint && same_protocol && same_models
}

/// Where to look. The host resolves these; the capability resolves nothing.
///
/// The product's trees come from the host too: the report needs to name a
/// target path, and only the host knows where our trees are.
pub fn scan_context(
    os_home: &Path,
    project: &Path,
    product: atomcode_capabilities::ProductDirs,
) -> ScanContext {
    ScanContext {
        os_home: os_home.to_path_buf(),
        project: project.to_path_buf(),
        product,
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
Expected: PASS —— 3 tests。

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
    let ctx = atomcode::import_cmd::scan_context(&home, &project, dirs.clone());
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

（`apply_provider_accounts` 的完整实现与**最终签名**在 Task 7。**Task 4 先写同签名的占位**让编译通过 —— 签名从一开始就定死，Task 7 只换实现不改签名：）

```rust
/// Import the provider accounts an external agent has configured.
///
/// Placeholder in Task 4; Task 7 fills it in. The signature is final.
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
git add crates/atomcode-cli/src/import_cmd.rs crates/atomcode-cli/src/main.rs crates/atomcode-cli/src/lib.rs crates/atomcode-cli/Cargo.toml
git commit -m "feat(cli): add atomcode import, a read-only asset report

Prints what another agent left behind and where each item stands: read
directly, can be brought over, or cannot. The JSON form carries the same
rows with stable status names."
```

---

## Task 5: TUI `/import` 命令

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
    pub status: String,
    pub detail: String,
}

/// Read the assets another agent left behind.
#[async_trait]
pub trait Migration: Send + Sync {
    /// The report. Read-only.
    async fn report(&self) -> Result<MigrationReportView, String>;
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo check -p atomcode-tui`
Expected: FAIL —— `migration` 模块未声明。

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
        match port.report().await {
            Ok(report) => Outcome::Said(render_import_report(&report)),
            Err(error) => Outcome::Refused(error),
        }
    }
}

/// The report as lines: one heading per source, one line per asset.
///
/// The empty case reuses the product table's sentence — the CLI says it too,
/// and a sentence this build already says is not written twice.
fn render_import_report(report: &crate::migration::MigrationReportView) -> String {
    use crate::i18n::product::{t as pt, Msg as PMsg};
    if report.sources.is_empty() {
        // `product::t` returns a `Cow<'static, str>`, so it needs `.into_owned()`
        // to become the `String` this returns.
        return pt(PMsg::MigrationReportEmpty).into_owned();
    }
    let mut out = String::new();
    for source in &report.sources {
        out.push_str(&source.display_name);
        out.push('\n');
        for asset in &source.assets {
            // The status word is part of what the row means, so it is shown —
            // the CLI shows it too, and the two must not differ.
            out.push_str(&format!(
                "  {} [{}] — {}\n",
                asset.name, asset.status, asset.detail
            ));
        }
    }
    out
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

`crates/atomcode-i18n/src/screen/messages.rs` 的 `pub enum Msg` 加：

```rust
    CmdAboutImport,
    NoMigrationPort,
```

`crates/atomcode-i18n/src/screen/en.rs` 加：

```rust
        Msg::CmdAboutImport => "Look at the assets another AI coding agent left behind".into(),
        Msg::NoMigrationPort => "No migration service is available".into(),
```

`crates/atomcode-i18n/src/screen/zh_cn.rs` 加：

```rust
        Msg::CmdAboutImport => "查看其他 AI 编码 Agent 留下的资产".into(),
        Msg::NoMigrationPort => "迁移服务不可用".into(),
```

- [ ] **Step 6: 写端口实现**

`crates/atomcode-cli/src/tui_migration.rs`（新建）：

> **别忘了声明模块**：`crates/atomcode-cli/src/lib.rs` 的模块区加 `pub mod tui_migration;`（只建文件不声明模块，代码不会进编译）。

```rust
//! The host side of the migration port: it scans the disk and hands the screen
//! a view it can lay out.

use async_trait::async_trait;
use atomcode_tui::migration::{
    Migration, MigrationAssetView, MigrationReportView, MigrationSourceView,
};
use std::path::PathBuf;

pub struct DiskMigration {
    home: PathBuf,
    project: PathBuf,
    product: atomcode_capabilities::ProductDirs,
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
    ) -> Self {
        Self {
            home,
            project,
            product,
            config_path,
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
        };
        let sources = atomcode_capabilities::migration::builtin_sources();
        let mut report = atomcode_capabilities::migration::scan(&sources, &ctx);
        // The same re-check the CLI does — the screen must not say "can be
        // brought over" about an account that is already in the config.
        let drafts: Vec<_> = sources
            .iter()
            .flat_map(|source| source.provider_drafts(&ctx))
            .collect();
        atomcode::import_cmd::mark_already_imported(&mut report, &drafts, &self.config_path);
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
                            status: format!("{:?}", asset.status).to_lowercase(),
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

`crates/atomcode-cli/src/lib.rs` 的 `launch::Ports { ... }` 加一项：

```rust
                migration: Some(Arc::new(crate::tui_migration::DiskMigration::new(
                    dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(".")),
                    working_dir.clone(),
                    dirs.clone(),
                    config_path.clone(),
                ))),
```

> 字段类型是 `Option<Arc<dyn Migration>>`，所以要 `Arc::new(...)` 包起来 —— 直接传 `DiskMigration` 类型对不上。

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
    fn the_import_report_lists_each_source_and_its_assets() {
        let report = crate::migration::MigrationReportView {
            sources: vec![crate::migration::MigrationSourceView {
                source_id: "claude-code".into(),
                display_name: "Claude Code".into(),
                assets: vec![crate::migration::MigrationAssetView {
                    name: "web-access".into(),
                    status: "active".into(),
                    detail: "Read directly".into(),
                }],
            }],
        };
        let text = super::render_import_report(&report);
        assert!(text.contains("Claude Code"), "{text}");
        assert!(text.contains("web-access"), "{text}");
    }

    #[test]
    fn an_empty_import_report_says_so() {
        let text = super::render_import_report(&crate::migration::MigrationReportView {
            sources: vec![],
        });
        assert!(!text.trim().is_empty());
    }
```

- [ ] **Step 9: 跑测试**

Run: `cargo nextest run -p atomcode-tui import`
Expected: PASS —— 2 tests。

Run: `cargo check -p atomcode-cli`
Expected: 退出 0。

- [ ] **Step 10: 提交**

```bash
git add crates/atomcode-tui/src/migration.rs crates/atomcode-tui/src/commands.rs crates/atomcode-tui/src/lib.rs crates/atomcode-tui/src/plugin.rs crates/atomcode-tui/src/launch.rs crates/atomcode-cli/src/tui_migration.rs crates/atomcode-cli/src/lib.rs crates/atomcode-i18n/src/screen/
git commit -m "feat(tui): add /import, another agent's assets on screen

The screen asks the host for the report and lays it out; the host owns the
disk. Same shape as /setup."
```

---

## Task 6: provider 映射（中立草案）

**Files:**
- Modify: `crates/atomcode-capabilities/src/migration/provider.rs`
- Modify: `crates/atomcode-capabilities/src/migration/claude.rs`
- Modify: `crates/atomcode-capabilities/src/migration/codex.rs`
- Modify: `crates/atomcode-capabilities/src/migration/mod.rs`

- [ ] **Step 1: 写失败的测试**

在 `provider.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // The tests call `provider_drafts`, a method on the trait — so the trait
    // has to be in scope, not just the types.
    use crate::migration::{MigrationSource, ScanContext};
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
        };
        let drafts = super::super::claude::ClaudeSource.provider_drafts(&ctx);
        assert_eq!(
            drafts[0].api_key,
            Some(ApiKeyRef::Plaintext("sk-secret-value".into())),
            "a literal secret is marked as one, so the host can ask before writing it"
        );
    }

    #[test]
    fn no_provider_configuration_means_no_drafts() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ScanContext {
            os_home: home.path().to_path_buf(),
            project: std::path::PathBuf::from("/work"),
            product: crate::ProductDirs::new(home.path().join(".ours"), ".ours"),
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

        let merged =
            super::super::codex::merge_codex_configs(&[user_config, project_config]);
        let (provider, _) = merged
            .providers
            .get("gateway")
            .expect("the provider is merged");
        assert_eq!(
            provider.get("base_url").and_then(|v| v.as_str()),
            Some("https://project.invalid/v1"),
            "the project's value wins"
        );
        assert_eq!(
            provider.get("env_key").and_then(|v| v.as_str()),
            Some("GW_TOKEN"),
            "the inherited credential is NOT dropped"
        );
        assert_eq!(
            provider.get("wire_api").and_then(|v| v.as_str()),
            Some("responses"),
            "the inherited protocol is NOT dropped"
        );
    }

    #[test]
    fn a_missing_wire_api_means_responses_not_chat_completions() {
        // Codex's `WireApi` has `#[default] Responses`
        // (`codex-rs/model-provider-info/src/lib.rs:117`).
        assert_eq!(
            super::protocol_for("codex", None),
            "openai-responses",
            "a config with no wire_api speaks Responses"
        );
        assert_eq!(super::protocol_for("codex", Some("responses")), "openai-responses");
        assert_eq!(super::protocol_for("codex", Some("chat")), "openai-compatible");
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

`provider.rs` 追加（放在 `ProviderAccountDraft` 之后）：

```rust
/// The protocol an external agent's endpoint speaks, mapped to ours.
///
/// Codex's `wire_api` names its own family; Claude's `ANTHROPIC_*` variables
/// name a vendor endpoint.
///
/// **A missing `wire_api` means Responses, not chat/completions.** Codex's own
/// `WireApi` has `#[default] Responses`
/// (`codex-rs/model-provider-info/src/lib.rs:117`), so a config that names a
/// provider but no protocol speaks Responses. Mapping that to
/// `openai-compatible` (chat/completions) would send the request to an
/// endpoint that does not speak it.
///
/// Claude's is the other way round: its `ANTHROPIC_*` variables name an
/// Anthropic endpoint whether or not a protocol is spelled out.
///
/// A protocol we do not know is **not** guessed at — it becomes
/// `unsupported:…` and the report says so, instead of the import failing
/// later against an endpoint that speaks something else.
pub fn protocol_for(source: &str, wire_api: Option<&str>) -> String {
    match wire_api {
        Some("responses") => "openai-responses".into(),
        Some("anthropic") => "anthropic-compatible".into(),
        Some("chat") | Some("openai") => "openai-compatible".into(),
        Some(other) => unsupported_protocol(source, other),
        None if source == "claude-code" => "anthropic-compatible".into(),
        None => "openai-responses".into(),
    }
}

/// The protocol id for a wire family we do not support.
///
/// The report says the endpoint speaks something we cannot, so the person
/// sees it instead of the import silently failing later.
pub fn unsupported_protocol(source: &str, wire_api: &str) -> String {
    format!("unsupported:{source}:{wire_api}")
}

/// Read Claude's `env` block out of its settings file.
pub fn claude_env(home: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = std::fs::read_to_string(home.join(".claude").join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("env")?.as_object().cloned()
}
```

`provider.rs` 顶部补 `use std::path::Path;`。

**为什么没有启发式**：两个来源的语义都是已知的，不需要靠字符串形状猜。

- Claude 的 `env` 块是 **名字 → 值**，值**就是**密钥本身 → 永远是 `Plaintext`。
- Codex 的 `env_key` 是 **变量的名字**，值在环境里 → 永远是 `EnvVar`。

一个「长得像变量名就当引用」的启发式会把 Claude 的 `ANTHROPIC_API_KEY=MY_KEY`（值恰好像名字）误判成引用，把一个真密钥当名字存进我们的文件 —— 那样运行时读不到密钥，而文件里还留着一个看似无害的字符串。**按来源分别处理，不猜。**

**Claude 的草案**（加进 `claude.rs` 的 `impl MigrationSource`）：

```rust
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        let Some(env) = super::provider::claude_env(&ctx.os_home) else {
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
        vec![ProviderAccountDraft {
            id: "claude".into(),
            protocol: super::provider::protocol_for("claude-code", None),
            base_url,
            api_key: key,
            models: Vec::new(),
            source_path: ctx.os_home.join(".claude").join("settings.json"),
        }]
    }
```

（`claude.rs` 顶部加 `use super::provider::{ApiKeyRef, ProviderAccountDraft};`）

**Codex 的草案**（加进 `codex.rs` 的 `impl MigrationSource`）：

```rust
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        // Codex layers its config: the project's `.codex/config.toml` wins over
        // the user's `~/.codex/config.toml`. So the two are **merged by id**,
        // project over user — otherwise the same provider id appears twice and
        // the second one is a conflict instead of an override.
        let merged = merge_codex_configs(&[
            ctx.os_home.join(".codex").join("config.toml"),
            ctx.project.join(".codex").join("config.toml"),
        ]);

        let mut drafts: Vec<ProviderAccountDraft> = merged
            .providers
            .into_iter()
            .map(|(name, (provider, source_path))| {
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
                ProviderAccountDraft {
                    id: name.clone(),
                    protocol: super::provider::protocol_for(
                        "codex",
                        provider.get("wire_api").and_then(|v| v.as_str()),
                    ),
                    base_url,
                    api_key: key,
                    source_path: source_path.clone(),
                    // The model belongs to the provider that was actually
                    // selected, not to whichever came first in the table.
                    models: if merged.model_provider.as_deref() == Some(name.as_str()) {
                        merged.model.iter().cloned().collect()
                    } else {
                        Vec::new()
                    },
                }
            })
            .filter(|draft| draft.base_url.is_some() || draft.api_key.is_some())
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
    /// Provider id -> the provider's table AND the file it was read from, so
    /// the report can show which file to look at.
    providers: std::collections::BTreeMap<String, (toml::Value, std::path::PathBuf)>,
    /// The provider the person selected, if the config names one.
    model_provider: Option<String>,
    /// The model the person was using.
    model: Option<String>,
}

/// Merge the config files in order — later files win.
///
/// **Tables are merged key by key, not replaced.** Codex merges its config
/// layers recursively (`codex-rs/config/src/merge.rs:59`, `merge_toml_values`
/// walks into nested tables), so a project file that overrides only
/// `base_url` under a provider id keeps the `env_key` and `wire_api` the user
/// file set. Replacing the whole table would drop the credential — the very
/// thing the person most needs to keep.
fn merge_codex_configs(paths: &[std::path::PathBuf]) -> MergedCodexConfig {
    let mut providers = std::collections::BTreeMap::new();
    let mut model_provider = None;
    let mut model = None;
    for path in paths {
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
                    .and_modify(|existing| merge_table(&mut existing.0, provider))
                    .or_insert_with(|| (provider.clone(), path.clone()));
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
    let (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) =
        (base, overlay)
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
git add crates/atomcode-capabilities/src/migration/
git commit -m "feat(capabilities): map external provider accounts to drafts

A credential that names an environment variable stays a name, so the
secret never lands in our files. A literal secret is marked as one, so the
host can ask about that single item before writing it."
```

---

## Task 7: 第二期 —— 写入 `config.toml`（逐项确认）

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
        }
    }

    fn ctx_for(dir: &std::path::Path) -> atomcode_capabilities::migration::ScanContext {
        atomcode_capabilities::migration::ScanContext {
            os_home: dir.to_path_buf(),
            project: dir.to_path_buf(),
            product: atomcode_capabilities::ProductDirs::new(dir.join(".ours"), ".ours"),
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
// `ProviderAccountDraft` 已经由 Task 4 的 import 引入了 —— **把它加进那一条**，
// 不要在这里再写一条同名 import（同名 import 编译不过）。
use atomcode_capabilities::migration::ApiKeyRef;
use atomcode_config::provider_book::{AccountInput, BookError, ModelInput, ProviderBook};
```

Task 4 那一条 import 因此变成：

```rust
use atomcode_capabilities::migration::{
    self, ApiKeyRef, AssetKind, AssetStatus, MigrationReport, ProviderAccountDraft, ScanContext,
};
```

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
            if secret.contains('$') {
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
            if std::env::var(secret).is_ok() {
                return Err(format!(
                    "{}: an environment variable of the same name is set, so this key cannot be \
                     written unambiguously — rename the variable or set the key by hand",
                    draft.id
                ));
            }
            Some(secret.clone())
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
    for source in sources {
        drafts.extend(source.provider_drafts(ctx));
    }
    if drafts.is_empty() {
        return Ok((t(Msg::MigrationImportNone).into_owned(), 0));
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
            ask_to_confirm(draft)?
        } else if yes {
            true
        } else {
            if !interactive {
                skipped.push(draft.id.clone());
                continue;
            }
            ask_to_confirm(draft)?
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
fn ask_to_confirm(draft: &ProviderAccountDraft) -> Result<bool, String> {
    use atomcode_i18n::product::{t, Msg};
    use std::io::Write;
    print!(
        "{}",
        t(Msg::MigrationImportConfirm {
            id: &draft.id,
            protocol: &draft.protocol,
            endpoint: draft.base_url.as_deref().unwrap_or("-"),
            models: &draft.models.join(", "),
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
    let ctx = atomcode::import_cmd::scan_context(&home, &project, dirs.clone());
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

> **退出码**：有东西没导入 → 非 0。Task 4 的占位实现与这里的最终实现**签名一致**（都返回 `Result<(String, usize), String>`），Task 7 只换实现、不改签名，所以 Task 4 的调用方不用改。

> `cli_config_path()` 已在 Task 4 定义（同一个 `main.rs`），**不要在这里再定义一遍**。它接受 `--config` 传下来的路径，所以验收时可以把配置写到临时目录。

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
    },
```

`product/en.rs` 加：

```rust
        Msg::MigrationImportDone { imported } => format!("Imported: {imported}.").into(),
        Msg::MigrationImportSkipped { skipped } => format!("Not imported: {skipped}.").into(),
        Msg::MigrationImportNone => "No provider accounts were found to import.".into(),
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
        } => {
            let models = if models.is_empty() {
                String::new()
            } else {
                format!(" with models {models}")
            };
            format!(
                "Import account `{id}` ({protocol} -> {endpoint}){models} with the key found in the \
                 other agent's config? [y/N] "
            )
            .into()
        }
```

`product/zh_cn.rs` 加：

```rust
        Msg::MigrationImportDone { imported } => format!("已导入：{imported}。").into(),
        Msg::MigrationImportSkipped { skipped } => format!("未导入：{skipped}。").into(),
        Msg::MigrationImportNone => "没有发现可导入的 provider 账号。".into(),
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
        } => {
            let models = if models.is_empty() {
                String::new()
            } else {
                format!("，模型 {models}")
            };
            format!(
                "要导入账号 `{id}`（{protocol} -> {endpoint}）{models}，并使用在另一个 Agent 的 \
                 配置里找到的密钥吗？[y/N] "
            )
            .into()
        }
```

> **`.into()` 不能漏**：这些 arm 返回 `Cow<'static, str>`，`format!` 给出的是 `String`，所以要 `.into()` 才匹配得上（既有 arm 也是这么写的，见 `product/en.rs:1044`）。

- [ ] **Step 7: 跑测试**

Run: `cargo nextest run -p atomcode-cli import_cmd`
Expected: PASS —— 6 tests。

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

## Task 8: 门与验收

**Files:** 无新增文件；只跑门。

- [ ] **Step 1: 编译门**

Run: `bash gates/compile.sh`
Expected: 退出 0。

- [ ] **Step 2: 受影响 crate 的测试**

Run: `cargo nextest run -p atomcode-capabilities --features migration`
Expected: 全绿。

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

```bash
atomcode import
atomcode import --json | head -40
```

Expected: 报告列出本机 `~/.claude` 与 `~/.codex` 的资产；项目级 `CLAUDE.md`（本仓库没有）与 `AGENTS.md`（本仓库有）状态正确；`~/.claude/CLAUDE.md` 标 `can move`；`~/.claude/skills/*` 标 `in effect`。

**不**用真实的 `~/.claude` 跑 `--apply`：那会把**本机真实的密钥**写进文件。而且只改 `ATOMCODE_HOME` 并不隔离**源** —— 扫描的是 OS 家目录，与 `ATOMCODE_HOME` 无关。

所以 `--apply` 的验收用**人工夹具**：一个临时的源目录 + 一个临时的目标配置，两个都不碰真实家目录。

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

# 3. 用 HOME 指过去跑（扫描读的是 OS 家目录）
#    注意：`--config` 是**顶层**参数，不带 `global = true`（`main.rs:646`），
#    所以它必须写在子命令**前面**。
HOME="$src" atomcode --config "$dst/config.toml" import --apply --yes

# 4. 看结果：只有变量名，没有密钥值
cat "$dst/config.toml"
```

Expected:
- 退出码 0。
- `$dst/config.toml` 里有 `[provider_accounts.gateway]`，`api_key` 是 `${ATOMCODE_TEST_GW_TOKEN}`（**变量引用**，不是密钥值）。
- 原有�� `[provider_accounts.existing]` **还在** —— 导入是追加，不是覆盖。

**再跑一次同一条命令**（回检）：Expected: `gateway` 标为 `already imported`，而不是 `can be brought over`；退出码仍为 0，配置**不再被改写**。

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
- **已验证**：Task 8 的每条命令与其输出。
- **未验证**：真实 Claude/Codex 会话文件的转换（本期不做）；`~/.claude.json` 里 `projects.*.mcpServers` 的逐项目解析（本期只取顶层 `mcpServers`）。
