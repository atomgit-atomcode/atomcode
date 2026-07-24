# Windows SChannel(native-tls) 默认后端 — 设计

- 日期：2026-07-24
- 状态：设计已确认（采用 Option A），待写实现计划
- 分支：release/v5.0.3
- 前置：本设计**扩展**已有的 endpoint-aware TLS 回退（commit `522c6f2a`，rustls 1.3 → 1.2）。

## 动机 / 根因（已实测坐实）

用户在某个人网络下，atomcode 到 `*.atomgit.com`（尤其 `llm-api.atomgit.com` 聊天）的请求在 TLS 握手阶段被 reset（Windows `os error 10054`）。浏览器与 curl 正常。

穷举实测（同机同域）：

| 客户端 + TLS 版本 | 结果 |
|---|---|
| curl（**SChannel**）TLS 1.3（默认） | ❌ reset |
| curl（**SChannel**）**TLS 1.2**（`--tls-max 1.2`） | ✅ 通（HTTP 405） |
| atomcode（**rustls**）TLS 1.3 | ❌ reset |
| atomcode（**rustls**）**TLS 1.2**（`ATOMCODE_TLS_MAX=1.2`，v5.0.2 带修复、env 确认生效） | ❌ **仍 reset** |

**结论**：中间设备同时按两轴拦——(a) **TLS 1.3**（任何客户端）、(b) **rustls 的 ClientHello 指纹**（任何版本）。唯一能穿的组合是 **SChannel + TLS 1.2**。atomcode 是 rustls-only，配不出，故 `522c6f2a` 的"只锁版本"对该网络不足。

佐证：仓库已有 `f0855fc5 feat(web_fetch): curl fallback for TLS-fingerprint blocks` —— 开发者早知 rustls 指纹会被此类 middlebox reset，只在 web_fetch 加了 curl 兜底。

## 关键约束（决定了方案形态）

reqwest 0.12.28 的 `TlsBackend::default()`（`tls.rs:589`）：rustls 只在 `not(feature="default-tls")` 时是默认；而 `native-tls = ["default-tls"]`。**因此一旦某 crate 启用 `native-tls` feature，该 crate 里所有未显式 `.use_rustls_tls()` 的 client 在 Windows 上默认走 SChannel。** 无法做到"native-tls 只给回退用、rustls 仍默认"。

据此选定 **Option A：顺势让 Windows 以 SChannel 为默认后端**（native Windows 应用/浏览器/curl 同栈），而非到处 `.use_rustls_tls()` 硬撑 rustls（脆、footgun）。

## 目标

在 **Windows** 上，让 4 个建 client 的 crate（auth / codingplan / core / capabilities）以 **SChannel(native-tls)** 为默认 TLS 后端；managed AtomGit 端点在被 reset 时的回退**仅需版本降级到 TLS 1.2**（沿用 `522c6f2a` 现有逻辑，现在作用于 SChannel）。非 Windows 完全不变（rustls + 现有 rustls-1.2 回退）。

## 非目标

- 不改非 Windows 平台的 TLS（Linux/macOS 保持纯 rustls；不引入 OpenSSL）。
- 不做 curl 子进程兜底（聊天是 SSE 流式，复杂且脆）。
- 不新增 per-client 运行时后端选择函数（SChannel 是 Windows 编译期默认，不是运行时开关）。

## 设计

### 1. 依赖：Windows-only 启用 reqwest `native-tls`

在 4 个 crate 的 `Cargo.toml` 已有的 `[target.'cfg(target_os = "windows")'.dependencies]` 段（auth/core 已有该段；codingplan/capabilities 若无则新增）加：

```toml
[target.'cfg(target_os = "windows")'.dependencies]
reqwest = { version = "0.12", features = ["native-tls"], default-features = false }
```

- Cargo 对同一依赖在 `[dependencies]` 与 `[target.*.dependencies]` 的 feature 取**并集**：Windows 构建 = `rustls-tls` + `native-tls`（默认后端翻为 SChannel）；Linux/macOS = 仅 `rustls-tls`（不变）。
- `native-tls` 在 Windows = `schannel` crate（纯 Windows API，无 OpenSSL、无额外系统依赖，交叉编译到 windows target 亦可）。
- 涉及 crate：`atomcode-auth`、`atomcode-codingplan`、`atomcode-core`、`atomcode-capabilities`（v2 聊天在此）。
- **Cargo feature unification 提醒**：feature 按 crate 全局取并集。只要任一 crate 在 Windows 启用 reqwest `native-tls`，**整个 Windows 构建的 reqwest 都带 native-tls** → Windows 上**所有** reqwest client（含 telemetry/updater/tuix version_check 等未在上表的 crate）都默认 SChannel。这与 Option A 方向一致（这些也都是 atomgit-adjacent 流量，SChannel 更兼容），是**有意接受**的效果，非意外。在这 4 个 crate 显式声明是为表达意图；即便只声明一个，unification 的最终效果相同。

