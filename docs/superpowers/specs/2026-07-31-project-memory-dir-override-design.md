# Project-scope 记忆目录可配置(ATOMCODE_PROJECT_MEMORY_DIR)—— 设计文档

- 日期：2026-07-31
- 分支：release/v5.0.3
- 范围：`crates/atomcode-capabilities/src/memory/store.rs`（+ 配置文档一条）
- 类型：新增环境变量覆盖入口，默认行为完全不变、向后兼容

## 背景与问题(用户反馈)

memory 工具的 global scope 由 `MemoryStore::global()` 经 `super::config_dir()` 解析，可被 `ATOMCODE_HOME` 重定向；而 project scope 由 `MemoryStore::project()` **硬编码** `<working_dir>/.atomcode/memory.md`，**没有任何覆盖入口**。

以 atomcode 为内核、对外使用自有品牌/配置目录的宿主应用，因此两者不对称：global 口径可控，project 记忆只能落在 `.atomcode/`。结果宿主写侧无法与其对外宣称的配置目录对齐（只能在读取侧做兼容），排查"记忆写到哪了"时也易找错位置、误判为未持久化。

## 现状(已核对)

- `MemoryStore::new(path)` 本就以完整路径参数化；`global()` 走 `config_dir()`（尊重 `ATOMCODE_HOME`）；唯 `project(project_root)` 硬编码 `.atomcode`。
- **⚠️ 存在两份 `MemoryStore`，两份都在生产用、`project()` 都硬编码 `.atomcode`**（计划期 grep 发现，spec 初稿曾漏）：
  1. `atomcode-capabilities/src/memory/store.rs`（用户点的，L1 端口）：被 tuix `event_loop/commands.rs`（`/memory`/`/remember`/prompt 组装）、`atomcode-clix/src/code.rs`、`atomcode-capabilities/src/tools/memory.rs:40`（`remember` 工具）、以及 **prompt 注入 hook `memory/hook.rs:49`（`MemoryStore::project(project_root)`）** 使用。
  2. `atomcode-config/src/config/memory.rs`（原版；store.rs 是它的 VERBATIM 端口，两者字节兼容）：被 **daemon（webui）`atomcode-daemon/src/commands.rs:527/531`（`MemoryStore::global()`/`project()`）** 使用。
- 结论：**两份 `project()` 必须同样修，否则另一条路径（TUI 或 daemon/webui）仍落回 `.atomcode`，宿主 bug 只修一半**。每份内部读/写/注入都经各自 `project()`，故各改一处即覆盖该 crate 的全部路径，无调用点 churn。
- `.atomcode` 每项目目录名在全仓另有约 8 处硬编码（skills/commands/hooks/setup/lock/sensitive_path 等），无统一解析器——**本设计不触碰它们**（见"范围决策"）。

## 范围决策(brainstorming 收敛)

| 决策点 | 结论 |
|---|---|
| 覆盖范围 | **只管 memory**：新增 memory 专用覆盖，不做全项目目录名统一（避免大改敏感路径 + 不做虚假普遍化）。代价：宿主项目内 skills/hooks 仍在 `.atomcode`（它们不归 memory 管，宿主自行处理）。 |
| 机制 | **环境变量** `ATOMCODE_PROJECT_MEMORY_DIR`（风格同 `ATOMCODE_HOME`），集中在 `project()` 解析，零调用点 churn。不做装配期配置。 |
| 迁移 | **不迁移(MVP)**：默认 `.atomcode` 不变 → 存量项目零影响；宿主设自定义目录后在新目录重新开始。宿主如需可自行搬文件，或后续再加。 |

## 具体设计

**两份 `MemoryStore` 各自施加同一改动**（`atomcode-capabilities/src/memory/store.rs` 与 `atomcode-config/src/config/memory.rs`），保持二者字节兼容。下述以 store.rs 为例，config/memory.rs 逐字镜像。

