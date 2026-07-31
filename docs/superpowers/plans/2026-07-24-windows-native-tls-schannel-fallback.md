# Windows SChannel(native-tls) 默认后端 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Windows 上让 atomcode 的 reqwest client 以 SChannel(native-tls) 为默认 TLS 后端，绕过 middlebox 对 rustls 指纹的拦截（现有 TLS-1.2 版本回退保持不变，届时作用于 SChannel）。

**Architecture:** 仅 Windows target 给 reqwest 启用 `native-tls` feature → reqwest 默认后端在 Windows 构建里翻为 SChannel（feature unification 使整个 Windows 构建生效）。SChannel 原生信任 Windows 系统证书库，故 Windows 下跳过 rustls 专用的 `add_trusted_roots`（否则会把系统证书重新喂进 native-tls 解析器，风险引入 #514 式构建失败）。非 Windows 完全不变。

**Tech Stack:** Rust；Cargo target-specific dependencies；reqwest 0.12（rustls-tls + native-tls）；`cfg!(target_os = "windows")`。

## Global Constraints

- native-tls **只在 Windows target** 启用：`[target.'cfg(target_os = "windows")'.dependencies]`，`default-features = false`（避免打开 reqwest 一堆默认 feature）。非 Windows 绝不引入 native-tls / OpenSSL。
- `add_trusted_roots` 在 Windows 用 `if !cfg!(target_os = "windows")` **运行时常量**门控（**不用** `#[cfg]` 属性），以保证函数在 Windows 仍被引用、不触发 `dead_code` 警告，同时调用被编译期消除。
- 不新增运行时后端选择函数；不改 `atomcode_config::tls` 逻辑；不改现有 `max_tls_version(TLS_1_2)` 回退。
- 本改动无单元测试接缝（Cargo/cfg 编译期行为，且 SChannel 仅 Windows 运行）→ 属 TDD 的 config 例外；验证靠 `cargo check`/现有测试 + Windows 真机。
- Cargo 对同一 dep 在 `[dependencies]` 与 `[target.*]` 的 features 取并集；target 项只需写 delta `["native-tls"]` + `default-features = false`。

---

### Task 1: Windows SChannel 默认后端 + Windows 跳过 add_trusted_roots

**Files:**
- Modify: `crates/atomcode-auth/Cargo.toml`（已有 windows target 段，约 31 行）
- Modify: `crates/atomcode-core/Cargo.toml`（已有 windows target 段，约 87 行）
- Modify: `crates/atomcode-capabilities/Cargo.toml`（已有 windows target 段，约 228 行）
- Modify: `crates/atomcode-codingplan/Cargo.toml`（**无** windows target 段 → 文件末尾新增）
- Modify: `crates/atomcode-core/src/provider/mod.rs:152`（gate add_trusted_roots 调用）
- Modify: `crates/atomcode-capabilities/src/provider/openai_compat.rs:262-263`（gate add_trusted_roots 调用）

**Interfaces:**
- Consumes: 现有 `add_trusted_roots(builder)`（两处 crate-local，签名不变）；reqwest `native-tls` feature（提供 SChannel 后端 + `.build()` 走 native-tls 默认）。
- Produces: 无新公共符号。行为变化：Windows 构建的 reqwest 默认后端 = SChannel。

- [ ] **Step 1: auth — 给 windows target 的 reqwest 加 native-tls**

在 `crates/atomcode-auth/Cargo.toml` 的 `[target.'cfg(target_os = "windows")'.dependencies]` 段内（`windows-sys = ...` 那行后面）追加：

```toml
# Windows: enable reqwest's native-tls backend (SChannel). Some networks RST the
# rustls TLS fingerprint at the connection layer (os error 10054) while SChannel
# (browser/curl-native) passes; making SChannel the default on Windows dodges it.
# Feature-unions with the base `rustls-tls`; keeps `default-features = false`.
reqwest = { version = "0.12", features = ["native-tls"], default-features = false }
```

