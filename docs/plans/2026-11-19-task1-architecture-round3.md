# Task 1 架构调整（第三轮）：候选也来自注册表

> 交接规格，给实现方（team implementer）用。
> 前两轮：`2026-11-19-task1-architecture-handoff.md`（让注册表记录发现事实）、
> `2026-11-19-task1-architecture-verification.md`（验证侧）。
> **背景**与已完成的部分见那两份。

## 为什么要这一轮

前两轮把「**状态**」交给了注册表（`discovered` 记录 + `registry_says`）。复验发现：

> **推断仍未彻底移除** —— 报告另有 `MAX_DEPTH = 8`、忽略清单、顶层平铺规则和循环检测。

根因是：**报告仍然自己遍历磁盘**（`collect_skill_rows` 走文件系统），只把**状态**
换成了注册表给的。于是「候选从哪来」与「状态从哪来」是两套逻辑，仍会分叉。
复验复现的 4 处里，3 处都是这一个根因：

| 复现 | 为什么错 |
|---|---|
| 注册表经 `skills` 与显式根 `skills/node_modules` 两条路加载了 `foo`；报告从 `skills` 扫描时输出 **0 行**，从内层根扫描才输出 `Active` | 注册表**找到了**这个资产，报告却因为「自己的遍历没走到」而漏报 |
| 两个独立技能目录各有自己的资源，`SKILL.md` 都链接到同一个文件；注册表标了低优先级项 `replaced=true`，报告把**两个目录都**报成 `Active` | 报告按**文件 identity** 匹配，而资产的真实身份是 **bundle 目录** —— 两个 bundle 指向同一份 `SKILL.md` 是两个资产 |
| 平铺候选从未被加载，却借到目录项的覆盖诊断，报「被覆盖」 | 诊断按**文件路径**回退匹配，跨了入口形态 |

第 4 处是**空列表语义**问题（见下面第 3 条）。

## 这一轮的核心决定

> **候选也来自注册表。报告只在注册表根本不知道的树（别人的树）上自己遍历。**

于是：

- **`SkillTree::Scanned`**（我们扫的树）→ 候选 = 注册表的 `discovered` 里
  **来源根属于这棵树**的条目。报告**不再遍历磁盘**。
  - 注册表没发现的 → **报告里没有这一行**（注册表没把它当资产，报告也不该有）。
  - 于是「嵌套技能目录」「忽略目录里的技能」「超深度的技能」这些**根本不会出现** ——
    它们不是注册表的资产。
- **`SkillTree::Foreign { target }`**（别人的树：`~/.codex/skills`、
  `~/.config/opencode/skills` 等）→ 候选 = **报告自己遍历**（注册表不知道它们），
  状态由 `registry_says` 决定（若它其实已被宿主加载进技能根 → 照注册表说的报）。

这是终局：**「资产」这个概念只有一个来源** —— 注册表发现了什么，什么才是资产。
报告不再自己定义一遍。

## 已核实的源码事实

### 注册表侧（`crates/atomcode-capabilities/src/skills/registry.rs`）

- `insert_parsed(&self, parsed: Result<Parsed, String>, path: &Path, kind: TemplateKind, entry: EntryKind)`
  —— 两个调用点：
  - 平铺文件：`self.insert_parsed(parse_skill_file(&p, namespace), &p, kind, EntryKind::FlatFile)`
  - 技能目录：`self.insert_parsed(parse_skill_dir(&p, &skill_md, namespace), &skill_md, TemplateKind::Skill, EntryKind::SkillDir)`
  - **注意**：技能目录传的是 **`&skill_md`**（`<dir>/SKILL.md`），不是 bundle 目录 `<dir>`。
- `Skill.skill_dir: PathBuf`（`skills/skill.rs:44`）—— `parse_skill_dir` 把 **bundle 目录**
  传给了 `build_skill`（`skills/skill.rs:659`）。**这就是资产身份**。
- `SkillDiagnostic` 是 `pub`，**且有外部字面量构造**（`crates/atomcode-cli/src/host.rs:3441`
  与 `:3446`）—— **不能给它加字段**（会破坏那两个构造点）。
- `SkillIndex` 现有字段：`directories` / `diagnostics` / `discovered` /
  `skills` / `commands` / `skill_records` / `command_records`。

### 迁移模块侧（`crates/atomcode-capabilities/src/migration/mod.rs`）

