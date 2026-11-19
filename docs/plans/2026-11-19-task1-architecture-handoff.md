# Task 1 架构调整规格：让注册表记录发现事实

> 这是一份**交接规格**，给实现方（Claude Code，或顶替它的 team implementer）用。
> 背景与验收见 `docs/plans/2026-11-17-external-agent-asset-migration-plan.md` 的 Task 1。

## 为什么做这次调整

Task 1 已经过十一轮「实现 → 审查 → 修复」。每一轮修的都是**同一类**问题：迁移模块用
「根 + 路径 + 第二次解析」去**推断**注册表已经知道的事，于是两套逻辑在同一份磁盘状态下
得出不同结论。

已经修过的边界 case 累计有：路径别名、符号链接（文件级与目录级）、忽略目录、深度上限、
嵌套目录、同名命令、定义相同的两份技能、非致命警告、异名符号链接根、双索引……**每一个都是
同一个病**。继续逐个修是打地鼠。

**这次换做法**：让注册表**记录它实际发现了什么**，报告**只依据注册表的事实**裁决。这样
「注册表会不会走到这个条目」这个问题**根本不需要问**了 —— 注册表已经知道了。

## 当前代码状态

- `crates/atomcode-capabilities/src/migration/` —— 已实现（`mod.rs` 1088 行、`report.rs`、
  `provider.rs`、`text.rs`、两个空壳 `claude.rs`/`codex.rs`）。**未提交。**
- `crates/atomcode-capabilities/src/skills/registry.rs` —— **未改**。运行时也在用它。
- `crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs` —— 已加 9 条 migration 文案。**未提交。**
- `crates/atomcode-config/src/provider_book.rs` —— 已加 `account_id_for` / `effective_endpoint`。**未提交。**
- `crates/atomcode-capabilities/Cargo.toml`、`src/lib.rs` —— 已加 `migration` feature。**未提交。**
- `gates/compile.sh` —— 已把 `migration` 加进 `FEATURE_GATED`。**未提交。**

工作区**同时**有别的 Agent 的并行改动（`crates/atomcode-capabilities/src/provider/discovery*.rs`、
`crates/atomcode-cli/src/tui_providers.rs`、`crates/atomcode-daemon/src/api_*.rs`、
`crates/atomcode-i18n/src/screen/**`、`crates/atomcode-tui/src/**`、`webui/**`）——
**绝对不要碰**。

## 已核实的源码事实

### 注册表（`crates/atomcode-capabilities/src/skills/registry.rs`）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub reason: String,
    /// Overrides are informational; failures are surfaced as warnings.
    pub warning: bool,
}

#[derive(Default)]
struct SkillIndex {
    directories: Vec<PathBuf>,
    diagnostics: Vec<SkillDiagnostic>,
    skills: BTreeMap<String, Arc<Skill>>,
    commands: BTreeMap<String, Arc<Skill>>,
}

