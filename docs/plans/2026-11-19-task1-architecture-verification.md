# Task 1 架构调整的验证规格

> 交给 **Codex**（测试与验证方）。实现方已按
> `docs/plans/2026-11-19-task1-architecture-handoff.md` 完成改动。

## 背景

Task 1（`crates/atomcode-capabilities/src/migration/`）此前十一轮都在修**同一类**问题：
迁移模块用「根 + 路径 + 第二次解析」去**推断**技能注册表已经知道的事，两套逻辑在同一份
磁盘状态下分叉（符号链接、忽略目录、深度上限、别名路径、嵌套目录……）。

**这次换做法**：注册表**记录它实际发现了什么**（含被同名的另一份顶掉的那一份），
报告**只依据这份记录**裁决 —— 不再有任何推断。

**你要验证的是**：这个新做法是否真的消掉了那一整类分叉，以及有没有引入新问题。

## 本次改动的范围（只有这些文件）

- `crates/atomcode-capabilities/src/skills/registry.rs` —— 新增 `DiscoveredTemplate`
  结构、`SkillIndex.discovered` 字段、`SkillRegistry::discovered()` 访问器；
  `insert_parsed` 现在记录每次发现，并在被顶掉时标 `replaced = true`。
- `crates/atomcode-capabilities/src/skills/mod.rs` —— 导出 `DiscoveredTemplate`。
- `crates/atomcode-capabilities/src/migration/mod.rs` —— `SkillTruth` 改为存
  `discovered`；删掉三个推断助手（`ancestors_are_plain` / `walked_path` /
  `discovered_indexes`）；新增「注册表怎么说」的查询；`verdict` 改为五步，
  第 1 步先问注册表。
- `crates/atomcode-capabilities/src/migration/text.rs` 与
  `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs` —— 新增文案。

**工作区里还有别的 Agent 的并行改动**（`crates/atomcode-capabilities/src/provider/discovery*.rs`、
`crates/atomcode-cli/src/tui_providers.rs`、`crates/atomcode-daemon/src/api_*.rs`、
`crates/atomcode-i18n/src/screen/**`、`crates/atomcode-tui/src/**`、`webui/**`）——
**绝对不要修改它们**。

## 你要做的

### 1. 跑完整验证

```bash
cargo nextest run -p atomcode-capabilities --features migration -E 'test(migration::tests::)'
bash gates/compile.sh
cargo nextest run -p atomcode-capabilities
cargo nextest run -p atomcode-capabilities --features migration
cargo nextest run -p atomcode-config
cargo nextest run -p atomcode-i18n
cargo nextest run -p atomcode-cli
cargo nextest run -p atomcode-coding
cargo nextest run -p atomcode-tui
bash gates/layers.sh
bash gates/product-dir.sh
cargo fmt --all
cargo fmt --all -- --check
```

**关于 `cargo fmt --all`**：它会格式化整个 workspace，**包括别人的并行改动**。执行前
`git status --porcelain` 记基线；执行后**逐文件比对**，把**本任务范围以外**、且**只是被
fmt 改动**的文件用 `git checkout -- <单个文件>` 撤销。**绝不要**用 `git checkout .` /
`git restore .` / `git stash`。

**关于 `bash gates/compile.sh`**：它曾在共享工作区因两个 canary 文件名争用而失败。先在共享
工作区跑；若因争用失败，在**隔离副本**（`cp -R` 到临时目录）里再跑一次并说明差异。

### 2. 验证「推断已彻底移除」

**a) 静态检查**：确认 `migration/mod.rs` 里**不再有**任何复刻注册表遍历规则的代码 ——
没有「根 + 路径前缀」式的判断、没有忽略目录清单、没有深度上限常量、没有「还原候选路径」
的助手。报告里只应剩下：
- 读注册表的 `discovered()` / `diagnostics()` / `all()` / `slash_commands()`；
- 与注册表**同一套解析器**的 `parse_skill_file` / `parse_skill_dir`（取名字 + 判断能否解析）。