- `SkillTruth { discovered: Vec<(PathBuf, String, EntryKind, bool)>, skills, commands, diagnostics }`
- `SkillTruth::load(ctx)`：`let dirs = if ctx.skill_dirs.is_empty() { standard_skill_dirs(...) } else { ctx.skill_dirs.clone() };`
- `registry_says(name, path, is_skill_dir)` —— 按 `(文件 identity, EntryKind, 名字)` 匹配。
- `verdict(name, path, parses)` —— 第 1 步问 `registry_says`；第 2、3 步查
  `self.diagnostics`（按**文件路径**匹配，`filter(|(at, _, _)| *at == file)`）；
  第 4 步「说不清原因」。
- `collect_skill_rows(kind, scope, dir, depth, max_depth, ancestors, registry, tree, out)` ——
  **报告自己的遍历**，带 `MAX_DEPTH`、忽略目录、环检测。
- `SkillTree::Foreign { target }` 分支：`judge()` 里先问 `registry.registry_says(...)`。
- `ScanContext { os_home, project, product, skill_dirs: Vec<PathBuf>, opencode_config_dir, opencode_data_dir }`。

## 要做的 5 件事

### 1（核心）注册表记录「资产身份」与「来源根」

**`DiscoveredTemplate` 加两个字段**：

```rust
    /// The ASSET this entry is — the thing a person would call "this skill".
    ///
    /// **A skill directory is its BUNDLE directory, not its `SKILL.md`.** Two
    /// bundles may ship the same `SKILL.md` (a symlink to one file); they are
    /// two assets that happen to share a body. Matching the file alone would
    /// report both as one.
    ///
    /// For a flat template file this is the file itself.
    pub asset: PathBuf,
    /// The scanned root this entry was found under — the root EXACTLY as
    /// handed to the registry.
    ///
    /// The report enumerates a tree by its root; this says which root found
    /// the entry, so "the assets under THIS root" is a question the
    /// registry answers.
    pub root: PathBuf,
```

`insert_parsed` 里填：
- 技能目录：`asset` = **bundle 目录**（`<dir>`，不是 `<dir>/SKILL.md`）、`root` = 传入的根。
  - **`insert_parsed` 现在拿不到 bundle 目录** —— 它的 `path` 实参是 `&skill_md`。
    要么给 `insert_parsed` 加一个 `asset: &Path` 参数，要么让调用点传目录。
    **请选一个，并在返回里说明最终签名。**
- 平铺文件：`asset` = 文件本身、`root` = 传入的根。
  - 平铺文件的 `root` 就是它被扫的那个目录（`load_standard_dir` 的 `dir`）。
    `insert_parsed` 现在的 `path` 就是文件，`root` 需要**另外传**。

> **`root` 怎么拿到**：`scan_skill_dir` 递归时知道**最初**的根吗？现在它只知道 `dir`。
> `load_standard_dir` → `load_template_dir(dir, …)` → `scan_skill_dir(dir, 0)`。
> 所以**根就是 `scan_skill_dir` 的第一次调用的 `dir`** —— 递归时要**一路传下去**
> （加一个 `root: &Path` 参数）。
>
> 注意：`load_template_dir` 也被**外部**调用（`reload_dirs_with_explicit` 里的
> `fresh.load_template_dir(dir, Some(namespace), *kind)`）—— 那里的 `dir` 就是根。

### 2（核心）报告的 `Scanned` 候选来自注册表

`SkillTruth` 增一个字段（或改 `discovered` 的元素类型）：

```rust
    /// Every entry the registry's walk found: its ASSET identity, the name
    /// it was registered under, the FORM it was found in, the root that
    /// found it, and whether a later same-name entry displaced it.
    discovered: Vec<(PathBuf, String, EntryKind, PathBuf, bool)>,
```

（元素顺序自定，但 `registry_says` 与候选枚举都要用同一个形状。）

`registry_says` 改成按**资产身份**匹配：

- 候选是**目录技能** → 资产身份 = 那个**目录**；
- 候选是**平铺文件** → 资产身份 = 那个**文件**。

**并且** `registry_says` 要能回答「这个**目录**是不是一个资产」——
候选枚举现在要问的是「注册表发现了哪些**目录技能**」，所以匹配要按 `asset` + `EntryKind::SkillDir`，
而**不是**按 `<dir>/SKILL.md` 的 identity。

> 这里有个连带影响：**报告里那一行的 `path` 应该显示什么？**
> 现在显示的是 `<dir>`（技能目录）或 `<file>`（平铺文件）—— 保持不变。
> 只是**匹配**用的键从「文件 identity」换成「资产 identity」。

### 3（中）`ScanContext.skill_dirs` 要能表达「明确为空」

