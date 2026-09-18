# tui 的「一切都是插件」测绘

2026-09-18。两个起因:

1. `/copy` `/save` 这批命令做完之后,用户问「不光是命令,学一学 harness 一切都是插件的
   思想,看能不能举一反三」。
2. 下游 fork 的三份测绘报告回来了,结论是**90% 的定制是「上游把本该是数据的东西写成了
   常量和裸字符串」**,并且点名新 tui 正在重犯 tuix 的老毛病(欢迎块的品牌行、mascot
   的字面调色板、只有一个 `ATOMCODE_ASCII` 逃生口)。报告的原话是「趁热改是十几行,
   等下游移植完再改就是再扫一遍 165 处字符串」。

这页先按一把尺子把 tui 里每样东西量一遍,标出哪些已经是行、哪些还是硬编码,再排要不要补。

## 尺子:什么才算「是插件」

harness 的约定有四条,少一条都不算:

1. **由行登记,不由装配函数硬写。** 装配函数提供槽,行填内容。
2. **登记者就是实现者。** 谁做这件事谁登记,不设领域专用的中央注册表
   (见 `feedback_describe_self_colocation`)。
3. **卸载是挂载的另一半。** `ctx.effect(...)` 在行卸载时把登记撤掉,不留悬挂项。
4. **冲突在挂载时拒绝,不靠顺序。** 两行争一个名字要当场报错;要覆盖,得显式说出来。

下游报告补上了第五条,针对的不是行而是行里的**内容**:

5. **产品身份是数据,不是常量。** 名字、图形、文案、能力假设,凡是「另一家发行版会不同」
   的东西,都应当是一个能从配置树里换掉的值。判别法:问「fork 要改这个得动几个文件」。

## 现状

| 东西 | 在哪 | 是不是行 | 说明 |
|---|---|---|---|
| 面板 | `rows.rs` `panel!` 宏 + `SCREEN` 清单 | ✅ | `[[insert]] disabled = true` 就关掉 |
| 流的生产者 / 视图 | `module.rs` `Modules::{add_view,add_producer,remove_*}` | ✅ | 由行挂,冲突拒绝,卸载对称 |
| 命令集 | `command.rs` `CommandSet` + `rows.rs` `commands!` 宏 | ✅ | 2026-09-18 补 `CommandSet::overrides`:下游可显式接管某一条,别的不动 |
| 能力自己的命令 | `commands.rs` `AgentCatalogCommands` | ✅ | 来自 agent 的 description,能力行自己登记(B1) |
| 用户自定义命令 | 每个 `user_invocable` 的 skill 自动成一条命令 | ✅ | 运行时通道,不必写 Rust |
| **按键** | `keymap.rs` `Keys` + `rows.rs` `DefaultKeysRow` | ✅ | 2026-09-18 补。原来是 `assemble()` 里一句 `keys.add(&Default_)`;现在是 `KeysSvc` + 一条行 + `Keymap::overrides` |
| **品牌 / mascot** | `content.rs` `Brand`/`Mascot` + `rows.rs` `BrandRow` | ✅ | 2026-09-18 补。名字、许可证、像素画与调色板都是值,宽度从画里算;`[[patch]] id = "tui-brand"` 改,不碰 Rust |
| **终端能力** | `caps.rs` `Overrides` + 表面行的 `unicode`/`colors`/`cell_background` | ✅ | 2026-09-18 补。原来只有 `ATOMCODE_ASCII` 一个逃生口,答三个问题里的一个 |
| 布局 | `LayoutSvc`:`Layout::{set,apply}` | ✅ | 行可以改区域树。`default_layout()`(`host.rs`)只是初值 |
| 浮层 | `overlay.rs` `Overlay` trait + `Overlays::open` | ✅ | 谁需要谁开 |
| 画布 | `SurfaceSvc` | ✅ | 缝。headless 与真终端同一个 trait |
| 位图 | `RastersSvc` | ✅ | 按 (module id, key) 寻址 |
| **问题的种类** | `ask.rs` `question_for` 的兜底 | ✅ | 2026-09-18 补。认不出的 kind 照 payload 画,而不是回 `None`——回 `None` 会让屏幕当着人说自己不能提问 |
| **注入的种类** | `content.rs` `INJECTIONS` 常量表 | ⚠️ | `/showinject` 只认表里那七种;下游加一种注入,折叠不认 |
| 主题角色 | `theme.rs` `Role` + `resolve(role, caps)` | — | 闭集,**有意如此**:颜色从终端自己的调色板推出来,开放会把「能力屏蔽层」捅漏 |
| 字形 | `caps.rs` `Glyph` | — | 同上,闭集是屏蔽层的一部分 |
| 日志事实 → 块 | `modules/transcript.rs` 的 `match` | — | `SessionEvent` 是内核的闭集,穷举 `match` 正是要的;要另起一套渲染的下游自己挂 `Producer` |

## 已补(2026-09-18)

### O1 按键成行 ✅

原来 `assemble()` 里一句 `keys.add(&Default_)`,而面板、命令、模块全是行——同一个 App
两套规矩。现在与 `Commands` 同形:`Keys` 内部加锁、`KeysSvc` 由 `ui-tui2` 提供、
`tui-keys-default` 行挂上并在卸载时撤掉、`Keymap::overrides` 允许显式接管某个键位。