**b) 逐条实测这些场景**（这**不是**回归 —— 这些正是新做法要解决的）：

| 场景 | 构造 | 期望 |
|---|---|---|
| 路径别名 | `skill_dirs` 给 `/private/var/...`，扫描用 `/var/...` | 胜者 `Active`；落败者「被覆盖」；报告的 `path` 仍是调用方给的**原路径** |
| 符号链接（文件级） | `skills/foo/SKILL.md` 是指向根外的符号链接 | 落败者「被覆盖」 |
| 符号链接（目录级） | `skills/foo` 整个是指向根外的符号链接目录 | 落败者「被覆盖」 |
| 嵌套技能目录 | `outer/` 有 `SKILL.md`（它本身是技能），`outer/foo` 是技能目录 | `outer/foo` → 「没有生效，说不出原因」（注册表**从未发现**它 —— 遇到技能目录就停止下降） |
| 忽略目录 | `skills/node_modules/foo` 是技能目录 | 「没有生效，说不出原因」；但**显式**把 `node_modules` 当扫描根时 → 「被覆盖」 |
| 深度上限 | 根下 9 层分组目录再放 `foo/SKILL.md` | 「没有生效，说不出原因」；**正好 8 层** → 「被覆盖」 |
| 嵌套平铺命令 | 只扫 `configured/` 与 `high/commands/`，候选 `configured/commands/foo.md` | 「没有生效，说不出原因」（注册表只在各根顶层加载平铺文件） |
| 定义完全相同的两份技能 | 两份内容完全一致的同名技能（注册表**不记**覆盖诊断） | 落败者「被覆盖」← **这一条现在靠 `replaced` 记录直接成立** |
| 非致命警告 | 所有副本带 `args: oops`（技能**仍然加载**） | 落败者「被覆盖」，**不是**「加载器拒绝了它」 |
| 无效 UTF-8 | `skills/foo/SKILL.md` 写入 `0xff` | 「加载器拒绝了它 + 注册表原文」；同名合法命令 → `Active` |
| 异名符号链接根 | `aliased/commands` → `plain/`，两处各放同内容 `foo.md` | `plain/foo.md` → 「被覆盖」（按**匹配上的根的字面名**分类） |
| 一个目录经两个根 | 同一目录被两个根扫到 | 两个索引都算；任一索引有同名胜者即「被覆盖」 |
| 不在扫描范围内 | 用 `skill_dirs` 排除某目录后再扫它 | 「没有生效，说不出原因」 |
| `commands` 树里的技能目录 | `low/commands/foo/SKILL.md` 与 `high/skills/foo/SKILL.md` | 落败者「被覆盖」，且结论**不**取决于 `high/commands/` 下有没有同名 `foo.md` |

**c) 验证 `Foreign` 分支的三处修正**（外部树：Codex / OpenCode 的资产）：

1. **先问注册表**：若外部那棵树**其实已被注册表加载**（宿主把 `~/.config/opencode/skills`
   加进了技能根）→ 报「已经在生效」/`Active`，**不是**「可以搬」。
2. **解析不了**（无效 UTF-8）→ `Blocked` + **新文案**（不是「加载器拒绝了它」——
   那说的是注册表，而这里注册表根本没看这个文件）。
3. **复制建议不承诺「这样就能加载」**：文案改成「搬过来」的说法（不承诺它会赢 ——
   取决于目标目录里有没有同名的更高优先级定义）。

### 3. 验证悬空符号链接不再让整行消失

`foo/SKILL.md` 是**悬空**符号链接（目标不存在）。期望：仍有一行报告（`Blocked` +
新文案），**不是**整行从报告里消失（设计文档要求「没有一项资产可以凭空消失」）。

### 4. 验证注册表侧没有回归

`skills/registry.rs` 是**运行时也在用**的模块：

- `cargo nextest run -p atomcode-capabilities`（**默认 feature**）必须全绿。
- 确认 `insert_parsed` 的**既有行为一字未变**：覆盖诊断**仍然只在定义不同时**记
  （`!same_definition`）；警告的记录、`diagnose` 的调用、`diagnostics()` 的内容与顺序
  都不受影响。`discovered` 是**纯增量**（新增一个字段 + 一个访问器）。