**问题**：`SkillTruth::load` 现在是 `if ctx.skill_dirs.is_empty() { 标准根 } else { ctx.skill_dirs }`。
于是**宿主明确要求「不要扫任何技能根」**（运行时关掉技能工具时会给空列表），
报告却回落到标准根，把 `~/.claude/skills/foo` 报成 `Active`。

**修法**：`skill_dirs: Option<Vec<PathBuf>>`
- `None` = 宿主没给 → 用 `standard_skill_dirs`（保持现在的默认行为）；
- `Some(vec![])` = 宿主**明确**要求空 → **不扫任何**；
- `Some(dirs)` = 用它。

`ScanContext` 是 `pub`（公共 API），改字段类型是**破坏性**的 —— 但这个模块
**尚未发布**（本功能还没上线），所以现在改代价最小。

`ScanContext` 的文档注释要说明这个区别。

### 4（中）诊断按**资产身份**关联，不再按文件路径回退

**问题**：`verdict` 第 2、3 步查 `self.diagnostics`，按**文件路径**匹配 ——
于是平铺候选能借到目录项的覆盖诊断。

**修法**：诊断与发现**按同一个资产身份**关联。

因为 `SkillDiagnostic` **不能加字段**（外部字面量构造），所以关联要放在
**新结构**里 —— 建议给 `DiscoveredTemplate` 加一个字段：

```rust
    /// The diagnostics the registry recorded for THIS asset: its reason and
    /// whether it is a warning.
    ///
    /// **On the asset, not on the file.** A flat candidate and a skill
    /// directory can sit at the same path; a diagnostic about one is not
    /// about the other.
    pub diagnostics: Vec<(String, bool)>,
```

注册表在 `insert_parsed` 里把诊断挂到对应的发现记录上。

> **解析失败的条目没有发现记录**（`Err` 分支不记 `discovered`）。
> 它的诊断在 `self.diagnostics` 里，报告按**资产身份**（技能目录 = 目录，
> 平铺文件 = 文件）匹配即可 —— **不要**退回按 `<dir>/SKILL.md` 匹配，
> 那正是跨形态误配的来源。
>
> 提示：`Err` 分支拿到的 `path` 就是资产身份（平铺文件 = 文件本身；
> 技能目录时 `insert_parsed` 的 `path` 实参是 `&skill_md`，资产身份是它的**父目录**）。

### 5（中）`SkillTree::Scanned` 的候选**只**来自注册表

`collect_skill_rows`（报告自己的遍历）**只服务 `Foreign` 树**。

`Scanned` 树的新枚举方式：

```
for each entry in truth.discovered:
    if entry.root 属于这棵树（按 identity 或字面比较，见下）:
        if entry.form 是这个 kind 要的形态:
            生成一行（状态由 registry_says 给）
```

- **「属于这棵树」怎么判**：候选树用**根路径**给出（`skill_rows` 现在收的是 `root: &Path`）。
  判断 `entry.root` 与候选根**是不是同一个目录**：
  - 字面相等，**或** `identity(entry.root) == identity(candidate_root)`。
  - （路径别名场景：注册表拿到的根可能是 `/private/var/...`，候选树是 `/var/...`。）
- **`Foreign` 树**：候选**仍然自己遍历**（注册表不知道那些树），但：
  - 状态先问 `registry_says`（若它其实已被加载）；
  - 遍历遇到「深度上限 / 环 / 忽略目录」时，报**报告自己的诊断**
    （现有那几条文案：`report_depth_limit` / `report_symlink_cycle` 等），
    **不要**用「技能加载器拒绝了它」—— 那是注册表的话，而注册表没扫这棵树。

> `Foreign` 树的遍历**保留** `MAX_DEPTH` / 忽略目录 / 环检测 —— 但那不再是
> 「推断注册表发现了什么」，而是「报告自己能走多远」。这是**报告自己的能力边界**，
> 不是对注册表的猜测。

## 一条纪律

> **凡是报告自己做的事，文案要说「是报告做的」，不是「加载器做的」。**
>
> 注册表说的 → `skill_active` / `skill_overridden` / `skill_rejected`（加载器的话）。
> 报告自己做的 → `report_*` 那几条（报告的话）。
>
> 混用会让用户以为该去改技能文件，而真相是该改目录结构。

## 必须仍然成立的行为（实现后请逐条推演并回我）