判据 `a_row_takes_over_one_press_only_by_saying_so`,阴性对照是既有的
`two_rows_claiming_one_key_is_caught_at_mount_not_at_press`。

### O0 品牌、形象与能力假设成数据 ✅

下游报告的第 2 条。三处:

- `content.rs` 的 `let brand = "◆ AtomCode"` 与 `MASCOT_SOURCE`/`MASCOT_CELLS`/
  `mascot_colour` → 一个 `Brand { name, licence, mascot }` 值,`Mascot { rows, palette }`,
  **宽度 `Mascot::cells()` 从画里算**(报告特别点名不要 `MASCOT_CELLS` 常量)。
- `rows.rs` 的 `BrandRow`:`[[patch]] id = "tui-brand"` 带 config 就换掉,
  `mascot = false` 是没有吉祥物,字段留空则保留本版的值。
- `caps.rs` 的 `Overrides` + 表面行的 `unicode` / `colors` / `cell_background`:
  一支固定机型的车队在树里说一次,不必每台机器导一个环境变量。`ATOMCODE_ASCII`
  保留且仍然优先——它是个人在某一台坏终端上的逃生口。

判据 `another_build_can_call_itself_something_else`(自己的名字、自己的许可证、
自己的颜色、提示按新宽度对齐、没有吉祥物时不画)与 `a_build_can_say_what_its_terminals_do`。

### O2 认不出的问题也能问 ✅

`question_for` 返回 `None` 时,屏幕对人说「这个环境不支持提问」——那句话在 2026-09-18
被用户当 bug 报过一次(批量问题),当时按 kind 补了一条。真正的修法不是继续补 kind,
是让未知 kind 也能被画出来:payload 里有 `prompt`/`question`/`message`/`text` 就照着画,
`options` 有就用,没有就是「好 / 不了」;完全没有文字才算不是问题。开放的是**兜底**,
不是注册表——注册表会把「谁知道这种问题长什么样」从提问的那一方挪走,违反第 2 条。

`response_for` 的未知 kind 也从 `Null` 改成「人选的那个值」:`Null` 得留给「谢绝」,
否则提问方分不清「他说不」和「没人能问」。

判据 `a_kind_this_screen_never_heard_of_is_still_asked`。

## 还没补

### O3 注入种类(优先级低)

`INJECTIONS` 是常量表加一条「两张表必须一致」的判据,同时是默认折叠状态的来源。
要开放就得让行往里登记,代价不小,收益是下游的新注入能进 `/showinject`。等真有下游
注入时再做。

## 不做

- **主题角色、字形**:闭集是能力屏蔽层的设计,不是遗漏(见 `gates/tui.sh` 的
  「分层:OS 差异不得漏出屏蔽层」)。
- **把 `default_layout()` 做成可配**:0022 §8 去掉的可调布局说的就是这个。行仍可改
  区域树,那是装配期的自由,不是给人的旋钮。
- **`SessionEvent` → 块的注册表**:见上表最后一行。

## 不在这条线上(下游报告的其余条目)

报告里其余的条目都在这个 worktree 之外,列在这里免得丢掉。它们各自属于别的 crate 或
别的分支,应当各自成条,不要顺手塞进 M5.6:

| # | 在哪 | 一句话 |
|---|---|---|
| 1 | `feat/external-assembly` 分支 | 宿主注册自己的行(`HostState::with_plugins`)。报告说它是「其余所有『换一行实现』的前提」,**已做完、判据齐、未合** |
| 3 | `atomcode-coding` persona | 人设的身份句写死在 `persona.rs`;要么加 `identity`/`vendor` 字段,要么靠第 1 条整行 swap。注意 `ATOMCODE_PERSONA_PREFIX` 是会话恢复的识别键,改一个必须同步改另一个 |
| 4 | `atomcode-capabilities` | 25+ 处 `.atomcode` 裸字面量,常量 `distribution.rs` 早就有;配一条 CI 断言 |
| 5 | `atomcode-harness` policy 行 | 行声明了 `config = { allow, deny }`,插件却 `_config: &Value` 忽略 |
| 6 | `atomgit` | 做成一行(默认关),下游就不必物理删 8 个文件 |
| 7 | `atomcode-updater` | 版本号 `-N` 修订后缀被 `split('-').next()` 丢掉(`lib.rs:1107`)。报告说「tui 侧 version_check.rs 还有第二份」——核实过,那一份在 **tuix** 里,随 6.4 一起删,所以只剩 updater 这一处 |
| 8 | `atomcode-config` | 发行版级开关:`update_enabled()`、遥测默认、`DEFAULT_LOCALE`、环境变量别名 |
| 9 | 多个 crate | 7 处裸品牌串没走已有的 `{brand}` 机制 |
| 10 | i18n | CodingPlan 的 57 条商业词条,`{brand}` 覆盖不了 |
| 11 | 插件市场 | `BOOTSTRAP_MARKER_FILENAME` 应从市场列表内容哈希派生 |

## 与已有计划的关系

不冲突,是 M5.6 的同一片区域:

- O0、O1、O2 都不改 0021/0022 的任何结论,只是把「行贡献」与「身份是数据」这两条约定
  补齐到最后几处。
- `CommandSet::overrides` 与 `Keymap::overrides` 是同一个形状,后者照抄前者。
- O3 与 B2 的补全菜单/状态行无关,互不挡路。