pub struct SkillRegistry {
    skills: RwLock<SkillIndex>,
    native_roots: RwLock<Vec<PathBuf>>,
    notified: RwLock<BTreeSet<String>>,
}
```

`insert_parsed`（约第 214 行）现状：

```rust
fn insert_parsed(&self, parsed: Result<Parsed, String>, path: &Path, kind: TemplateKind) {
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
            let previous = match kind {
                TemplateKind::Skill => index.skills.insert(name, Arc::new(skill)),
                TemplateKind::Command => index.commands.insert(name, Arc::new(skill)),
            };
            if let Some(previous) = previous {
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

**关键**：两份定义**完全相同**时（`same_definition == true`），注册表**换掉了前者却不留任何
记录** —— 这正是报告必须靠推断才能回答的地方。

`TemplateKind`（约第 39 行）：

```rust
pub enum TemplateKind {
    Skill,
    Command,
}
```

访问器（约第 183 行）：

```rust
pub fn directories(&self) -> Vec<PathBuf> { self.read().directories.clone() }
pub fn diagnostics(&self) -> Vec<SkillDiagnostic> { self.read().diagnostics.clone() }
```

导出（`crates/atomcode-capabilities/src/skills/mod.rs`）：

```rust
pub use registry::{
    skill_install_dirs, standard_skill_dirs, SkillDiagnostic, SkillRegistry, SkillRoots,
    TemplateDirectory, TemplateKind,
};
```

### 迁移模块（`crates/atomcode-capabilities/src/migration/mod.rs`）

`SkillTruth` 现状：

```rust
struct SkillTruth {
    skills: Vec<(String, PathBuf)>,      // registry.all()
    commands: Vec<(String, PathBuf)>,    // registry.slash_commands()
    diagnostics: Vec<(PathBuf, String, bool)>,
    scanned: Vec<(PathBuf, PathBuf)>,    // (字面根, 根的 identity)
}
```

`verdict` 五步：

1. 我们的路径在任一索引里 → `Active`
2. 我们这条路径有 `warning == false` 的诊断 → 「被覆盖」
3. 我们这条路径只有 `warning == true` 的诊断 **且 `parses == false`** → 「加载器拒绝了它」+ 注册表原文
4. `discovered_indexes(path, is_skill_dir)` 非空 **且** 同名索引里有别的路径 → 「被覆盖」
5. 否则 → 「没有生效，说不出原因」

为做第 4 步，有三个**推断**助手 —— 它们要复刻注册表的遍历规则：

- `ancestors_are_plain(root, entry)` —— 复刻「遇到技能目录就停止下降」「跳过忽略目录」「深度上限 8」
- `walked_path(root_literal, root_identity, candidate)` —— 把候选还原成注册表遍历时的拼法
- `discovered_indexes(path, is_skill_dir)` —— 收集所有匹配根对应的索引

`collect_skill_rows` 已经用 `crate::skills::skill::parse_skill_file` / `parse_skill_dir` 取名字，
并用 `parses` 标记「解析成功与否」。

### 其他

- `crate::skills::TemplateKind` 是**公开**的（`skills/mod.rs:26` 导出）。
- `crate::skills::skill::parse_skill_file` / `parse_skill_dir` 是 `pub(crate)`，同 crate 可用。
  `Parsed { pub(crate) skill: Skill, pub(crate) warnings: Vec<String> }`。
- `Err` 是**致命**的（读不了文件、名字非法、`allowed-tools` 声明损坏）；
  `args:` 写错是 `Ok` 里的 `warnings` —— 技能照常加载。

## 要做的四件事

### 1（核心）注册表记录它发现了什么

**文件**：`crates/atomcode-capabilities/src/skills/registry.rs`、
`crates/atomcode-capabilities/src/skills/mod.rs`

**a) 新增公开结构**（放在 `SkillDiagnostic` 之后）：

```rust
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
    /// The name it was registered under.
    pub name: String,
    /// Which index it went into.
    pub kind: TemplateKind,
    /// Whether a later same-name entry displaced it.
    ///
    /// **A same-name entry displaces the earlier one even when the two
    /// definitions are identical** — the registry keeps the later one and
    /// says nothing, because nothing is wrong. A caller reporting "which one
    /// is in effect" needs to know the earlier one lost, and only the
    /// registry knows.
    pub replaced: bool,
}
```

`TemplateKind` 要能比较 —— **检查它现在的 derive**，若没有 `PartialEq` / `Eq` 就补上
（只是多派生两个 trait，不改任何行为）。

**b) `SkillIndex` 加字段**：

```rust
    /// Every entry the walk found, in the order it found them.
    discovered: Vec<DiscoveredTemplate>,
```

**c) `insert_parsed` 里记录每次发现**：

- 每次 `Ok(parsed)` 记一条（`replaced: false`）：

```rust
index.discovered.push(DiscoveredTemplate {
    path: path.to_path_buf(),
    name: name.clone(),
    kind,
    replaced: false,
});
```

- 当 `previous` 存在且 `previous.source_path != source` 时，把**先前那条**发现的 `replaced`
  置为 `true`：

```rust
if let Some(previous) = previous {
    if previous.source_path != source {
        if let Some(entry) = index
            .discovered
            .iter_mut()
            .rev()
            .find(|entry| entry.path == previous.source_path)
        {
            entry.replaced = true;
        }
    }
    // 覆盖诊断的条件**保持原样**（`!same_definition`）—— 相同定义不是问题，不该报诊断。
    if previous.source_path != source && !same_definition {
        index.diagnostics.push(SkillDiagnostic {
            path: previous.source_path.clone(),
            reason: format!("overridden by {}", source.display()),
            warning: false,
        });
    }
}
```

即：标 `replaced` 的条件是 `source_path != source`（不管定义相同与否）；记覆盖诊断的条件
仍是 `source_path != source && !same_definition`（**行为不变**）。

`iter_mut().rev().find(...)` 是为了找**最近**的那条（同名可能有多条历史）。

**d) 新增访问器**（放在 `diagnostics()` 旁边）：

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

**e) 导出**（`crates/atomcode-capabilities/src/skills/mod.rs`，按字母序插在 `SkillDiagnostic` 前）：

```rust
pub use registry::{
    skill_install_dirs, standard_skill_dirs, DiscoveredTemplate, SkillDiagnostic, SkillRegistry,
    SkillRoots, TemplateDirectory, TemplateKind,
};
```

### 2（核心）迁移模块改为「只依据注册表的事实」

**文件**：`crates/atomcode-capabilities/src/migration/mod.rs`

**a) `SkillTruth` 改为**：