- 确认 `SkillDiagnostic` 的字段与语义未变。
- 确认 `TemplateKind` 的 derive 改动（若加了 `PartialEq` / `Eq`）不影响任何现有代码。

### 5. 核对实施计划文档

`docs/plans/2026-11-17-external-agent-asset-migration-plan.md` 的 Task 1 代码块应与实现
**逐字一致**。逐项比对：

- `identity` / `same_file` / `file_id` / `SkillTree` / `skill_rows` / `collect_skill_rows` /
  `SkillTruth`（新字段）/ `verdict`（新五步）/ `judge` / `text.rs` 最小版（helper 数量与实现
  一致）/ i18n 清单与中英文 match 臂。
- **新结构** `DiscoveredTemplate` 与 `SkillRegistry::discovered()` 的代码块。
- Task 2 的 `text.rs` 完整版 + i18n 清单（**不重复** Task 1 已有的变体）+ 三处前置条件修复
  （`use super::{probe_dir, probe_file, …}` 导入、`for name in ["CLAUDE.md", "claude.md"]`
  两种拼法、Step 3 的「保留 Step 1 写的测试」提醒）+ 用户级与项目级去重（`super::same_file`，
  `Ok(true)` 与 `Err` 两分支共用 `already_reported`）。
- Task 3 的补文案段；Task 4 的调用点。
- 全文搜索**旧符号**残留（`was_discovered`、`discovered_index`（单数）、
  `competes_as_command`、`ancestors_are_plain`、`walked_path`、`verdict(kind, …)`、
  `loaded:` 字段、`scanned: Vec<PathBuf>`、`tree.judge(registry, &name, &path)`（无
  `parses` 的两参数版）、`Overridden by a skill named`、`同名的技能「`）。

**发现不一致，直接改计划文档**（允许），并在报告里列出改了哪里。

### 6. 检查 Task 2 的前置条件（不实现 Task 2）

Task 2 在计划里搜 `## Task 2:`。检查它的代码块与当前实现自洽：`skill_rows` 签名、
`text::*` 函数是否都在、Task 2 的替换块有没有**删掉** Task 1 已有而 Task 2 仍要用的那个、
`probe_file` / `probe_dir` / `instruction_verdict` / `SkillTruth::load` / `super::same_file`
的用法、`load_instructions` / `InstructionState` 的用法。用户级与项目级去重：在**隔离副本**里
实际编译 Task 2 的草案验证（借用检查）。

### 7. Task 1 的整体判断

Task 1 是后续 8 个 Task 的地基。请给一个**总的判断**：

- 「报告会不会说谎」这一类问题，现在**还剩哪些**没解决？
- 有没有**架构层面**的问题（而不只是边界 case），值得在 Task 2 开工前改掉？

## 验收标准

- 上面第 1 条的**全部**命令退出码符合预期（`tui-i18n.sh` 除外 —— 它在基线 `bb81f6f67` 上
  就已经失败，是既有问题，与本任务无关；请核实这一点）。
- 第 2–3 条的场景**全部**符合期望。
- `Cargo.lock` 逐字节未变；`hkdf` / `hmac` / `zeroize_derive` 三个 package 名均为 0。
- `migration` 模块的 `dead_code` 警告为 0。

## 返回

1. **验证证据**：每条命令 + 退出码 + 结果摘要（失败贴关键输出）。
2. **场景实测**：第 2、3 条每个场景的**实际**报告行（name / status / path / detail）。
3. **推断是否已彻底移除**：静态检查的结论。
4. **注册表侧无回归**的证据。
5. **计划文档核对**：第 5、6 条的逐条结论 + 你改了计划文档的哪些地方。
6. **Task 1 整体判断**（第 7 条）。
7. **格式化副作用**：改动了哪些本任务范围外的文件、如何处置的。
8. **未验证范围**。

**不要声称你跑过没跑的命令。**
