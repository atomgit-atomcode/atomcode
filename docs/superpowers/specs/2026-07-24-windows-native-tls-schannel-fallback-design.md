# Windows native-tls(SChannel) 回退 — 设计

- 日期：2026-07-24
- 状态：设计已确认，待写实现计划
- 分支：release/v5.0.3
- 前置：本设计**扩展**已有的 endpoint-aware TLS 回退（commit `522c6f2a`，rustls 1.3 → rustls 1.2）。

## 动机 / 根因（已实测坐实）

用户在某个人网络下，atomcode 到 `llm-api.atomgit.com`（及所有 `*.atomgit.com`）的请求在 TLS 握手阶段被 reset（Windows `os error 10054`）。浏览器与 curl 正常。

穷举实测（同机同域）：

| 客户端 + TLS 版本 | 结果 |
|---|---|
| curl（**SChannel**）TLS 1.3（默认） | ❌ reset |
| curl（**SChannel**）**TLS 1.2**（`--tls-max 1.2`） | ✅ 通（HTTP 405） |
| atomcode（**rustls**）TLS 1.3 | ❌ reset |
| atomcode（**rustls**）**TLS 1.2**（`ATOMCODE_TLS_MAX=1.2`，v5.0.2 带修复，env 生效） | ❌ **仍 reset** |

**结论**：中间设备同时按两个轴拦截——(a) **TLS 1.3**（任何客户端）、(b) **rustls 的 ClientHello 指纹**（任何版本）。唯一能穿过的组合是 **SChannel + TLS 1.2**。atomcode 的 provider 栈是 rustls-only（`reqwest` 仅启用 `rustls-tls`），配不出这个组合，故 `522c6f2a` 的"只锁版本"修复对该网络不足。

佐证：仓库已有 `f0855fc5 feat(web_fetch): curl fallback for TLS-fingerprint blocks` —— 开发者早已知道 rustls 指纹会被此类 middlebox reset，只在 web_fetch 加了 curl 兜底，登录/聊天路径未覆盖。

## 目标

在 **Windows** 上，让访问 managed AtomGit 端点的 client 在被 reset 时回退到 **native-tls(SChannel) + TLS 1.2**（复刻唯一能通的组合）。非 Windows 保持现有 rustls+1.2 回退。覆盖登录 / CodingPlan / 聊天三类 client。

## 非目标

- 不改非 Windows 平台的 TLS 行为（Linux/macOS 保持纯 rustls；避免引入 OpenSSL 构建依赖）。
- 不对**第三方**端点（api.openai.com 等）切 native-tls —— 仅 managed atomgit 域名。
- 不做 curl 子进程兜底（聊天是 SSE 流式，curl 子进程流式解析复杂且脆；native-tls 保留在 reqwest 异步管线内，更干净）。

## 设计

### 1. 策略层（`atomcode_config::tls`）

现有纯函数 `is_managed_https_url` / `should_cap_url` / `should_try_fallback` / `managed_tls12_latched` / `latch_managed_tls12` **签名不变**。新增一个纯函数：

```rust
/// 该 url 的 client 是否应使用 native-tls(SChannel) 后端而非 rustls。
/// 仅 Windows + managed atomgit 端点 + 处于 TLS-1.2 回退态（env 强制或已 latch）时为真。
/// 第三方端点即便被全局 env 锁 1.2 也不切后端（native-tls 只为绕 atomgit 前的指纹拦截）。
pub fn should_use_native_tls(url: &str) -> bool {
    cfg!(windows) && is_managed_https_url(url) && should_cap_url(url)
}
```

- 语义：`should_cap_url` 决定"锁 1.2"（可全局，含第三方）；`should_use_native_tls` 决定"换 SChannel 后端"（仅 managed×windows）。两个正交开关。
- 回退**探测**阶段（`should_try_fallback` 命中后重建 client）：重建时对 managed×windows 端点直接用 native-tls+1.2（不中间试 rustls-1.2）。探测用的"是否 native"由调用点按 `cfg!(windows) && is_managed_https_url(url)` 计算（此刻可能尚未 latch，故不能只依赖 `should_use_native_tls`）。

### 2. 依赖门控（Windows-only native-tls）

在四个建 managed client 的 crate 的 `Cargo.toml` 增加 **Windows-only** 的 reqwest `native-tls` feature：

```toml
[target.'cfg(windows)'.dependencies]
reqwest = { version = "0.12", features = ["native-tls"], default-features = false }
```