### 改动 —— `MemoryStore::project` 读环境变量(每份 store 各一处)

拆成"纯函数 + 读 env"两层，纯函数脱离 env 可稳定单测：

```rust
/// Resolve the project-scope memory file. `override_dir` = the value of
/// `ATOMCODE_PROJECT_MEMORY_DIR` (None/empty → default ".atomcode"). A relative value
/// nests under `project_root`; an absolute value is used as-is (std `Path::join`
/// semantics). `memory.md` is appended in either case.
fn project_memory_path(project_root: &Path, override_dir: Option<&str>) -> PathBuf {
    let dir = override_dir.filter(|s| !s.is_empty()).unwrap_or(".atomcode");
    project_root.join(dir).join("memory.md")
}

pub fn project(project_root: &Path) -> Self {
    let override_dir = std::env::var("ATOMCODE_PROJECT_MEMORY_DIR").ok();
    Self::new(project_memory_path(project_root, override_dir.as_deref()))
}
```

**行为**：
- `ATOMCODE_PROJECT_MEMORY_DIR` 未设/为空 → `<project_root>/.atomcode/memory.md`（**与现状逐字节一致**）。
- 设为相对值（如 `.myapp`）→ `<project_root>/.myapp/memory.md`。
- 设为绝对路径（如 `/opt/brand/mem`）→ `/opt/brand/mem/memory.md`（`Path::join` 遇绝对路径替换 base）。
- 读、写（`append`/`append_deduped`/`remove_matching`）、`merged_for_prompt`、`/memory` 读取全部随之对齐（都经 `project()`）。

**不引入**：缓存（`project()` 非热路径，每次读 env 廉价）、装配期参数、迁移逻辑、双路径 store。

## 兼容性

- 默认分支未变 → 现有测试（含 `tools/memory.rs::remember_writes_project_entry`，硬编码 `.atomcode`）在 env 未设时仍通过。
- 存量 `.atomcode/memory.md` 项目：不设 env → 零影响。
- 宿主：启动前 `ATOMCODE_PROJECT_MEMORY_DIR=<brand-dir>` 即对齐写侧口径。

## 测试计划

- **纯函数 `project_memory_path`(无 env,稳定)**：
  - `(root, None)` → `root/.atomcode/memory.md`
  - `(root, Some(".myapp"))` → `root/.myapp/memory.md`
  - `(root, Some("/abs/dir"))` → `/abs/dir/memory.md`
  - `(root, Some(""))` → 回退默认 `.atomcode`
- **env 集成一条(自设自清,避免并行泄漏)**：设 `ATOMCODE_PROJECT_MEMORY_DIR` 后 `project(root).path()` 落到自定义目录；`remove_var` 后回默认。（同文件内串行、测尾清理。）
- **两份 store 各自加上述纯函数测试 + 一条 env 测试**（capabilities 与 config 各一套）。
- **回归**：`cargo test -p atomcode-capabilities`（含 memory store + `remember_writes_project_entry`）与 `cargo test -p atomcode-config`（含 config/memory.rs 既有测试）全绿。

## 文档

- 配置文档补一条 `ATOMCODE_PROJECT_MEMORY_DIR`（与 `ATOMCODE_HOME` 并列）：默认 `.atomcode`，可为相对(挂 working_dir)或绝对目录，仅影响 project-scope 记忆文件位置——回应"记忆写到哪了"的可发现性痛点。

## 计划期已验证

- grep 兜底：无别处绕过 `MemoryStore` 直接拼 `.atomcode/memory.md`（除测试）。但**发现第二份 store**（config/memory.rs，daemon 在用）——已纳入设计，两份都改。`hook.rs` 注入侧走 `project()`，自动覆盖。

## 非目标(defer)

- 统一全仓 8 处 `.atomcode` 目录名（skills/commands/hooks/setup 等）到单一解析器 —— 更大、碰敏感路径，另议。
- 装配期配置入口、读旧写新迁移 —— 本 MVP 不做。
