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
`tools.register` + `contribute_prompt` **一模一样**。它那张图准,靠的不是声明贡献,而是**静态发现 + 手写表 + 断言**:

```ts
// scripts/gen-doc-graphs.ts:716
assertServiceRolesComplete(services)   // 断言每个发现的 ctx.<key> 都在手写表里分类过
```

注意 `services` **不是 grep 出来的**:它来自 `@deepseek-ai/dsh-typert-generator`
(`packages/typert/generator/src/analyzer.ts` 用 `ts.createProgram` / `ts.SourceFile`,
即 TypeScript **编译器 API + 类型检查器**)。这一点要紧——因为要拿到「贡献的**名字**」,
必须做类型级连接:atomcode 的 `toolbox.register(Arc::new(ReadFileTool::with_world(…)))`
里,名字在另一个文件的 `impl Tool::name()` 里,grep 拿不到。而 dsh 的
`ctx.tools.register(defineTool({ name: 'read', … }))` 名字是内联字面量,好办。
**语言差异决定了不能照抄这条路**,不是取舍。

atomcode 已经有自己的等价物(声明 + 闸门),所以这条路不抄。要改的是**一个具体的缺口**:

> 有 `provides()` 能声明的行,图上就有;只在 `apply` 体内注册的行,图上就没有。

所以决策是:**把「贡献」写成行级声明,让 `seam_map` 读得到**。具体到三类:

| 今天 | 改后 |
|---|---|
| `tools` 行建目录,十几个行往里 register | 每个贡献者声明 `provides = ["tools"]`(它已经在了),并在声明里带上**贡献的项名** |
| `adjust_layout` 是 tui 行 `apply` 里的匿名值 | `tui-layout` 成为**它自己的行**(`provides = ["tools", "system-prompt"]`),`adjust_layout` 与 `tui-layout` 那段提示词由它贡献 |
| `contribute_prompt(ctx, "id", order, text)` 的 ~24 处调用 | **不用改**:它们全部经由 `tools.rs:43` 这一个入口,由入口记录 |

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

可执行的做法:让**入口**记录「谁写的、写了什么」。两个入口已经在 `ctx` 里,而且本来
就知道名字:

```rust
// plugins/tools.rs:28 —— 工具的唯一入口
pub(super) fn mount(ctx: &Context, tools: Vec<Arc<dyn Tool>>) -> Result<(), String> {
    for tool in tools {
        let name = tool.name().to_string();     // ← 名字就在手边
        toolbox.register(tool)?;
        record(ctx.entry(), name);              // ← 新增:记在行上
        …
    }
}
// plugins/tools.rs:43 —— 提示词片段的唯一入口
pub(super) fn contribute_prompt(ctx: &Context, id: &str, rank: i32, text: &str) {
    prompts.contribute(id, rank, text);
    record(ctx.entry(), id);                    // ← 新增
    …
}
```

`Context::entry()`(`plexus/src/context.rs:69`)返回行 id,所以这是**运行时事实**,
不是静态推断。它比静态扫描更硬:守卫(`if let Some(tools) = …`)、`ctx.inject` 的延后
贡献、realm 覆盖,**都自动是对的**——没真注册的行就没有记录。

**落地清单(实测,不是估计):**

| 类别 | 数量 | 位置 |
|---|---|---|
| 工具入口 | 1 | `tools.rs:28` `mount` |
| 提示词入口 | 1 | `tools.rs:43` `contribute_prompt` |
| **绕过入口的真贡献** | **4** | `self_knowledge.rs:367`、`recall.rs:326`、`capabilities.rs:307`、`tui/plugin.rs:1453` |
| 第二个注册表(要加第三个入口) | 2 | `session.rs:146-147`(`SessionProjections`) |
| **不是贡献,不用管** | — | `mod.rs:47-124`/`atui.rs:30-32` 是**插件目录**注册;`team.rs:525`/`subagent.rs:80` 是从父目录**复制**进受限盒子(`for name in &self.allowed_tools { … restricted.register(tool) }`) |

所以工作量是「**收回 4 个绕过点 + 给 projections 加一个入口**」,不是先前说的
「改 24 处」。

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
- **扫源码发现贡献**(抄 dsh 的静态发现 + `assertServiceRolesComplete`):dsh 用的是
  TypeScript **编译器 API + 类型检查器**(`typert/generator/src/analyzer.ts`),不是 grep
  ——因为要把 `register(X)` 连到 X 贡献的**名字**上。Rust 侧要么有等价的类型级分析
  (得写 proc-macro 或 rust-analyzer 级工具),要么**根本不扫**:入口在运行时记录
  (§4)已经给出更硬的事实。**所以不是"做不到",是"不需要"。**
  若坚持静态:grep 有三个具体失败模式——(a) 算不出归属哪个行(一个文件常含多个插件:
  `world_tools.rs` 两个、`tools.rs` 四个);(b) 算不出贡献的名字(`ReadFileTool::with_world`
  的名字在另一个文件);(c) 不知道有没有真的注册(守卫 / `ctx.inject` 延后)。
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