涉及 crate：`atomcode-auth`、`atomcode-codingplan`、`atomcode-core`、`atomcode-capabilities`。这样：
- Windows 构建：reqwest 同时有 `rustls-tls`（默认）+ `native-tls`（SChannel）→ 可运行时 `.use_native_tls()`。
- Linux/macOS 构建：不引入 native-tls（不拖 OpenSSL/SecureTransport）。
- 已验证 reqwest 0.12.28 提供 `ClientBuilder::use_native_tls()`（`async_impl/client.rs:2118`）与 blocking 对应版，`native-tls` feature 存在（reqwest Cargo `native-tls = ["default-tls"]`）。

### 3. 各 client 建造点（沿用"每 leaf 内联"模式）

现状每个建 client 处已有：`if force_tls12 { builder = builder.max_tls_version(TLS_1_2); }`。扩展为（示意，async 版；blocking 版同形）：

```rust
if force_tls12 {
    builder = builder.max_tls_version(reqwest::tls::Version::TLS_1_2);
}
// Windows + managed 端点回退：换 SChannel 后端绕过 rustls 指纹拦截。
// SChannel 原生读 Windows 系统证书库，故此路径跳过 rustls 专用的 add_trusted_roots。
#[cfg(windows)]
let use_native = use_native_tls; // 由调用点传入（见 §1 探测/latch 逻辑）
#[cfg(windows)]
if use_native {
    builder = builder.use_native_tls();
}
```

- **证书**：native-tls(SChannel) 原生信任 Windows 系统证书库（含企业 MITM CA）→ native-tls client **不调用 `add_trusted_roots`**（`rustls_native_certs`/`rustls::RootCertStore` 对 native-tls 无意义）。故建造逻辑：`use_native` 为真时走"native-tls + 跳过 add_trusted_roots"分支；否则走现有 rustls + add_trusted_roots 分支。
- `skip_tls_verify` → `.danger_accept_invalid_certs(true)` 对 native-tls 同样有效，保留。
- 建造函数签名扩展：现有 `force_tls12: bool` 旁加 `use_native_tls: bool`（或合并成一个 `enum TlsMode { Default, Rustls12, Native12 }`，实现时择一，spec 不强制，但四处口径必须一致）。

### 4. 覆盖的 client

| crate / 文件 | client | 种类 |
|---|---|---|
| `atomcode-auth/src/oauth.rs` | 登录 `/auth/login`（acs.atomgit.com） | blocking |
| `atomcode-codingplan/src/client.rs` | claim/status/usage（api.gitcode.com） | blocking |
| `atomcode-capabilities/src/provider/openai_compat.rs` | v2 聊天（llm-api.atomgit.com） | async（SSE） |
| `atomcode-core/src/provider/mod.rs` + `openai.rs` | core provider | async |

三处的回退触发逻辑（`should_try_fallback` + 重建 + `latch_managed_tls12`）已在 `522c6f2a` 存在；本设计只是把"回退态的 client 怎么建"从 rustls+1.2 改成（Windows/managed）native-tls+1.2。

### 5. 触发与 latch（不变的机制）

- 正常用户：首个 client 仍是 rustls-1.3（不受影响）。
- managed 端点 connect-reset → `should_try_fallback` → 重建为（Windows）native-tls+1.2 → 成功 → `latch_managed_tls12()` → 后续 managed client 一开始就 native-tls+1.2。
- `ATOMCODE_TLS_MAX=1.2`：Windows 下对 managed 端点从头即 native-tls+1.2（逃生口/免首次探测）。

## 错误处理

- native-tls client 建造失败（罕见）：沿用现有 build 失败路径（返回 `ProviderError`/`Err`，不 panic）。
- native-tls+1.2 仍被 reset（middlebox 升级到连 SChannel-1.2 也拦）：回退已尽力，surface 原始 10054 错误（当前行为）。

## 测试

- 纯逻辑单测：`should_use_native_tls` 的门控矩阵——(managed×非managed) × (capped×未capped)，并在非 Windows 下恒为 false（用 `cfg!(windows)` 使断言随平台成立）。
- `is_managed_https_url` 既有测试不变。
- **真机验证（唯一权威）**：native-tls 的 SChannel 实际握手在本环境与 CI 均无法复现。最终必须由用户在 **Windows 真机**用带此修复的 build 验证：聊天/登录到 *.atomgit.com 能通。属"未真机"待验项。

## 风险 / 权衡

- Windows 构建二进制略增、多一条 SChannel 代码路径（仅 Windows）。
- 若 middlebox 未来连 SChannel-1.2 也拦，本方案失效——当前证据下的最合理解。
- `SSL_CERT_FILE` 在 native-tls(SChannel) 路径不生效（SChannel 只读系统库）；但 managed 端点用公网 CA + 企业库已覆盖，可接受。