1. 路径别名（`/var` vs `/private/var`）：胜者 `Active`、落败者「被覆盖」、报告的 `path` 仍是调用方给的**原路径**。
2. 符号链接（文件级 / 目录级）：落败者「被覆盖」。
3. **两个 bundle 目录指向同一份 `SKILL.md`**：两个**不同**的资产 —— 一个生效、一个「被覆盖」（**不能**都报 `Active`）。
4. 定义完全相同的两份技能（注册表**不记**覆盖诊断）→ 落败者仍「被覆盖」。
5. 非致命警告（`args: oops`）→ 落败者「被覆盖」，**不是**「加载器拒绝了它」。
6. 无效 UTF-8 → 「加载器拒绝了它 + 注册表原文」。
7. **嵌套技能目录 / 忽略目录里的技能 / 超深度的技能** → **报告里不出现**（注册表没发现它们，它们不是我们的资产）。
   - ⚠️ 这与上一轮的期望**不同** —— 上一轮期望它们「没有生效，说不出原因」。
     **这一轮它们根本不出现。** 请确认这个变化是**正确**的：注册表不认为它们是资产，
     报告就不该列它们。
8. 异名符号链接根（`aliased/commands` → `plain/`）→ 按**匹配上的根的字面名**分类。
9. 一个目录经两个根 → 两个索引都算，任一索引有同名胜者即「被覆盖」。
10. 不在扫描范围内的同名路径 → **不出现**（注册表没发现它）。
11. `commands` 树里的技能目录 → 与**技能**索引比较，结论不取决于 `high/commands/` 下有没有同名 `foo.md`。
12. `Foreign` 三态：已加载 → `Active`；解析不了 → `Blocked` + 新文案；可复制 → `Actionable` + 复制目标（措辞不承诺「就能加载」）。
13. 断链 / FIFO 的 `SKILL.md` → 仍有一行 `Blocked`（**报告自己的**诊断）。
14. `Foreign` 树超深 → 报告自己的诊断（**不是**「加载器拒绝了它」）。
15. **`skill_dirs: Some(vec![])`** → 报告里**没有**任何 `Active` 的技能行（不回落标准根）。
16. 同一份报告里不同来源看到**同一份**注册表事实。

## 验收标准（由 Codex 验证，不是你）

- `cargo nextest run -p atomcode-capabilities --features migration -E 'test(migration::tests::)'` 全绿。
- `cargo nextest run -p atomcode-capabilities --features skills -E 'test(skills::registry::tests::)'` 全绿（注册表是运行时也在用的）。
- `bash gates/compile.sh` 退出 0。
- `cargo fmt --all -- --check` 退出 0。
- `cargo nextest run -p atomcode-config`、`cargo nextest run -p atomcode-i18n` 全绿。
- `cargo nextest run -p atomcode-coding`、`cargo nextest run -p atomcode-tui`、`cargo nextest run -p atomcode` 全绿
  （沙箱会挡掉 socket / 回环 / `ps` 那批夹具 —— 那是环境限制，不是缺陷）。

**你不需要跑这些。**

## 约束（不变）

- 只改：
  - `crates/atomcode-capabilities/src/skills/registry.rs`
  - `crates/atomcode-capabilities/src/skills/mod.rs`
  - `crates/atomcode-capabilities/src/migration/**`
  - `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs`
- **不要**碰工作区里别人的并行改动（`provider/discovery*.rs`、`atomcode-cli/src/tui_providers.rs`、
  `atomcode-daemon/src/api_*.rs`、`atomcode-i18n/src/screen/**`、`atomcode-tui/src/**`、`webui/**`）。
- **不要**动 `crates/atomcode-cli/src/host.rs` —— 那里的 `SkillDiagnostic` 字面量构造
  是**为什么不能给 `SkillDiagnostic` 加字段**的原因。
- **禁止**写测试、**禁止**跑任何构建/测试/fmt 命令、**禁止**任何 git 写操作。
- 新增文案要走 `text.rs` + product 表三处同改（中文不得有英文硬编码）。

## 返回

改完后简短返回：
1. 改了哪些文件。
2. **最终签名**：`DiscoveredTemplate` 的字段、`insert_parsed` 的签名、`SkillTruth` 的字段、
   `registry_says` 的签名、`skill_rows` / `collect_skill_rows` 的签名、
   `ScanContext.skill_dirs` 的类型 —— 我要用它更新实施计划里 Task 2/3/4 的代码块。
3. 上面 16 条行为的推演结果（各一条）。
4. 新增/改动的 i18n 变体名。
5. 偏离之处。
6. 待验证项。

不要贴大段代码，不要声称你跑过验证。
