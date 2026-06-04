# `feature/app` 分支改动说明 —— 移动端远程访问(`/app` 命令)

> 面向 atomcode 维护者的评审/合并说明。本分支在不改动既有 `/webui`、`/sync`、
> daemon 主流程的前提下,**新增一个 `/app` 命令**,让手机 App 通过一台公网中继
> 扫码连入、与桌面端**共享同一个 LiveSession**(同一对话、双向实时同步)。
> 顺带修了两个与同步相关的既有问题。

---

## 1. 目标与整体思路

- 产品形态:用户在终端跑 `atomcode` → 敲 `/app` → 终端打印二维码 → 手机扫码即可在
  任意网络下连到这台电脑、和终端看同一段对话。
- **复用而非新造**:`/app` 完全复用官方 `/live`(进程内全局 `LiveSession`,见
  `live_api.rs` 的 `LIVE` 单例)+ `/webui` 的进程内 server 起停设施。手机和终端
  都附着在同一个 `LiveSession` 上,所以双向实时同步是官方机制原生支持的。
- 网络可达层放在**自建多租户中继**(独立仓库 `atomcode-relay`,不在本仓库):
  电脑端主动拨号到中继,手机连中继,中继按每个用户的 *route token* 路由 + 隔离。
  官方无需关心中继实现,只多了一个"不开浏览器 + daemon 模式"的进程内 server 入口。

```
手机 App ──HTTPS/SSE──> 中继(公网,自建) <──WSS 反向隧道── atomcode(/app) ─┐
                                                                          └─ 进程内全局 LiveSession ←→ 终端 TUI
```

---

## 2. 改动文件一览(8 个)

| 文件 | 改动 | 类别 |
|---|---|---|
| `crates/atomcode-daemon/src/lib.rs` | `ensure_app_server` / `stop_app_server` / `APP_DEFAULT_PORT` / `/live/cancel` 路由 | 新功能 |
| `crates/atomcode-daemon/src/live_api.rs` | `live_cancel` 处理器 | 新功能(停止) |
| `crates/atomcode-core/src/live/mod.rs` | `LiveSession` 增加可取消的当前回合令牌 + `cancel_turn()` | 新功能(停止) |
| `crates/atomcode-tuix/src/commands.rs` | 注册 `/app` 命令 | 新功能 |
| `crates/atomcode-tuix/src/event_loop/commands.rs` | `"app" =>` 命令处理 | 新功能 |
| `crates/atomcode-tuix/src/event_loop/mod.rs` | `LoopCtx.app_relay_child` 字段 + `PeerBusy` 渲染修复 | 新功能 + 修复 |
| `crates/atomcode-tuix/src/lib.rs` | `LoopCtx` 构造初始化新字段 | 新功能 |
| `crates/atomcode-tuix/Cargo.toml` | tokio 增加 `process` feature | 依赖 |

---

## 3. 新功能:`/app` 命令

### 3.1 进程内 App server(`atomcode-daemon/src/lib.rs`)
新增 `pub async fn ensure_app_server(host, port) -> Result<(String,u16),String>` + `stop_app_server()`
+ `pub const APP_DEFAULT_PORT: u16 = 13458`(错开 webui 13457、独立守护 13456)。

它复制 `ensure_server_and_open` 的绑定 + spawn 逻辑,但有**两点关键区别**:
1. **`webui_tokens: None` → `enforce_token=false`(daemon 模式)**。原因:手机 App 的 Cloud
   模式只发 `X-Atom-Token`(给中继做路由),不发 `Authorization: Bearer`。若用 webui 模式
   (`enforce_token=true`)会因缺 Bearer 而 401。鉴权边界落在**中继的 route token + 本机
   回环绑定**(server 只 listen `127.0.0.1`,仅本机隧道可达),与 `atomcode daemon` 的
   既有安全模型一致。
2. **不开浏览器**(App 用二维码配对)。
3. 与 `/webui` 共用同一个进程内全局 `LiveSession`(`ensure_live_session_seeded`),
   所以 TUI / 浏览器 / App 看到的是同一对话。

> 用独立的 `APP_SERVER` 静态句柄(非 `WEBUI`),因为两者 token 强制模式不同、需分别管理;
> 二者即使同时运行也共享同一个进程级 `LiveSession`,不会分裂会话。

### 3.2 `/app` 命令(`atomcode-tuix`)
- `commands.rs`:`BUILTIN_COMMANDS` 注册 `app`(`needs_args=true`,补全只到 `/app `)。
- `event_loop/commands.rs`:`"app" =>` 分支,流程(仿 `"webui" =>`):
  1. 用 TUI 当前会话 seed 全局 `LiveSession`(`ensure_live_session_seeded`);
  2. `ensure_app_server("127.0.0.1", APP_DEFAULT_PORT)` 起本机 server;
  3. 生成一段随机 route token(双 `uuid::Uuid::new_v4().simple()` 拼成 64 hex);
  4. **spawn 外部进程 `atomcode-relay-client`**(`tokio::process::Command`,`kill_on_drop(true)`)
     反向连中继:`run --relay <wss>/ws/daemon --token <T> --daemon http://127.0.0.1:<port> --supervise-daemon false`;
  5. 用 `render::qr::render_login_qr` 把配对 URI `atomcode-link://pair?r=<中继https根>&t=<T>&m=<机器名>`
     渲染成终端二维码;
  6. `attach_live_session` 让 TUI 也附着同一会话。
  - `/app stop`:kill 子进程 + `stop_app_server()` + 解除 TUI 附着。
  - 中继地址来源:命令参数 `/app <url>` 或环境变量 `ATOMCODE_APP_RELAY`;relay-client 路径
    可用环境变量 `ATOMCODE_RELAY_CLIENT_BIN` 覆盖,默认按 PATH 名 `atomcode-relay-client` 查找。