```rust
struct SkillTruth {
    /// Every entry the registry's walk found: the path's identity, the name
    /// it was registered under, and whether a later same-name entry
    /// displaced it.
    discovered: Vec<(PathBuf, String, bool)>,
    /// The names currently in effect, per index.
    skills: Vec<(String, PathBuf)>,
    commands: Vec<(String, PathBuf)>,
    /// Every diagnostic the registry produced: the path's identity, its
    /// reason, and its `warning` flag.
    diagnostics: Vec<(PathBuf, String, bool)>,
}
```

**`scanned` 字段删掉** —— 不再需要「注册表扫过哪些目录」这个信息了。

`load` 里：`discovered` 从 `registry.discovered()` 来，每个元素存
`(identity(&entry.path), entry.name.clone(), entry.replaced)`。

**b) 删掉三个推断助手**：`ancestors_are_plain`、`walked_path`、`discovered_indexes` ——
它们的活由注册表的记录接手了。

**c) 新增一个「注册表怎么说」的查询**（方法名可自拟，下面叫 `registry_says`）：

```rust
/// What the registry says about this asset, if it says anything.
///
/// `Some` means the registry has a FACT about this path — it found it (or
/// displaced it). `None` means the registry never walked it, so it has
/// nothing to say.
///
/// Matching is by **path identity** (the same file spelled two ways is the
/// same file) **and by name** (the same file registered under another name
/// is not this asset).
fn registry_says(&self, name: &str, path: &Path, is_skill_dir: bool) -> Option<(AssetStatus, String)>
```

判定：
- 找出 `discovered` 里 **identity 相同**的条目；
- 其中**注册名等于 `name`** 的那几条：
  - 有任何一条 `replaced == false` → 它**就是生效的那一个** → `(Active, text::skill_active())`；
  - 否则（有但全被顶掉）→ **被顶掉了** → `(Blocked, text::skill_overridden(name))`。
- **identity 相同但注册名不同** → 这个文件在生效，但**不是以我们的名字**生效 → 它**不是**
  我们的资产 → 返回 `None`（让它落到「没有生效，说不出原因」）。**绝不能**报 `Active`。

**d) `verdict` 改为**：

```rust
fn verdict(&self, name: &str, path: &Path, parses: bool) -> (AssetStatus, String) {
    let is_skill_dir = probe_dir(path).unwrap_or(false);
    let file = identity(&if is_skill_dir { path.join("SKILL.md") } else { path.to_path_buf() });

    // 1. What the registry ACTUALLY found. It walked these paths; we did
    //    not walk them a second time to find out.
    if let Some(verdict) = self.registry_says(name, path, is_skill_dir) {
        return verdict;
    }
    // 2. The registry's own words about this path.
    let ours: Vec<&(PathBuf, String, bool)> = self.diagnostics.iter().filter(|(at, _, _)| *at == file).collect();
    if ours.iter().any(|(_, _, warning)| !*warning) {
        return (AssetStatus::Blocked, text::skill_overridden(name));
    }
    // 3. A warning on a file that PARSED is not a rejection (a bad `args:`
    //    value still loads) — only the parser tells them apart.
    if !parses {
        if let Some((_, reason, _)) = ours.first() {
            return (AssetStatus::Blocked, text::skill_rejected(reason));
        }
    }
    // 4.
    (AssetStatus::Blocked, text::skill_not_in_effect())
}
```

### 3（中）`Foreign` 分支的三处过度承诺

现在 `SkillTree::Foreign { target }` 无条件给 `Actionable` + 「复制到 X 就能加载」。实测三种
它**不成立**的情况：

1. **文件本身解析不了**（无效 UTF-8）→ 复制过去也加载不了。
2. **这棵树其实已经被注册表加载了**（宿主把 `~/.config/opencode/skills` 加进了技能根）→ 它
   **已经在生效**，不是「可以搬」。
3. **复制过去仍然输给同名的更高优先级定义** → 「复制就能加载」不成立。

**修法**：

- **先问注册表**：若 `registry_says` 对这个资产有话说（`Active` 或 `Blocked`），**照它说的报**
  —— 情况 2 就这样解决了（外部树其实在生效）。
- **解析不了** → `Blocked`，用一条**新文案**（不要复用「加载器拒绝了它」—— 那说的是注册表，
  而这里注册表根本没看这个文件）。
- **「复制就能加载」的措辞**改成**不承诺它会赢**的说法：「bring it over」（搬过来）是描述
  **动作**，「to make it load」（这样就能加载）是**承诺**。而它会不会赢，取决于目标目录里有
  没有同名的更高优先级定义 —— 报告不该替用户打包票。
  - 英文从「Not in effect yet: copy it to {target} to make it load.」改成
    「Not in effect yet. Copy it to {target} to bring it over.」，中文对应改。
  - **函数名 `text::skill_copyable` 不改**，只改**文案内容**。

