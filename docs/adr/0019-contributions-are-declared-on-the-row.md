# 贡献者要能被声明:把 `apply` 里的 register 提到行上

状态: 提议(2026-09-14)。**取代**先前那版「容器收集贡献 / 给 `ServiceKey` 加元素类型」的
设计——那版已作废,理由见 §5。

## 背景:问题是真的,但不在机制上

`--dump-seams` 那张图**只画 `provide()`**,因为 `seam_map.rs:140-148` 只读
`Plugin::{provides, inject, uses}`。而往聚合体里塞东西的动作**全在 `apply` 体内**:

| 位置 | 做什么 |
|---|---|
| `crates/atomcode-tui/src/plugin.rs:1442,1453` | 提示词片段 + `adjust_layout` 工具 |
| `crates/atomcode-harness/src/plugins/tools.rs:35`、`world_tools.rs`、`team.rs:525`、`recall.rs:326`、`capabilities.rs:307` | 工具注册 |
| `contribute_prompt(...)` 共 24 处(`tools.rs` / `world_tools.rs` / `team.rs` / `persona.rs` / `self_knowledge.rs` / `findings.rs` / `ask.rs` / `policy_rows.rs` / `session.rs` …) | 提示词片段 |
| `session.rs:146-147` | 两个 session 投影 |

总量:按文件计 `plugins/mod.rs` 78 处、`tools.rs` 7 处、`capabilities.rs` 6 处、
`atui.rs` 4 处、`team.rs`/`self_knowledge.rs` 各 4 处……**这些没有一个出现在那张图上。**

后果是具体的:

- `--dump-seams` 说 `tools` 的提供方是 `["tools"]`(那个建 `ToolBox::new()` 的行)——
  而真正往里放工具的是另外十几个行,图上没有;
- 于是「这个工具谁给的」只能 grep;
- 一个工具也无法被单独开关(它不是一个可 patch 的行)。

## 决策

### 1. 不造机制:贡献者就是行,声明它贡献什么

参考实现(dsh,`~/project/deepseek-harness`)的做法是**命令式注册**:
`packages/fs/tool-fs/src/read.ts:68` 的 `applyReadTool` 里
`ctx.tools.register(defineTool({…}))` + `ctx.systemPrompt.section({…})`,和 atomcode 的
`tools.register` + `contribute_prompt` **一模一样**。它那张图准,靠的不是声明贡献,而是:

```ts
// scripts/gen-doc-graphs.ts:716
assertServiceRolesComplete(services)   // 扫源码得到所有 ctx.<key>,断言都在手写表里分类过
```

**扫源码发现 + 手写表分类 + 断言对齐**,表里的 providers/consumers 是**手写维护**的。

atomcode 已经有自己的等价物(声明 + 闸门),所以这条路不抄。要改的是**一个具体的缺口**:

> 有 `provides()` 能声明的行,图上就有;只在 `apply` 体内注册的行,图上就没有。

所以决策是:**把「贡献」写成行级声明,让 `seam_map` 读得到**。具体到三类:

| 今天 | 改后 |
|---|---|
| `tools` 行建目录,十几个行往里 register | 每个贡献者声明 `provides = ["tools"]`(它已经在了),并在声明里带上**贡献的项名** |
| `adjust_layout` 是 tui 行 `apply` 里的匿名值 | `tui-layout` 成为**它自己的行**(`provides = ["tools", "system-prompt"]`),`adjust_layout` 与 `tui-layout` 那段提示词由它贡献 |
| 24 处 `contribute_prompt(ctx, "id", order, text)` | 行声明 `provides = ["system-prompt"]` + 贡献项名;顺序与文本仍在 `apply`(它们是数据,不是接线) |

### 2. `Plugin` 加一个「我贡献了哪些具名项」的声明

```rust
fn contributes(&self) -> &'static [&'static str] { &[] }
```

语义:这一行会往它 `provides` 的那些槽里放**这些具名项**。它**不改装载、不改值**——
值仍在 `apply` 里交出去(那里才有 `ctx`)。它只让 `seam_map` 与 `--audit` 看得见。

于是:

```rust
impl Plugin for TuiLayoutPlugin {
    fn name(&self) -> &'static str { "tui-layout" }
    fn inject(&self) -> &'static [&'static str] { &["tools", "system-prompt"] }
    fn contributes(&self) -> &'static [&'static str] { &["adjust_layout", "layout-perception"] }
    async fn apply(&self, ctx: &Context, _cfg: &Value) -> Result<(), String> {
        // 与今天同样的两行:contribute 提示词、register 工具。
        // 差别只在于现在有东西声明它们的存在。
    }
}
```

