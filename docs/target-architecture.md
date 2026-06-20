# 目标架构（北极星）— AtomCode 去 core 终极态

> 状态：方向性目标，非现状。现状是「旧引擎(atomcode-core) + 新栈(kernel/capabilities/coding) + bridge 绞杀缝」并存的迁移中途。
> 本文定义迁移**收尾后**的形态，作为后续每一步重构的对照基准。
>
> 终极态里 **`atomcode-core`、`atomcode-bridge`、以及过渡用的 legacy-protocol 全部消失**。

---

## 1. 依赖图

```
                         ┌──────────────────────────────────────────┐
  前端 / 接入层           │  tuix    cli    daemon    webui   <新前端>  │
  (Frontends)            └───┬─────────┬────────┬────────┬───────────┘
                            │         │        │        │
                  ┌─────────┴─────────┴────────┴────────┴──────────┐
                  │  业务专业化 L2 (一业务一 crate, 平级)            │
                  │  coding      review      <wechat>   <docs-agent> │
                  └───────────────────────┬────────────────────────┘
                                          │ (+ foundation 横向供给)
                  ┌───────────────────────┴────────────────────────┐
                  │  atomcode-capabilities (L1)                      │
                  │  feature 门控的能力池: provider/tools/web/mcp/    │
                  │  skills/session/memory/codeintel/atomgit         │
                  └───────────────────────┬────────────────────────┘
                                          │
                  ┌───────────────────────┴────────────────────────┐
                  │  atomcode-kernel (L0 运行时)                      │
                  │  Agent 循环 / 中间件 / hook / tool&provider trait │
                  └───────────────────────┬────────────────────────┘
                                          │
                  ┌───────────────────────┴────────────────────────┐
                  │  atomcode-protocol (叶子, 唯一对外契约)           │
                  │  Command / Event / Message / Conversation 纯数据  │
                  │  全 serde 可序列化, 可跨进程/网络                  │
                  └─────────────────────────────────────────────────┘

  横切 (任意层可依赖, 自身只依赖 protocol 或无依赖):
     atomcode-foundation   应用级共享基础设施 (config/i18n/auth/plugin/...)
     atomcode-telemetry    遥测 (叶子)

  依赖方向铁律 (编译期强制, 只能向下):
     protocol ← kernel ← capabilities ← L2 ← 前端
     foundation / telemetry 是侧叶子, 只被「上层」依赖, 自己绝不向上依赖
```

**关键点：箭头只能向下。** 任何反向依赖（如 capabilities 依赖某个 L2、kernel 依赖 capabilities）都是架构违例，应编译期挡掉。

---

## 2. 每个 crate 的职责边界

下表三列：**拥有什么** / **绝不能含什么**（治理的牙齿）/ **依赖谁**。

### `atomcode-protocol`（叶子 · 唯一对外契约）
- **拥有**：驱动 ↔ 引擎之间交谈的**纯数据**——`AgentCommand`、`AgentEvent`、`StopReason`、`RequestId`；`Message`、`Conversation`、`Role`、`ImageContent`、`ReasoningBlock`、`MessageMeta`、`SessionSnapshot`、`CompactReport` 等消息数据。全部 `#[derive(Serialize, Deserialize)]`。
- **绝不能含**：任何运行时（tokio、channel、JoinHandle）、任何 trait 行为（如 `CompactionStrategy` 这类策略 trait 留在 kernel）、任何业务词汇（approval/persona/plan/git/review）。
- **依赖**：无（纯 std + serde）。
- **为什么独立**：webui 后端、微信 Node 桥、未来 TS 客户端 codegen，只需类型不需运行时；运行时重构不波及任何接入方编译。**这是「协议在 kernel 家族」的正解，但它装中立类型，不是 core 的 legacy 类型。**

### `atomcode-kernel`（L0 · 中立运行时）
- **拥有**：`Agent` 循环 / `AgentHandle` / `AgentBuilder` / `Outcome`；`clock`、`middleware`、`hook`、`stream`；`Tool` 与 `LlmProvider` 的**抽象 trait**；`CompactionStrategy` trait；`conformance`/`testkit`。
- **绝不能含**：approval、persona、code-intelligence、任何具体 provider/tool 实现、任何业务逻辑。**试金石**：类型名里出现 approval/persona/plan/code/git/review → 不属于 kernel。
- **依赖**：`protocol`。
- **不变量**：`knows nothing about approval, persona, or code-intelligence`（保持现有 spike 注释的承诺）。

### `atomcode-capabilities`（L1 · 能力池）
- **拥有**：真 provider 适配器（OpenAI 兼容：GLM/DeepSeek…）、真 tools（fs/bash/grep/glob/web）、mcp、skills、session、memory、codeintel、atomgit REST。全部 **feature 门控**。
- **绝不能含**：atomcode-core（已写死「NEVER add core」）、任何 L2/前端、任何具体业务 persona。能力是中立的、可被任意 L2 复用的。
- **依赖**：`kernel`（+ `protocol`）。**配置/密钥靠注入**（构造参数），不自己读 config/auth 文件。
- **不变量**：每个能力可选；`default = ["provider","tools"]`；联网/codeintel/mcp 等按需 opt-in，嵌入方能取最小子集。

