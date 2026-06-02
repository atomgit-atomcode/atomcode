# atomcode webui 设计（Phase 1 本地）

- 日期：2026-05-29
- 状态：已通过头脑风暴，待实现计划
- 作者：brainstorming 协作产出

## 背景与动机

atomcode 目前是 CLI + TUI 工具。TUI 在不同终端/平台存在兼容性问题，且不少用户不习惯
TUI 交互方式。目标是提供**另一种使用入口**：用户在 TUI 中输入 `/webui`（或命令行
`atomcode webui`）即可启动本地浏览器界面来对话、运行 agent、执行工具。

webui **不取代 TUI**，二者并存，webui 需功能基本完整（聊天、跑 agent、交互式工具批准）。

远期目标（Phase 2，本 spec 仅留蓝图）：每个用户拥有一个域名访问本地 atomcode，形态为
**官方中转隧道**。

## 关键决策（来自头脑风暴）

| 决策点 | 结论 |
|--------|------|
| 定位 | TUI 之外的并行入口，功能基本完整，不取代 TUI |
| 打包 | 前端构建产物用 `rust-embed` 嵌入二进制 |
| 后端 | 复用现有 daemon 服务逻辑（方案 A），webui 是它的第二个前端 |
| **服务形态** | **合进主程序进程内启动**：daemon crate 抽成库暴露 `run_server(...)`，`atomcode` 进程内 `tokio::spawn` 直接跑 server，**不依赖独立 `atomcode-daemon` 二进制**（普通用户 `curl \| sh` 只装主程序，没有 daemon） |
| 权限 UX | 交互式审批（浏览器逐次批准/拒绝工具调用） |
| 会话模型 | Phase 1：webui 会话与 TUI 会话各自独立运行，共享磁盘 session 历史，可互相 resume |
| **实时同步 (Phase 1.5)** | **方案 A 进程内「活动会话总线」**：webui 默认独立，提供「同步/接管当前 TUI 会话」开关；开启后 TUI 与浏览器是同一活动会话的两个视图，输入双向、渲染实时 |
| 工作目录 | 每会话独立 cwd，随 `/chat` 请求带 `working_dir`；切换不影响其它客户端。全局 `/cd` 仅作可选「设默认目录」 |
| 远期远程 | 官方中转隧道（出站连官方中转，子域名反代） |

## 现状事实（实现时依赖）

- `crates/atomcode-daemon`（axum）默认绑 `127.0.0.1:13456`，已提供：`/chat`(SSE 流式)、
  `/sessions`、`/config`、`/auth/login`、`/models`、`/mcp`，已带 CORS 层。
- daemon 当前用 `AutoPermissionMode::BypassAll`（`main.rs:2051`）——会自动批准所有工具调用，
  必须替换为交互式 decider。
- TUI（tuix）进程内跑 agent（`TurnRunner`），与 daemon 是独立进程。
- **`atomcode` 主程序已是 `#[tokio::main]`（CLI `main.rs:698`），依赖含 `tokio (full)` + `reqwest`**——
  进程内起 axum server 零额外成本，直接 `tokio::spawn`。
- **`atomcode-daemon` 是完全独立的二进制，单独发布；`install.sh` 每平台只下载 `atomcode` 主程序，
  不含 daemon；`atomcode-cli` 不依赖 daemon crate**——故必须把 server 逻辑下沉为库供主程序调用。
- `atomcode daemon` 子命令（CLI `main.rs:933+`）目前 re-exec 独立二进制；改造后它与 webui、VSCode
  都调同一个 `run_server`。
- daemon 已有 `/cd`(POST)、`/project`(GET)、`/projects`(GET)；`ChatRequest` 已支持可选
  `working_dir`（`main.rs:1782` 不带才回退全局）。**无文件系统目录列举端点**（需新增 `/fs/list`）。
- 已有 `atomcode_core::auth::oauth::open_browser(url)`（按平台 cfg）。
- 内置斜杠命令在 `event_loop/commands.rs` 的 `match cmd` 分发。
- 现有 `site/` 前端栈为原生 HTML + Tailwind。