- [ ] **Step 2: core — 同样加 native-tls**

在 `crates/atomcode-core/Cargo.toml` 的 `[target.'cfg(target_os = "windows")'.dependencies]` 段内追加同一行（同上注释可精简为一行注释）：

```toml
# Windows SChannel backend for reqwest (dodges rustls-fingerprint RST). See release TLS fix.
reqwest = { version = "0.12", features = ["native-tls"], default-features = false }
```

- [ ] **Step 3: capabilities — 同样加 native-tls**

在 `crates/atomcode-capabilities/Cargo.toml` 的 `[target.'cfg(target_os = "windows")'.dependencies]` 段内追加：

```toml
# Windows SChannel backend for reqwest (dodges rustls-fingerprint RST on *.atomgit.com).
reqwest = { version = "0.12", features = ["native-tls"], default-features = false }
```

注意：capabilities 的 `reqwest` 是 `optional = true`（provider feature）。target 段这行也应保持可选口径——写成 `optional = true` 以匹配基座声明：

```toml
reqwest = { version = "0.12", features = ["native-tls"], default-features = false, optional = true }
```

- [ ] **Step 4: codingplan — 新增 windows target 段**

`crates/atomcode-codingplan/Cargo.toml` 没有 windows target 段。在文件末尾（`[dev-dependencies]` 段之前或之后均可，惯例放 `[dependencies]` 之后、`[dev-dependencies]` 之前）新增：

```toml
[target.'cfg(target_os = "windows")'.dependencies]
# Windows SChannel backend for reqwest (dodges rustls-fingerprint RST on api.gitcode.com).
reqwest = { version = "0.12", features = ["native-tls"], default-features = false }
```

- [ ] **Step 5: core — Windows 跳过 add_trusted_roots**

`crates/atomcode-core/src/provider/mod.rs` 第 152 行，把：

```rust
    builder = add_trusted_roots(builder);
```

改为：

```rust
    // On Windows the default TLS backend is native-tls (SChannel), which trusts the
    // Windows system cert store natively. The rustls-specific root layering is both
    // redundant AND risky there — it re-feeds certs through native-tls's parser, which
    // can reject a cert rustls accepted (a #514-style build failure in reverse). Use a
    // runtime `cfg!` (not `#[cfg]`) so `add_trusted_roots` stays referenced on Windows
    // (no dead_code warning) while the call is compiled out.
    if !cfg!(target_os = "windows") {
        builder = add_trusted_roots(builder);
    }
```

- [ ] **Step 6: capabilities — Windows 跳过 add_trusted_roots**

`crates/atomcode-capabilities/src/provider/openai_compat.rs` 第 262-263 行，把：

```rust
    if trust_os_roots {
        builder = add_trusted_roots(builder);
    }
```

改为：

```rust
    // Skip the rustls root-layering on Windows: the native-tls (SChannel) default backend
    // trusts the Windows system store natively, and re-feeding certs through native-tls's
    // parser risks rejecting one rustls accepted. Runtime `cfg!` keeps the fn referenced
    // (no dead_code) while compiling the call out on Windows.
    if trust_os_roots && !cfg!(target_os = "windows") {
        builder = add_trusted_roots(builder);
    }
```

- [ ] **Step 7: 验证非 Windows 构建不变（rustls 路径）**

Run:
```bash
cargo check --workspace 2>&1 | tail -5
```
Expected: `Finished`，无 error（默认 target 非 Windows：native-tls 未启用，reqwest 仍 rustls-only；`if !cfg!(windows)` = `if !false` → add_trusted_roots 仍调用，路径与今天逐字节一致）。

- [ ] **Step 8: 验证 provider / #514 证书测试仍绿（非 Windows rustls 路径）**

Run:
```bash
cargo test -p atomcode-core -p atomcode-capabilities --features provider 2>&1 | grep -E "test result: (ok|FAIL)|error\[" | tail -20
```
Expected: 全部 `test result: ok`，无 `FAIL`/`error`。特别是 `atomcode-core` 的 `build_http_client_tls_tests`（#514）在非 Windows 仍走 rustls + add_trusted_roots，应全绿。

- [ ] **Step 9: 尝试验证 Windows target 编译（有工具链才做）**

Run:
```bash
rustup target list --installed | grep -q windows && \
  cargo check -p atomcode-capabilities --features provider --target "$(rustup target list --installed | grep windows | head -1)" 2>&1 | tail -15 || \
  echo "no windows target installed — defer Windows compile check to real-machine build"
