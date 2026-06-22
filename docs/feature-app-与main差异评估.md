# `feature/app` 与 `main` 差异评估

> 截至 2026-06-11（`feature/app` = `143514d5`，`main` = `7395b295`，即 v4.25.0 发布合并点）。
> 回答的问题：**这个分支对 core 等核心模块的改动大不大？合并/跟随官方升级的风险有多高？**
>
> 结论先行：**改动很小且几乎全部是纯新增**。与 main 相比共 6 个提交、11 个代码文件、
> `+898 / -95` 行（另有 143 行文档）。core 实质性改动只有 1 个文件（`live/mod.rs`），
> 不触碰对话引擎、工具系统、provider、上下文管理等任何核心域；已经历一次 main 合并，
> 仅需 1 个小适配提交，验证了冲突面可控。

---

## 1. 总量

| 维度 | 数值 |
|---|---|
| 领先 main 的提交 | 6 个（含 1 个 merge main、1 个纯文档） |
| 改动代码文件 | 11 个 |
| 代码行 | **+898 / -95**（不含文档；-95 里大部分是同文件内的逻辑搬家，非删功能） |
| 新增外部依赖 | **0**（仅 tuix 的 tokio 加了 `process` feature） |

提交清单：

```
143514d5 feat(app): 断线恢复进行中回合 + 手机远程命令 + 默认生产中继
f94ec9e8 feat(app): 手机点开历史对话→桌面 cd 并恢复同一会话(/cd 带 session_id)
a20fd5e3 feat(app): /app 启动 relay-client 时透传全局接入口令 --register-secret
173b870e fix(app): 合并 main 后 ensure_live_session 更名+参数顺序对齐
14149548 Merge origin/main into feature/app
5107e7d9 feat(app): 新增 /app 命令(移动端经中继远程访问 + 同会话双向同步)
```

## 2. 按模块拆解

| 模块 | 行数 | 改动文件 | 性质 |
|---|---|---|---|
| **atomcode-core** | +186 / -4 | 2 个 | 纯新增（见下） |
| **atomcode-daemon** | +165 / -5 | 2 个 | 新端点/新函数 + 1 个请求体加可选字段 |
| **atomcode-tuix** | +544 / -85 | 5 个 | 大头；全部在 TUI 层（命令 arm + 事件转发） |
| atomcode-cli | +3 / -1 | 1 个 | 枚举穷尽匹配补 2 个忽略 arm |

### 2.1 atomcode-core（核心模块，重点回答）

只动了 2 个文件，**全部是增量扩展，零行为变更**：

- `src/agent/mod.rs`（+12/-0）：`AgentEvent` 枚举加 2 个新变体
  （`SessionSwitched`、`RemoteSlashCommand`），纯定义，只有 live-sync 路径会产生。
- `src/live/mod.rs`（+174/-4，其中约 50 行是新增测试）：
  - `LiveEvent` 加 4 个变体（会话切换 / 远程命令 / 命令输出）+ 对应 4 个 `notify_*` 方法；
  - `LiveSession` 加 `turn_buffer` 进行中回合回放缓冲 + `join_with_replay()`
    （修"手机退后台回来丢执行过程"）。协调器内用 tap 通道转发执行器事件，
    `run_turn` 的 trait 签名**未改**；既有 `join()`/`subscribe()` 行为不变。
  - `-4` 是协调器 turn 边界处快照刷新挪进同一临界区（语义等价）。

**不涉及**：conversation、turn/TurnRunner、tool、provider、ctx、session 存储、auth、
pricing 等所有核心域 —— 一行未动。

### 2.2 atomcode-daemon

- `lib.rs`（+101/-4）：新函数 `ensure_app_server`/`stop_app_server`（/app 的回环
  server，复用既有 `run_server`）；`ChangeDirRequest` 加可选 `session_id`
  （`#[serde(default)]`，老客户端不受影响）；新路由 `/live/command`。
- `live_api.rs`（+64/-1）：`to_wire` 补新事件 arm；`live_stream` 在 snapshot 后下发
  回放（对老前端就是普通事件，协议兼容）；新 handler `live_command`；
  `live_switch_session` 等 3 个小函数。

### 2.3 atomcode-tuix（改动大头，但都在 UI 层）

- `event_loop/commands.rs`（+524/-85）：`/app` 命令 arm（约 200 行，含二维码/拉起
  relay-client 子进程）；`execute_slash_command` 包一层输出镜像（CaptureRenderer）；
  4 个信息类命令（status/cost/whoami/diff）的文本构建抽成共用函数（-85 主要来源，
  逻辑搬家非删除）；会话恢复/LiveSession 重播种两个 helper。
- `event_loop/live_sync.rs`（+29）/ `event_loop/mod.rs`（+70/-15）：新 LiveEvent →
  AgentEvent 映射 + 3 个处理 arm（项目/会话跟随、远程命令）。
- 其余 3 个文件合计 +6 行（注册命令名、LoopCtx 加一个字段、Cargo feature）。

## 3. 合并/升级风险评估

| 风险点 | 评估 |
|---|---|
| 跟随官方 main 升级 | **低**。已实测一次合并 main（`14149548`），仅 1 个 20 行的适配提交（官方把 `ensure_live_session` 改名+调参数顺序）。 |
| 冲突高发文件 | `event_loop/commands.rs`（官方改动频繁）。但本分支的改动集中在独立的 `/app` arm 与新增函数，与官方 arm 间冲突多为相邻行级，好解。 |
| 协议兼容 | 全部向后兼容：`/cd` 新字段带 serde default；`/live` wire 新事件类型，老前端按未知类型忽略；snapshot 帧结构未动。 |
| 行为回归面 | 非 sync 模式（纯终端用户）零路径变化：所有新逻辑都挂在 `sync_session.is_some()` / 新端点 / 新事件之后。 |
| 测试 | core live 8 个单测全过（含新增回放缓冲并发测试）；tuix 802 过（3 个失败为 main 上既有的环境相关 skills 测试）。 |

## 4. 一句话给评审

这是一个**加法分支**：core 只加了事件定义和一个回放缓冲（1 个文件有实质逻辑），
daemon 加了一个回环 server 入口和两个端点，大头在 TUI 命令层；不改 turn 引擎、
不改工具与权限模型、不加依赖、全协议向后兼容，官方升级跟随成本已被一次真实
merge 验证为"一个小适配提交"。
