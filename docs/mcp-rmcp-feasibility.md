# rmcp 替换可行性评估

**评估对象：** 用官方 Rust SDK [`rmcp` 3.1.0](https://crates.io/crates/rmcp)（2026-07-31 发布，MSRV 1.88，Apache-2.0）替换 `crates/atomcode-capabilities/src/mcp/` 里手写的 MCP 客户端。
**评估日期：** 2026-08-04（分支 `release/v5.0.5`）
**证据基础：** rmcp 3.1.0 crate 源码本地解包逐文件读；并在 `scratchpad/rmcp-probe` 里用 **260 行**在 rmcp 上重新实现了我们全部 `McpClient` trait（stdio 全量 + HTTP 连接），`cargo check` 通过。下文所有结论都带 `文件:行号`。

---

## 0. 结论

**可行，推荐"薄适配"：只换传输层，registry / trust / 审批 / config / 命名 / 事件流全部保留。**

- **API 面完全够用**，不需要为了迁就 SDK 改我们的抽象——`McpClient` trait 的 5 个方法在 rmcp 上一一对得上（已编译验证）。
- **真正的成本不在 API 适配，而在 `reqwest 0.12 → 0.13`**：rmcp 3.1.0 硬依赖 reqwest ≥0.13.2（`rmcp-3.1.0/Cargo.toml:759-765`），我们 8 个 crate 全部 pin 0.12。
- 依赖树增量出乎意料地小：**只新增 8 个 crate 名**，其余 119 个已在树内。
- 净代码变化约 **−1000 行**（删两个 transport 1386 行，加 adapter ~260 行 + 重连 ~120 行）。

---

## 1. 替换的缝在哪

缝是现成的，且只有两个构造点：

| 位置 | 内容 |
|---|---|
| `mcp/client.rs:22-41` | `trait McpClient`：`initialize` / `list_tools` / `call_tool` / `server_name` / `status` |
| `mcp/registry.rs:116` | `servers: BTreeMap<String, Arc<dyn McpClient>>` —— registry 只经 trait 对象访问 |
| `mcp/registry.rs:361-386` | 构造点 ①（后台并行连接） |
| `mcp/registry.rs:487-512` | 构造点 ②（`add_server`） |

`registry.rs` 其余 1148 行、`tool.rs` 217 行（kernel `Tool` 适配 + `risk()=Risky`）、`config.rs` 842 行、`trust.rs` 242 行**完全不动**。

---

## 2. rmcp client 侧 API 面（实测）

```rust
// 连接：三种生命周期模式（rmcp-3.1.0/src/service/client.rs:585-597）
ClientLifecycleMode::Initialize                              // 旧 initialize 握手
ClientLifecycleMode::Discover { preferred_versions }         // 2026-07-28 无状态 server/discover
ClientLifecycleMode::Auto { preferred_versions, legacy_version } // 先探 discover，证实是旧服务器才回落

let service = client_info.serve_with_lifecycle(transport, mode).await?;  // :601
```

| 能力 | rmcp 位置 |
|---|---|
| 协议版本常量 | `src/model.rs:170-186`（`V_2024_11_05` … `V_2026_07_28`；**注意 `LATEST` = `2025-11-25`**，2026-07-28 需显式指定） |
| `tools/list`（自动翻页） | `src/service/client.rs:1618` `list_all_tools()` |
| `tools/call`（自动驱动 MRTR 轮次） | `src/service/client.rs:1824` `call_tool()`；不驱动 MRTR 的单发是 `:1793` `call_tool_once()` |
| 服务器信息 | `src/service.rs:1018` `peer_info()` → `Option<Arc<InitializeResult>>`，其中 `server_info: Option<Implementation>` |
| 错误分类 | `src/service.rs:79-97` `ServiceError::{McpError, TransportSend, TransportClosed, Timeout, Cancelled, …}` |
| 401 判定 | `src/service/client.rs:96` `auth_challenge()` / `:119` `is_authorization_required()` |
| 单请求超时（含 progress 重置） | `src/service.rs:761-780` `PeerRequestOptions` + `:850` `send_request_with_option` |
| 取消 | `src/service.rs:1073` `cancellation_token()` / `:1157` `cancel()` / `:1042` `is_transport_closed()` |
| stdio 传输 | `src/transport/child_process.rs:61` `TokioChildProcess::new` / `:150` builder（可拿 stderr）/ `:81` `graceful_shutdown` |
| HTTP 传输配置 | `src/transport/streamable_http_client.rs:1706` `with_uri` / `:1718` `auth_header` / `:1746` `custom_headers` / `:1702` `reinit_on_expired_session` |
| OAuth | `src/transport/auth.rs:1016` `AuthorizationManager`、`:3483` `OAuthState`、`:264` `CredentialStore` trait（可挂我们的文件 store）、`:1619` `register_client`、`:699` `with_application_type`、`:2003` `exchange_code_for_token_with_issuer` |

---

## 3. 能力逐项映射

✅ = rmcp 直接顶掉　⚠️ = 需保留/上移我们的实现　🆕 = 白捡的新能力

| # | 我们现在 | rmcp | 结论 |
|---|---|---|---|
| 1 | 子进程 spawn/env/kill `transport_stdio.rs:107-158` + `Drop:662` | `child_process.rs:61/150`，`graceful_shutdown:81`（关 stdin → 等 → 超时才 kill） | ✅ 且更稳（我们只有 `start_kill`） |
| 2 | Windows `.cmd` 包装 `transport_stdio.rs:635-660` + 12 个测试 | 只有 `which_command`（`child_process.rs:218`，需 `which-command` feature），**不做 `cmd.exe /C` 包装** | ⚠️ 保留我们的纯函数，零成本 |
| 3 | stdout 垃圾行容忍 `transport_stdio.rs:465-513`（`MAX_SKIP_LINES`）+ 启动 drain `:519` | `async_rw.rs:323-345` 跳过不可解析行（含 BOM），测试 `async_rw.rs:713` | ✅ |
| 4 | `Content-Length:` 帧兼容 `transport_stdio.rs:594-620` | **无**（rmcp 只支持 NDJSON，全仓 grep 无 content-length） | ⚠️ 行为回退（非 spec 要求，仅容错） |
| 5 | 断线重连：generation 计数 + EPIPE 识别 + 副作用不重放 `transport_stdio.rs:321-394, 569-592` | **无重连**（`service.rs`/`child_process.rs` 无 reconnect） | ⚠️ 上移重写 ~120 行 |
| 6 | `Mcp-Session-Id` 捕获/回送/DELETE 清理 `transport_http.rs:77-83, 139-147, 299-348` | 内建，另有 `reinit_on_expired_session` | ✅ |
| 7 | SSE 帧解析 `transport_http.rs:435-478` + 8 个测试 | `sse-stream` crate | ✅ |
| 8 | `Accept: application/json, text/event-stream` `transport_http.rs:23` | 内建 | ✅ |
| 9 | **`MCP-Protocol-Version` 头 —— 我们没有** | `streamable_http_client.rs:81,122,1141` | 🆕 补上 2025-06-18 起的硬要求 |
| 10 | 自定义 headers `transport_http.rs:127-129` | `custom_headers` | ✅ |
| 11 | OAuth bearer 注入 + 过期刷新 `transport_http.rs:231-249` | `auth_header`（静态）或 `AuthClient`/`AuthorizationManager`（自动刷新 + `CredentialStore`） | ⚠️ 两条路，见 §6 P2 |
| 12 | 401 → "run `atomcode mcp login`" `transport_http.rs:166-176` | `is_authorization_required()` + `www-authenticate` 挑战 | ✅ 判定更准 |
| 13 | per-server 超时（外层 `tokio::time::timeout`） | `PeerRequestOptions`（支持 progress 重置 + total 上限） | ✅ 更强 |
| 14 | registry `cancelled` watch 通道 | `CancellationToken` 贯穿 | ✅ 对接顺滑 |
| 15 | `readOnlyHint`/`destructiveHint` 保守判定 `types.rs:87-96` | `model/tool.rs:54-73` 字段一致 | ⚠️ 判定逻辑留我们这（rmcp `:152` 的默认语义与我们不同） |
| 16 | 无客户端缓存 | `service/client/cache.rs:54-63` **默认开**（`default_ttl=0`、`serve_stale_on_error=true`） | ⚠️ 需知悉：与 `/mcp reload` 语义交互 |
| 17 | 协议版本硬编码 `2024-11-05`（`transport_stdio.rs:268`、`transport_http.rs:358`、`oauth.rs:516`） | `Auto` 模式自动协商 2026-07-28 ↔ legacy | 🆕 一次性解决版本落后 4 个修订 |

---

## 4. 代价

### 4.1 依赖增量：只有 8 个新 crate

对比 rmcp（`client,transport-child-process,transport-streamable-http-client-reqwest,auth,reqwest`，`default-features = false`）的 127 个传递依赖与本仓 `Cargo.lock` 的 457 个：

```
aws-lc-rs  aws-lc-sys  nix  oauth2  process-wrap  rmcp  rustls-platform-verifier  sse-stream
```

其余 119 个（tokio / reqwest / hyper / rustls / serde / futures / tower / url / chrono …）已在树内。`schemars` 不会进来（只被 `server` feature 拉），`rmcp_macros`、`jsonwebtoken` 同理。

### 4.2 ⚠️ 最大成本项：reqwest 0.12 → 0.13

rmcp 3.1.0 要求 `reqwest = "0.13.2"`（`rmcp-3.1.0/Cargo.toml:759`）。本仓 8 个 crate pin 0.12：

```
atomcode-auth:16,41   atomcode-cli:52          atomcode-capabilities:47,239
atomcode-codingplan:12,19   atomcode-telemetry:10   atomcode-tuix:39   atomcode-updater:21
```

两条路：

- **(a) 全树升 0.13**：一次性，语义干净，但要过一遍所有 `reqwest::` 调用点（`ClientBuilder`、`proxy::apply_*_proxy_policy`、blocking client、TLS feature 名从 `rustls-tls` 改成 `rustls`）。
- **(b) 让 0.12 / 0.13 共存**：改动最小，但二进制里两套 hyper/TLS 栈，编译时间和体积都涨。

建议 (a)，且它本来也该做。

### 4.3 aws-lc-rs 可规避

`aws-lc-rs`/`aws-lc-sys`（C + 汇编，需 cmake/clang，交叉编译痛点）来自 reqwest 0.13 的 `rustls` feature 默认 provider：

```
aws-lc-rs ← rustls 0.23.43 ← hyper-rustls 0.27.9 ← reqwest 0.13.4 ← rmcp 3.1.0
```

用 rmcp 的 `reqwest-tls-no-provider` feature（`Cargo.toml:107-110` → `reqwest?/rustls-no-provider`），由我们自己装 ring provider（树里已有 `ring 0.17.14` + `rustls 0.23.37`）即可绕开。

### 4.4 工具链

rmcp `edition = "2024"`、MSRV 1.88；我们 edition 2021、rustc 1.94 —— **无阻碍**（依赖用 2024 edition 与本 crate 用 2021 互不影响）。

### 4.5 代码增删

行数按 P0（`17b32860`）之后的实际文件计，产品/测试按 `#[cfg(test)]` 位置切分。

| 项 | 产品 | 测试 | 合计 |
|---|---:|---:|---:|
| `transport_stdio.rs` 总量 | 664 | 96 | 760 |
| └ **保留**：`wrap_cmd_script` + `windows_wrap_command`（`:614-652`）及其 12 个测试（`:665-760`） | −39 | −96 | −135 |
| └ 实际可删 | 625 | 0 | **−625** |
| `transport_http.rs`（全删：SSE 解析 + 会话 + 协议头测试都随之消失） | 528 | 191 | **−719** |
| rmcp adapter（`scratchpad/rmcp-probe` 实测） | +260 | | +260 |
| 重连逻辑上移（generation + EPIPE + 不重放） | +120 | | +120 |
| adapter 行为测试（替代被删的 191 行 HTTP 侧测试） | | +~150 | +~150 |
| **净变化** | | | **≈ −814** |

不给 adapter 补测试的话是 −964；**合理区间是净减 810~960 行**。

被顶掉的 1,153 行产品代码占 `mcp/` 全部 3,921 行产品代码的 **29%** —— 剩下 71%（registry 807 / oauth 774 / config 553 / types+tool+trust+client+mod+util 595）是 rmcp 不提供的自有产品逻辑。这就是推荐薄适配而非整体替换的量化依据。

P2 若再换掉 OAuth：`oauth.rs` 774 行产品 + 71 行测试（视 §6 决策）。

---

## 5. 风险

1. **官方定位**：2026-07-28 发布博客把 Tier 1 定为 TS/Python/Go/C#，**Rust 标注为 beta**。虽然 3.0.0 当天发、3.1.0 三天后跟进、README 自称 production-ready，但跟规范的节奏比 TS 慢半拍。
2. **API 稳定性**：58 个已发布版本，3.0.0 → 3.1.0 只隔 3 天，`#[non_exhaustive]` 遍布（probe 里 `ClientInfo`/`Implementation`/`StreamableHttpClientTransportConfig` 都不能用结构体字面量构造）。升级要跟。
3. **无重连语义**：`RunningService` 在传输断开后就是死的，我们那套"副作用不重放"的策略必须自己在上层重建。
4. **`Content-Length` 帧兼容丢失**（§3 #4）——只影响不守 NDJSON 规范的第三方 server。
5. **默认开启的响应缓存**（§3 #16）——`tools/list` 会被缓存，需确认与我们"连接先于首轮、reload=重开"的缓存红线不冲突（大概率无冲突，因为我们的 reload 就是重建连接）。

---

## 6. 分阶段方案

### P0 — 止血，不引 SDK（~40 行，独立价值）
1. `transport_stdio.rs:268` / `transport_http.rs:358` / `oauth.rs:516` 的 `2024-11-05` → `2025-11-25`；
2. HTTP 补发 `MCP-Protocol-Version` 头（2025-06-18 起要求，严格 server 会 400）；
3. 用 `InitializeResult.protocol_version` 做真协商而不是丢弃。

**这一步无论是否上 rmcp 都该做。**

### P1 — 传输层替换（主体）
1. 先做 §4.2 的 reqwest 0.13 升级（独立 PR，可单独回滚）；
2. 新增 `mcp-rmcp` Cargo feature，在 `registry.rs:361` / `:487` 两个构造点按 feature 选实现，**双跑对照**；
3. adapter 直接用 probe 的 260 行为骨架（`scratchpad/rmcp-probe/src/lib.rs`）；
4. 重连逻辑上移到 adapter：`ServiceError::TransportClosed` / `is_transport_closed()` → 重建 transport → 重新 `serve_with_lifecycle`，**保持"call_tool 已发出则不自动重放"**；
5. `wrap_cmd_script` 保留，喂给 `TokioChildProcess`；
6. 生命周期用 `Auto { preferred: [2026-07-28, 2025-11-25], legacy: 2025-06-18 }`；
7. 删 `transport_stdio.rs` / `transport_http.rs`，把两边共 20 个单测改成对 adapter 的行为测试。

### P2 — OAuth（可选，独立决策）
- **省事路**：保留 `oauth.rs` 全部登录/刷新逻辑，只把拿到的 token 通过 `auth_header` 喂给 rmcp。改动最小，GitHub 特例（`oauth.rs:372-450`）零风险。
- **彻底路**：换 `AuthorizationManager`，把 `McpTokenStore`（`oauth.rs:102-152`）实现成 `CredentialStore`（`auth.rs:264-270`），白捡 2026-07-28 的 `iss` 校验（RFC 9207）、`application_type`（SEP-837）、按 issuer 绑定凭证。代价是 GitHub 特例要重新验证，且 TOML 存储格式要迁移（`McpOAuthToken` → `StoredCredentials`，字段可一一映射）。

建议先走省事路，等 P1 稳定 soak 之后再评估。

### P3 — 补能力（可选）
resources / prompts（`list_all_prompts`、`read_resource` 现成）、`structuredContent`、tasks 扩展。

---

## 7. 验收口径

- `cargo test -p atomcode-capabilities --features mcp` 全绿；
- 双跑对照：同一份 `.mcp.json`，新旧实现对 stdio（npx 类）+ HTTP（deepwiki/context7 类）+ 有状态 HTTP（Figma Dev Mode，验证 session id）+ OAuth（GitHub MCP）各跑一次 `tools/list` + 一次 `tools/call`，输出逐字节对照；
- 杀掉 stdio 子进程验证重连与"不重放"；
- `cargo tree -p atomcode-capabilities --features mcp | grep -c reqwest` 确认没有双版本；
- 二进制体积与冷编译时间前后对比。