```
Expected: 若装了 windows target → `Finished`（native-tls feature + cfg 门控在 Windows 编译通过、无 dead_code 警告）；否则打印跳过提示（Windows 编译由真机 build 兜底）。

- [ ] **Step 10: 提交**

```bash
git add crates/atomcode-auth/Cargo.toml \
        crates/atomcode-core/Cargo.toml \
        crates/atomcode-capabilities/Cargo.toml \
        crates/atomcode-codingplan/Cargo.toml \
        crates/atomcode-core/src/provider/mod.rs \
        crates/atomcode-capabilities/src/provider/openai_compat.rs
git commit -m "fix(tls): Windows 用 SChannel(native-tls) 默认后端绕过 rustls 指纹拦截

middlebox 同时拦 TLS 1.3 和 rustls 指纹,唯一能通的是 SChannel+1.2。
Windows target 给 reqwest 加 native-tls feature(默认后端翻为 SChannel),
现有 max_tls_version(1.2) 回退届时作用于 SChannel;Windows 跳过 rustls
专用的 add_trusted_roots(SChannel 原生信任系统库,避免 native-tls 解析器
拒证书)。非 Windows 完全不变。"
```

- [ ] **Step 11: 记录真机验证待办（唯一权威）**

在提交说明或 PR 描述里注明：**需 Windows 真机验证**——用带此修复的 Windows build，在复现网络下确认：
1. 聊天到 `llm-api.atomgit.com` 能通（不再 10054）；
2. 登录 / CodingPlan 到 `*.atomgit.com` 能通；
3. 若配置了第三方 provider（如 OpenAI），仍正常（SChannel 对普通网络无碍）。
CI 与本开发环境均无法复现 SChannel 行为，故此步只能由用户完成，属"未真机"待验。

---

## Self-Review

**Spec coverage：**
- spec §1（Windows-only native-tls，4 crates，feature unification）→ Steps 1-4。
- spec §2（Windows 跳过 add_trusted_roots，两处 build_http_client*，SChannel 原生信任）→ Steps 5-6。
- spec §3（现有 max_tls_version(1.2) 回退不变，作用于 SChannel）→ 未改动即满足（无对应 step，正确）。
- spec §4（覆盖 auth/codingplan/core/capabilities）→ Steps 1-4 覆盖四 crate 的 Cargo；auth/codingplan blocking client 无 add_trusted_roots，仅需 Cargo（Steps 1、4）。
- spec §测试（无新增纯逻辑；编译门控；真机权威）→ Steps 7-9（编译/回归）+ Step 11（真机待办）。
- spec §风险（SSL_CERT_FILE 在 Windows 失效、blast radius、#514 反向风险）→ 已在 Step 5-6 注释与 Step 11 说明中体现。

**Placeholder scan：** 无 TBD/TODO；每个代码步骤含完整改前/改后代码与确切命令、预期输出；Step 9 对"无 windows 工具链"给了确定的降级分支。

**Type consistency：** 未引入新符号；`add_trusted_roots(builder)` 签名沿用现有；两处门控均为 `if [trust_os_roots &&] !cfg!(target_os = "windows")` 同一口径；Cargo target 行四处一致（capabilities 额外带 `optional = true` 以匹配其 optional reqwest 基座声明——已在 Step 3 显式说明）。
