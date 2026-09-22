# atomcode-tui 是无头前端,不是 tuix 的替代

状态: **已被 [`0012`](./0012-atomcode-tui-replaces-tuix.md) 取代(2026-09-12)**。
下面的「失效条件」第一条已触发:plexus 线的目标改成替换现有栈。「直接用 tuix
为什么不是省事的选项」一节和那三个数字仍然有效,作为工作量估算保留。

原状态: 已决定(2026-09-06)

## 背景

我用一整个会话把 `atomcode-tui` 往「产品 UI」的方向推:插件化的面板行、
flex 布局、终端能力屏蔽、第二层交互件,最后照着两张真实截图对齐 tuix 的
调色板、字形和布局。

用户叫停:「方向不对,工作量实在是太大了。」

数字支持他:

```
atomcode-tui    12,472 行
atomcode-tuix  117,499 行     ← 9.4 倍
```

而且我做的是容易的那 10%。剩下的是 16 个模态、60 个命令、retained 渲染、
resize、diff 查看器、onboarding 向导、插件管理、用量面板。

## 「直接用 tuix」为什么不是省事的选项

`tuix::run` 吃一个 `SpawnedRuntime`。两个方向都不是接口耦合,是**具体类型耦合**:

```rust
ReadyRuntimeControl { handle: CodingRuntimeHandle, … }   // 命令：具体句柄，不是 trait
RuntimeEventPayload::Native(CodingRuntimeEvent)          // 事件：coding 的类型
DriverEvent::SessionTransitionFinished {
    operation: atomcode_coding::ReconfigureKind, … }     // 连 driver 事件都是
```

量出来的规模:

| | 数量 |
|---|---|
| tuix 调用的 `CodingRuntimeHandle` 方法 | **20** |
| tuix 里 `atomcode_coding::` 类型的出现处 | **440** |
| 匹配 `RuntimeEventPayload` 的处 | **141** |

那 20 个不是通用的 agent 操作,是 coding 特有的:`snapshot` / `restore_snapshot` /
`rewind_points` / `undo_to_prompt` / `mcp_tools` / `withdraw_mcp_tools` /
`reassemble_provider` / `deactivate_provider` / `context_stats`。harness 要顶上
这条缝,得先把其中好几个**当作独立功能实现出来**——那不是适配代码。

半通的那一半:`RuntimeEventPayload::Ui(AgentEvent)` 收的是内核的 `AgentEvent`,
所以**显示方向能喂,控制方向不能**。一个只能看不能打字的 tuix 没有用。

结论:tuix 和 `atomcode-coding` 不是「UI 依赖一个接口」,是同一件东西的两半。

## 决策

**tuix 继续是产品 UI,不动它。`atomcode-tui` 定位为 harness 的无头前端。**

它今天就已经在做一件 tuix 做不了的事,而且这才是它该被评价的地方:

* 20 条端到端测试驱动完整 UI,**不需要 tty、不需要模型、不需要人**
* `--audit` 在没有终端的机器上检查装配
* `--demo` 打印一整帧,用来看渲染,也用来在 CI 里当回归
* 无头 surface 是一行,和终端 surface 平级可换

一个 `ui` 缝后面挂两个前端,本来就是那条缝存在的意义。**错的是我想让其中一个
取代另一个**——而那个念头是我自己加的,不在任何人的要求里。

## 因此不再做的事

* 不再往 tuix 的外观上对齐。已经做完的对齐(角色调色板、`●`/`⎿`、`❯`、
  底部状态行)留着——它们让代码本身更好(颜色跟随用户终端主题、能力注入),
  但它们不是一条要继续走的路。
* 不搬那 16 个模态、60 个命令。
* 不做启动横幅、流式计时行、上下文窗口百分比。

## 保留的事

* 三层结构与两条闸门:它们守的是**这个 crate 自己**能不能被测,和外观无关。
* `--demo` 与端到端:这是它的产出物。
* 本会话 harness 侧的成果(`describe_self`/`operations`/`settings`、`recall` +
  会话按项目分桶、`memory` 工具)与本决策无关,继续有效,tuix 那边也能用——
  它们是 harness 的行,不是 UI。

## 失效条件

* 若 plexus/harness 线的目标变成**替换现有栈**,这条决策就要重议:那时 UI 必须
  跟上,而账单是上面那三个数字。
* 若 tuix 侧愿意把 `RuntimeControl` 改成一条 trait 缝,B 的成本会重新可算 ——
  但那是 tuix 的重构,不是 harness 的桥。