### `atomcode-coding` / `atomcode-review` / `<新业务>`（L2 · 业务专业化，平级）
- **拥有**：把 kernel + capabilities 子集**组装**成一个具体业务 agent。coding：persona/discipline/plan_mode/自纠错装配；review：评审规则。
- **绝不能含**：另一个业务的逻辑（coding 不含 review 词汇，反之亦然）；前端/UI 代码。
- **依赖**：`kernel` + `capabilities`（选 feature）+ 可选 `foundation`。
- **不变量**：**新业务 = 新 L2 crate，永不在 coding 里开分支**。微信助手、文档 agent 都是 coding/review 的平级兄弟。

### 前端 `tuix` / `cli` / `daemon` / `webui` / `<新前端>`（接入层）
- **拥有**：UI/渲染/输入（tuix）、参数与生命周期（cli）、HTTP/WS 服务（daemon/webui）。通过 `protocol` 的 Command/Event handle 驱动某个 L2。
- **绝不能含**：业务推理逻辑（属于 L2）、引擎实现（属于 kernel/capabilities）。
- **依赖**：某个 L2 + `protocol` + `foundation` + `telemetry`。
- **daemon 的特殊职责**：把 kernel handle 暴露成 HTTP/WS，让**非 Rust 业务**（TS webui、Node 微信桥）不链 Rust 即可接入——「协议出网」的适配器。

### `atomcode-foundation`（侧叶子 · 应用级共享基础设施）
- **拥有**：`config`、`i18n`/`locale`、`auth`（加载/存储 auth.toml）、`plugin`、`self_update`、`setup`、`notify`、`process_utils`、`input_history`、`telemetry_bootstrap`、`trace`、`live`。
- **绝不能含**：agent 运行逻辑、协议类型、业务逻辑。纯「应用怎么把自己跑起来」的横切。
- **依赖**：尽量无（最多 `protocol`）。**关键约束**：引擎需要的 config/auth 是**值注入**给 capabilities/L2 的——foundation 负责「读盘」，引擎只收到普通值，所以 foundation **不被** kernel/capabilities 依赖，只被 L2 装配处和前端依赖。

### `atomcode-telemetry`（叶子 · 横切）
- **拥有**：遥测上报。**依赖**：无内部依赖（保持现状叶子）。

---

## 3. 四类「业务对接」分别插哪层

| 场景 | 例子 | 插入点 | 动作 |
|---|---|---|---|
| 同 agent 换前端 | webui / 新 TUI / 移动端 | **protocol 层** | 进程内连 handle，或经 daemon 走 HTTP/WS |
| 新领域/新垂直 | 微信助手 / 文档 agent | **新 L2 crate** | 装配 kernel+capabilities，**不 fork coding** |
| 新能力 | 新工具 / 新 provider | **capabilities + feature** | L2 按需 opt-in |
| 嵌入方要子集 | 只 tools 不联网 | **capabilities feature 选择** | 依赖 kernel + 选定 features |

口诀：**任何「对接」需求，先归类到这四类的哪一类。绝大多数是「新 L2」，而不是动 kernel。**

---

## 4. 终极态会消失的东西

| 消失项 | 现状 LOC | 去向 |
|---|---|---|
| `atomcode-core` 旧引擎 | ~75k | 删除（kernel/capabilities/coding 已重写） |
| `atomcode-core` 共享设施 | ~16k | 搬入 `atomcode-foundation` |
| `atomcode-bridge` | ~1.5k | 删除（绞杀缝是脚手架，旧引擎一死即拆） |
| 过渡用 legacy-protocol | — | 不建则无；若建则最后删 |

**注意**：迁移期可能临时出现一个「抽 core 的 legacy AgentCommand/AgentEvent」的 `atomcode-protocol-legacy` 脚手架，用于让驱动早点脱离 90k core。它**不是**本文的 `atomcode-protocol`（中立契约），用完即删，别混淆。

---

## 5. 落地顺序（与本文对照）

1. **冻结协议面**：把 kernel 的 `event`/`message`/`request` 中立类型剥成 `atomcode-protocol` 叶子（数据进叶子，trait 行为留 kernel）。这是所有业务的绑定点，最先稳定。
2. **抽 `atomcode-foundation`**：把 core 的 config/i18n/auth/plugin/... 共享设施挪出，core 只剩纯引擎。
3. **驱动直迁 kernel 词汇**：把 tuix/cli/daemon 的 `core::agent::*` 调用点逐个迁到 `protocol`/`kernel`，bridge 当临时适配器。
4. **v2 默认化 + 删旧引擎**：最后一个驱动迁完 → 删 core 旧引擎 + 删 bridge。
5. core 清零，从 workspace 移除。

> 北极星不变量：**kernel 永远中立；协议是唯一对外契约；新业务一律新 L2 或新前端，绝不进 kernel。** coding 只是第一个业务。