### 2. Windows 跳过 `add_trusted_roots`（SChannel 原生信任系统库）

`add_trusted_roots`（core `provider/mod.rs` 与 capabilities `provider/openai_compat.rs` 各一份）用 `rustls_native_certs` + `rustls::RootCertStore` 给 **rustls** 补 OS 根证书（#514）。SChannel **原生读 Windows 系统证书库**（含企业 MITM CA），这套对 SChannel 冗余且其 rustls 预校验无意义。

- 在两处 `build_http_client*` 里，把 `add_trusted_roots(builder)` 调用用 `#[cfg(not(target_os = "windows"))]` 门控；Windows 走 SChannel 原生信任。
- `add_trusted_roots` 函数体及其 `rustls_native_certs`/`rustls` 用法相应 `#[cfg(not(target_os = "windows"))]`，避免 Windows 下 dead-code 警告 / 无谓依赖编译。
- `skip_tls_verify` → `.danger_accept_invalid_certs(true)`：对 native-tls 同样有效，保留、不门控。

### 3. TLS-1.2 回退（现有逻辑，不改，现作用于 SChannel）

`522c6f2a` 已有的 endpoint-aware 回退**原样保留**：
- `atomcode_config::tls`（`should_cap_url` / `should_try_fallback` / `latch_managed_tls12` / `is_managed_https_url`）签名与逻辑**不变**。
- 各建 client 处 `if force_tls12 { builder.max_tls_version(TLS_1_2) }` **不变**——reqwest 会把 `max_tls_version` 转成 native-tls 的协议版本（`client.rs:573-591 to_native_tls()`），所以在 Windows 下它锁的是 **SChannel 的 1.2**。
- Windows 流程：managed 请求 → SChannel-1.3 → reset → `should_try_fallback` → 重建 SChannel+1.2 → 通 → `latch_managed_tls12()` → 后续 managed client 从头 SChannel+1.2。`ATOMCODE_TLS_MAX=1.2` 可免首次探测。
- 非 Windows：rustls-1.3 → reset → rustls-1.2（与今天完全一致）。

**注意**：本设计**不新增** `should_use_native_tls`（前一版草案里的函数），因为后端由编译期 feature 决定，非运行时。

### 4. 覆盖的 client（与现有回退同范围）

| crate / 文件 | client | 种类 |
|---|---|---|
| `atomcode-auth/src/oauth.rs` | 登录 acs.atomgit.com | blocking |
| `atomcode-codingplan/src/client.rs` | api.gitcode.com | blocking |
| `atomcode-capabilities/src/provider/openai_compat.rs` | v2 聊天 llm-api.atomgit.com | async(SSE) |
| `atomcode-core/src/provider/mod.rs`+`openai.rs` | core provider | async |

`build_http_client*` 的 `max_tls_version` 回退与 `#[cfg(not(windows))] add_trusted_roots` 改动落在 core 与 capabilities 两处 `build_http_client*`；auth/codingplan 的 blocking client 同样 Windows→SChannel 默认（无 add_trusted_roots，本就没有），版本回退已在 `522c6f2a`。

## 已知副作用 / 风险

- **Windows 端整个构建的 reqwest client 都改走 SChannel**（feature unification：含 web_fetch/web_search/mcp/第三方 provider/telemetry/updater/version_check）。SChannel 是 Windows 原生 TLS，通常更兼容；但这是较大面的行为改动，**CI 无法复现，唯一权威是 Windows 真机验证**。
- **`SSL_CERT_FILE` 在 Windows 不再生效**（SChannel 只读系统证书库）。Windows 用户通常用系统库而非该环境变量；可接受、需在文档/发行说明提示。
- **顺带收益**：SChannel 原生读系统库 → Windows 上 #514 的 rustls 证书初始化脆弱性（坏 OS 根 / 空 store）不再适用；web_fetch 的指纹问题在 Windows 也一并缓解。
- 若 middlebox 未来连 SChannel-1.2 也拦，本方案失效——当前证据下的最合理解。

## 测试

- **纯逻辑无新增**（不新增 `should_use_native_tls`；`is_managed_https_url` 等既有测试不变）。
- **编译门控**：`cargo check`（默认 target，非 Windows）确认 `#[cfg(not(target_os="windows"))]` 门控后 add_trusted_roots 在非 Windows 仍编译、rustls 路径不变；`cargo check --target x86_64-pc-windows-*`（若工具链可用）确认 Windows 下 native-tls feature 编译通过、无 dead-code 警告。
- **真机验证（唯一权威）**：SChannel 实际握手在本环境与 CI 均无法复现。最终必须由用户在 **Windows 真机**用带此修复的 build 验证：聊天/登录/CodingPlan 到 *.atomgit.com 能通，且第三方 provider（如配置了 OpenAI）仍正常。属"未真机"待验项。