### 3. `--dump-seams` 多一列「contributions」

```
tools  — The live tool catalog            [Core]
  provided by:  tools, tool-fs-world, tool-search-world, tui-layout, team, recall, …
  contributing: adjust_layout (tui-layout), read_file (tool-fs-world), recall (recall), …
  consumed by:  agent-loop
```

聚合体持有者(`tools`)与**贡献者**分开列——这正是今天那张图丢掉的信息。

### 4. `--audit` 加一条一致性检查

**声明了 `contributes` 的行,必须真的贡献了那些名字**;反之,一个 `apply` 里
`register`/`contribute` 了东西、却没在 `contributes` 里声明的行,**应当被判红**。
后半条是这条设计能否维系的唯一保证——否则又是一张靠人记的表(dsh 靠
`assertServiceRolesComplete` 断言,这里是同一个位置)。

可执行的做法:让 `tools::mount` / `contribute_prompt` 这两个**唯一的写入口**记录
「谁写的、写了什么」(它们已经有 `ctx`),`--audit` 把它与 `contributes()` 对照。
两个入口就是两个地方,不必给每个调用点加参数。

## 与 ADR 0018 的关系

这条**不**满足原则 8 的字面(「接线的建立由 host」),但它是 dsh 与这个仓库共同的形态:
**值由行交出,声明由容器收集**。ADR 0018 §3 已把这一点记为「原则 vs 实况」的分歧,
本设计是那个分歧下**可做的那一半**——不搬 `register`,但让被注册的东西**可声明、可检、
图上可见**。

「UI 不接 Agent 细节」由**另一条**解决(把缝挪到中立位置,使 `atomcode-tui` 不依赖
`atomcode-harness`),那条属于 ADR 0018 的 L1,与本设计正交。

## 权衡过、没做的

- **容器收集贡献**(`ServiceKey` 加元素类型 + 折叠契约、`provide` 支持多值):
  **作废**。理由见 §5。
- **扫源码发现贡献**(抄 dsh 的 `assertServiceRolesComplete`):Rust 没有反射,
  `apply` 体里的 `register` 在编译期不可见——**做不到**。dsh 能做是因为 TS 的 AST 可扫。
  这是语言差异,不是取舍。**这条正是本设计必须走「显式声明」的原因。**
- **给每个贡献项各建一行**(`adjust_layout` 一行、每个 prompt 片段一行):最彻底,
  可单独 patch,但会导致 ~24 个行只为一段文本存在,`--dump-config` 噪音过大。取中道:
  **行为行,贡献项名在声明里列**。

## 未做

- 本页只定形状。落地要动:`Plugin` trait(加 `contributes`)、`seam_map`(多一列)、
  `audit`(一致性)、`tools::mount` 与 `contribute_prompt`(记录写者)、
  `tui-layout` 拆成一个行。量级中等,但碰 `--dump-seams` 的输出格式与
  `tests/seam_convention.rs` 的既有断言。
- 上游未定:ADR 0018 的 L1(协议中立化)与本设计正交,谁先做都可以;但
  `tui-layout` 拆行会让 UI crate 多一个行——**如果 L1 也做,那行应当写在中立侧**。

## §5 为什么作废了「容器收集贡献」

先前那版设计的核心是「插件声明贡献值,容器折叠成聚合体」。查 dsh 后作废,三条:

1. **参考实现没有这个机制。** dsh 的 `tool-fs` 是命令式
   `ctx.tools.register(...)` + `ctx.systemPrompt.section(...)`,与 atomcode 同形。
2. **`Bundle` 不是那个意思。** dsh 的 `SERVICE_ROLES` 里 `bundle` 只有一个实例
   (`ctx.agentLoop` / `dsh-agent-loop`),note 写着 "The one concrete loop plugin;
   extension packages depend on dsh-agent events and services, not on this package"——
   它的意思是**由一个具体组合包填的槽**,而 atomcode 的注释抄成了「something that exists
   so other rows can attach to it, not something with behaviour of its own」,**方向相反**。
   所以「激活 `Bundle`」这个选项不存在:它是个抄歪的注释,0 实例。
3. **它要动的面比收益大。** 给 `ServiceKey` 加元素类型 = 27 个键里凡有贡献者的都要改,
   且要新定义折叠顺序/重名/realm 覆盖三条语义;而它换来的只是「图上可见」——
   而**不必改 plexus 也能让图上可见**(本设计 §2)。