**新文案需要**（`crates/atomcode-i18n/src/product/{messages,en,zh_cn}.rs` **三处同改**）：
- 一条「这个文件解析不了，搬过去也加载不了」的新变体（例如 `MigrationForeignUnparseable`）——
  措辞自拟，中英文语义对应，中文版不得有英文硬编码。
- `MigrationSkillCopyable` 的**内容**改成不承诺的措辞（变体本身**不新增**，只改 `en.rs` /
  `zh_cn.rs` 里那条臂的文字）。

### 4（中）悬空符号链接让整行资产消失

**复现**：`foo/SKILL.md` 是一个**悬空**符号链接（目标不存在）。两个跟随链接的探测都按「不存在」
处理，于是这个资产**从报告里整行消失** —— 而设计文档明确要求「没有一项资产可以凭空消失」。

**根因**：`std::fs::metadata` **跟随**符号链接 → 目标没了就说「不存在」；
`std::fs::symlink_metadata` **不跟随** → 还能看见这个条目存在。

**修法**（`collect_skill_rows` 里）：

- 探测 `SKILL.md` 时，除了 `probe_file`（跟随链接），再用 `symlink_metadata` 看**条目本身**在不在：
  - 条目在、但 `probe_file` 说不是普通文件 → 它是个**断链** → 产出一行 `Blocked`（路径是那个
    `SKILL.md`，用一条**新文案**说明「这里是断链，所以这个技能加载不了」），然后 `continue`。
- 循环里探测条目类型时也一样：`probe_file` 与 `probe_dir` 都说不成立时，用 `symlink_metadata` 看
  条目是否**存在**（存在 → 断链 → 产出一行 `Blocked`）。

**新文案需要**（同样**三处同改**）：一条「断链」的说法（例如 `MigrationBrokenLink`）。

> **不要**用「读不了」那条（`unreadable_file`）—— 断链不是「权限不足」，是「目标没了」，
> 用户要做的动作不同。

## 新实现后必须仍然成立的行为

请**自己推演**每一条（新逻辑应该**更容易**满足它们，因为不再有推断）：

1. 路径别名（`/var` vs `/private/var`）：胜者 `Active`、落败者「被覆盖」、报告的 `path` 仍是
   调用方给的原路径。
2. 符号链接的技能（`SKILL.md` 链到根外，**以及**技能目录本身是链接）：落败者「被覆盖」。
3. 无效 UTF-8 的技能 + 同名合法命令：技能报「加载器拒绝了它 + 注册表原文」，命令报 `Active`。
4. 两份**内容完全相同**的同名技能（注册表**不记**覆盖诊断）→ 落败者仍报「被覆盖」←
   **这一条现在靠新记录直接成立**。
5. 非致命警告（`args:` 写错，技能**仍然加载**）：落败者报「被覆盖」，**不是**「加载器拒绝了它」。
6. 嵌套的平铺命令、嵌套技能目录、忽略目录里的技能、超出分组深度上限的技能：**注册表从未发现**
   它们 → 「没有生效，说不出原因」。
7. 不在扫描范围内的同名路径 → 「没有生效，说不出原因」。
8. `commands` 树里的技能目录 → 与**技能**索引比较，结论不取决于 `high/commands/` 下有没有同名
   `foo.md`。
9. 异名符号链接根（`aliased/commands` → `plain/`）下的平铺命令 → 按**匹配上的根的字面名**分类 →
   「被覆盖」。
10. 命令被覆盖时文案中性（不含「技能」/「skill」）。

## 验收标准（由 Codex 验证，不是实现方）

- `cargo nextest run -p atomcode-capabilities` 全绿（默认 feature —— `skills/registry.rs`
  是运行时也在用的）。
- `cargo nextest run -p atomcode-capabilities --features migration` 全绿。
- `cargo nextest run -p atomcode-capabilities --features migration -E 'test(migration::tests::)'`
  全绿（3 条判据）。
- `cargo nextest run -p atomcode-cli -p atomcode-coding -p atomcode-tui` 全绿（消费方）。
- `bash gates/compile.sh` 退出 0。
- `bash gates/layers.sh`、`bash gates/product-dir.sh` 全绿。
- `cargo fmt --all -- --check` 退出 0。
- `Cargo.lock` 逐字节未变；`hkdf` / `hmac` / `zeroize_derive` 三个 package 名均为 0。

## 不要做的事

- 不要改 `insert_parsed` 之外的任何现有注册表行为（覆盖诊断条件、警告记录、`diagnose` 调用）。
- 不要改 `SkillDiagnostic` 的字段或语义。
- 不要动工作区里**别人的**并行改动。
- 不要执行任何 git 写操作。
- 不要做「顺手」的额外重构或额外功能。