## 架构与进程模型

```
┌──────────┐   /webui    ┌─────────────────────────────┐
│  TUI 进程 │ ──────────▶ │ 1. 探测 :13456 健康检查        │
│ (tuix)   │             │ 2. 没跑则 spawn atomcode-daemon│
└──────────┘             │ 3. 生成一次性 token            │
                         │ 4. open_browser(127.0.0.1:    │
                         │    13456/?token=xxx)          │
                         └──────────────┬────────────────┘
                                        │
┌───────────────────────┐   HTTP/SSE   ▼
│ 浏览器 (embedded SPA)  │ ◀──────▶ ┌──────────────────────┐
│  - 聊天/流式            │          │ run_server()（daemon 库）│
│  - 工具批准卡片         │          │  - 复用 /chat /sessions  │
│  - 会话侧栏/目录切换/配置 │          │  - 静态资源+权限流+/fs/list│
└───────────────────────┘          │  - 进程内 TurnRunner     │
                                    └──────────────────────┘
       （以上整体在同一个 atomcode 进程内 · tokio::spawn）
```

webui server 与 TUI 跑在**同一个进程**：daemon 服务逻辑下沉为库函数 `run_server(...)`，
`/webui` 在已有 tokio runtime 上 `tokio::spawn` 起它——不 spawn 子进程、不依赖独立二进制
（普通用户只装了 `atomcode` 主程序）。独立 `atomcode-daemon` 二进制与 VSCode 扩展仍在，
改为同样调用 `run_server`。`/webui` 包装的三步：起 server（若未起）→ 生成 token → 开浏览器。

## 组件划分

### 前端 `webui/`（新目录，独立构建）

- 技术栈：**Preact + Vite + Tailwind**。产物为纯静态 `dist/`，runtime 体积小（~10-20KB），
  适合 embed；Tailwind 与现有 `site/` 一致。
- 模块：聊天流式视图、工具批准卡片、会话侧栏/切换、配置表单、登录入口。
- 构建产物用 `rust-embed` 打进 daemon。

### daemon crate 改造（库化）

- 抽 `lib.rs` 暴露 `pub async fn run_server(opts: ServerOpts) -> anyhow::Result<()>`（含 Router +
  AppState 构建 + bind + serve）；原 `main.rs` 变薄壳调它。VSCode/独立二进制路径不变。
- `mod webui`：`rust-embed` 静态资源 handler。`GET /` 命中嵌入资源，未命中路由 fallback 到
  `index.html`（SPA 路由）。
- dev 模式：环境变量 `ATOMCODE_WEBUI_DEV=http://localhost:5173` 时重定向到 vite dev server。
- `/chat` 把 `BypassAll` 换成已有的 `InteractivePermissionDecider`，新增 `/chat/permission` 回送决定。
- token 鉴权中间件：**可插拔**，本地 token 与远期账号 token 共用一条校验链（为 Phase 2 预留）。
- 新增 `GET /fs/list?path=`：列子目录（`~` 展开 + loopback + 越权防护），供前端目录浏览器。

### 主程序（atomcode-cli）改造

- 依赖 daemon crate（lib）。`/webui` 与 `atomcode webui` 都在进程内 `tokio::spawn(run_server(...))`，
  用进程内单例（`OnceLock`）保证只起一次。
- 前端 `rust-embed` 嵌入在 **daemon crate**（`webui.rs` 内随 `run_server` 服务）；主程序依赖 daemon
  库即自动包含，无需在 cli 重复嵌入。
- `event_loop/commands.rs` 加 `"webui"` 分支 → `ensure_server_and_open()`：起 server（若未起）→
  mint token → 开浏览器。
- `atomcode webui` 子命令：命令行直接启动 + 开浏览器（headless，不进 TUI）。
- `/webui stop`：停掉进程内 server 任务（abort spawn 的 handle / 触发 shutdown watch）。