- `event_loop/mod.rs`:`LoopCtx` 增加 `app_relay_child: Option<tokio::process::Child>`
  (`kill_on_drop`,TUI 退出 / `/app stop` 自动清理子进程)。
- `lib.rs`:`LoopCtx` 构造处初始化 `app_relay_child: None`。
- `Cargo.toml`:tokio features 增加 `"process"`(用于 spawn relay-client)。**仅此一项新依赖能力,
  无新增三方 crate。**

> **设计取舍**:relay-client 用"外部子进程"而非"内嵌成库",是为了不把中继协议(700+ 行)
> 和 `tokio-tungstenite`(WS + TLS 栈)塞进本仓库、不与 `atomcode-relay` 仓库产生协议同步负担。
> 代价:发布时需把 `atomcode-relay-client` 二进制一并打包(见 §6 产品化)。

---

## 4. 修复 1:对端轮次回复在 TUI 不显示(`event_loop/mod.rs`,`AgentEvent::PeerBusy`)

**现象**:手机发消息,桌面 TUI 显示了用户气泡,但**不显示 AI 回复**,要再发一句才一起冒出来。

**根因**:本地轮次结束走 `AgentEvent::TurnComplete`,其**第一步** `renderer.render(UiLine::AssistantLineBreak)`
把流式累积的助手行"收尾落地"。而对端(手机/webui)轮次结束走 `AgentEvent::PeerBusy(false) → on_turn_complete()`,
**缺少这个 AssistantLineBreak**,助手行一直挂在"流式当前行"不提交,直到下一轮才被一起刷出。

**修复**:`PeerBusy(false)` 分支补 `renderer.render(UiLine::AssistantLineBreak) + flush() + think.reset()`
再 `on_turn_complete()`(`think.reset()` 防上一轮 think-stripper 残留状态吞掉下一轮文本)。

> 这是既有 `/sync`/`/webui` 路径就存在的镜像渲染缺口,本次一并修正。

## 5. 修复 2 / 新增:停止生成(`/live/cancel`)

**现象**:任一端点「停止」无法真正中断正在生成的回合 —— 官方 `/live` **没有取消端点**。
`coordinator` 每轮 `run_turn(..., CancellationToken::new())` 是临时新建、不保存,外部无从取消。

**改动**:
- `atomcode-core/src/live/mod.rs`:
  - `LiveSession` 增加 `current_cancel: Arc<Mutex<Option<CancellationToken>>>`;
  - `coordinator` 每轮**建令牌→登记→`run_turn`→清空**(令牌透传给执行器,执行器已把它
    交给 `TurnRunner::run`,见 `live_api.rs` `run_turn`,中断能力本就具备);
  - 新增 `pub async fn cancel_turn(&self) -> bool`,取消已登记的令牌。
- `atomcode-daemon/src/live_api.rs`:新增 `live_cancel` 处理器(调 `current_live_session().cancel_turn()`)。
- `atomcode-daemon/src/lib.rs`:注册路由 `POST /live/cancel`。

任一视图(手机/webui/TUI)调用 `/live/cancel` 都能停同一会话正在跑的回合;停止后 daemon 广播
`state:running=false`,各端 UI 自然复位。

---

## 6. 新增对外接口 & 依赖

- **新增 HTTP 端点**:`POST /live/cancel`(取消当前回合)。其余 `/live*` 不变。
- **新增进程内 server 入口**:`ensure_app_server`(13458,daemon 模式、回环、无浏览器)。
- **新增 TUI 命令**:`/app [中继地址]` / `/app stop`。
- **新增 cargo 能力**:`atomcode-tuix` 的 tokio `process` feature(无新三方 crate)。
- **运行期外部依赖**:`/app` 会 spawn `atomcode-relay-client`(独立仓库 `atomcode-relay` 的产物)。

---

## 7. 不变 / 边界

- 未改动 `/webui`、`/sync`、`/chat`、daemon 既有路由与鉴权逻辑;`/app` 是新增旁路。
- 闭源签名 crate `atomcode-codingplan-crypto` **未触碰**;`build-official.sh` 仍自动注入/还原 stub。
- 中继(relay-server / relay-client)在**独立仓库** `atomcode-relay`,本仓库零引用。

## 8. 构建与产品化 TODO

- **构建(含签名)**:在本分支根目录 `bash scripts/build-official.sh`(不带分支参数 = 用当前分支)。
- 产品化(发布前):
  1. 把 `atomcode-relay-client` 二进制随安装包分发(放进 PATH)→ `/app` 免设 `ATOMCODE_RELAY_CLIENT_BIN`;
  2. 给 `/app` 的中继地址加配置默认值(指向线上中继)→ 用户免设 `ATOMCODE_APP_RELAY`;
  3. 运维侧只需部署 relay-server,部署文档见 `atomcode-relay/DEPLOY.md`。
- 已知后续可优化:对端轮次的**逐 token 实时**在 TUI 的呈现(当前是回合结束时整体落地)。
