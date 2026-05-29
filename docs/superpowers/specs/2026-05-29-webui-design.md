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
| 打包 | 前端构建产物用 `rust-embed` 嵌入 `atomcode-daemon` 二进制 |
| 后端 | 复用现有 `atomcode-daemon`（方案 A），webui 是它的第二个前端 |
| 权限 UX | 交互式审批（浏览器逐次批准/拒绝工具调用） |
| 会话模型 | webui 会话与 TUI 会话各自独立运行，共享磁盘 session 历史，可互相 resume |
| 远期远程 | 官方中转隧道（daemon 出站连官方中转，子域名反代） |

## 现状事实（实现时依赖）

- `crates/atomcode-daemon`（axum）默认绑 `127.0.0.1:13456`，已提供：`/chat`(SSE 流式)、
  `/sessions`、`/config`、`/auth/login`、`/models`、`/mcp`，已带 CORS 层。
- daemon 当前用 `AutoPermissionMode::BypassAll`（`main.rs:2051`）——会自动批准所有工具调用，
  必须替换为交互式 decider。
- TUI（tuix）进程内跑 agent（`TurnRunner`），与 daemon 是独立进程。
- CLI 已有 `atomcode daemon` 子命令，re-exec 进 `atomcode-daemon` 二进制
  （`atomcode-cli/src/main.rs:933+`）。
- 已有 `session.open_browser_best_effort()`（`event_loop/commands.rs:2710`）。
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
│ 浏览器 (embedded SPA)  │ ◀──────▶ ┌────────────────────┐
│  - 聊天/流式            │          │ atomcode-daemon     │
│  - 工具批准卡片         │          │  - 复用 /chat /sessions│
│  - 会话侧栏/配置        │          │  - 新增静态资源 + 权限流│
└───────────────────────┘          │  - 进程内 TurnRunner  │
                                    └────────────────────┘
```

daemon 是 webui 的后端，独立于 TUI 进程。`/webui` 只是 daemon 的"自动启动 + 开浏览器"包装，
复用 `atomcode daemon` 子命令已有的 re-exec spawn 逻辑。

## 组件划分

### 前端 `webui/`（新目录，独立构建）

- 技术栈：**Preact + Vite + Tailwind**。产物为纯静态 `dist/`，runtime 体积小（~10-20KB），
  适合 embed；Tailwind 与现有 `site/` 一致。
- 模块：聊天流式视图、工具批准卡片、会话侧栏/切换、配置表单、登录入口。
- 构建产物用 `rust-embed` 打进 daemon。

### daemon 新增

- `mod webui`：`rust-embed` 静态资源 handler。`GET /` 与 `/assets/*` 命中嵌入资源，未命中路由
  fallback 到 `index.html`（SPA 路由）。
- dev 模式：环境变量（如 `ATOMCODE_WEBUI_DEV=http://localhost:5173`）时反代/重定向到本地
  vite dev server，便于前端热更新。
- `WebPermissionDecider`：实现现有 permission trait，替换 `BypassAll`。
- token 鉴权中间件：**可插拔**，本地 token 与远期账号 token 共用一条校验链（为 Phase 2 预留）。

### TUI 新增

- `event_loop/commands.rs` 加 `"webui"` 分支 → 调用新 helper `ensure_daemon_and_open(token)`。
- `/webui stop`：关掉由 `/webui` 启动的 daemon（若确为它启动）。

### CLI 新增

- `atomcode webui` 子命令：等价于命令行直接启动 + 开浏览器，不进 TUI。

## 数据流：聊天 + 交互式权限

聊天复用现有 `POST /chat` 的 SSE 流。交互式工具批准新增双向流：

1. agent 要调危险工具 → `WebPermissionDecider` 在 SSE 里推一条 `permission_request`
   （含 tool、参数、call_id）并**阻塞等待**。
2. 前端弹审批卡片，用户点批准/拒绝/总是允许。
3. 前端 `POST /chat/permission { call_id, decision }`。
4. daemon 用 `tokio::sync::oneshot`（按 call_id 索引的 map）唤醒被阻塞的 decider，返回决定，
   agent 继续。
5. 超时（如 5 分钟无响应）默认拒绝。

决定粒度对齐 TUI：`Approve` / `Deny` / `AlwaysAllow`（本会话该工具）。

## 安全模型（本地）

- daemon 默认只绑 `127.0.0.1`（现状已是）。
- **一次性 session token**：`/webui` 启动时生成随机 token 写入 daemon，浏览器 URL 带
  `?token=`，前端存内存并在后续请求头 `Authorization: Bearer` 带上，daemon 校验。
  防止本机其它用户/恶意网页 CSRF 到 daemon。
- 加 `Origin` 校验，仅接受 loopback origin。
- token 不落盘，随 daemon 退出失效。

## `/webui` 命令生命周期

- `/webui`：健康检查 → 必要时 spawn daemon（等就绪）→ 生成 token → 开浏览器。已在跑则直接开。
- `/webui stop`：关掉由 `/webui` 启动的 daemon。
- 端口被占/spawn 失败 → TUI 内明确报错。
- 复用 `open_browser_best_effort()`，开不了就打印 URL 供手动点击。

## Phase 2 蓝图（仅留接口，不实现）

- daemon 内预留 `mod tunnel`：daemon 主动向官方中转服务建立出站长连接（WebSocket/QUIC），
  中转按子域名 `alice.atomcode.dev` 反代回来。
- 鉴权复用现有 OAuth/coding-plan 账号体系（统一身份）。
- Phase 1 须预留：token 鉴权中间件做成可插拔（本地 token / 账号 token 共用校验链）；
  权限流走 SSE 已天然适配远程。
- 中转服务端、账号-子域名映射、TLS 证书属 Phase 2 独立 spec。

## 测试策略

- daemon：权限流单测（oneshot 唤醒、超时默认拒绝、token 校验拒绝无 token 请求）。
- 静态资源 handler：embed 资源能取到、SPA fallback 正确。
- `/webui` 命令：daemon 探测/spawn 逻辑（mock 健康检查）。
- 前端：先手动验证（聊天流式、审批卡片、会话切换），不上重型 e2e。

## 范围边界（YAGNI）

- 本 spec 仅 Phase 1 本地。Phase 2 中转隧道、账号-子域名、TLS 另开 spec。
- 不做重型前端 e2e；不引入额外后端服务（不新建独立 webui 二进制）。
- webui 不接管 TUI 进程内正在运行的实时会话（如需，属另一设计方向）。