### 体积控制

- webui server + axum + 嵌入前端约 +1~3MB。用 cargo feature `webui`（默认开）门控，瘦身构建可关。

## 数据流：聊天 + 交互式权限

聊天复用现有 `POST /chat` 的 SSE 流。交互式工具批准新增双向流：

1. agent 要调危险工具 → 复用 core 的 `InteractivePermissionDecider` 发 `ApprovalRequest`，
   server 的 SSE 循环把它转成 `permission_request` 事件（含 tool_name、reason、call_id、参数）
   下发，decider **阻塞等待**其 `response_rx`。
2. 前端弹审批卡片，用户点批准/拒绝/总是允许。
3. 前端 `POST /chat/permission { session_id, decision }`。
4. server 经 `PermissionResponders`（`session_id -> mpsc::Sender<PermissionDecision>`）把决定
   送回该 session 的 decider，唤醒它，agent 继续。
5. 超时（如 5 分钟无响应）默认拒绝。

决定粒度对齐 TUI：`Approve` / `Deny` / `AlwaysAllow`（本会话该工具，由 `PermissionStore` 承载）。

## 工作目录切换

- **语义：每会话独立 cwd**。前端为当前会话保存 `working_dir`，每次 `POST /chat` 带上它
  （DTO 已支持）；切目录只改前端状态，**不影响同时连着的 VSCode 或另一个 webui 标签页**。
  resume 历史会话时用该 session 存档里的 `working_dir`。
- **选择方式**（三层，叠加）：
  1. 最近项目下拉——复用现有 `GET /projects`（从 session 历史聚合的目录）。
  2. 手动路径输入——支持 `~` 展开（复用 `/cd` 的展开逻辑）。
  3. 目录浏览器——新增 `GET /fs/list?path=` 列子目录，前端做面包屑 + 文件夹点选。
- **入口**：顶栏 cwd 面包屑（可点）+ 新建会话时的目录选择器。
- **可选全局**：切换器底部 checkbox「同时设为 daemon 默认目录」勾选时才调 `POST /cd`（影响所有客户端），默认不勾。
- **安全**：`/fs/list` 仅 loopback + token 鉴权；只返回目录名、不返回文件内容；对 `..` 与符号链接做规范化，避免越权探测。

## 安全模型（本地）

- server 默认只绑 `127.0.0.1`（现状已是）。
- **一次性 session token**：`/webui` 起 server 时 mint 随机 token，浏览器 URL 带 `?token=`，
  前端存内存并在后续请求头 `Authorization: Bearer` 带上，server 校验。
  防止本机其它用户/恶意网页 CSRF 到 server。
- 复用现有 `origin_is_allowed`，仅接受 loopback origin。
- token 不落盘，随进程退出失效。
- **兼容既有客户端**：VSCode 扩展当前无 token；token 中间件仅对 webui 敏感写路由强制，或采用
  「有有效 token **或** 旧客户端标识」放行，避免 break VSCode。

## `/webui` 命令生命周期

- `/webui`：进程内 server 未起则 `tokio::spawn(run_server(...))`（端口占用则提示）→ mint token →
  `open_browser("http://127.0.0.1:13456/?token=...")`。已起则直接 mint + 开浏览器。
- `/webui stop`：停掉进程内 server 任务（abort handle / shutdown watch）。
- 端口被占 → 命令内明确报错并提示。
- 复用 `open_browser`，开不了就打印 URL 供手动点击。

## Phase 1.5：TUI ⇄ webui 实时同步（方案 A · 进程内活动会话总线）

**目标**：浏览器里问的内容在 TUI 实时渲染，TUI 里输入的内容在打开的浏览器里实时渲染——
双向、同一会话。

**前提利好**：Phase 1 已把 server 改为进程内 `tokio::spawn`，TUI 与 server **同进程共享内存**，
故同步是**进程内广播**，不需要跨进程 IPC / 网络同步协议 / 冲突合并。

### 核心：`LiveSession` 总线（放 `atomcode-core`）

core 同时被 tuix 与 daemon 库依赖，故总线放 core，避免依赖环。结构：

- `conversation: Arc<Mutex<Conversation>>`——**单一数据源**。
- `events: tokio::sync::broadcast::Sender<TurnEvent>`——**事件扇出**，TUI 渲染器与 webui SSE 都订阅。
- `input_tx: mpsc::UnboundedSender<UserInput>`——**唯一输入入口**，任一端的用户消息都投这里。
- `turn_state: Arc<Mutex<TurnState{Idle|Running}>>`——**单写者守卫**，保证同一会话同一时刻只跑一个 turn。
- **单一 turn 协调器**任务：从 `input_tx` 取消息 → 若 Idle 则置 Running、追加到 conversation、用
  `TurnRunner`（与 Phase 1 同一套）跑 turn、把 `TurnEvent` 发到 broadcast → 完成置 Idle。

模型：**一个活动会话，多个视图**。TUI 与浏览器都退化为「订阅 broadcast 渲染 + 投 input_tx 输入」。

### 同步模式（默认值，已确认）

- **webui 默认独立会话**；提供「同步/接管当前 TUI 会话」开关。仅开启时才走 `LiveSession`。
- **并发**：turn 进行中，另一端输入框**禁用 + 提示「对方正在对话」**（不排队，避免乱序）。
- **晚加入**：webui 开启同步时，先收会话快照（replay 现有 messages）再接实时 broadcast 增量。
- **权限审批**：审批请求经 broadcast 同时出现在 TUI 与浏览器，**任一端可批准，先到先得**，另一端关闭卡片。

### TUI 侧改造

- 同步开启时，TUI 输入不再走自己的 `AgentLoop`，而是投 `LiveSession.input_tx`；TUI 渲染改为订阅
  broadcast。不同步时维持现状。
- 这是 Phase 1.5 主要工作量（碰 TUI 事件循环），故独立于 Phase 1 基础功能、后做。

### webui server 侧

- 新增 `GET /live`（SSE：先 replay 快照、后推 broadcast）与 `POST /live/message`（投 input_tx）；
  暴露「当前活动会话」标识。基础 webui 的 `/chat` 路径不变，仅在「同步开启」时切到 `/live`。

## Phase 2 蓝图（仅留接口，不实现）

- server 内预留 `mod tunnel`：进程主动向官方中转服务建立出站长连接（WebSocket/QUIC），
  中转按子域名 `alice.atomcode.dev` 反代回来。`run_server` 库化后，隧道挂载点天然可复用。
- 鉴权复用现有 OAuth/coding-plan 账号体系（统一身份）。
- Phase 1 须预留：token 鉴权中间件做成可插拔（本地 token / 账号 token 共用校验链）；
  权限流走 SSE 已天然适配远程。
- 中转服务端、账号-子域名映射、TLS 证书属 Phase 2 独立 spec。

## 测试策略

- server：权限桥接单测（`PermissionResponders` 按 session 路由、未知 session 返回 false、
  超时默认拒绝）、token 校验单测（无 token 401、Bearer 解析）。
- 静态资源 handler：embed 资源能取到、SPA fallback 正确。
- `/fs/list`：列目录、`~` 展开、`..` 越权被拒。
- 进程内启动：`run_server` 库函数可在 tokio 任务内起、单例只起一次。
- 前端：先手动验证（聊天流式、审批卡片、会话切换、目录切换），不上重型 e2e。

## 范围边界（YAGNI）

- 本 spec 仅 Phase 1 本地。Phase 2 中转隧道、账号-子域名、TLS 另开 spec。
- 不做重型前端 e2e。
- 独立 `atomcode-daemon` 二进制保留（VSCode/Docker 用），但 webui 不依赖它——server 逻辑库化后两者共用。
- 配置编辑、`/fs/list` 的文件级浏览（只到目录）等留待后续增强。
- webui 不接管 TUI 进程内正在运行的实时会话（如需，属另一设计方向）。
