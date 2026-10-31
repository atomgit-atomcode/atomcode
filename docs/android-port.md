# Android 移植（II 类：手机本地跑 agent）

目标：在手机上独立完成 VibeCoding，前端复用现有 `webui`；与 PC 的互通走 Git，后续再做。

## 文档索引

| 文档 | 讲什么 |
|---|---|
| 本文 | 移植的整体形态、接缝、构建、能力现状 |
| [plugin-market.md](plugin-market.md) | 插件市场：独立仓库、三层元数据、存储后端可切换、客户端契约 |
| [plugin-ecosystem.md](plugin-ecosystem.md) | 插件目录（10 个）、三种形态、构建输入从哪来、新增插件流程 |
| [external-daemon.md](external-daemon.md) | 连接设备上的外部 `atomcode daemon`：发现机制、会话目录、token 边界、前台服务 |
| [manual-verification.md](manual-verification.md) | 手动验证清单 v1（历史基线） |
| [manual-verification-v2.md](manual-verification-v2.md) | 手动验证清单 v2：以 `market.json` 为准核验产物 |
| [manual-steps-ui-doze.md](manual-steps-ui-doze.md) | 插件页 UI 与 Doze 的细化手动步骤 |
| [manual-verification-report-v2.md](manual-verification-report-v2.md) | 最近一次实测报告 |

## 1. 形态

```
APK
├── MainActivity (Kotlin)      WebView  →  http://127.0.0.1:<port>/?token=…
│                               （页面 = 现有 webui/dist，内嵌在 .so 里）
└── AgentService (前台服务)    持有 wake lock，保证长回合不被系统冻结
        │ JNI
        ▼
libatomcode_android.so (crates/atomcode-android, cdylib)
        │ prebound_listener + run_server(ServerOpts)
        ▼
atomcode-daemon（HTTP + CodingRuntime + 会话事件日志）
        ▼
atomcode-coding / kernel / capabilities（未改动）
```

关键点：**没有第二套 UI，也没有第二套 runtime**。前端是现有 webui，agent 是现有 daemon，
Android 层只决定「产品目录在哪」「服务活多久」。

## 2. 为什么这么接（三处既有接缝）

| 接缝 | 位置 | 用途 |
|---|---|---|
| `ServerOpts::prebound_listener` / `owner_shutdown` | `crates/atomcode-daemon/src/lib.rs` | 本来是给 TUI 进程内 `/webui` 用的；Android 复用它，先绑定 `127.0.0.1:0` 拿到真实端口，再交给 `run_server` |
| `ATOMCODE_HOME` | `atomcode-config::distribution::HOME_ENV` | `Config::config_dir()` 原样采用，宿主把它指向 `context.filesDir`，就完成了 `ProductDirs` 的注入 |
| `world::{FileSystem, Shell}` | `crates/atomcode-capabilities/src/world.rs` | 工具的执行世界。Android 只需让 `LocalShell` 走 `/system/bin/sh`，其余工具不变 |

## 3. 对既有代码的改动（全部很小）

| 文件 | 改动 |
|---|---|
| `crates/atomcode-android/*` | **新增**。宿主层 `host.rs`（启动/停止）+ JNI 层 `jni_api.rs` |
| `crates/atomcode-capabilities/src/tools/bash.rs` | `build_command` 增加 `cfg(target_os = "android")` → `/system/bin/sh`；工具描述与 `Shell:` 行增加 Android 文案（模型不该收到「这是 bash」的假消息） |
| `crates/atomcode-capabilities/src/tools/open_file.rs` | Android 上直接拒绝并说明「用 App UI 打开」，而不是去 spawn 一个不存在的 `xdg-open` |
| `Cargo.toml` | 新增 `[profile.android]`：继承 release，但 `panic = "unwind"`（`panic = "abort"` 会让一次 panic 杀掉整个 App 进程） |
| `scripts/build-android.sh` | 新增。解析 NDK、设 linker、编 cdylib、拷到 `jniLibs/arm64-v8a/` |
| `android/` | 新增。Gradle + Kotlin 工程骨架 |

## 4. 构建

```bash
rustup target add aarch64-linux-android
export ANDROID_NDK_HOME=/path/to/ndk          # r26+
bash scripts/build-android.sh                 # 只编 .so
bash scripts/build-android.sh --apk           # 再打 debug APK
```

### Windows 上的实际做法（2026-10-24 验证通过）

本机环境：Windows + JDK 21 + Gradle 8.11 + AGP 8.7.3 + NDK r26b + OnePlus PLT140（Android 16 / arm64-v8a）。

**1. 工具链**

```powershell
# SDK/NDK 装在工作区内（已 gitignore），不污染全局
sdkmanager --sdk_root=E:/atomcode/.android-sdk "platforms;android-35" "build-tools;35.0.0" "ndk;26.1.10909125"
# 写机器本地 SDK 路径（已 gitignore）
echo "sdk.dir=E:/atomcode/.android-sdk" > android/local.properties
```

**2. 编译 Rust 库**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
ANDROID_HOME=E:/atomcode/.android-sdk bash scripts/build-android.sh
```

**3. 打 APK**

```bash
cd android
gradle assembleDebug --no-daemon
# → android/app/build/outputs/apk/debug/app-debug.apk（43M，含 lib/arm64-v8a/*.so）
```

> **不要用 `./gradlew` 首次构建**：wrapper 会去 `services.gradle.org` 下载发行包，
> 在 TLS 被拦截的网络里会以 `PKIX path building failed` 失败。用本机 `gradle` 即可。
> 只有在需要固定版本、且网络可达时，才 `gradle wrapper --offline` 生成后使用。

### 辅助程序 APK（make / tcc / zig）

`make`、`tcc` 与 `zig` 需要 spawn 子进程，无法作为插件跑，所以放在单独的
`com.atomcode.tools` 里（只提供 `nativeLibraryDir`，无 Activity、无服务）。主 APK 不打包它，
按需安装。

**在设备上装**：插件页的包管理区有一个「下载并安装辅助程序」按钮。它从插件市场取
包（清单里的 `helper` 条目），daemon 核对仓库公布的 sha256 后写进 App 私有目录，
再交给系统安装器 —— 装不装由系统的提示决定，App 不越权静默安装。

**在开发机上装**（走本地构建，不经市场）：

```bash
# 1) 构建（产物 ~51 MB，含 zig 二进制）
cd android-tools && gradle assembleDebug --no-daemon --offline
#    → android-tools/app/build/outputs/apk/debug/app-debug.apk

# 2) 安装
adb install -r android-tools/app/build/outputs/apk/debug/app-debug.apk

# 3) 确认主 APK 认到了它
adb logcat -d | grep helperLibraryDir
#    I AtomCode: helperLibraryDir: /data/app/…/com.atomcode.tools-…/lib/arm64
```

插件页顶部的提示会随安装状态变化；未安装时给的是上面那个下载按钮。`zig` 尚需标准库
（见 [plugin-ecosystem.md](plugin-ecosystem.md)），`make`/`tcc` 装完即可用。

> helper 的二进制在它的 `nativeLibraryDir` 里，主 App 通过 `<queries>` 声明才能看见这个包
> （Android 11 起不声明就查不到，会抛 `NameNotFoundException`）。装好后 App 要重启一次
> 才会把 `make`/`tcc`/`zig` 链进 `<data>/bin`。

**4. 装到手机**

```bash
ADB=.android-sdk/platform-tools/adb.exe
$ADB devices                                   # 需要先开 USB 调试
$ADB install -r -t android/app/build/outputs/apk/debug/app-debug.apk
$ADB shell am start -n com.atomcode.app/.MainActivity
```

**5. 看日志（排查顺序）**

```bash
$ADB logcat -c
$ADB logcat -s AtomCode:V AndroidRuntime:E chromium:V
```

正常启动时 `AtomCode` 标签会依次出现：

```
agent service created
agent server listening at http://127.0.0.1:<port>/?token=…
```

若只有 `agent server failed to start: …`，看 `nativeLastError` 的正文——它来自 Rust 侧的
`anyhow` 链（配置目录不可写、端口绑定失败等）。
若出现 `AndroidRuntime: FATAL EXCEPTION`，先确认 APK 里带的是 `arm64-v8a`，
设备却不是 arm64（`adb shell getprop ro.product.cpu.abi`）。

构建产物：`android/app/src/main/jniLibs/arm64-v8a/libatomcode_android.so`。

只出 `arm64-v8a`：Rust 核心每 ABI 约 40–60 MB，四个 ABI 就是四倍体积换没人用的硬件。

### 已验证（2026-10-24，Windows + NDK r26b / 26.1.10909125）

```
$ bash scripts/build-android.sh
Finished `android` profile [optimized] target(s) in 1m 34s
→ copied 41M to android/app/src/main/jniLibs/arm64-v8a/

$ file .../libatomcode_android.so
ELF 64-bit LSB shared object, ARM aarch64, dynamically linked,
for Android 26, built by NDK r26b (10909125), stripped

$ llvm-readelf --dyn-syms ...
  Java_com_atomcode_app_NativeAgent_nativeInit
  Java_com_atomcode_app_NativeAgent_nativeStart
  Java_com_atomcode_app_NativeAgent_nativeStop
  Java_com_atomcode_app_NativeAgent_nativeLastError

$ llvm-readelf -d ... | grep NEEDED
  liblog.so  libdl.so  libm.so  libc.so
```

依赖只有系统库——没有把 OpenSSL 或其它需要额外打包的 `.so` 带进来。

## 5. 现在能工作 / 还不能工作

**已就位（✓ = 真机实测过）**

- ✓ 进程内启动 daemon，WebView 加载 webui，token → HttpOnly cookie 交接（与浏览器同一条路径）。
- ✓ 会话与配置落在 app 私有目录（`ATOMCODE_HOME`）。
- ✓ 长回合保活：前台服务 + `PARTIAL_WAKE_LOCK`；`idle_timeout_secs = 0` 关掉空闲自杀。
- ✓ `bash` 工具走 mksh；模型被告知没有 bash 专有语法。
- ✓ 打开真实目录：`MANAGE_EXTERNAL_STORAGE`，授权后 workspace 落在
  `/storage/emulated/0/AtomCode`；未授权则回退到 app 私有目录，并在首屏说明原因。
- ✓ 目录浏览与切换：`GET /fs/list?path=…` 能列出共享存储（DCIM / Download / …），
  `POST /cd` 切换后 `GET /project` 立即反映新 `working_dir` —— 即 webui 的 CwdPicker 在手机上可用。
- ✓ telemetry 关闭（见下），`open_file` 明确拒绝。

### 登录（没有终端也能登录）

手机上没有 `/login` 这个斜杠命令，但**不需要它**：webui 自己有完整的登录入口
（设置 → 账户 → 登录）。它的流程是：

```
LoginButton  → POST /auth/login/start {open_browser:false}
             → {login_id, url}
             → window.open(url, '_blank')        ← 授权页
             → 每 2 秒 POST /auth/login/{id}/poll  → pending / authorized
```

daemon 侧不需要回环回调（是轮询式授权），所以手机与桌面走的是同一套接口。

**手机壳必须做两件事，否则登录会卡住：**

1. **外链交给系统浏览器**。WebView 打不开新标签页，`window.open` 返回 `null`，
   webui 会显示「浏览器拦截了授权页的弹出」。
   `MainActivity` 现在通过 JS 桥（`AtomCodeHost.openExternal`）覆写 `window.open`，
   并用 `shouldOverrideUrlLoading` 兜住 `location.href` 的跳转，统一走
   `Intent.ACTION_VIEW` 交给系统浏览器。
2. **授权页在 App 外完成，App 内继续轮询**。用户切回 App 时页面仍在轮询，
   拿到 `authorized` 后 `poll` 返回登录态，侧栏自动变成已登录。

**真机实测（2026-10-24）**

```
POST /auth/login/start
{"login_id":"43ea6c92-…","url":"https://acs.atomgit.com/s/8a4d7a4e",
 "expires_in_seconds":600,"daemon_instance_id":"a75d1cc8-…"}

POST /auth/login/43ea6c92-…/poll
{"status":"pending","user":null,"retry_after_ms":2000}

DELETE /auth/login/43ea6c92-…   → 200
```

**未由机器验证**的一步：在浏览器里真正完成 OAuth 并回到 App 看到已登录——那需要真人账号，
我只验到「拿到授权链接 + 轮询返回 pending + 取消成功」。

### provider 与网关签名（开源构建的硬限制）

**症状**：登录成功，但发消息报

```
[错误: coding runtime provider failed: gateway authentication is unavailable in this build:
 https://llm-api.atomgit.com/v1]
```

**根因**：与 Android 无关。AtomGit 网关（`llm-api.atomgit.com`）要求每个请求带**请求签名**，
纯 OAuth Bearer 会被拒。实测（用已登录账号的 access_token 直连）：

```
POST https://llm-api.atomgit.com/v1/chat/completions
Authorization: Bearer <access_token>
→ HTTP 403 {"detail":{"code":"ATOMCODE_SIG_MISSING",
                     "message":"请升级到最新版 AtomCode 后再试。"}}
```

签名算法在**闭源** crate `atomcode-codingplan-crypto` 里，官方构建管线用它覆盖仓库中的公开 stub。
公开 stub 的 `ALGORITHM_VERSION = 0`，于是
`atomcode-auth::gateway_crypto::signer_available()` 返回 `false`，
provider 层在发请求前就直接拒绝——这就是上面那句 "unavailable in this build"。

**这解释了为什么桌面的官方版能用**：它带 overlay。开源仓库（含本 Android 构建）没有。

#### 两条可行路径

**A. 官方 overlay（已接入并验证）**

私有 crate 在 `https://atomgit.com/atomgit_atomcode/atomcode-codingplan-crypto`（需授权）。
它与公开 stub 的 API 契约**逐字一致**：

```rust
pub const ALGORITHM_VERSION: u8;   // stub: 0    overlay: 1
pub fn sign_v1(method, path, body, oauth_token, user_id, timestamp_unix,
               nonce, client_version) -> Vec<(&'static str, String)>;
```

overlay 返回五个请求头（顺序固定）：
`X-AtomCode-Sig`（`v1:<64 hex>`）、`X-AtomCode-Ts`、`X-AtomCode-Nonce`、`X-AtomCode-Alg`、`X-AtomCode-Ver`。

**接入步骤**（overlay 到位后）：

```bash
# 1. 取到 overlay，放进 workspace 覆盖公开 stub
git clone --depth 1 <private-url> overlay/atomcode-codingplan-crypto
rm -rf crates/atomcode-codingplan-crypto
cp -r overlay/atomcode-codingplan-crypto crates/atomcode-codingplan-crypto
rm -rf crates/atomcode-codingplan-crypto/.git

# 2. 挡住专有代码，别让它进版本库
git update-index --skip-worktree crates/atomcode-codingplan-crypto/Cargo.toml \
                                crates/atomcode-codingplan-crypto/src/lib.rs
git update-index --skip-worktree Cargo.lock

# 3. 带 feature 交叉编译
ANDROID_HOME=<sdk> ATOMCODE_ANDROID_CRYPTO=1 bash scripts/build-android.sh
```

- `crates/atomcode-android/Cargo.toml` 的 `codingplan-crypto` feature 转发到
  `atomcode-daemon/codingplan-crypto` → `atomcode-auth/codingplan-crypto` → overlay crate。
  （`cargo tree -e features -i atomcode-auth` 已验证这条链成立。）
- 用公开 stub 时打开该 feature **不改变任何行为**（版本检查仍报 "unavailable"），
  所以同一条命令在 overlay 前后都能跑。
- 启动时 `host::gateway_signer_available()` 会告诉宿主这个构建有没有网关能力。

**别把开关漏掉（2026-10-25 复现并修好）**：overlay 在工作树里、`ALGORITHM_VERSION = 1`，
但用不带 `ATOMCODE_ANDROID_CRYPTO=1` 的命令打包，链接到的仍是公开 stub，聊天时报

```
[错误: coding runtime provider failed: gateway authentication is unavailable in this build:
 https://llm-api.atomgit.com/v1]
```

三条判据足以区分两种产物：启动日志 `gateway signing available`（stub 构建是
`gateway request signing is unavailable …` 的 warning）；`.so` 里能否搜到
`X-AtomCode-Sig`；以及 `build-android.sh` 头部打印的 `crypto:` 行。带开关重编后
真机 `/chat` 直接拿到模型回复（`OK`，prompt 22298 / completion 43）。

**为什么必须用 `skip-worktree`**：overlay 的 `Cargo.toml` 声明了 `hmac`/`hkdf`/`zeroize_derive`，
而 `atomcode-auth` 用 `optional = true` 的 path 依赖引着这个 crate —— `Cargo.lock` 记的是**所有**
optional 依赖的解析结果，不管 feature 开不开，所以这些私有包必然落进 lock。
`skip-worktree` 让本机构建照常污染磁盘上的 lock，而 `git status` / `git add` / `git commit` 看不到它。
（仓库版 lock 保持干净，判据是 `hkdf`/`hmac`/`zeroize_derive` 三个名字计数为 0。）

**真机实测（2026-10-24，带真实 overlay 的 APK）**

```
POST /chat  provider=AtomGit-qwen3.8-27b
data: {"type":"runtime_info","provider":"AtomGit-qwen3.8-27b","model":"qwen3.8-27b"}
data: {"type":"reasoning","content":" The user is asking me to reply with exactly \"PONG\"…"}
data: {"type":"text","content":"\n\nPONG"}
data: {"type":"tokens","prompt":22299,"completion":35,"total":22334}
data: {"type":"done","stop_reason":"stopped", …}
```

**完整能力验证**（同一台设备）：

| 能力 | 结果 |
|---|---|
| 网关 provider 对话 | ✓ 真实模型回复 `PONG` |
| 写文件 + 读回 | ✓ `/storage/emulated/0/AtomCode/hello.txt`（20 字节，内容正确） |
| `bash` 工具 | ✓ `echo $((6*7))` → `42`（8ms） |
| 工具调用计数 | ✓ `tool_calls: 2`、`rounds: 3` |
| 流式 | ✓ reasoning / text / tokens / done 分段到达 |

**B. 自带 API key 的第三方 provider（不依赖 overlay）**

网关账号被服务端托管，手动新建网关账号会被拒（`refuse_gateway`），
但**任何非网关端点**都可以，走标准 Bearer 鉴权，不需要签名器：

```bash
# 在手机的 webui：设置 → 供应商 → 新建
#   协议：openai-compatible
#   base_url：https://api.deepseek.com/v1   （或任何 OpenAI 兼容端点）
#   api_key：你的 key
#   模型：例如 deepseek-chat
```

真机实测这条路径的完整链路是通的——用一个假 key 发消息，拿到的是**真实的**上游错误，
而不是签名门：

```
data: {"type":"runtime_info","provider":"probe/deepseek-chat","model":"deepseek-chat"}
data: {"type":"warning","message":"API key 未授权或已失效（HTTP 401）：… Your api key: ****robe is invalid"}
data: {"type":"done","stop_reason":"provider_error", …}
```

即：鉴权头正确发出、上游确实收到了请求，只是 key 是假的。换成真 key 即可对话。

#### 对照表

| 需求 | 路径 | 现状 |
|---|---|---|
| 用官方 AtomGit 账号 + 托管模型 | A（官方 overlay） | **✓ 已跑通**：真实模型回复 + 工具调用 + 写文件 + bash |
| 用自有 key + 任意 OpenAI 兼容端点 | B | 链路已通（假 key 拿到真实 401），未配真 key |
| 离线/无网关场景 | B + 本地 Ollama | `ollama` provider 已支持，未测 |

### telemetry 外联核查（2026-10-24）

曾观察到真机日志出现到 `acs.atomgit.com` 的 HTTPS 连接，需要确认是不是 telemetry 在上报。

**结论：telemetry 处于关闭状态，不可能发送。** 三条独立证据：

1. `resolve()`（`atomcode-telemetry/src/config.rs`）把 `CliOverride{disabled:true}` 映射为
   `TelemetryState::Disabled("cli:--no-telemetry")`；Android 宿主正是传的这个值。
2. `Telemetry::init`（`runtime.rs:198`）在 `Disabled` 时直接返回
   `enabled:false, tx:None, queue:None, sender_task:None` —— 没有队列也没有发送任务。
3. `maybe_emit_install_completed` 第一行就是 `if !self.enabled { return; }`，首次安装事件也不上报。

**那批连接的归因**：未能复现。冷启动 20 秒内 `acs.atomgit.com` 出现 0 次；随后逐个调用
`/models`、`/auth/status`、`/config`、`/codingplan/status`、`/providers`、`/plugins`、`/skills`、
`/sessions`、`/project`、`/live/meta`，每个请求前后计数都是 0 增量。设备上既无 `auth.toml`
也无 `config.toml`（`/auth/status` 走不联网分支），且全仓库只有 `atomcode-auth` 构造该域名的 URL，
其后台线程只在实际登录时启动。

**仍未归因**：早期那次突发（约 2 秒节奏、连续 8 秒以上）。要彻底钉死需要一次带 URL 日志的
debug 构建（在 reqwest 客户端层打点），这会改动生产代码，暂不为此改。当前证据足以排除
「telemetry 在偷偷上报」这一条，`AGENTS.md` 的凭据/遥测边界未被破坏。

**已知缺口（下一步，不是隐藏项）**

1. **外部工具：已补 jq / rg / curl / python3，node / npm 装不了**（2026-10-25 更新）。

   真机实测（`command -v`，agent 自己的 bash 工具给出的原始输出）：

   | 有 | 缺 |
   |---|---|
   | toybox 一整套：`sed` `awk` `grep` `find` `tar` `unzip` `sort` `wc` `cut` `tr` `diff` `patch` `sha256sum` `xxd` `strings` …（共 211 个 applet） | `node` `npm` `perl`、C 工具链 |
   | 随 APK 打包：`git` `jq` `rg` `curl` `python3`（RustPython） | |

   Android 10+ 禁止从 app 数据目录 `exec`，所以打包的工具必须进 `nativeLibraryDir`（伪装 `lib*.so`），
   由宿主在 `<data>/bin` 写同名符号链接并前置 `PATH`——`git` 当年就是这么落地的，`jq`/`rg`/`curl`
   复用同一条通道：

   | 工具 | 二进制 | strip 后 | 来源 |
   |---|---|---|---|
   | `jq` 1.7.1 | `libjq.so` | 883 KB | jq + 自带 oniguruma，Linux NDK 静态交叉编译 |
   | `rg` 15.2.0 | `librg.so` | 4.98 MB | `cargo install ripgrep --target aarch64-linux-android` |
   | `curl` 8.11.1 | `libcurl.so` | 4.67 MB | git 的构建脚本本来就编译它，顺带走一遍 staging |
   | `python3`（RustPython 0.6.0） | `libpython3.so` | 33.5 MB | `cargo install rustpython --features freeze-stdlib`，stdlib 冻结进二进制 |

   `python3` 走的是 **RustPython**（纯 Rust 的 Python 3 解释器），`freeze-stdlib` 把标准库
   冻进可执行文件，所以 APK 里只有**一个文件**，不需要单独的 `Lib/` 树。
   真机实测：`Python 3.14.0.alpha`，`python3 -c "print(2**10)"` → `1024`，
   带 `import json, os, sys` 的脚本 `ALL PASS`。

   `node`/`npm`/`perl` 仍然**不打包**：没有官方 Android 构建，运行时本身数十 MB；
   描述里明确说「没有包管理器，别承诺安装」，模型据此改用
   `read_file` / `grep` / `glob` / `edit_file`，有 python3 后也可以直接写脚本验证。

   **这一项曾经是个真 bug**：`bash` 工具的 Android 描述硬编码了一句
   「The phone ships almost no CLI toolset — `git`, `ripgrep`, `node`/`npm`, `python3`, `jq`,
   `sed -i` and many coreutils are absent」。这句话两头都错——`git` 随 APK 打包、`sed -i` 在
   toybox 里能用——而模型**照着它下的结论**，于是拒绝使用明明可用的工具、还提议去装装不了的东西。
   现在改为**按设备实测**生成（`crates/atomcode-capabilities/src/tools/bash/android_tools.rs`）：
   扫一遍 `PATH` 里真实可执行的命令，把「有哪些 / 缺哪些」写进描述。
   真机实测后模型回答变成：

   ```
   (1) Yes — `sed`, `awk`, `grep`, `find`, `tar`, and `git` are all available.
   (2) No — `node`, `npm`, and `python3` are not installed, and there is no package manager to install them.
   ```

   构建：`bash scripts/build-android-git.sh` 会一并产出这三个（需要 Linux NDK + Rust 工具链），
   从零构建的前置条件与步骤见该脚本头部注释。
2. **Git 互通只差凭据与 UI**：设备上已有可用的 `git`，clone / pull / push 机制实测通过；
   但「把 HTTPS token 安全交给 agent」和 webui 里的 Git 面板还没做（见上一节「还没做」）。
3. **SAF 未接**：现在用全盘访问换取真实路径。若日后要上架 Google Play，需要改用 SAF +
   持久的 `content://` 授予，并给工具层补一个 SAF 世界的实现（`world::{FileSystem,Shell}` 接缝）。
4. **未验证**：前台服务在 Doze 下长时间运行、内存占用都还没量过；
   审批卡在窄屏上的表现也没看（`PermissionCard` 是 webui 现有的组件）。

### P5：窄屏适配（已实现，真机实测 2026-10-25）

webui 原本按桌面排版写的，手机上（360×785）有三处硬伤：顶部按钮被状态栏压住、
模式下拉框溢出屏幕左侧、composer 控件挤在一行被压成「q…」。这一节记录根因与修法。

**根因与归属**

| 现象 | 根因 | 修在哪 |
|---|---|---|
| ☰ 与会话标题被状态栏压住 | Android 15+（targetSdk 35）强制边到边，窗口铺到状态栏下 | webui 读 `env(safe-area-inset-top)`（实测该机为 32px），`.app` 留出顶部内边距 |
| composer 底部被系统栏/键盘盖住 | WebView 的 `env(safe-area-inset-bottom)` 在本机恒为 0，拿不到真实数值 | 宿主层 `MainActivity` 用 `ViewCompat.setOnApplyWindowInsetsListener` 取真实 systemBars / ime inset，给 WebView 容器加底部内边距 |
| 模式下拉框溢出屏幕（`x = -93`） | 下拉框用 CSS `right: 0` 锚在触发器右边缘，而 `min-width: 200px` 大于触发器宽度（86px） | 改成按视口定位：`placeDropdown` 量出触发器位置后把菜单钳制在屏内，必要时翻到另一侧 |
| 下拉框浮到屏幕顶端，不贴着选择框 | 定位时用「可用空间高度」而不是菜单自身高度算 `top`，菜单的底边并没有贴住触发器 | `placeDropdown` 改用菜单**实测高度**定位：上方打开时底边贴触发器顶边，下方打开时顶边贴触发器底边；菜单先以 `visibility:hidden` 渲染一帧量出高度再定位 |
| 模型 / 强度菜单点了没反应（模式正常） | `ModelSelector` 里留着自己的一份「点击外部关闭」处理器，它检查的是**触发器容器**；而菜单已 portal 到 `body`，菜单项落在容器之外 → mousedown 先关菜单，选项收不到 click | 删掉组件内的旧处理器，外部点击判定统一由 `useDropdown` 负责，并把**菜单本身**算作「内部」 |
| 模式 / 强度 / 模型挤在一行 | 窄屏下三个控件同处一行，模型名被压成「q…」 | 窄屏媒体查询里显式分两行：强度+模型一行，模式+发送键一行 |

**实测数据（OnePlus PLT140 / Android 16，视口 360×785）**

- 顶部：`.app` 的 `padding-top` 为 32px，`.main-column` 从 `y=32` 起，☰ 按钮在 `y=42`，
  不再压在状态栏下；侧栏抽屉是 `position: fixed`，不吃 `.app` 的内边距，单独加了同一条
  `padding-top`。
- 下拉框：模式 / 模型 / 强度三个菜单的 `overflowsLeft / overflowsRight / overflowsTop /
  overflowsBottom` 全为 false，所有条目可见。
- 下拉框贴位：落地态与对话态各测一次，三个菜单的 `gapAbove` 均为 **4px**（菜单底边到
  触发器顶边），即菜单紧贴选择框上方，不再浮到屏幕顶端。
- 控件重排：模型名完整显示为 `qwen3.8-27b`，模式框与发送键同处第二行。
- 键盘：弹出时 composer 完整可见，未被输入法遮挡（宿主按 ime inset 收缩容器）。
- 点击可用：菜单 portal 到 `body` 后，点击选项仍能选中（外部点击判定必须把菜单算在内，
  否则 mousedown 先关菜单、选项收不到 click）。真机用**真实触摸事件**（CDP
  `Input.dispatchTouchEvent`，与手指同路径）逐个复测：模式 `Build → Plan`、模型
  `qwen3.8-27b → glm5.3-flash`、强度 `Medium → Low`，三者 `changed` 均为 true，
  且菜单点击后关闭。

**新增文件**

- `webui/src/lib/dropdownPlacement.ts`：视口内定位 + 顶部安全区感知（`safeTopInset()`）。
- `webui/src/lib/dropdownPlacement.test.ts`：9 条用例，覆盖「紧贴触发器」「窄触发器 +
  宽菜单」「贴右边缘」「上下空间不足时翻转」「高度受限」「菜单宽于视口」「不得滑进状态栏」。
- `webui/src/lib/useDropdown.ts`：共用的下拉框 hook（先量尺寸再定位 + 外部点击判定）。
- `webui/src/lib/dropdownMenus.test.ts`：3 条用例，钉死「外部点击判定只在 `useDropdown`
  一处」「判定必须把菜单本身算作内部」「菜单必须 portal 到 body」。

**注意**

- 模式/模型下拉框改用 `createPortal` 挂到 `body`，与 Sidebar 既有弹出菜单同一做法：
  移动端侧栏是 `transform` 的抽屉，会成为 `position: fixed` 的包含块并裁剪子元素。
- 桌面端不受影响：桌面没有状态栏，`env(safe-area-inset-top)` 为 0，菜单仍在触发器附近。

### P5.5：shell 能力与「写脚本验证」（真机实测 2026-10-25）

这一段记录两件容易被误判的事：**mksh 到底能做什么**，以及**agent 在手机上怎么写验证脚本**。
两者都曾经被写错，而且错的都是我们自己的描述。

**mksh 被低估了**。原 `android_suffix` 写着「no arrays, no `set -o pipefail`」和
「Use `case` instead of `[[ … ]]`」，真机逐项实测后全部推翻：

| 能力 | 旧描述 | 实测（OnePlus PLT140 / Android 16） |
|---|---|---|
| `set -o pipefail` | 不支持 | **支持** |
| `[[ … ]]` | 让改用 `case` | **支持**（只有 `=~` 正则不支持） |
| 数组 | 不支持 | **支持**（`a=(x y)` 与 `set -A a -- x y` 都行） |
| 花括号展开 `{a,b}` | 不支持 | **支持**（展开成 `a b`） |
| heredoc / herestring | （未提） | **支持**，但依赖 `TMPDIR` 可写——见下 |
| `${v:1:3}` / `${v/a/b}` / `local` / `trap` / `$((…))` | （未提） | **支持** |
| `[[ … =~ … ]]` | 不支持 | 不支持（确实要改用 `case`） |
| 进程替换 `<(…)` | 不支持 | 不支持 |
| `select` | （未提） | 不支持 |

**一个真 bug：`TMPDIR` 没设，heredoc 静默失败**。mksh 用临时文件实现 heredoc 和
herestring；`TMPDIR` 为空时它落到 `/data/local`——app 进程写不了——于是
`cat <<EOF` 报 `can't create temporary file …: Permission denied`。这等于把「用 heredoc
写一个验证脚本」这条最顺手的路堵死了。现在 `host.rs` 在启动时把 `TMPDIR` 指向
`<data>/tmp` 并确保目录存在；修完后同一段脚本直接跑通。

**一个纯环境限制：工作目录是 `noexec`**。`/storage/emulated/0` 按 `noexec` 挂载，
所以 `./script.sh` 会以 `can't execute: Permission denied`（exit 126）失败，即使
`chmod +x` 也不行；`sh script.sh` 正常。已写进工具描述，模型不再浪费一个回合去撞它
（真机复测：改描述后 agent 一次用对）。

**结论：当时不补解释器，后来补了 RustPython**。`sh`（上述能力齐全）+ `awk` + `jq` + `bc`
已覆盖验证脚本需要的文本处理、JSON、算术；`micropython` 之类只提供 Python 子集、模型容易
写出不兼容代码，收益不抵复杂度。后来改为打包 **RustPython**（完整的 Python 3 解释器，
stdlib 冻结进二进制），`python3 script.py` 现在可直接用——见前面「外部工具」一节。

**真机端到端**：让 agent 用 heredoc 写 `verify.sh`（数组 + 循环 + 非空断言）并运行，
一次通过：

```
OK: name #1 'Alice' is non-empty
OK: name #2 'Bob' is non-empty
OK: name #3 'Carol' is non-empty
All assertions passed
```


### P5.6：核对 agent 自述的「不可用清单」（2026-10-25）

让 agent 自己盘点环境后，它交了一份 `lose.txt` 式清单。逐条核对的结果是：
**绝大多数属实，一条是真 bug，另有一条结论下得过满**。

**属实且无法解决**（平台本身没有，也不打算打包）：

| 类别 | 缺失 | 说明 |
|---|---|---|
| 运行时/解释器 | `node` `npm` `ruby` `perl` `php` `lua` `java` `go` `rustc` `dotnet` | 无官方 Android 构建；运行时数十 MB（`python3` 已改为打包 RustPython，见上表） |
| 编译工具链 | `gcc` `cc` `clang` `make` `cmake` `ninja` `gdb` `strace` | NDK 的 clang 只在构建机上用，不进 APK |
| 常用工具 | `wget` `openssl`(CLI) `sqlite3` `ssh` `scp` `adb` `su`/`sudo` `xz` `zip` `less` `vim` `nano` | toybox 只提供 `more`/`vi`/`unzip`；`zip` 只能解不能打 |

**核对后推翻的结论**：

1. 「无法运行任何解释型脚本」——过满。`sh`（mksh，能力见 P5.5）、`awk`、`jq`、`bc`
   都在，验证脚本能写也能跑。
2. 「本机缺少 CA 根证书链（不是断网、不是墙）」——**这是真 bug，但归因错了**。
   根证书在设备上（`/system/etc/security/cacerts`），我们甚至已经把它们拼成了
   `<data>/ca-bundle.pem` 给 git 用；漏的是**没告诉 curl**。

**真 bug：curl 的 HTTPS 全靠 `-k` 才能过**。`git clone https://…` 一直正常，
`curl https://…` 却报 `SSL certificate problem: unable to get local issuer certificate`
（exit 60 / code 000）。原因是每个 TLS 客户端读自己的变量：git 读 `GIT_SSL_CAINFO`
（已设），curl 读 `CURL_CA_BUNDLE`（没设）。同一次构建里一个工具信、另一个不信，
是最容易让人误判成「没有根证书」的状态。

修法：在 `shims.rs` 里把三个变量统一指向同一份 CA 包（`ca_env_vars`），并加测试钉死。

```
GIT_SSL_CAINFO = CURL_CA_BUNDLE = SSL_CERT_FILE = <data>/ca-bundle.pem
```

真机复测（同一台设备，修复前后对比）：

| | 修复前 | 修复后 |
|---|---|---|
| `curl https://atomgit.com/` | `000`（证书错） | **`200`** |
| `git ls-remote https://atomgit.com/…` | 正常 | 正常 |

**另外三条属实但属于环境/配置，不是缺件**：工作目录 `noexec`（见 P5.5）、当时目录
不是 git 仓库（清场后的正常状态）、skills/MCP/AGENTS.md 均为 0（用户没配，不是加载失败）。


### P5.7：能不能把 CPython / Zig 做成「可安装插件」？（真机实测评估，2026-10-25）

想法很自然：APK 不膨胀，用户按需装 `python3`、`zig` 这类包，不用了再删。
先看平台允许什么——这里有一条**决定性的分界线**，实测数据如下（同一台设备、
同一个 app 进程域 `untrusted_app`）：

| 操作 | 结果 | 说明 |
|---|---|---|
| `execve` 数据目录里的二进制 | ❌ `Permission denied`（exit 126） | Android 10+ / targetSdk 29+ 的 W^X 限制 |
| `dlopen` 数据目录里的 `.so` | ✅ `dlopen OK` | AOSP sepolicy 明确保留这条路径 |

（`dlopen` 这一条不是查文档得出的：编了一个 20 行的探针放进 `nativeLibraryDir`，
让它 `dlopen` 复制到 `<data>/` 里的 `.so`，探针打印 `dlopen OK`；
同一次会话里 `execve` 那个副本是 126。）

**所以：插件只能是「库」或「数据」，不能是「可执行文件」。**

> 这条限制容易测错。用 `adb shell run-as` 测会得出「能 exec」的**假结论**——
> `run-as` 落在 `runas_app` 域，而 app 进程是 `untrusted_app`。必须在真正的
> app 进程里测（用它自己的 `bash` 工具）才算数。

**四种形态，按可行性排序：**

| 形态 | 平台允许 | 适用 | 不适用 |
|---|---|---|---|
| **A. 数据包**（stdlib、wheel、模型、字体） | ✅ 本就可行 | 只要不 exec，随便装 | — |
| **B. `.so` 插件**（`dlopen`） | ✅ 实测可行 | 可被嵌入的解释器：CPython(`libpython3.so`)、RustPython、Lua、QuickJS | 进程式工具链：Zig / clang / node（它们要 exec 自己和子进程） |
| **C. 独立 helper APK** | ✅（二进制在它自己的 `nativeLibraryDir`，天然可 exec） | **任何必须 exec 的工具链**：Zig、clang、node、完整 CPython | 需要写 IPC、要有安装引导 |
| **D. 远程执行** | ✅ | 真实项目的全量构建/测试 | 必须有一台配对机器 |

**为什么重型工具链只能走 C 或 D**：Zig / clang / node 是「进程模型」程序——编译器要
fork/exec `cc1`、`ld`，npm 要起子进程。就算能把它们 `dlopen` 进来，它们内部的 exec
照样会被同样的策略拒绝。这不是打包技巧能绕过的。

**形态 B 的三个坑**（如果要做）：

1. **崩溃隔离**：脚本 segfault 会带走整个进程，而 agent 是长跑进程。解法是在独立的
   app 进程（`android:process=":runtime"`）里加载——那个进程由 zygote fork 出来，
   不涉及 exec，策略允许。
2. **可执行代码校验**：下载的 `.so` 就是可执行代码，必须做签名/哈希校验再加载。
3. **每个插件要定制集成层**：CPython 走 C API、RustPython 走 Rust API，没有统一接口；
   `dlopen` 只解决「能加载」，不解决「怎么驱动」。

**形态 C 的形状**：helper APK 把工具链放进自己的 `jniLibs`（跟本产品打包 git 的办法
完全一样），主 app 通过 Binder/AIDL 让它执行命令、转发 stdout/stderr/退出码。
安装/卸载就是装/卸 APK，用户可见可控；未安装时主 app 降级为「提示去装」。

**建议的组合**（如果真要做）：

- 小解释器（RustPython ~20 MB）**直接内置**，不进插件系统——否则每台设备首次使用都要下载。
- 重型工具链（Zig ~120 MB、完整 CPython、node）走**形态 C（helper APK）**。
- 纯数据（pip 包、stdlib、模型）走**形态 A**，随时装删。
- 真实项目的构建/测试仍然优先**形态 D**：手机上可用内存约 1.4 GB，跑大项目构建本来就吃力。

**成本量级**：A 约 1–2 天；B 约 1–2 周（交叉编译 + 独立进程 + 每插件集成）；
C 约 1–2 周（IPC + helper + 安装引导 + 降级）；D 约 3–5 天。

**结论**：想法方向对，但要换个形态——把「插件 = 可执行包」改成「插件 = 数据包 or
helper APK」，因为 `execve` 那条路在 targetSdk 29+ 上是死的，而 `dlopen` 只对
「可被嵌入的库」有意义。**按这个结论实现的框架与 CPython 插件见下一节。**

### P5.8：运行时插件框架 + CPython 插件（已实现，真机验证 2026-10-25）

上面评估选了形态 B（`.so` 插件 + `dlopen`）。这一节是落地实现。

**为什么必须是「库」而不是「程序」**：Android 对 targetSdk 29+ 的应用禁止从数据目录
`execve`，但 `dlopen` 允许（P5.7 的实测）。所以插件不是可执行文件，而是共享库；真正被
执行的只有 `nativeLibraryDir` 里的一个启动器。

**三个部件**

| 部件 | 位置 | 职责 |
|---|---|---|
| `packages` 模块 | `crates/atomcode-daemon/src/packages.rs` | 只管磁盘：拉取、校验、解包、列表、删除。从不加载代码 |
| `pluginhost` | `crates/atomcode-android/src/bin/pluginhost.rs` | 唯一能执行的部分：`dlopen` 插件的库并调用入口，**独立进程**（插件崩了带不走 daemon） |
| CPython 插件包 | `scripts/build-android-cpython-plugin.sh` | `libpython3.12.so` + `plugin_main` shim + 精简 stdlib |

**包格式**：一个 `.tar.gz`，根目录带 `package.json`——一个文件下载、一个哈希校验：

```json
{ "name": "cpython", "version": "3.12.10",
  "entry": "lib/plugin.so", "symbol": "plugin_main" }
```

装到 `<product>/packages/<name>/`。换成 tar.gz 而不是目录树，是为了让「下载」和「校验」
各只有一个对象。

**插件 ABI**：

```c
int plugin_main(int argc, char **argv);   // argv[0] 是包名，argv[1..] 是调用者的参数
```

调用前 `pluginhost` 会设 `PLUGIN_DIR`（包目录），插件据此找到随包的数据（CPython 用它
设 `PYTHONHOME`），不需要猜自己在哪。

**API**（都在 daemon 里，`GET /packages` 一类）：

| 路由 | 作用 |
|---|---|
| `GET /packages` | 已安装列表（名字、版本、入口、占用字节） |
| `POST /packages/install` | `{url\|path, sha256?}` —— 下载或本地安装，校验后原子替换 |
| `DELETE /packages/:name` | 卸载（删目录） |

安装先解到 `.staging-*`，校验通过才 `rename` 到位；升级时旧目录先挪成 `.retired-*`，
失败会**换回去**——一次失败的升级不能变成一次卸载。

**安全边界**（都有测试）：包名只能是 `[A-Za-z0-9][A-Za-z0-9._-]*`（不可能是路径）；
包内路径只接受普通组件，绝对路径 / 盘符 / `..` 一律拒绝整个包；`entry` 必须在包内且存在；
体积上限 512 MB；给了 `sha256` 就必须匹配。

**CPython 插件**：`libpython3.12.so`（25 MB，SONAME `libpython3.12.so.1.0`，靠
`-rpath $ORIGIN` 找到同目录的库）+ 一个 40 行的 `plugin_main` shim（把 argc/argv 交给
`Py_BytesMain`）+ 精简 stdlib。打完 **15 MB 压缩 / 47 MB 展开**。

裁剪掉的：`test`（CPython 自带测试，133 MB）、`idlelib`、`tkinter`、`turtledemo`、
`ensurepip`、`lib2to3`、`pydoc_data`、`config-3.12`、`__pycache__`。

**真机端到端**（OnePlus PLT140 / Android 16）：

```
POST /packages/install {"path":"/sdcard/Download/cpython-3.12.10-arm64.tar.gz","sha256":"7820…"}
→ {"package":{"name":"cpython","version":"3.12.10","size_bytes":47640903,…},"success":true}

$ pluginhost cpython t_cp.py
version 3.12.10
impl CPython
json 1
cwd_ok True
ALL OK

DELETE /packages/cpython → {"removed":true,"success":true}
$ pluginhost cpython -c "print(1)"
pluginhost: cannot read …/packages/cpython/package.json: No such file or directory (os error 2)
```

**踩到的坑**：`tar -C dir -cf - .` 会先写一条 `./` 条目，最初的路径校验把它当成越界
（`bundle entry escapes the package root: ./`），于是**任何一个正常打的包都装不上**。
现在区分「归档根目录」（跳过）和「逃逸」（整包拒绝）。

#### 其他插件：Lua 与 SQLite

CPython 是最麻烦的一个（要交叉编译整个解释器 + 拆 stdlib）。纯 C 的程序简单得多，
于是有了两条更轻的插件，同时把它们收敛成一个**通用模式**：

> 程序本身有 `int main(argc, argv)` → 编译时用 `-Dmain=<符号名>` 把它改名 →
> `scripts/plugins/plugin_main.c` 里的 `plugin_main` 直接转发。**一行行为差异都没有**，
> 不 fork、不 exec、不开第二进程。

| 插件 | 版本 | bundle | 展开 | 做法 |
|---|---|---|---|---|
| `lua` | 5.4.7 | 132 KB | 316 KB | 把 `src/*.c`（除 `luac.c`）连同改名后的 `lua.c` 静态链进一个 `.so` |
| `sqlite3` | 3.50.4 | 732 KB | 1.5 MB | `sqlite3.c`（引擎）+ `shell.c`（CLI，`main` 改名）+ shim |

两个都是一条命令（`scripts/build-android-plugins.sh`）产出，纯 C、无外部依赖，
体积小到适合当默认推荐。真机实测：

```
$ lua -e "print(_VERSION, 2^10)"
Lua 5.4	1024.0

$ sqlite3 t2.db "create table t(a int, b text);
                 insert into t values (1, char(97,108,112,104,97)), (2, char(98,101,116,97));
                 select a || char(58) || b from t order by a;"
1:alpha
2:beta

GET /packages/health
{"packages":[
  {"name":"cpython","ok":true,"detail":"3.12.10"},
  {"name":"lua","ok":true,"detail":"5.4.7"},
  {"name":"sqlite3","ok":true,"detail":"3.50.4"}],"success":true}
```

`sqlite3` 在手机上有额外意义：以前 `.db` 文件只能靠 `read_file` 看二进制，
现在能直接建表查数据。`LUA_USE_DLOPEN` 是开着的，所以 Lua 侧 `package.loadlib` 也能用。

**不受支持的那类**：像 Zig / clang / node 这种「进程模型」工具链做不成插件——它们要
`exec` 自己和子进程，而那条路在 targetSdk 29+ 上是死的（见 P5.7）。插件模式只适合
「能被嵌进一个进程」的程序，Lua 和 SQLite 正好是。

**和内置 RustPython 的分工**：内置 `python3` 走 RustPython，零安装、体积在 APK 里；
CPython 插件按需装，能跑需要真 CPython 语义的东西（`platform`、字节码细节、将来的 C 扩展）。
两者用不同的命令，不互相覆盖。

#### 装完就像普通命令（别名）

只靠 `pluginhost cpython …` 用起来别扭，所以安装时会在 `<product>/bin` 建一条
`cpython -> pluginhost` 的符号链接——`bin/` 正是 `git`/`jq`/`python3` 那些 shim 所在的
目录，已经在 `PATH` 里。于是 `cpython script.py` 直接可用。

- **谁决定命令名**：manifest 的 `name`，加上可选的 `aliases`（如 `["pycp"]`）。
- **`pluginhost` 怎么知道自己被当成谁**：看 `argv[0]` 的文件名。名字等于 `pluginhost`
  （或 `libpluginhost.so`）时走显式形式 `pluginhost <command> [args…]`；否则就是别名调用，
  `argv[0]` 的文件名就是包名/别名，其余都是它的参数。插件拿到的 `argv[0]` 是**调用者
  敲的那个名字**，所以 `sys.argv[0]` 看着和真二进制一样。
- **可以接管已有命令，但必须可还原**：见下面「接管 host 命令」。
- **卸载时摘链**：先删链接再删目录，否则会留下一条指向已删除包的链接，exec 时报一层
  莫名的加载器错误。

真机实测：

```
$ command -v cpython
/data/user/0/com.atomcode.app/files/atomcode/bin/cpython
$ cpython -c "import sys; print(sys.version.split()[0], sys.implementation.name)"
3.12.10 cpython
$ pluginhost cpython -c "print(6*7)"     # 显式形式仍然可用
42
```

#### 接管 host 命令（`python3` 之争）

最初的做法是「插件绝不抢占 host 已有的命令名」。这条守卫挡住了真实诉求：装完 CPython
插件后，`python3` 仍然指向随 APK 打包的 RustPython，于是问「检查 python 环境」得到的
永远是 RustPython——**看起来像插件没生效，其实是它根本没拿到那个名字**。

现在改成「可以接管，但必须可还原」：

- 装包时若同名链接已存在且指向别处（`libpython3.so` 这类），不再拒装，而是先把
  **被覆盖目标的 basename** 写进 `<product>/.bin-overrides/<name>`，再把链接改成
  `→ pluginhost`。
- 为什么只存 basename：host 链接指向 APK 的 `nativeLibraryDir`，而那个绝对路径
  **每次 app 更新都会变**（`/data/app/~~hash/…`）。存全路径正好在最需要它的时候失效。
  还原时用「当前还活着的某条 host 链接」反推出目录，再拼回 basename。
- 卸载、或升级后不再声明该别名时，`remove_alias_link` 摘掉自己的链接并**还原** host
  链接，然后删掉记录。
- **host 必须让路**：`shims` 每次启动都会重写自己的链接（`git`、`jq`、`python3`…）。
  它现在先查 `.bin-overrides/<name>`，有记录就跳过——否则重启一次就把 `python3` 悄悄
  抢回去了。
- 唯一仍然拒绝的情况：那个名字被一个**真实文件**占着（不是链接），因为没有东西可还原。

真机闭环（装 → 重启 → 卸载）：

```
install cpython（aliases: ["python3","python"]）
  bin/python3 -> pluginhost          .bin-overrides/{python,python3}
  $ python3 --version                → Python 3.12.10 / cpython

重启 app
  bin/git     -> …/新 hash…/libgit.so     ← host 照常重写自己的
  bin/python3 -> pluginhost               ← 被接管的让路

uninstall cpython
  bin/python3 -> …/当前 hash…/libpython3.so   ← 还原到当前目录，不是旧路径
  $ python3 --version                → RustPython 3.14.0.alpha
```

#### 描述里必须说出插件命令

比「谁该叫 python3」更根本的一个 bug：`bash` 工具描述里的工具清单来自固定白名单
`TOOL_NAMES`，里面没有 `cpython`/`lua`/`sqlite3`/`tcc`。于是**命令装了、能跑，但模型
不知道它存在**——它没有任何线索去试，只会继续用 `python3`。

现在描述会在固定清单之外，把「自己 bin 目录里不在白名单的符号链接」也列出来
（`extra_commands_in`：找到含 `pluginhost` 的那个 PATH 目录，列出其中的链接名），并附一句
提醒：清单里出现的名字可能已被某个插件提供，用 `command -v` 确认真身。

真机验证（问模型「描述里说装了哪些额外命令」，不执行任何东西）：

```
git-credential-store, …, lua, sqlite3, tcc
```

#### webui 管理面板

侧栏加了「运行时插件」入口（展开态与收起 rail 都有），面板复用 `.modal-card` 家族，
窄屏自动变成底部抽屉。功能：列出已装包（名字、版本、占用、可用的命令名）、按 URL 或
设备上的 `.tar.gz` 路径安装、可选 SHA-256 校验、卸载。文案中英双语。

API 客户端在 `webui/src/api.ts`（`listPackages` / `installPackage` / `removePackage`），
面板在 `webui/src/components/Packages.tsx`。

**这一轮踩的坑**：第一次验证时 `bin/` 里始终没有 `cpython` 链接，装一个名为 `git` 的包
也没被拒绝。原因不在代码——我在 gradle 还在写 APK 时就去 `adb install`，装到的是上一次
的包，设备上的 `.so` 比新构建的小 11 KB。用 `strings` 对比设备上 `libatomcode_android.so`
里有没有新代码才定位到。**构建没完全结束前不要装包**。

#### 安装进度流

47 MB 的包在手机网速下装很久，一个不动声色的按钮和「卡死」在观感上没有区别。所以
安装另外提供了一个 SSE 版本：

| 路由 | 行为 |
|---|---|
| `POST /packages/install` | 一次性：等到装完才返回（脚本/API 用） |
| `POST /packages/install/stream` | SSE：边装边推事件，最后一定有且只有一个 `done` 或 `error` |

事件形状（沿用仓库既有的 `data: <json>` + `type` 字段风格）：

```json
{"type":"progress","stage":"download","received":4194304,"total":15039873}
{"type":"progress","stage":"verify"}
{"type":"progress","stage":"unpack","files":585}
{"type":"progress","stage":"link"}
{"type":"done","package":{…}}
```

- **字节进度按 4 MB 节流**：UI 只显示百分比，事件更密只是白花带宽。
- **解包按文件数上报**（每 200 个 + 结束时报一次），因为解包只有一两秒、逐文件刷屏没有意义。
- **本地 `path` 也走分块读**：不是只有下载才有进度，从 `/sdcard` 读 15 MB 同样要时间。
- **`done`/`error` 保证恰好一个**：客户端不必猜流是「结束了」还是「断了」。

面板用 `installPackageStreaming()` 订阅，显示阶段文案 + 百分比进度条；总数未知时用不确定态
（横扫动画）而不是冻在某个宽度。若一个事件都没收到就失败（例如 dev 前端对着旧 daemon，
端点不存在），会**回退到一次性安装**，而不是让一次本可成功的安装直接报错。

真机实测：

```
data: {"type":"progress","stage":"download","received":4194304,"total":15039873}
…（4 档）
data: {"type":"progress","stage":"verify"}
data: {"type":"progress","stage":"unpack","files":200}
data: {"type":"progress","stage":"unpack","files":585}
data: {"type":"progress","stage":"link"}
data: {"type":"done","package":{"name":"cpython",…}}
```

#### 取消安装

断开进度流就是取消：客户端 `AbortController` 一断，服务端那侧的 `mpsc` 发送立刻失败，
于是下载/读取停止、解包在下一个 200 文件检查点停下，**staging 目录随错误路径一起删除**。

- 判据不是「读到一半就算了」——`install_bytes_with` 的进度回调返回 `bool`，
  `false` 表示调用方已经不在了，解包直接 `return Err("install cancelled")`，
  走的是和校验失败完全相同的清理路径。
- 只有解包阶段可中断。链接（写 `bin/` 符号链接）只有几毫秒，中断在那里反而会留下
  「包装好了但命令叫不响」的中间态，所以不在那一步响应取消。

真机实测（三个断点各断开一次，每次后立刻查目录）：

```
t=0.35 -> [. .. cpython]      ← 无 .staging-*
t=0.45 -> [. .. cpython]
t=0.6  -> [. .. cpython]
```

#### 健康检查：装上了不等于能用

一个包可以「安装成功」却根本加载不了：架构不对、少了符号、依赖了手机没有的库。
以前只有在 shell 里跑一次才看得到，现在面板直接说原因。

- `pluginhost --check <command>`：走和真实运行**完全相同**的解析 + `dlopen` + `dlsym`
  流程，然后**不调用**入口点就退出。成功打印 `ok <name> <version> <entry>`，失败把
  加载器的原话打到 stderr。
- `GET /packages/health`：对每个已装包跑一次上面的检查，返回
  `[{name, ok, detail}]`。探测走 `spawn_blocking` 并加 15 秒上限——加载 25 MB 的
  `libpython` 要一会儿，而一个病态的 `.so` 不该把面板拖住。
- 面板每行显示绿/红一行字，底部有「重新检查」。

真机实测（故意装一个 `lib/nope.so` 里放纯文本的包）：

```json
{"name":"brokenpkg","ok":false,
 "detail":"cannot load …/lib/nope.so: dlopen failed: \"…/lib/nope.so\" is too small to be an ELF executable: only found 24 bytes"}
{"name":"cpython","ok":true,"detail":"3.12.10"}
```

#### 版本回滚

升级不再「替换即销毁」：被换下来的那一版留在 `packages/.previous-<name>/`，所以一个装得上
但跑不对的新版本可以直接换回去，不必再去找旧的 bundle。**只保留一代**——再升一次，
更早的那版就丢了，这是刻意的取舍（磁盘与「能退回一步」之间的平衡）。

| 路由 | 作用 |
|---|---|
| `POST /packages/:name/rollback` | 把当前版与保留版对调 |

- **对调而不是还原**：回滚后当前版变成新的「保留版」，所以回滚本身也可以再回滚一次。
- **三步交换**：current → `.swapping-<uuid>`，previous → current，swap → previous。任何一步
  失败都能退回原状，不会出现「两个目录都在、但都不是完整包」的中间态。
- **命令名跟着换**：不同版本可能声明不同的 `commands`。回滚时先摘掉当前版的链接，再按
  新版本的 manifest 重建；建链失败就把两个目录换回去，保证包仍然有它原来的命令名。
- **卸载连两代一起删**：`.previous-<name>` 属于这个包，卸载时连同提交的命令名一起清掉。

同一轮还修了一个升级期的旧漏洞：新版本若**不再声明**某个命令名，那条 `bin/` 链接以前会
留下来——它仍指向 `pluginhost`，但没有任何包认领，于是命令会以
「no package provides …」失败。现在升级时会摘掉这类链接。

真机实测：

```
install v1.0 → version 1.0
install v2.0 → version 2.0, previous_version 1.0   （磁盘内容 payload-2）
rollback     → version 1.0, previous_version 2.0   （磁盘内容 payload-1，bin/demopkg 仍在）
uninstall    → packages 下只剩 cpython，无 demopkg 链接
```

**已知未做**：只保留一代版本（退回两步需要重装）；CPython 的 `_ctypes` 未编入（缺 libffi），
所以 `ctypes` 不可用；健康检查只验证「能否加载」，不验证插件跑起来是否真的正确。


### P5.9：helper APK —— 跑「进程模型」工具（真机验证 2026-10-25）

P5.7 的结论是「Zig / clang / node 这类工具做不成 `.so` 插件」。它们必须 `exec` 自己和
子进程，而 targetSdk 29+ 禁止从 app 数据目录 `execve`。这一节解决那一半。

#### 决定性实验：跨包 exec 是允许的

在写任何代码之前先问了一个问题：**主 App 能不能执行另一个 APK 的 `nativeLibraryDir`？**
如果能，helper APK 就只是个「文件容器」，不需要 Service、不需要 Binder、不需要 IPC。

做法：建一个除了一条 native lib 之外什么都没有的 APK，装上，然后从主 App 自己的
`bash`（真 app 进程域 `untrusted_app`）执行它：

```
$ P=/data/app/~~s16…/com.atomcode.toolsprobe-…/lib/arm64/libprobe.so
$ $P --version
jq-1.7.1                       ← rc=0
$ ln -sf $P $HOME/probe_link && $HOME/probe_link --version
jq-1.7.1                       ← rc=0
```

**可以。** 于是整个 IPC 层都不需要存在了——这是这一节最重要的结论，它把「跨进程桥」
从设计里删掉了。

> 注意这与「exec 自己的数据目录」是两回事：那条依旧被禁（P5.7 实测 126）。允许的是
> 别的包的 `nativeLibraryDir`。另外**每次使用都要重新解析路径**：app 更新会换成新的
> `/data/app/~~hash/` 目录，缓存路径会在下一次更新后失效。

#### helper APK 里装了什么

| 工具 | 版本 | 装成 | 为什么要它 |
|---|---|---|---|
| `make` | 4.4.1 | `libmake.so` | 会 `exec` 编译器——正是插件装不下的那类 |
| `tcc` | 0.9.28rc | `libtcc.so` | 手机上唯一能提供的 C 编译器 |
| `cc` | — | `libtcc.so` | Makefile 按 `cc` 找编译器 |

APK **0.57 MB**（`libmake.so` 0.84 MB + `libtcc.so` 0.40 MB，压缩后）。构建用
`scripts/build-android-tools.sh`。

#### 主 App 怎么接

1. Kotlin 用 `PackageManager.getApplicationInfo("com.atomcode.tools")` 取
   `nativeLibraryDir`——需要清单里的 `<queries>`，否则 Android 11+ 连装着都查不到。
2. 经 JNI 传进 Rust（`nativeStart` 的第 4 个参数，可空：没装 helper 就是 `null`）。
3. `shims::link_helper_tools` 在 `<data>/bin` 建 `make` / `cc` / `tcc` 三个符号链接指向
   那些二进制——和 `git`、`jq` 走的是同一条通道，所以 `PATH` 里直接可用。
4. helper 卸载后这些链接会被摘掉，命令变成「找不到」而不是一个指向已删除目录的错误。

启动日志会把结果说清楚：

```
I AtomCode: helperLibraryDir: /data/app/~~rwi…/com.atomcode.tools-…/lib/arm64
I atomcode: atomcode_android::host: helper APK tools are available tools=make,cc,tcc
```

#### 真机验证：跨进程链跑通

关键不是「make 能跑」，而是 **make 能把它身边的 cc 拉起来**——这正是 clang→cc1 需要的
形状：

```
$ make --version
GNU Make 4.4.1
$ tcc -v
tcc version 0.9.28rc (AArch64 Linux)
$ make                      # Makefile 里写的是 `cc -c hello.c -o hello.o`
cc -c hello.c -o hello.o
$ ls -l hello.o
-rw-rw---- 1 u0_a251 media_rw 1054 2026-10-25 17:17 hello.o
```

三个二进制都在 helper 的 `nativeLibraryDir` 里，链子是「主 App → helper 的 make →
helper 的 cc」，全程没有我们自己那套不可执行目录的参与。

#### 两个构建坑（脚本已处理）

1. **`c2str.exe` 是宿主工具**。tcc 的 Makefile 用 `$(CC)` 生成它，交叉编译下它变成
   ARM64 二进制、在构建机上跑不起来。解法：用宿主 `cc` 编它、跑出 `tccdefs_.h`、
   `touch` 之后让 make 别再重建。
2. **bionic 没有独立的 `libpthread`**（已并入 libc），tcc 的 `LIBS` 里那个 `-lpthread`
   会让链接直接失败，要覆盖成 `-lm -ldl`。
3. 附带：`libtcc1.a` **没有**构建——tcc 是「运行 tcc 自己」来生成它的，交叉编译下做不到。
   `tcc -c` 不需要它。

#### 老实说清楚限制

- **编出来的东西仍然不能执行**。产物落在数据目录，而那是不允许 exec 的；换任何打包方式
  都改不了。所以手机上的编译器只能用来**编译/检查/产出可拉走的产物**，不能「改完立刻跑」。
  这条限制了 zig/clang 上手机的实际价值，动手装 200 MB 之前值得先想清楚。
- `tcc` 只当**自由式 C** 的编译器用：没有编 `libtcc1.a`，也没有 bionic 头文件，所以
  `#include <stdio.h>` 这类程序通不过。`make` + 自包含 C 单元（生成 `.o`）是它能干的。
- helper 目前**没有 UI 入口**：装了它，工具就出现在 `PATH` 里；没装就是没有。面板里
  还没有「去装 helper」的引导，下一步该补这个。


### P5.10：CPython 的 C 扩展全都加载不了（真机定位并修复 2026-10-25）

一份外部环境报告指出：装完 CPython 插件后，`import socket`、`ssl`、`asyncio` 等一批模块
报同一个错——

```
ImportError: dlopen failed: cannot locate symbol "PyExc_OSError"
  referenced by ".../lib/python3.12/lib-dynload/_socket.cpython-312.so"
```

报告结论正确，根因在我们的 `pluginhost`。

#### 真正的机制（报告推断到一半，这里补全）

真机实测的三条事实：

| 事实 | 怎么测出来的 |
|---|---|
| `libpython3.12.so.1.0` **确实导出** `PyExc_OSError` | `strings` 命中 5 次（设备上没有 `nm`，用它判断会误判成「未导出」） |
| `_socket.cpython-312.so` 对它**没有任何 `DT_NEEDED`** | `strings _socket…so \| grep libpython` = 0 |
| 进程内 `dlopen(…, RTLD_GLOBAL)` **不管用** | 加了也不生效；而以 `LD_PRELOAD` 启动进程则生效 |

也就是说：CPython 的扩展模块（`python` 主程序也是）**不链接 libpython**，完全指望解释器的
符号在进程全局作用域里。而 bionic 不会因为你在运行中间 `dlopen(RTLD_GLOBAL)` 一个库，就把它
放进「对后续 `dlopen` 可见」的全局组。我们原来用私有方式加载插件，于是每个扩展都在找一个
永远找不到的符号。

#### 两次失败的尝试（都记在这里，免得后人重走）

1. **`dlopen(entry, RTLD_NOW | RTLD_GLOBAL)`** —— 无效。入口库和 libpython 都进去了，但
   libpython 是作为**依赖**被带进来的，bionic 保留它首次加载时的分组，不做提升。
2. **manifest 声明 `preload`，进程内先 `dlopen` 它** —— 同样无效，原因同上一条：问题不在
   顺序，而在 bionic 不认这种「运行中提升」。

#### 有效的修法：重新 exec 自己

`pluginhost` 现在先读 manifest 的 `preload`，若非空就**带 `LD_PRELOAD` 重新 exec 自己**
（并用 `arg0()` 把原始 `argv[0]` 带过去，否则别名丢失、`-c` 会被当成命令名），子进程里链接器
在启动时就把 libpython 放进全局组——这正是 `LD_PRELOAD` 一直有效的机制。

```json
{ "name": "cpython", …, "aliases": ["python3","python"], "preload": ["lib/libpython3.12.so.1.0"] }
```

用一个环境标记防止递归，stdio 与退出码原样透传，调用方感觉不到多了一次 exec。

#### 复测（不再需要设置任何 `LD_PRELOAD`）

```
socket=OK  asyncio=OK  subprocess=OK  decimal=OK  select=OK  math=OK  json=OK
zlib=OK  hashlib=OK  array=OK  fcntl=OK  termios=OK
ssl=FAIL  sqlite3=FAIL  ctypes=FAIL  bz2=FAIL  lzma=FAIL  curses=FAIL  readline=FAIL
```

**后半段是构建期缺件，不是加载问题**：这批扩展在我们的交叉编译环境里根本没有源码依赖
（无 OpenSSL 头、无 sqlite3、无 libffi、无 ncurses），链接器不会凭空生出一个 `.so`。要补齐得
在构建 CPython 时先交叉编译这些库，属于另一件事。

#### 补上 OpenSSL：HTTPS 与 pip 一起回来了

上一节把 `_ssl` 归为「构建期缺件」是对的，但**不难补**——git 和 curl 用的 OpenSSL 3.0.16
本来就是我们在 WSL 里交叉编译出来的（`~/android-git-build/prefix`）。重编 CPython 时
`--with-openssl=$PREFIX` 指过去即可：

```
checking whether OpenSSL provides required ssl module APIs... yes
checking whether OpenSSL provides required hashlib module APIs... yes
Checked 111 modules (31 built-in, 65 shared, …, 13 missing)
```

共享模块从 61 个变成 65 个，`_ssl.cpython-312.so` 与 `_hashlib.cpython-312.so` 出现。

**代价**：两个模块各自静态链了一份 OpenSSL（5.4 MB + 4.3 MB），所以插件包
**15 MB → 20 MB**（展开 47 MB → 59 MB）。要省这一块得让它们共享一个 `libssl.so`，
属于后续优化。

**打包策略也跟着改了一条**：`ensurepip` 不再被裁掉。之前裁它是因为「没有 `_ssl` 的 pip
连不上任何 index」，现在装上能用了。

真机实测（`run-as` 直接跑，不经过模型）：

| 能力 | 结果 |
|---|---|
| `import ssl` | `OpenSSL 3.0.16 11 Feb 2025` |
| `hashlib.pbkdf2_hmac` | ✅ `0a38253555ce37f5`（之前 `AttributeError`） |
| `hashlib.scrypt` | ✅ `d6fff5b4` |
| HTTPS（带 `SSL_CERT_FILE`） | ✅ `https status 200` |
| `python3 -m ensurepip` | ✅ `Successfully installed pip-25.0.1` |
| `python3 -m pip install six` | ✅ 下载并安装 → `import six` = 1.17.0 |

所以那份报告的「HTTPS 必须交给 curl」「pip 这条路走不通」两条结论，在补上 OpenSSL 之后
都不再成立。**仍然成立的**是剩下那批缺件：`sqlite3`、`ctypes`、`bz2`、`lzma`、`curses`、
`readline`——它们各自还缺一个库（libsqlite3、libffi、libbz2、liblzma、ncurses），补法相同，
只是还没有做。

一个小坑：`ensurepip` 会把 `pip` 启动脚本装到 `<package>/bin`，那个目录不在 `PATH` 上
（会打印一条 warning）。用 `python3 -m pip …` 即可，不必依赖那个脚本。

#### 再补 sqlite3：Python 侧终于能直接读数据库

同一套办法：把 sqlite 3.50.4 的 amalgamation 用 NDK 静态编成 `libsqlite3.a`
（1.5 MB）连同 `sqlite3.h` 装进 `$PREFIX`，再在**完全相同的** configure 参数下重编
CPython——`-I$PREFIX/include` / `-L$PREFIX/lib` 早已在那里，setup 自己就找到了：

```
Checked 111 modules (31 built-in, 66 shared, …, 13 missing, 0 failed on import)
```

共享模块 65 → **66**，`_sqlite3.cpython-312.so`（1.6 MB）出现。

累计体积变化（相对最初没有 SSL 的版本）：

| 项 | 原始 | +OpenSSL | +sqlite3 |
|---|---|---|---|
| bundle | 15 MB | 20 MB | **21 MB** |
| 展开 | 47 MB | 59 MB | **61 MB** |

意义：以前要在 Python 里操作数据库只能 `subprocess` 调 `sqlite3` 命令行再解析输出，
现在 `import sqlite3` 直接用，`subprocess` 那层可以去掉。

真机实测（`run-as` 直跑）：

```
python3 -c "import sqlite3; print(sqlite3.sqlite_version)"   → 3.50.4
Python 建表并写入 2 行                                       → python rows: 2
sqlite3 CLI 读同一个库文件                                    → 1:alpha / 2:beta
CLI 再插一行，Python 回读                                     → python now sees 3 rows
```

两个方向都通，说明 Python 的 `_sqlite3` 与命令行工具用的是同一种库文件格式、没有各自的
私有封装。

#### 再补 ctypes：libffi 与 bionic 缺的 `__clear_cache`

`_ctypes` 要 libffi。PREFIX 里原本有一个，但**是个半成品**：ARM64 的汇编对象没进去
（`llvm-nm` 里 `U ffi_call_SYSV`、`U ffi_call`），`ffi.h` / `ffitarget.h` 也没装。
干净重编之后才完整（`T ffi_call`、`T ffi_prep_cif` 有定义，头文件 14 KB + 2.7 KB）。

第二个坑更隐蔽：**bionic 不导出 `__clear_cache`**。libffi 的 arm64 闭包代码在写完可执行
跳板后要刷指令缓存，链接时就会撞上 `undefined symbol: __clear_cache`。

`-D__clear_cache=__builtin___clear_cache` 是假解法——aarch64 上编译器把这个 builtin
**又下降回同名调用**，所以它只是把问题改了个名字。真正的办法是把缓存维护写成指令：

```c
for (; cursor < last; cursor += 64) __asm__ __volatile__("dc cvau, %0" ::"r"(cursor) : "memory");
__asm__ __volatile__("dsb ish" ::: "memory");
for (cursor = begin; cursor < last; cursor += 64) __asm__ __volatile__("ic ivau, %0" ::"r"(cursor) : "memory");
```

代码在 `scripts/cpython-plugin/clear_cache.c`，编成目标文件后 `llvm-ar rcs` 追加进
`libffi.a`——任何拉取 ffi 对象的链接都会顺带解决它，不用给每个模块加额外 `-l`。

真机实测：

```
python3 -c "import ctypes"                                 → ctypes ok
CDLL("libc.so").strlen(b"hello") / getpid()                → 5 / 29960
CFUNCTYPE 闭包回调 double(21)                               → 42
Structure(3, 4) → bytes                                    → 03000000 04000000
```

闭包回调这一条是重点：它走的正是 libffi 写跳板 + 刷缓存那条路，能返回 42 说明补的
`__clear_cache` 是真管用的，不只是链接过。

体积：`_ctypes.cpython-312.so` 只有 0.7 MB，包仍是 **21 MB**（展开 61 MB）。

剩下没补的只有 `bz2`（libbz2）、`lzma`（liblzma）、`curses`/`readline`（ncurses）——都是
同一个套路，缺的都是"一个静态库"。

#### 再补 bz2 / lzma：`.tar.bz2` 与 `.tar.xz` 能解开了

同一个套路，两个静态库：`bzip2 1.0.8` 的 `libbz2.a`（84 KB）+ `bzlib.h`、
`xz 5.6.4` 的 `liblzma.a`（351 KB）+ `lzma.h`，装进 `$PREFIX` 后照原参数重编：

```
Checked 111 modules (31 built-in, 68 shared, …, 11 missing, 0 failed on import)
```

共享模块 66 → **68**，缺失 13 → **11**，`_bz2.cpython-312.so`（0.13 MB）与
`_lzma.cpython-312.so`（0.25 MB）出现。

同时补了一个**打包遗漏**：`include/python3.12`（`Python.h` 等头文件）以前根本没进包，
所以 `sysconfig.get_config_var("INCLUDEPY")` 指向一个不存在的路径。现在它随包发布。

累计体积：

| 项 | 原始 | +OpenSSL | +sqlite3 | +ctypes | +bz2/lzma |
|---|---|---|---|---|---|
| bundle | 15 MB | 20 MB | 21 MB | 21 MB | **22 MB** |
| 展开 | 47 MB | 59 MB | 61 MB | 61 MB | **63 MB** |

#### 再补 curses / readline / gdbm：三个库、两次重编

| 来源 | 产物 | 补上的模块 |
|---|---|---|
| ncurses 6.5（`--enable-widec`） | `libncursesw.a` + `libpanelw.a` + 头 | `_curses` |
| gdbm 1.24 | `libgdbm.a` | `_gdbm` → `dbm.gnu` |
| readline 8.2 | `libreadline.a` | `readline` |

有两处需要额外处理，都是「CPython 只认某个固定库名」造成的：

1. **`-lpanel` 找不到**：widec 构建把 panel 装成 `libpanelw.a`，而 CPython 要 `-lpanel`。
   复制一份改名即可。
2. **`-lreadline` 单独链不上**：静态 `libreadline.a` 引用了 ncurses 的 termcap 符号
   （`tgetent`/`tputs`/`tgetstr`/`tgoto`），而 CPython 只加 `-lreadline`。把 ncurses 的
   目标文件**并进同一个归档**，`-lreadline` 就自足了——归档成员按需拉取，不需要第二个 `-l`。

第一次重编只出了 `_curses` 和 `_gdbm`；补上这两处后第二次重编才拿到 `readline`：

```
Checked 111 modules (31 built-in, 71 shared, …, 8 missing, 0 failed on import)
```

共享模块 68 → **71**，缺失 11 → **8**。

真机实测（`run-as` 直跑）：

```
import curses / readline / dbm / dbm.gnu / dbm.dumb   → 全部 OK
dbm.gnu 写入 k1/k2 后重新打开                          → keys ['k1','k2']，k2 → v2
```

（`import gdbm` 失败是正常的——CPython 没有叫 `gdbm` 的模块，它是 `_gdbm` + `dbm.gnu`。）

#### 为什么 node 不在这条路上

这一点容易被「再做几个库不就行了」带偏：**node 不是标准库缺口。**

标准库缺口指的是「CPython 自己的一个模块没编出来」——补一个 C 库、重编一次，模块就回来了，
全部走同一条 `.so` 加载路径。node 是**另一个完整的运行时**：

- 它是**进程模型**程序：要 `exec` 自己和子进程，而本 app 的数据目录 `noexec`（§P5.7 实测
  126），所以它只能走 helper APK 那条路（§P5.9），不是插件的 `dlopen` 路径；
- 没有官方 Android 构建，体积约 50 MB；
- 补它既不改 CPython 一行，也不进标准库清单。

同理，`gcc`/`clang`/`cargo` 也不在这条路上——它们是工具链，不是 Python 模块。



#### 两处核查：`sem_open` 的真相，以及「更新后包还在」

**一、`mp.Queue`/`Pool` 不是配置问题，是平台空实现**

v3 报告把它归因为「CPython 未启用 `HAVE_SEM_OPEN`」。实测不成立：

```
pyconfig.h:  #define HAVE_SEM_OPEN 1
             #define HAVE_SEM_UNLINK 1
```

两个宏都是 1。真正的原因是 CPython 源码里一行显式排除：

```c
#if !defined(POSIX_SEMAPHORES_NOT_ENABLED) && !defined(__ANDROID__)
    _MULTIPROCESSING_SEM_UNLINK_METHODDEF
#endif
```

而 `multiprocessing/synchronize.py` 是 `from _multiprocessing import SemLock, sem_unlink`
**一起**导入，少一个就抛「lacks a functioning sem_open」。于是模块里 `SemLock` 在、
`sem_unlink` 不在。

把那个 `!defined(__ANDROID__)` 去掉重编之后：模块能导入、`SemLock` 有了，但
`mp.Queue()` 立刻变成

```
OSError: [Errno 38] Function not implemented
```

**`ENOSYS`——bionic 的 `sem_open()` 是个空实现。** 所以 CPython 的排除是对的，补丁已撤回。
撤回是刻意的：一个明确的 `ImportError` 比「导入成功、首次用才炸」诚实得多。

> 顺带记一个教训：我曾用 `strings /system/lib64/libc.so | grep sem_open` 看到符号就以为可用。
> **符号存在 ≠ 功能实现**——bionic 会为不支持的接口留符号（返回 `ENOSYS`）。判断能力要跑一次，
> 不能只看符号表。

结论：`Queue` / `Pool` / `ProcessPoolExecutor` 在本平台不可用；并行走
`ThreadPoolExecutor`（IO）或 `Process` + `Pipe`（CPU），后者实测可用。

**二、运行时更新会清掉包内的 site-packages——现在不会了**

装新运行时是**整个包目录替换**，所以 pip 和它装进 `.../packages/cpython/lib/python3.12/
site-packages` 的所有包都会被抹掉（v3 报告里「site-packages 不存在（重装时被清除）」就是这个）。

修法：把安装目标放到包目录**之外**。插件 shim 在启动时把
`$HOME/python-packages`（产品树，稳定）加入 `PYTHONPATH`（追加而不是覆盖已有值），
目录不存在则创建。于是：

```sh
python3 -m ensurepip                                   # pip 仍在包内，更新会丢
python3 -m pip install --target $HOME/python-packages six   # 这个不会丢
```

真机验证（先装 six，再把同一个运行时**重装一次**）：

```
重装后：import six   → six 1.17.0      ← 持久目录里的活下来了
重装后：python -m pip → No module named pip   ← 包内的被清空，正好证明包目录被换过
```

#### 补记：别名接管的归属，以及 `_dbm` 的真相

**一、接管记录必须记住「谁占的」**

接管（§「接管 host 命令」）原来是这么判断链接归属的：目标指向 `pluginhost` 就是我们的。
问题在于**两个包都指向 `pluginhost`**——一旦两个包声明同一个命令名，先卸载的那个会把
另一个的链接一并删掉，那条命令就废了。

改法：

1. 记录从「一行 basename」变成「两行：basename + 归属包名」；
2. 装包时如果这个名字已被**别的包**占着，直接拒装并点名：
   `` `python3` is already provided by package `cpython` ``；
3. 卸载时按记录里的归属判断，别人的名字不碰；
4. **旧记录自愈**：早期写下的记录没有归属行，装包/卸载时从已装包的 manifest 反查真实持有者
   （`current_holder`），顺手把记录补成新格式。

第 4 条不是洁癖——真机上第一版补丁就是因为旧记录只有一行，让测试包把 `python3` 抢走了。
补上反查之后重测：

```
抢名安装 → {"success":false,"error":"`python3` is already provided by package `cpython`"}
bin/python3 -> pluginhost        python3 --version -> Python 3.12.10
.bin-overrides/python3 -> "libpython3.so\ncpython\n"
```

**二、`_dbm`：根因是我自己上一步的合并把库弄坏了**

之前诊断成「configure 的静态归档判定过不去」，真相更难看：为了让
`AC_SEARCH_LIBS([dbm_open], [ndbm gdbm_compat])` 通过，我用 `llvm-ar` 把
`libgdbm.a` 并进 `libgdbm_compat.a`——**第一次合并写错了，把 compat 自己的目标文件丢了**，
归档里只剩 gdbm 核心。第二次再从那个已经坏掉的归档里提取，自然什么也提不到：

```
$ llvm-nm --defined-only libgdbm_compat.a | grep -E " (dbm_open|gdbm_open)$"
0000000000000ae8 T gdbm_open        ← 只有核心，没有 dbm_open
```

重装干净的 gdbm（`--enable-libgdbm-compat`），这次把两个归档**解到不同目录**再合并
（同名目标文件不会互相覆盖），链接测试立刻通过：

```
checking for library containing dbm_open... -lgdbm_compat
checking for _dbm module CFLAGS and LIBS... -DUSE_NDBM -lgdbm_compat
Checked 111 modules (…, 73 shared, …, 6 missing)
```

真机：`import dbm.ndbm` / `_dbm` 均 OK，ndbm 写入读回一致，lib-dynload 共 **73** 个。

教训写在这里：**合并静态归档时必须避免同名目标文件互相覆盖**，否则你得到的是一个看起来
存在、实际缺成员的库，而 configure 的失败信息只会说「没找到符号」。

**三、`Cargo.lock` 的结论：本地有污染，但进不了提交**

```
HEAD 的 Cargo.lock:            hkdf 0 / hmac 0 / zeroize_derive 0   ← 干净
工作树的 Cargo.lock:            各 1，且只来自 atomcode-codingplan-crypto（私有 overlay）
git ls-files -v Cargo.lock   → S Cargo.lock（skip-worktree）
```

三个包全部由本地的私有 crypto overlay 拉进来；`Cargo.lock` 与该 crate 的 `Cargo.toml` 都带
`skip-worktree`，`git add` 明确拒绝暂存。也就是说本地这份带污染的 lock 只能服务本地构建，
进不了版本库——**保持原样是正确做法**，改动它反而会破坏本地 overlay 的构建。

#### 复核环境检查报告：「C 扩展全坏、没有 pip」的真相

2026-10-26 的一份设备环境报告结论是：`cpython` 的 C 扩展**全部** `dlopen` 失败
（`cannot locate symbol "PyExc_ValueError"` 等）、`hashlib` 所有算法 unsupported、
`python3` 是 RustPython、没有 pip，并据此判断「libpython 符号未导出，需要重新编译才能修复」。

**观察是真的，判断是错的。** 真机复核后定位到两个互不相干的原因。

**原因一：设备上装的那份 `package.json` 缺 `aliases` 与 `preload`。**

```json
{"name":"cpython","version":"3.12.10","entry":"lib/plugin.so","symbol":"plugin_main"}
```

少了 `aliases`，`python3`/`python` 就不会指向它，仍是 RustPython；少了 `preload`，
`pluginhost` 不会带 `LD_PRELOAD` 重新 exec 自己，于是**所有**共享模块都找不到 libpython
的符号——报错文本与「libpython 没导出符号」一模一样，但符号其实都在。

重装一份清单完整的包之后，同一台设备、同一个命令：

```
python3 impl: cpython 3.12.10
math zlib socket subprocess asyncio decimal array unicodedata sqlite3 ssl ctypes bz2 lzma  全部 OK
md5 0cc175b9 / sha256 ba7816bf
```

**原因二：从普通 shell 跑时，包根目录被解析到不存在的位置。**

```
pluginhost: cannot read /data/user/0/com.atomcode.app/packages: No such file or directory
```

`packages_root()` 原来只看 `$HOME/packages`。App 自己跑时宿主把 `HOME` 设成产品树，
所以没事；但 `adb shell`、或任何继承到的是**数据目录**的场景，`HOME` 是
`/data/user/0/com.atomcode.app`，产品树在它下面的 `files/atomcode/`，于是每次查找都失败。
报告正是从这种 shell 测的，这一步让「C 扩展全坏」变得必然。

改法是加候选顺序（第一个存在的胜出）：`ATOMCODE_PACKAGES_DIR` → `ATOMCODE_HOME/packages`
→ **启动器自身路径回推**（`<product>/bin/<命令>` → `<product>/packages`）→ `$HOME/packages`
→ `$HOME/files/atomcode/packages`。真机复测（`HOME` 设为数据目录）：

```
cpython 能否自己找到包: ok 3.12.10
cpython C 扩展: all ok
python3 指向: cpython
```

**顺带修掉报告里唯一成立的真问题：pip 在每次更新后消失。**

`ensurepip` 默认装进包自己的 site-packages，而运行时的安装/升级是整目录替换，
所以每更新一次 pip 就没了，看起来就是「没有 pip，什么都装不了」。现在安装完运行时
自动执行一次 `ensurepip --user`，pip 落在 `$HOME/.local/lib/python3.12/site-packages`
——在包目录之外，更新不动它：

```
pip 25.0.1 from …/atomcode/.local/lib/python3.12/site-packages/pip (python 3.12)
Successfully installed attrs-26.1.0        ← 重装包之后依然可用
```

**报告里仍然成立的部分**（这些是边界，不是缺陷）：`/tmp` 不可写；Lua 没有 `lfs`/`socket`；
没有 `node`/`npm`/`gcc`/`clang`；RustPython 缺 `_ssl`/`_sqlite3`（HTTPS 与 sqlite3 用 cpython）；
共享存储 `noexec`。另有两处事实错误：「Android 13」实为 **Android 16（SDK 36）**，
`android13` 只是内核分支名；「没有任何包管理器」也不成立——cpython 自带 pip，走镜像可用。

#### 卸载运行时会不会留下死文件：实测与处理

**会。** 卸载只删 `<product>/packages/<name>`（以及保留的 `.previous-<name>`），而运行时的
用户数据在包目录之外。真机实测（卸载 cpython 前后）：

```
packages         132M → 1.7M      ← 本体已删
python-packages   15M → 15M       ← 用户 pip 装的库，没删
.local            13M → 13M       ← pip 本体与用户 site，没删
```

那 28 MB 此时**没有任何解释器能使用**，是货真价实的死文件；而且 `.local/lib/python3.12`
还锁着版本号，将来换 3.13 运行时也永远不会被用上。

处理方式：**不自动删，但绝不静默。**

- 自动删有数据风险——`python-packages` 里是用户自己装的库，不是生成的垃圾；
- 所以卸载后由 `GET /packages/leftovers` 报告它们，面板上出现「运行时残留 21.9 MB」
  一栏（只在**没有**解释器时出现，装了运行时它们就是活的），带一个「清理残留」按钮，
  `DELETE /packages/leftovers` 真正释放空间。

真机验证：卸载后接口报告 `python-packages 10.8MB + .local 11.1MB = 21.9MB`；点清理后
`freed_bytes: 21943182`，两个目录消失，产品目录从 27MB 降到 5.3MB；重装运行时后该栏自动消失。

#### pip：改成随运行时包自带

上一轮用「安装后跑 `ensurepip --user`」让 pip 持久化，实测**根本没装上**。查下去是两层：

1. `ensurepip` 不接受 `--no-warn-script-location`（那是 pip 的参数），直接用法错误退出；
2. 去掉后仍然失败：`PermissionError: [Errno 13] Permission denied: ''` —— 内嵌解释器的
   `sys.executable` 是**空字符串**，而 `ensurepip` 靠 `subprocess` 拉起自己来完成引导。

```
bin/python3 -c "import sys;print(repr(sys.executable))"       → ''      ← 按路径调用
PATH=…bin:$PATH python3 -c "…"                                → 完整路径  ← 按名字调用
```

`Py_SetProgramName` 对 `Py_BytesMain` 无效（3.12 那条路径自己建 `PyConfig`），要修就得
重写成 `Py_InitializeFromConfig` —— 代价不划算。于是改成**打包时把 `ensurepip` 自带的
pip wheel 直接解进 `site-packages`**：不需要子进程，安装完就有 pip。

```
pip 25.0.1 from …/packages/cpython/lib/python3.12/site-packages/pip (python 3.12)
Successfully installed attrs-26.1.0
```

代价是包体从 22 MB 涨到 24 MB（展开 65→71 MB）。

**已知限制**：`sys.executable` 为空还意味着 `multiprocessing` 的 `spawn` 起不来（`fork`
可用），以及任何 `subprocess.run([sys.executable, …])` 的写法会失败。用 `fork` 或显式
写 `python3` 即可。

#### 这份环境报告的复核

它描述的状态基本准确（Python 3.12.10 + pip、Lua 5.4.7、SQLite 3.50.4、tcc/make/git、
noexec、mksh 不支持 `=~` 与进程替换），三处需要更正：

| 报告说法 | 实际 |
|---|---|
| 「Android 13」 | **Android 16（SDK 36）**；`android13` 是内核分支名 |
| 「无包管理器，无法安装」 | cpython 自带 pip 且走镜像可用（`pip install attrs` 成功） |
| 「缺失：numpy、cffi、lupa」 | 属实；这三者都需要为 aarch64-android 交叉编译，不是纯 Python 包 |

「标准库缺 `tkinter`、`lua`」也属实：前者无 GUI，后者是 `lupa` 提供的桥接，装了 lupa 才有。

#### 给「缺失的第三方包」做插件：cffi 走通了

报告列的 `numpy` / `cffi` / `lupa` 都不是纯 Python 包，PyPI 上也没有
`linux_aarch64_android` 的 wheel（manylinux 那几个链的是 glibc，设备上没有），
而手机上又编不了（缺系统 C 头文件）。所以只能在构建机上对着**同一套 CPython 与 libffi**
交叉编译。

cffi 是最容易的一个：它的后端就是 `src/c/_cffi_backend.c` 一个翻译单元加 libffi，
不需要 Python 的构建系统。固化在 `scripts/build-android-cffi-wheel.sh`：

```sh
bash scripts/build-android-cffi-wheel.sh <cffi-sdist.tar.gz>
→ dist/cffi-2.1.1-cp312-cp312-linux_aarch64.whl   (201 KB)
```

装到设备时必须给平台参数，pip 推不出 Android 的 tag：

```sh
python3 -m pip install --platform linux_aarch64 --only-binary=:all: \
  --no-deps --target $HOME/python-packages <wheel>
```

真机验证（`ffi.dlopen` + `cdef` + 真实调用）：

```
cffi 2.1.1
strlen 10          ← libc.strlen(b"hello cffi")
getpid 5902
```

两个实践要点：

- **wheel 文件名不能被改**。pip 靠文件名判断类型与 tag，改名成 `cffi.whl` 会直接
  报 `is not a valid wheel filename`。
- **ABI 模式下必须先 `ffi.cdef` 声明再访问**，否则 `libc.strlen` 抛
  `AttributeError`——这不是 cffi 坏了，是用法。

`numpy` 是同类但重得多（要 meson/ninja 交叉构建整套 C 源），`lupa` 需要 Lua 头文件；
这两者尚未做。

#### SQLite 与 Lua：把「编译期砍掉的能力」补回来

一份 2026-10-26 的 Python/Lua/SQLite 报告指出三个硬伤：SQLite 无 FTS3/4/5、无 RTree、无数学
函数且禁止 `.load`；Lua 完全没有 C 扩展；本文档后面那份报告又提到 pip 装了东西但环境像裸装。
三条都属实，也都修掉了。

**SQLite：加上 FTS／RTree／数学函数／可加载扩展。** 之前只是按默认选项编 amalgamation，
所以 `create virtual table … using fts5` 直接报 `no such module: fts5`。现在统一带

```
-DSQLITE_ENABLE_FTS3 -DSQLITE_ENABLE_FTS4 -DSQLITE_ENABLE_FTS5
-DSQLITE_ENABLE_RTREE -DSQLITE_ENABLE_MATH_FUNCTIONS -DSQLITE_ENABLE_COLUMN_METADATA
```

并且**不再**定义 `SQLITE_OMIT_LOAD_EXTENSION`。同一份 `sqlite3.c` 同时喂给 CLI 插件和
CPython 的 `_sqlite3`（`libsqlite3.a`），两边能力必须一致，脚本里用同一个 `$FEATURES`。

两个只有踩过才知道的坑：

- 开了数学函数后 `_sqlite3.so` 引用 `trunc`，而模块没链 `-lm`，导入直接
  `cannot locate symbol "trunc"`。修法是 `PY_LDFLAGS` 带 `-lm`（`NEEDED: libm.so` 出现即对）。
- CPython 的 `enable_load_extension()` 由 **configure 开关**决定，不是编译标志：
  `pyconfig.h` 里是 `/* #undef PY_SQLITE_ENABLE_LOAD_EXTENSION */`。手动重链模块没用，
  必须 `./configure --enable-loadable-sqlite-extensions`。

真机验证（CLI 与 Python 各测一遍）：

```
CLI    :  fts5 建表插入 → 1 ；select sqrt(16), pow(2,10) → 4.0|1024.0 ；rtree → 1
Python :  fts5 → 1 ；sqrt → 4.0 ；rtree → 1 ；sqlite_version 3.50.4
         enable_load_extension → True，且 enable_load_extension(True) 调用成功
```

**Lua：让 `require` 真的能找到并加载 C 模块。** 原来有三层障碍，缺一层都不行：

1. `package.cpath` 指向 `/usr/local/lib/lua/5.4/?.so`——Android 上不存在。加一个 Lua 专用
   shim（`scripts/lua-plugin/plugin.c`），在启动时把 `LUA_PATH`/`LUA_CPATH` 指到产品目录
   （`<product>/lua/?.lua`、`<product>/lua/?.so`，末尾 `;;` 保留内置默认值，且不覆盖用户已设的值）。
2. 解释器根本没编 dlopen：`${sources[@]}` 是在**链接那一行**才被编译的，那行漏了
   `-DLUA_USE_DLOPEN`，于是 `loadlib.c` 走的是「动态库不可用」分支，`require` 报
   `dynamic libraries not enabled; check your Lua installation`。补到链接行即可。
3. 即使 dlopen 通了，模块仍报 `cannot locate symbol "luaL_checkversion_"`——bionic 不会把
   已加载库的符号提升为全局（与当年 `_ssl` 完全同一个坑）。解法是给 lua 包声明
   `"preload": ["lib/plugin.so"]`，让 `pluginhost` 带 `LD_PRELOAD` 重执行。

真机验证：用 NDK 编一个 6 KB 的 `mylib.so` 放进 `<product>/lua/`，

```
lua -e 'print(require("mylib").hello())'  →  hello from a C module
```

**Python 第三方包**：pip 可用之后把 `requests`／`certifi`／`urllib3`／`tzdata`／`setuptools`
装进 `$HOME/python-packages`（包目录之外，运行时更新不丢）。

三轮真机自检（其中两轮是直接让设备上的 atomcode 自己跑命令回报）确认：标准库全通、FTS5／
RTree／`sqrt` 两侧可用、`enable_load_extension` 可用、Lua C 模块可 `require`、pip 与 requests 正常。

**还没做的**（设备自己的结论，也确实需要人拍板）：真正的 C/C++ 编译器（现在只有 tcc）、
`rustc`/`cargo`、`node`/`npm`。三者都大，且受 noexec 限制——编出来的**可执行文件跑不了**，
只有 `.so` 能被 dlopen，所以它们解决的是「能编」而不是「能跑」。

#### 修掉 `sys.executable` 为空：一个改动解锁一类问题

内嵌解释器的 `sys.executable` 一直是空串，凡是「重新执行自己」的东西都因此失败：
`ensurepip` 的引导、`multiprocessing` 的 spawn、`subprocess.run([sys.executable, …])`。
之前判断「要改就得重写 `Py_BytesMain` 成 `Py_InitializeFromConfig`」，所以没做——那个判断只对了一半。

**真正便宜的做法**：`Py_SetProgramName` 对 `Py_BytesMain` 确实无效（它自己建 `PyConfig`），
但**不需要改 C**。shim 在启动前把启动器的绝对路径放进 `ATOMCODE_PYTHON_EXE`，随包发布的
`sitecustomize.py` 在解释器起来时把它赋给 `sys.executable` 即可。

踩到的两个坑都不是「找不到路径」那么简单：

- `plugin_main` 收到的 `argv[0]` 是**包名**（`pluginhost` 注释原文：「argv[0] is the package
  name」），不是调用者敲的路径，所以 `realpath(argv[0])` 必然失败。改为用 `PLUGIN_DIR`
  （`<product>/packages/<name>`）上溯两级得到 `<product>/bin/<name>`。
- **不能用 `realpath` 的结果**：`bin/python3` 是指向 nativeLibraryDir 里 `.so` 的软链，
  解析后的目标是共享库、`execve` 起不来；软链本身反而能起（实测 `<product>/bin/python3 --version`
  → rc=0）。所以保留未解析的路径。

验证：`sys.executable` 有值、`subprocess.run([sys.executable, "-c", "print(42)"])` → `rc=0 out=42`。

**`spawn` 还有第二道门**：`multiprocessing` 的 resource tracker 无条件 `import _posixshmem`，
而 bionic 没有 `shm_open`/`shm_unlink`（`call to undeclared function 'shm_open'`），configure
直接把这个模块关掉了。补一个自己的实现（`scripts/cpython-plugin/posixshmem.c`），用 `$TMPDIR`
下的普通文件承载，名字收敛成单个叶子组件以防越界。之后 spawn 实测：

```
exitcode: 0    wrote: True    child pid 29297
```

#### 设备上编 C：`cheaders` 数据包

`tcc` 在 Android 上没有可用 include 路径（`-vv` 显示安装前缀 `/usr/local/lib/tcc`，不存在），
所以「有 tcc」和「能编 C」是两件事。补成一个**数据包**（`scripts/build-android-cheaders.sh`，
2.7 MB）：bionic 头 + 编译器自带头 + `crti/crtn` + libc/libm 存根 + `include/python3.12` +
`include/lua5.4`。

三个非显然的点：

- **`crti.o`/`crtn.o` 谁都不提供**：NDK 只有 `crtbegin_so.o`/`crtend_so.o`，tcc 却坚持要
  crti/crtn，于是自己编两个空对象顶上（它们只承载 `.init/.fini` 序言，共享库不需要）。
- **`-nostdlib` 是必须的**：不加就去找 `/usr/local/lib/tcc` 里的 CRT。
- **`-DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD` 也是必须的**：bionic 的 `bits/ioctl.h` 在
  `__overloadable` 后面又声明了一次 `ioctl`，tcc 不实现那个扩展，读成「redefinition」。

真机两条都验过：普通 `.so`（`atomcode_probe(5) = 12`）和 **Python 扩展**
（`import tccmod` → `hello from tcc`）。后者能成立还依赖运行时的 `preload` 把 libpython 提成全局。

顺带补了**数据包**这种形态：manifest 的 `entry` 允许为空，表示「只有文件、没有命令」——
不给它建 bin 链接（否则就是一个注定失败的命令），健康探测直接跳过。判据
`a_data_only_package_installs_without_owning_a_command`。

#### 残留目录改成 manifest 声明，而不是写死 python

原来 `leftovers` 里写死 `["python-packages", ".local"]`，还靠「有没有 python 包」判断死活。
现在由包自己声明：manifest 加 `data_dirs`，安装时记进 `packages/.data-dirs.json`。

关键是**为什么需要注册表**：包被卸载后 manifest 就没了，磁盘上没有任何东西还记得「
`python-packages` 曾属于 cpython」。所以卸载时不能删记录，只能等清理时再删——这也正好是
判断「它是残留还是活的」的唯一依据：**声明的目录 + 所有者已不在 = 残留**。

接口因此多了一个字段（`package`），清理后同步删掉注册项，报告不会继续指名一个已不存在的目录。
判据 `declared_data_dirs_are_leftovers_only_after_the_owner_is_removed`、
`an_escaping_data_dir_is_refused`。

#### numpy：被 Cython 和网络同时挡住（未完成）

numpy 1.26.4 的 sdist **不带** 32 个 `.pyx` 对应的生成 C（`numpy/random/*` 全在里面），
`setup.py` 因此硬性要求 Cython；而两台机器的网络同时不可用（WSL 镜像 `000`，设备 DNS
`no address associated with hostname`，随后 `ping` 直接 `Operation not permitted`），
本地也没有任何 Cython 轮子。结论：**先取到 Cython 3.0 轮子再继续**，其余步骤（NDK clang +
`--cross-compile`、组装 `cp312-cp312-linux_aarch64` wheel、`pip install --platform`）已就绪。

#### 能力边界写进工具描述

模型看到的是 shell 工具描述，不是这份文档。所以 `android_tools::capability_notes` 按
**实际装了什么**追加句子：有 cpython 才说 pip/FTS5/spawn 可用、有 lua 才说 C 模块可 require、
有 tcc **且**装了 cheaders 才给出那条编译命令（少了头文件包它第一步就失败）。
判据 `capability_notes_only_describe_what_is_actually_installed`。

#### numpy：交叉编译放弃，改走预编译 wheel + 运行时兼容层

先试的是在构建机上交叉编译，三条路都堵死：

1. **numpy 1.26.4 的 distutils 构建没有交叉编译开关**（`--cross-compile` 不被识别），
   配置探测要「编译并运行」测试程序，这些程序在 x86_64 主机上跑不了，于是
   `MANDATORY_FUNCS` 全部被判缺失，构建直接 `SystemError`。给探测打补丁越过后，链接阶段
   又暴露出更根本的问题：它把**宿主**的 Python 路径混了进来（`-I/usr/include/python3.12`、
   `-L/usr/lib/x86_64-linux-gnu`），最后连 `--fix-cortex-a53-843419` 都报「仅支持 AArch64」。
2. **Termux 仓库的 `python-numpy` 是按 Python 3.14 编的**（`cpython-314-…so`、
    `NEEDED: libpython3.14.so`），与我们的 3.12 ABI 不兼容。
3. meson 交叉构建是正路，但需要主机侧 meson+ninja 与一份 cross file——留作后续。

真正跑通的是 **Chaquopy 的 Android wheel**：`numpy-1.26.2-0-cp312-cp312-android_21_arm64_v8a.whl`。
它要求运行时配合两件事，都在这个改动里解决：

- **`libpython3.12.so` 这个名字必须能解析**。我们的库 SONAME 是 `libpython3.12.so.1.0`，
  而 bionic 只按 **SONAME** 匹配已加载的库，所以短名永远解析不到。做法是把包里那份
  libpython 的 SONAME 字符串原地改成短名（等长、NUL 填充），然后把包内**所有** `.so` 里
  指向长名的 `DT_NEEDED` 一并改写——否则我们自己编的 `_posixshmem` 会反过来说
  `library "libpython3.12.so.1.0" not found`（这一步第一次放错了位置，`_posixshmem`
  是后编的，漏掉了，回归测试才发现）。
- **`libopenblas.so` / `libc++_shared.so` 要能按名字找到**。app 私有目录不在加载器的搜索
  路径里，而 bionic 不会因为「目录里有这个文件」就解析 `DT_NEEDED`。解法是在 shim 里
  启动前 `dlopen(…, RTLD_GLOBAL)` 这两个伴随库，把它们的 SONAME 注册进全局组；文件不在
  就跳过（没装 numpy 的机器不需要付出代价）。代价是运行时包从 24 MB 涨到 **29 MB**。

真机验证：

```
numpy 1.26.2   matmul → [[0,2,4],[6,8,10],[12,14,16]]   det 0.0   sum 36.0
```

安装命令（平台标签必须显式给，pip 推不出 Android）：

```sh
python3 -m pip install --platform android_21_arm64_v8a --only-binary=:all: \
  --no-deps --target $HOME/python-packages <wheel>
```

#### Doze 压测：结论为负，而且指向一个具体设计问题

两轮实测，都不合格：

| 场景 | 结果 |
|---|---|
| `dumpsys deviceidle force-idle`（强制深空闲） | 第一个 `sleep 90` 正常走完（`A 22:30:13 → B 22:31:43`），随后服务不再接受任何连接：宿主转发、设备内部 `urllib` 都超时；`unforce` 之后仍未恢复，只能重启进程 |
| 真实熄屏静置（不强制 Doze） | 回合停摆：只打到「开始执行第 1 条命令」，没有任何 `tool_result`，SSE 流结束 |

根因在服务侧的唤醒锁时序：`publishStatus` 在「活跃回合数为 0」时**释放** `PARTIAL_WAKE_LOCK`。
释放之后 CPU 一旦睡下去，5 秒一次的轮询线程自己就跑不起来，于是「回合刚开始、锁已释放」
的这个窗口里，agent 没有任何东西把 CPU 拉回来——回合就停在那里。`force-idle` 更进一步，
连前台服务也被挂起，套接字留在 LISTEN 但没人 accept。

修复方向有取舍，需要拍板：

1. **由 daemon 通知**：`/chat` 开始与结束时通过 JNI 回调 Kotlin 去取/放唤醒锁。最准，但要在
   Rust 侧持有 JavaVM + 全局引用，改动跨语言。
2. **文件式心跳**：daemon 把「有活跃回合」写进一个文件，服务在**拿到锁期间**轮询它；锁始终
   以短超时续租（如 60 秒）。实现简单，但会一直在熄屏时保持 CPU 唤醒。
3. **申请电池优化豁免**（`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`）让系统不要 Doze
   本应用。最省事，但属于用户可见的权限请求，且商店上架有限制。

#### Doze 的修复：让运行时说「回合开始了」

按上表第三条之外的方案组合实现（**JNI 通知为主，电池优化豁免为辅**）：

- **daemon 侧**：`ActiveChatRegistry::admit` 成功时、`complete` 使活跃数归零时，调用一个
  **由宿主注入**的钩子（`set_turn_hook`）。放在 daemon 而不是 Android crate 里，是为了让
  daemon 继续不认识 JNI；钩子在索引锁释放**之后**触发，避免宿主代码在持锁时回调。
- **JNI 侧**：`nativeStart`（还在 Java 线程上）缓存 `JavaVM` 与
  `com/atomcode/app/AgentPower` 的全局引用——attached 的原生线程没有应用 class loader，运行时
  再去 `find_class` 是找不到的。之后每次回合边界只做一次
  `call_static_method(…, "turnActive", "(Z)V", …)`。
- **Kotlin 侧**：`AgentPower` 用 `@JvmStatic` 暴露 `bind/unbind/turnActive`；
  `AgentService.onTurnActive(active)` 直接取/放 `PARTIAL_WAKE_LOCK`。健康轮询保留（它只负责
  改写通知文案），但**只能加锁不能因为「0 个活跃回合」就放锁**——那个 5 秒轮询在 CPU 睡下后
  根本不会运行，这正是原来的 bug。
- **电池优化豁免**：首次启动请求一次 `ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`
  （清单里加了权限，用 `SharedPreferences` 只问一次，OEM 抛异常也吞掉）。用户拒绝不构成错误：
  降级路径就是 JNI——锁照样跟着回合走，只是系统仍可能在长回合里把进程冻住。

`adb shell cmd deviceidle whitelist ±com.atomcode.app` 可以在不点对话框的情况下切换豁免状态，
两种状态都要跑一遍熄屏长回合。

#### Python 载荷：插件可以直接提供 Python 包

`numpy` 这类科学计算包体积大、且在设备上编不出来（见上节），装成插件比 `pip install` 多三样
东西：**清理精确**、**体积可见**、**可回滚**。为此 manifest 增加一个字段：

```json
{ "name": "numpy", "version": "1.26.2", "python_packages": "lib/python" }
```

安装时 daemon 会把 `lib/python/` 下的每个子项**软链**到 `<product>/python-packages/`（运行时
本来就把这个目录放在 `PYTHONPATH` 上），并把「链接名 → 所属包」记进
`packages/.python-links.json`。三个不显然的取舍：

- **用软链不用拷贝**：拷贝会让 30 MB 变成 60 MB，而且卸载后留下一整份死拷贝——正是 leftovers
  要防的那种问题。软链只有一份，卸载按记录精确摘除。
- **冲突就拒装**：目标位置已有真实文件/目录（不属于任何包）时拒绝，不覆盖用户放在那里的东西；
  名字已被别的包持有时同样拒绝。
- **失败可回滚**：链到一半失败会把已建的链接撤掉，不留半装状态。

判据 `a_python_payload_is_linked_into_python_packages_and_unlinked_with_it`（unix-only，
机制本身就是软链）。打包脚本 `scripts/build-android-numpy-plugin.sh` 把预编译 wheel 解成
`lib/python/` 再打 tar，numpy 1.26.2 的包 4.5 MB。

#### 大件评估：Zig 编译器与 libnode

设备自己列出的下一个缺口是「真 C/C++ 编译器、rustc/cargo、node/npm」。先不谈 rustc，另外两个的
结论如下——两者都不是「装个包」的事，且都受同一条硬约束限制：**共享存储 noexec、数据目录
禁止 execve**，所以任何「程序」都必须按我们已有的方式包装：把 `main` 改名，编成 `.so`，由
`pluginhost` dlopen（Lua 与 SQLite 就是这么做的）。编出来的产物也只能是 `.so`，可执行文件跑不了。

**Zig（自带 clang 前端）—— 可行，但收益比看起来小。**

- 形态合适：官方有 `aarch64-linux` 的静态 tarball（压缩约 45 MB，展开 200 MB 上下），自带
  LLVM/clang，`zig cc` 能产出 Android 的 `.so`，正好喂给 `ctypes` 与 Lua 的 `require`。
- 主要风险不在体积，而在**它是个会调用自己的程序**：`zig` 大量依赖「重新执行自己」来完成
  编译子过程，并维护全局 cache 目录。我们这套 dlopen 包装对「跑一次就退」的工具（lua、
  sqlite3）够用，对自调用的工具需要额外验证——这与 `multiprocessing` 撞上 `sys.executable`
  为空是同一类问题。
- 相对 tcc 的增量是 C++ 与 C99+ 的完整支持；如果目标只是「编个小 `.so` 给 Python/Lua 用」，
  tcc 现在就能做。

**libnode —— 可行，是一个项目而不是一次配置。**

- 路径清楚：`./configure --shared` 产出 `libnode.so`，再用一个 embedder shim 调
  `node::Start(argc, argv)`，形态与 `pluginhost` 完全一致（它本来就是被设计成可嵌入的）。
- 代价：Android aarch64 的 node 构建要带 ICU（数十 MB）、V8 在 arm64 上要开 JIT 且对
  `mprotect`/W^X 有要求，交叉编译需要一串补丁；整体是「天」级而不是「小时」级。
- 而且它只能解决「跑 JS 脚本」，不能解决「装 npm 包」——npm 自己还要 spawn 子进程。

结论：两者都留作独立任务，动手前应先确认「编出来的 `.so` 谁来用」，否则容易变成装了个
用不上的大件。

#### 试做结果：libnode 成立，Zig 需要「换一个可执行位置」

两者都在真机上跑通了，但成立方式完全不同——差别来自**它们是「库」还是「程序」**。

**libnode：按插件形态走通。** nodejs-mobile 的产物本来就是给嵌入用的：`bin/arm64-v8a/libnode.so`
导出 `node::Start(argc, argv)`。所以包装只有两行（`scripts/node-plugin/plugin.cc`），manifest
与 Lua/CPython 完全一致（`entry: lib/plugin.so`、`symbol: plugin_main`、`aliases: ["node"]`、
`preload: [libnode.so, libc++_shared.so]`）。真机：

```
v18.20.4 android arm64 2          ← node -e 'console.log(process.version, …, 1+1)'
[2,4,6] true                       ← 数组与 require("fs")
```

三条边界必须提前说清（都是「解释器是库、不是进程」的后果）：`process.execPath` 指向
`pluginhost`，所以 `child_process.fork/spawn` 无法再拉起 node；预编译 addon 链的是 glibc，
装不上；JS 段错误会把整个 agent 一起带走。

**Zig：不是插件，而是「必须放进 nativeLibraryDir 的程序」。** 1.3 亿字节的
`zig-linux-aarch64` 是**静态链接的 `ET_EXEC`**（`NEEDED` 为 0），所以：

- `dlopen` 不了它，插件形态不成立——这与 make/tcc 当初的结论是同一条；
- 数据目录 `execve` 被禁，唯一能执行的位置是 APK 的 `nativeLibraryDir`。

做法就是给现有 APK 加一个「伪装成库的可执行文件」：把 zig 二进制放进
`jniLibs/arm64-v8a/libzig.so`（`useLegacyPackaging = true` 保证它被解压到磁盘），
`<product>/bin/zig` 软链过去即可执行。二进制约 135 MB，APK 从 61.5 MB 涨到 **112.3 MB**。

真机：`zig version` → `0.13.0`；`zig cc -shared -fPIC` 产出的 `.so` 只依赖 `libc.so`（bionic），
`ctypes` 加载后 `atomcode_zig(5) = 10`、`(7) = 12`。

两个必须显式给的环境，否则它会以奇怪的方式失败：

| 变量 | 不设的后果 |
|---|---|
| `TMPDIR=<product>/tmp` | `unable to make temporary file: Permission denied`（`/tmp` 不属于应用） |
| `ZIG_LIB_DIR=<product>/zig/lib` | 找不到自带的标准库/libs（二进制在 APK 里，`../lib` 不存在） |

`-target aarch64-linux-android` + `--sysroot` 这条路**没有**走通：Zig 0.13 不认
`android26` 这种带 API level 的三元组，而我们拼的 sysroot 也没能被它的 libc 探测接受
（`-v` 显示 `<...>` 搜索列表里只有 `zig/lib/include`）。默认 target 反而直接产出 bionic 链接的
`.so`，所以可用；只是若要指定 API level，得等 Zig 的 android libc 支持更完整。

**顺带记一条工程事实**：GitHub release 直链在这台设备上约 5 KB/s（54.6 MB 传了 7 分钟只到
2 MB），换 `gh-proxy.com` 立刻到 330 KB/s。以后给设备取大件先试代理，别怀疑代码。

#### 划清楚：哪些缺项是设计边界，不打算修

外部报告列了一批"缺失"，但其中相当一部分不是缺陷，补了也没意义：

| 缺项 | 为什么不修 |
|---|---|
| `venv` 隔离失效 | 解释器寄宿在 `libpluginhost.so` 里，没有独立进程启动流程；`pyvenv.cfg` 天然被忽略。隔离的正确做法是 `pip install --target` + `PYTHONPATH` |
| `manylinux*` wheel | glibc 与 bionic 是两套 C 库，`libc.so.6` / `libpthread.so.0` 在本机不存在。这不是打包能解决的 |
| `_tkinter` | 没有 X11/显示服务；Tk 还要另带 Tcl |
| `crypt` | bionic 不提供 libcrypt；而且该模块 3.12 已废弃、3.13 移除 |
| `_curses_panel` | 只差个库名映射，但 curses 面板在**非 TTY** 设备上没有使用场景 |
| `_dbm` / `dbm.ndbm` | 走 ndbm 老接口，需要额外的 compat 归档；新代码用 `dbm.gnu`（已可用）即可 |
| `_uuid` | 纯 Python 回退本来就可用，为它编整个 util-linux 不值 |
| `nis` / `spwd` / `_posixshmem` | Android 没有这些平台概念 |
| `tcc` 缺 `crt1.o` / `crti.o` | 补齐也编不出**能跑**的可执行文件——数据目录 `noexec`。要 C 性能就走 `tcc -shared` + `ctypes`，这条已经通 |

**对报告的一处更正**：报告说 `sysconfig._sysconfigdata` 缺失，实测包里
`lib/python3.12/_sysconfigdata__linux_.py` 是存在的，`sysconfig.get_config_var(...)`
也能取值。它指向构建机的路径（`/home/john/...`）属于交叉编译的正常现象，不是文件缺失。

真机实测（`run-as` 直跑）：

```
import bz2, lzma                                  → ok
tarfile write/read mode "w:bz2" / "r:bz2"         → hello bz2
tarfile write/read mode "w:xz"  / "r:xz"          → hello xz
packages/cpython/include/python3.12/Python.h      → 存在
```

第三种压缩格式到这里齐了：之前 `.tar.bz2` / `.tar.xz` 一律解不开，现在三种都能读写。

#### 对那份外部报告的两处勘误

1. **`.bin-overrides/python3` 的内容是 `libpython3.so`** —— 报告解读为「宿主期望预加载该
   soname，但该文件不存在」。实际它是**接管记录**：CPython 包声明 `python3` 别名时，把原来
   指向随 APK 的 RustPython（`libpython3.so`）的链接记下来，供卸载时还原。与预加载无关，
   也不表示那个文件「应该」在同目录。
2. 报告把 `zlib` 归入「加载失败」是对的（它确实是共享扩展 `zlib.cpython-312.so`），但我们
   自己中途一次核对用了错误的模块名 `_zlib` 得出「文件不存在」，那是我们查错了，报告没错。


### P6：后台存活、内存与通知（真机实测 2026-10-26）

三件事各测了一遍，其中一件查出真 bug。

#### 1. 内存：没有压力，先前报告的「只剩 118 MB」是误读

```
MemTotal      5616 MB
MemAvailable  1856 MB
分配 512 MB   ✅ ok
分配 1 GB     ✅ ok
分配后 MemAvailable  2077 MB
```

`free` 输出的 “free” 列不是可用内存。以 `MemAvailable` 为准，1 GB 级别的分配在这台机器上
毫无问题——任何据此判断「内存很紧、别跑大数据」的结论都该作废。

#### 2. 息屏：长任务连续跑完，没有被节流

起一个 7×20 s 的打点循环（`tick N` + `DONE_ALL_TICKS`），立刻熄屏，等 170 s 再点亮：

```
DONE_ALL_TICKS   ← 全 7 个 tick 跑完
dumpsys deviceidle: mState=ACTIVE mLightState=ACTIVE
```

说明息屏（屏幕关、CPU 唤醒锁由前台服务持有）不会打断 agent。**真正的 Doze 没有验证到**——
它要求设备静止且未充电约 30 分钟才进入，这次 `mState` 一直是 `ACTIVE`。要验那一段得挂机半小时以上。

#### 3. 通知：以前**根本没显示**，因为从没申请过权限

前台服务一直在跑，但通知在系统里根本不存在。查出来是：

```
POST_NOTIFICATIONS: granted=false, flags=[USER_SENSITIVE_WHEN_GRANTED|USER_SENSITIVE_WHEN_DENIED]
appops: POST_NOTIFICATION: ignore
```

Android 13+ 把通知变成运行时权限，而 manifest 里虽然声明了 `POST_NOTIFICATIONS`，**代码里从来
没有申请过**。于是「后台通知/进度」这一项的真实状态是：有服务、无通知。`pm grant` 在这台
OEM 机器上也走不通（`Neither user 2000 nor current process has
android.permission.GRANT_RUNTIME_PERMISSIONS`），必须走系统弹窗让用户点。

修法两步：

1. `MainActivity.onCreate` 在 Android 13+ 且未授权时 `requestPermissions(...)`；用户点「允许」
   之后通知才真正出现。
2. 通知内容改成活的：`/health` 增加 `active_chats`（正在跑的 `/chat` 回合数），
   `AgentService` 每 5 s 轮询一次 loopback 上的 `/health` 并重写通知文案。

真机验证：

```
空闲：  text=String (Idle — the agent runs on this device)
回合中：text=String (Working on 1 conversation(s)…)   ← /health 同时报 active_chats:1
```

一个刻意选择：轮询拿不到结果时**不改文案**（`-1` 直接 return），而不是写「空闲」——把「问不到」
说成「没在干活」正是最误导人的那种错误。



设备上没有 git。先试过的「数据目录里放 `/system/bin/sh` 垫层」被 SELinux 直接拒绝
（`untrusted_app` 对 `app_data_file` 没有 exec，日志是 `the bundled git is installed but cannot run:
… Permission denied`）。唯一可执行的位置是 APK 的 `nativeLibraryDir`，而 Gradle 只打包
`lib*.so` 名字。整条链因此是：

| 约束 | 做法 |
|---|---|
| 文件名必须是 `lib*.so` | `git` → `libgit.so`，`git-remote-https` → `libgit-remote-https.so` |
| 库要以文件形式解压出来 | `packaging { jniLibs { useLegacyPackaging = true } }` |
| git 按裸名找自己的子程序 | `scripts/patches/git-android-lib-lookup.patch`：名字在 `PATH` 里找不到时，落到 `<GIT_EXEC_PATH>/lib<name>.so` |
| 工具层按 `PATH` 找 `git` | 宿主在 `<app data>/bin` 写符号链接指向库目录并前置 `PATH`；`GIT_EXEC_PATH` 指向库目录 |
| 系统根证书是散装 PEM | 拼成 `<app data>/ca-bundle.pem`，经 `GIT_SSL_CAINFO` 与 `http.sslCAInfo` 交给 libcurl |
| 全局配置不能靠 `HOME` 猜 | `GIT_CONFIG_GLOBAL` 固定指向 `<app data>/.gitconfig`（`credential.helper=store`、`safe.directory=*`、`autocrlf=false`、CA 路径） |

实现在 `crates/atomcode-android/src/shims.rs`，启动时由 `host.rs` 调用一次。

构建（Linux/WSL；`sdkmanager` 装的 Windows NDK 不带 `linux-x86_64` prebuilt）：

```bash
ANDROID_NDK_LINUX_HOME=~/android-git-build/ndk/android-ndk-r26b bash scripts/build-android-git.sh
```

脚本静态交叉编译 OpenSSL 3.0.16 → curl 8.11.1 → git 2.47.1，strip 后落到
`android/app/src/main/jniLibs/arm64-v8a/`：`libgit.so` 3.4M、`libgit-remote-https.so` 6.3M，
NEEDED 只有 `libz.so libdl.so libc.so`（都是系统库）。

同一个脚本还产出四个**额外工具**（`jq` / `rg` / `curl` / `python3`，见「已知缺口」一节的表格）：
`libjq.so` 883K、`librg.so` 4.98M、`libcurl.so` 4.67M、`libpython3.so` 33.5M。它们走同一条
通道——库目录放二进制、`<data>/bin` 放同名符号链接——所以不需要第二套机制。宿主启动时逐个
`--version` 探测并打日志：

```
AtomCode: bundled git is available git=git version 2.47.1
AtomCode: bundled tool is available tool=jq version=jq-1.7.1
AtomCode: bundled tool is available tool=rg version=ripgrep 15.2.0
AtomCode: bundled tool is available tool=curl version=curl 8.11.1 …
AtomCode: bundled tool is available tool=python3 version=Python 3.14.0.alpha …
```

**RustPython 的两个构建坑**（脚本已自动处理，这里记下原因以免日后被"简化"掉）：

1. **`-lffi` 找不到**。RustPython 的 `ctypes` 依赖 libffi，而 NDK 不带它。
   `build-android-git.sh` 会先静态交叉编译 libffi 3.4.6 到 `$PREFIX`，再把 `-L$PREFIX/lib`
   加进链接（`libffi.a` 缺失时脚本自己拉源码编译）。
2. **`undefined symbol: __clear_cache`**。libffi 的闭包代码在 bionic 上找不到这个符号；
   `-D__clear_cache=__builtin___clear_cache` 只把调用改名，AArch64 上编译器仍会下降成同名
   调用，所以必须**链接 NDK 的 compiler-rt**：
   `lib/clang/17/lib/linux/libclang_rt.builtins-aarch64-android.a`。

另外 `cargo install` 要带 `--locked`：不带时依赖解析会漂移，`rustpython-codegen` 会
以 `expected &str, found StringLiteral` 之类的错误编译失败——那是版本不一致，不是代码问题。

特性选择上刻意避开 C 依赖：不启用 `mimalloc`（C 分配器）、不启用 `ssl-*`（aws-lc 需要
cmake/nasm）。`freeze-stdlib,stdio,threading,host_env,importlib` 已经覆盖普通脚本所需。

从零构建需要 Linux NDK（`ANDROID_NDK_LINUX_HOME`）与 Rust 工具链（`cargo` + `rustup target add
aarch64-linux-android`）；脚本会在缺 `cargo` 时直接说明，而不是埋在 cargo 的报错里。

**真机实测（OnePlus PLT140 / Android 16）**

- 应用进程日志：`bundled git is available git=git version 2.47.1` —— 符号链接 → 库目录的 exec
  在真实 `untrusted_app` 域里被放行，这是整个方案的关键假设。
- 以应用 uid + 应用自己写的配置跑通：`git clone`（HTTPS，系统根证书）、`git commit`、
  `git push`（`send-pack` → `receive-pack`，推到设备上的裸仓库）。
- 补丁生效：`git-remote-https` 的裸名查找落到 `$GIT_EXEC_PATH/libgit-remote-https.so`。

**踩过的坑**

- `SSL_CERT_FILE` 对这个静态 curl 的 OpenSSL 默认路径不生效，HTTPS 一律
  `unable to get local issuer certificate`；换 `GIT_SSL_CAINFO` 后通过。
- bionic 的 `getpwuid()` 给 `pw_gecos = NULL`，`git clone/checkout` 在 `copy_gecos` 段错误 →
  构建加 `NO_GECOS_IN_PWENT=1`（git 自己的 Android 平台分支本来就这么定）。
- API 26 没有 `pthread_setcancelstate` → `NO_PTHREADS=1`；bionic 没有 librt → `NEEDS_LIBRT=`。
- `GIT_TEMPLATE_DIR` 不能指到被 clone 的目录上，否则 `git init` 递归复制自身把磁盘写满。

**凭据入口（daemon 侧，真机已验证）**

手机没有终端，也没有可回答的凭据提示（`GIT_TERMINAL_PROMPT=0` 保证提示会变成失败回合，
而不是卡住）。所以 token 只能由调用方递进来一次，存到 git 自己的 `credential.helper=store`
读取的位置。这就是 `crates/atomcode-daemon/src/git_credentials.rs` 与三个路由：

```
POST   /git/credentials           {host, username, token}   写/替换该 host 的凭据
GET    /git/credentials           → [{host, username}]      只回主机与账号，永不回 token
DELETE /git/credentials/:host                                删掉该 host
```

`host` 必须是裸主机名（`atomgit.com`，不是仓库 URL）；写盘是 `$HOME/.git-credentials`，
权限 0600，落盘用临时文件 + rename。Android 宿主把 `HOME` 无条件设成 app 私有产品目录，
所以「daemon 写到哪」和「git 去哪读」是同一个位置。

`credential.helper=store` 会被 git 以 `git-credential-store` 子进程拉起，因此 `PATH` 里还多一条
指向 `libgit.so` 的符号链接——没有它，存进去的 token 永远不会被读到，私有远端 push 会报
「像没给凭据一样」。

**真机实测（2026-10-24）**

```
POST /git/credentials {"host":"atomgit.com","username":"alice","token":"…"}
GET  /git/credentials → {"credentials":[{"host":"atomgit.com","username":"alice"}]}

- -rw------- 1 u0_a348 u0_a348 41 .git-credentials
- https://alice:tok_secret_123@atomgit.com
$ printf 'protocol=https\nhost=atomgit.com\n\n' | git credential fill
  username=alice
  password=…
```

即：写进去的凭据被 git 自己的 helper 读了出来。未验证的是「真 token 打真远端」——需要
一个私有仓库和有效 token。

**P4 剩余面（已完成）**

- webui 已有 Git 面板（`webui/src/components/SourceControl.tsx`）：仓库状态、暂存 / 取消暂存、
  提交、拉取 / 推送、远端与凭据管理、克隆 / 初始化，入口在侧栏「源代码管理」。
- daemon 侧是 `crates/atomcode-daemon/src/git_ops.rs` 的八个路由（见下节）。
- agent 仍可直接用 `bash` 工具跑 git；面板只是同一仓库的另一种操作方式，不持有第二份状态。

**webui Git 面板（已实现）**

现状：webui 只有会话、技能、MCP 和设置，没有源代码管理入口。下面是把「手机上的
VSCode 源代码管理」补上所需的最小设计；代码已按此落地。

**目标与范围**

- 在 webui 里查看当前工作目录的仓库状态：分支、上游、领先/落后、已暂存与未暂存的改动。
- 完成日常闭环：暂存 / 取消暂存、提交、拉取、推送；没有仓库时初始化或克隆。
- 管理 HTTPS token：填入、列出（只显示 host 与账号）、删除。token 永不回显。
- 非目标（本阶段不做）：分支切换 / 合并 / 变基 / 冲突解决、历史浏览、逐行 diff、
  子模块、SSH 私钥管理。冲突或复杂操作仍交给 agent 的 `bash` 工具。

**daemon 侧：`crates/atomcode-daemon/src/git_ops.rs`**

新增路由全部挂在受保护 Router 上，与 `/git/credentials` 相邻：

| 方法 | 路径 | 请求体 | 返回 |
|---|---|---|---|
| GET | `/git/status` | — | `repo`、`root`、`branch`、`upstream`、`ahead`、`behind`、`has_commits`、`changes[]`、`remotes[]` |
| POST | `/git/init` | — | `output` |
| POST | `/git/stage` | `{paths: string[]}` | `output` |
| POST | `/git/unstage` | `{paths: string[]}` | `output` |
| POST | `/git/commit` | `{message: string}` | `output`、`commit` |
| POST | `/git/pull` | `{remote?, branch?}` | `output` |
| POST | `/git/push` | `{remote?, branch?}` | `output`、`upstream` |
| POST | `/git/clone` | `{url, dir?}` | `output`、`dir`、`root` |

`changes[]` 的每一项是 `{path, status, staged, untracked, orig_path?}`：`status` 是 git 的
两字符状态码，`staged` 表示索引里有改动，`orig_path` 只在重命名 / 复制时出现。
全部错误走现有的 `json_error`，与其它端点保持同一形状。

命令执行约束：

- 不经过 shell，参数以数组传递；`git` 从 `PATH` 解析，桌面用系统 git，Android 用
  `shims.rs` 前置进 `PATH` 的打包版。
- cwd 取 `AppState.project` 里的 `working_dir`（与 `/project` 同一个权威来源）；先用
  `git rev-parse --show-toplevel` 找到仓库根，之后的操作都在根上执行，所以工作目录是
  仓库子目录时也能正常工作。
- 环境固定 `GIT_TERMINAL_PROMPT=0`（没有终端可答）、`GIT_PAGER=cat`、`GIT_EDITOR=true`，
  并显式带 `-c credential.helper=store`：Android 的全局配置本来就有，桌面系统 git 不一定有，
  显式带上才能保证两边都读 `~/.git-credentials`。
- 本地操作（status / stage / unstage / commit / init）超时 30 秒，网络操作（pull / push /
  clone）超时 180 秒；超时即杀掉子进程并返回可读错误，不留下悬挂请求。
- 返回给前端的命令输出只保留尾部并截断长度；token、密码和完整 remote URL 中的 userinfo
  不进入任何响应或日志。
- `paths` 只接受仓库内的相对路径，拒绝绝对路径、盘符和 `..`；`clone` 的 `dir` 同样限制为
  项目目录下的相对路径，为空时从 URL 末段推导。
- `status` 用 `git status --porcelain=v1 -z --branch --untracked-files=all` 一次取全
  （含 `##` 分支行），避免为每个字段各起一个进程；`pull` 带 `--no-edit`；`push` 在分支
  没有上游时退化为 `push -u <remote> HEAD`，并在响应里回报新的上游。

状态所有权：

- git 仓库本身是唯一权威。daemon 不在内存里缓存仓库状态，每个请求现问 git；webui 也不
  维护第二份 git store，面板打开或手动刷新时重新拉取。
- 不新增 runtime owner，也不把 git 生命周期塞进 kernel；它只是 daemon 的本地命令端点，
  与正在跑的 agent 回合互不持有状态。

**webui 侧**

- 入口：`Sidebar.tsx` 的 `.sidebar-actions` 增加第四行「源代码管理」，与「新建对话 /
  技能 / MCP」同一视觉层级；窄屏抽屉和桌面侧栏共用这一入口。
- 面板：`SourceControl.tsx`，复用现有 `.modal-overlay` / `.modal-card`。桌面是居中的
  宽卡片，手机沿用现有媒体查询变成底部抽屉（全宽、`85dvh`），不引入第二套面板框架。
- 布局自上而下：仓库行（分支、上游、`↑ahead ↓behind`、刷新）→ 暂存区 → 未暂存区 →
  提交框与提交按钮 → 拉取 / 推送 → 远端与凭据 → 克隆 / 初始化。文件行同时提供暂存 /
  取消暂存按钮，不依赖 hover，触屏可直接点。
- 不是仓库时，面板只显示「当前目录不是 Git 仓库」、初始化按钮和克隆表单；克隆成功后用
  返回的 `root` 刷新状态。
- 凭据区列出 `GET /git/credentials` 的结果，新增时要求 host（裸主机名）与账号；删除走
  `DELETE /git/credentials/:host`。输入框只提交，不在本地存储 token。
- 请求走 `webui/src/api.ts` 的 `authHeaders()` / `apiFetch`，与其它端点同一鉴权路径；
  文案进 `i18n.ts` 的 zh / en 两套字典。

**失败与边界**

- 未安装 git / 打包版不可执行：`/git/status` 返回 `repo: false` 并在错误里说明「找不到
  git」，面板显示可行动的提示，而不是空白列表。
- 没有提交时取消暂存：先尝试 `git restore --staged`，失败再退到 `git rm --cached`。
- 拉取遇到冲突：命令失败原样返回 git 的说明，仓库保持冲突状态；面板不假装成功，也不
  自动 reset。
- 推送没有上游：按上面的 `-u` 规则处理；detached HEAD 或无 remote 时直接报错。
- 私有远端 401：错误信息指向凭据区，提示补 token；不提示用户在手机终端里输入。

**验证**

- daemon 单测覆盖 porcelain 解析（分支 / 上游 / ahead-behind / 重命名 / 未跟踪）、路径
  校验和输出截断；路由级用临时仓库跑一遍 stage → commit → status（`git_ops.rs` 的
  `mod tests`）。
- `cargo check -p atomcode-daemon --all-targets`、`cargo test -p atomcode-daemon --lib git_ops`
  与 `cargo fmt --all -- --check`。
- webui `npm run typecheck` / `npm run build`。

**真机实测（2026-10-24，OnePlus PLT140 / Android 16）**

APK 重编后按 `/git/*` 路由逐个调用（`adb forward` + 打印在 logcat 里的端口与 token，
工作目录 `/storage/emulated/0/AtomCode`）：

| 场景 | 结果 |
|---|---|
| 非仓库 `GET /git/status` | `repo:false` + `error:"current directory is not a git repository"` |
| `POST /git/init` | 建 `.git`，随后 status 为 `branch:"main"`、`has_commits:false`、文件列为未跟踪 |
| stage / unstage | 无 HEAD 时 unstage 走 `git rm --cached`，状态在 `??` 与 `A ` 之间正确切换 |
| `POST /git/commit` | 返回 40 位 commit SHA，status 变干净、`has_commits:true` |
| 空提交信息 / `../evil.txt` / `/etc/passwd` | 均 400 且给出可读原因，不落到 git |
| 无远端 push、无上游 pull | 原样返回 git 的 `fatal:` 说明，不假装成功 |
| `POST /git/clone` 本地路径 | 成功克隆到项目内子目录，`dir`/`root` 正确 |
| clone 绝对目标 / 已存在目标 | 均 400 拒绝 |
| `POST /git/credentials` + `GET` + `DELETE` | host/账号往返正确；token 落在 `files/atomcode/.git-credentials`，权限 `-rw-------`，列表从不回显 token |
| 空裸仓库克隆 → push → pull | push 建 `refs/heads/main`（`222dafd`）并自动设上游；另一端提交后 pull 快进到 `a5a2521`；裸仓库 `rev-parse` 与面板回报一致 |

真机暴露并已修复的两个解析缺陷（都补了回归单测）：

1. 未出生分支头是 `## No commits yet on main...origin/main [gone]`，原实现剥掉
   「No commits yet on」后把整段当成分支名；现改为剥前缀后继续按 `...` 与尾部
   `[...]` 组解析，`has_commits` 仍为 false。
2. 上游已被删除时 git 标 `[gone]`，原实现仍把它当作有效上游；现在清空 `upstream`，
   面板显示「无上游」，不会给出必然失败的 pull。

**真机 UI 实测（2026-10-24，同一台设备）**

设备重新上线后通过 WebView devtools（`adb forward tcp:9300
localabstract:webview_devtools_remote_<pid>` + CDP）驱动页面，视口 360×785：

| 检查项 | 结果 |
|---|---|
| 侧栏入口 | `.sidebar-action` 中找到「源代码管理」，点击后面板打开 |
| 窄屏形态 | `.modal-card.source-control` 为 `x=0 / width=360`（全宽）、`top=352`（贴底）、仅上圆角 14px、overlay `align-items:flex-end` —— 即底部抽屉 |
| 面板内容 | 标题「源代码管理」；仓库行与 `?? AtomCode/`、`?? hello.txt`、`?? remote/` 等未跟踪文件行（每行带「暂存」按钮） |
| token 表单 | 填 `atomgit.com` / `phoneuser` / token 后点「保存凭据」：提示「已保存 atomgit.com 的凭据」，列表出现 `atomgit.com phoneuser`，**两个输入框被清空**（token 不留在页面状态） |
| 删除凭据 | 点「删除」后该行消失，回到「还没有保存的凭据」空态 |

为让上述验证可重复，`MainActivity.onCreate` 增加了一条仅 debuggable 构建生效的
`WebView.setWebContentsDebuggingEnabled(true)`；release 构建保持 devtools 关闭。

验证后已清理真机测试残留：工作区恢复到只有 `hello.txt`（`AtomCode/`、`remote/`、
`.git`、`a.txt` 已删），app 私有目录里的 `remote.git`、`tmpclone` 也已删除。

**仍未验证**（环境限制，非代码缺口）

- 真机上的私有远端 clone / push（需要有效 token 与真实私有仓库）。已验证的是本地裸仓库
  的 clone → push → pull，HTTPS 链路仍只有「凭据被 git 的 store helper 读到」这一层。

### P5.11：附件链路、通知摘要与示例资产（已实现，2026-10-30）

`docs/android-next-steps.md` 里"待做"的前三项，这一轮做完。共同点：**都在既有接缝上加
一段，没有新开生命周期**。

#### 一、附件：手机拍照的图终于能发出去

**症状**：文件选择器能拉起（`MainActivity.onShowFileChooser`），选完图却什么都不发生。
根因不在 Android 侧——webui 的 `addImageFiles` 按 **2 MB** 硬拒绝，而手机拍照的图通常
3–8 MB / 4000px 宽，等于附件功能在 Android 上永远走不通。服务端的
`image_normalize`（长边 1568px、目标 512 KB）本来就能处理，但它只在字节过了网之后才跑。

**修法**：把这件事提前到浏览器里，与 daemon 用同一个长边上限。

| 位置 | 改动 |
|---|---|
| `webui/src/lib/imageAttach.ts` | **新增**。解码 → 长边压到 1568px → 按质量阶梯（0.9/0.82/0.72/0.6）重编码到 2 MB 以内；截图类（PNG/GIF、像素不多）仍走 PNG 保文字，照片走 JPEG |
| `webui/src/components/Chat.tsx` | `addImageFiles` 改为"压缩后入列"，只有连解码都不该尝试的**原始 40 MB** 才拒绝；新增"有图片读不出来"的提示 |
| `webui/src/i18n.ts` | 新增 `attach.unreadable` |

- 已在预算内的图**原样发送**，不重编码（避免一代代画质损失）。
- 解不出来的图不再静默丢弃：列表保持原样并给出提示。
- `image_normalize` 的 1568px 与 Anthropic 的视觉降采样目标一致，不会把图压到模型认不出。
- 单测：`webui/src/lib/imageAttach.test.ts`（7 条：等比缩放、极端全景不塌成 0 像素、
  3–8 MB 照片的落点、是否值得解码、PNG/JPEG 选择、质量阶梯单调）。

**未做**（记在 next-steps）：直接拍照（`ACTION_IMAGE_CAPTURE` + `FileProvider`）。

#### 二、通知栏报回合结果

**症状**：回合结束后通知退回"空闲"，人不知道刚才那轮成没成，必须切回应用。

**修法**：daemon 按会话记住最近几轮的结果，由 `/health` 报给宿主；Kotlin 已经在轮询这个
端点，直接用现成节奏，没有新增通道。

| 部件 | 位置 |
|---|---|
| `turn_outcome` 模块 | `crates/atomcode-daemon/src/turn_outcome.rs`：`LastTurn { outcome, stop_reason, summary, session_id, tool_calls, duration_ms, finished_at_unix }`，按会话各一格、最多 4 格（`RECENT_LIMIT`），同一会话再跑一轮是**替换自己那格**并前移 |
| 记录点 | live 回合：`live_hub.rs` 的 `TurnFinished(Completed / SnapshotUnavailable)` 与 `RuntimeStopped`（仅在真有回合在跑时），会话 id 取当前 binding；`/chat` 回合：同一个回调映射器，用 session id |
| 上报 | `HealthResponse.last_turns`（`Vec`，新的在前；没有回合时为空数组） |
| 显示 | `AgentService.fetchStatus()` 用 `org.json` 解析；一个会话时不带列表，两格以上折叠成"N 个会话有结果"并展开为 `InboxStyle`，每行 `标题 · 已完成/失败(原因)/已停止` |
| 标题 | 宿主按需 `GET /sessions` 取一次 `id → name`，只在出现**更新的** `finished_at_unix` 时刷新；取不到就退回 `#<前 6 位>` |

- **成败怎么判**：`Stopped`/`MaxRounds`/`RateLimited`/`StoppedByPolicy`/`PolicyDenied` 算
  "完成"（都有产出）；`Cancelled` 单独一类；`ProviderError`/`InternalError`/`Timeout`/
  guard 类算失败。`StopReason` 是 `#[non_exhaustive]`，不认识的未来原因一律按失败报——
  对没弄清的原因声称成功是唯一错误的答案。
- **摘要为什么这么短**：通知正文会被系统截断；daemon 侧把最后一条助手文本压成单行、
  按**字符**（不是字节）截到 80 个字符，避免把中文截断在半个字上。折叠态只有一行结论，
  展开态（`BigTextStyle`）才带模型最后那句。
- 单测：`turn_outcome::tests`（5 条），含"只保留最后一轮"与非穷尽原因的兜底。

#### 三、示例资产：新装不再是一个裸智能体

**症状**：`/skills` 空目录、`/mcp/status` 是 `{"servers":[],"trusted":false}`，没有任何
东西告诉用户这两套机制怎么用。桌面有 setup 向导负责播种（`setup::run` 解包
`setup-seeds.tar.zst`），手机没有那个时机。

**修法**：同一批资产，换一个更保守的安装策略。

| 部件 | 位置 |
|---|---|
| 示例技能（2 个） | `crates/atomcode-capabilities/assets/setup-seeds/skills/{project-check,add-mcp-server}/SKILL.md` |
| 安装 | `example_skills::{EXAMPLE_SKILLS, seed_missing}`（`crates/atomcode-capabilities/src/example_skills.rs`，无 feature 门控，直接 `include_str!`，手机构建不需要 zstd/tar） |
| 调用 | `crates/atomcode-android/src/host.rs`，在建运行时之前播种到 `<product>/skills` |

- `project-check`：手机上怎么做"跑一遍项目自检再汇报"（含 `noexec`、没有 node、大项目
  先跑子集这些实测出来的边界）。
- `add-mcp-server`：MCP 到底配在哪（用户级 `mcp.json` / 项目级 `.mcp.json`、JSONC）、
  `trust` 与 `blocked` 的含义、工具全名 `mcp__<server>__<tool>`，以及**Android 的硬限制**：
  stdio 的 `command` 必须落在 `nativeLibraryDir`，所以要么交给随包的 `python3`，要么走 `url`。
- **只播一次**：`<skills_dir>/.example-seeds` 记账。改过的示例不会被覆盖，**删掉的也不会
  被复活**（判据见单测 `an_edited_or_deleted_example_is_left_alone`）。台账是隐藏文件、
  在示例目录之外——放目录里边删目录就一起没了，删除就会失效。

**仍未做**：市场侧的 zig 二进制包（见下节），以及环境运行时（Go / node 22 / clang /
cmake，都是仓库外的交叉编译产物）。

#### 四、相机直传：不用退出应用去拍照

**症状**：`onShowFileChooser` 给的是系统文件选择器（相册/下载），没有相机入口——
想拍一张得先退出应用、用相机拍、再从相册选回来。

**修法**：把 `ACTION_IMAGE_CAPTURE` 作为 chooser 的 initial intent 挂到文件选择器上，
两个入口在同一屏。

| 位置 | 改动 |
|---|---|
| `MainActivity.onShowFileChooser` | 改为 `chooserFor(params)`：`params.createIntent()` + `EXTRA_INITIAL_INTENTS`（相机） |
| `MainActivity.cameraCaptureIntent()` | 输出写进 `cacheDir/captures/` 的 FileProvider URI；没有相机/未配置 provider 时退回纯文件选择器 |
| `onActivityResult` | 相机走 `EXTRA_OUTPUT`，结果里不带 URI —— 预览文件存在且非空就回填它 |
| `res/xml/file_paths.xml` | 新增 `<cache-path name="captures" path="captures/" />`（此前只开放了 helper APK 目录） |

- 拍照产物落在 cache，**延迟 60 秒**才删，另有一条"十分钟前的残留"清扫（见下面的 bug）。
- 相机只接受一个输出 URI，所以拍照这条路上多选退化成一张；多选走文件选择器。
- 转出来的图同样是 `content://`，因此和相册选图走完全相同的压缩链路（见本节一）。

**真机实测跑出两个真 bug（都是我们的，不是设备怪癖）**

第一版：点「确定」后回到应用，页面一片空白、没有任何提示。日志给出根因，两层：

```
I om.oplus.camera: open sharedFd failed with Permission denied
W cr_ContentUriUtils: Cannot open content uri: content://com.atomcode.app.files/captures/…jpg
                      java.io.FileNotFoundException: open failed: ENOENT
```

1. **chooser 不把 URI 授权传给 `EXTRA_INITIAL_INTENTS`**。`Intent.createChooser` 上的
   `FLAG_GRANT_*_URI_PERMISSION` 没有到达相机包，相机写不进去。修法：对
   `queryIntentActivities(capture, 0)` 里**每一个**能响应的相机包显式
   `grantUriPermission`（`MATCH_DEFAULT_ONLY` 会漏），chooser 上也带同一组 flag，
   并在清单里声明 `<queries><intent><action IMAGE_CAPTURE>`（Android 11 起不声明根本
   看不见相机）。修完日志变成 `camera capture granted to com.oplus.camera`、
   `file chooser result code=-1 uris=1 captureBytes=2279890`。
2. **把文件删在 WebView 读之前**。`onActivityResult` 里先 `clearPendingCapture()`（删文件）
   再 `callback.onReceiveValue(uris)`，而 WebView 是**异步**读那个 URI 的 —— 文件早已
   不存在，于是 ENOENT、页面报"有图片无法读取"。修法：改成 `releasePendingCapture()`
   （只收回授权与记账）+ `deleteCaptureLater(file)`（60 秒后删），并加一条
   `pruneStaleCaptures`（十分钟前的残留）兜住"用户进相机又返回"的情况。

顺带修掉一个**静默失败**：文件不可用时 `addImageFiles` 直接 `return`，用户看到的只是
"点了没反应"；现在会提示 `attach.unreadable`。

另外备了一条兜底：个别 OEM 相机忽略 `EXTRA_OUTPUT`、把（缩小过的）JPEG 放在
`Intent` 的 `data` extra 里返回；这条也接住了（写回同一份 cache 文件，再交给页面）。

**真机验证（同一台设备，OPPO 相机 `com.oplus.camera`）**

| 检查项 | 结果 |
|---|---|
| 选择器入口 | `uiautomator` 里同时出现 `text="相机"` 与 `text="照片和视频"` |
| 授权 | `camera capture granted to com.oplus.camera` |
| 拍照落盘 | `file chooser result code=-1 uris=1 captureBytes=2279890 extras=false` |
| 页面结果 | 1 张缩略图，`image/jpeg`，base64 **263,931 字符**（约 197 KB），无错误提示 |

#### 五、MCP 示例服务器：从"只有说明"到"能真的调用"

**症状**：上一轮只给了 `add-mcp-server` 技能（说明），`/mcp/status` 仍是空的——
说明和能调用的服务器不是一回事。

**修法**：随包一个**能跑**的最小 MCP 服务器，首启写脚本 + 写 `mcp.json` 条目。

| 部件 | 位置 |
|---|---|
| 服务器 | `crates/atomcode-capabilities/assets/setup-seeds/mcp/example_server.py`（纯标准库、NDJSON JSON-RPC，工具 `now` / `text_stats`，都标 `readOnlyHint`） |
| 播种 | `example_mcp::seed_missing`（`crates/atomcode-capabilities/src/example_mcp.rs`），`host.rs` 首启调用 |
| 配置 | 走 CLI `mcp add` 同一条 `merge_stdio_mcp_server_into_json_file`，`command: python3`，`args: [<product>/mcp/example_server.py]` |

- **只在没有 `mcp.json` 时写配置**：改一份用户维护的文件（还是带注释的 JSONC）不是启动钩子该做的事。
- **只在 `libpython3.so` 在场时写**：条目点名的命令跑不了，比没有条目更糟。缺运行时只写脚本，并记录原因。
- 脚本用 `%` 以外的写法、只 import 标准库，是为了随包的 RustPython 也能跑；stdout 只走协议消息（诊断走 stderr），一个多余的 print 就会毁掉握手。

**真机实测（PLT140 / Android 16，2026-10-30，debug APK 56679312 字节）**

| 检查项 | 结果 |
|---|---|
| 首启播种 | logcat `example skills installed skills=project-check,add-mcp-server`、`example MCP server configured (example-tools)` |
| 落盘 | `files/atomcode/skills/{project-check,add-mcp-server}/SKILL.md` + `.example-seeds`（29 字节台账）；`mcp.json` 一条 `example-tools` |
| `/skills` | `skills:add-mcp-server`、`skills:project-check` 均被索引 |
| 服务器协议（设备内 `python3`） | initialize / tools/list / 两次 `tools/call` 全部按 NDJSON 应答：`now` → `2026-10-30 10:37:31 …`，`text_stats` → `lines: 1 words: 2 characters: 11` |
| `/mcp/status` | `{"name":"example-tools","status":"connected","tool_count":2}` |
| 重启后 | 再次启动记 `example MCP server was not configured reason=…mcp.json already exists`（幂等，不碰既有配置） |
| 模型侧 | 一轮"调用返回当前时间的 MCP 工具"：`last_turn {outcome: ok, stop_reason: stopped, summary: "2026-10-30 10:53:05", tool_calls: 2}` |

#### 附件与通知的真机数据（同一台设备）

| 检查项 | 数据 |
|---|---|
| 大图附件 | 页面内造 2000×1500 24-bit BMP，**9,000,054 字节** → 1 张缩略图，`image/jpeg`，base64 **223,799 字符**（约 168 KB），耗时 **680 ms**，无错误提示 |
| 旧行为对照 | 同一张图在新包之前报"图片不能超过 2M，已自动忽略超限图片"——即 2 MB 硬门槛确实拦掉了手机大图 |
| 带图回合 | 点发送后模型回复 "Abstract wallpaper with broad rainbow and dark arcs sweeping from the lower lef…"（`active_live` 1→0，11.5 s，`outcome: ok`）——图确实到了模型那边 |
| 通知（折叠） | `android.text=空闲 —— 智能体在本机运行 · 上一轮已完成`，`template=BigTextStyle` |
| 通知（展开） | `android.bigText` 第二行为该回合最后一句：`PONG` / `2026-10-30 10:53:05` |

**踩到的坑（值得记）**：第一版实测拿到的是**旧的错误文案**——`npm run build` 没跑，
`.so` 里嵌的仍是旧 `webui/dist`。**改前端后必须先 `cd webui && npm run build` 再
`build-android.sh`**，否则设备上跑的是上一版页面（Kotlin/Rust 都编译通过，现象却像"改动没生效"）。

**仍未做**：通知按会话区分（多会话时 N 条通知/分组），见 `docs/android-next-steps.md`。

#### 六、helper 瘦身 + zig 的 provider APK（真机验证 2026-10-30）

**症状**：helper APK（`com.atomcode.tools`）82 MB，其中 `libzig.so` 解压后 141 MB ——
为一个工具让所有人都背下整条工具链；而它需要的标准库本来就在市场里是独立包
（`zig-lib`，20.4 MB）。

**为什么不能做成「市场数据包」**（上一版文档的建议，实测否定）：在真实 app 进程里
（agent 自己的 `bash` 工具，`untrusted_app` 域）执行一份复制到 `<data>/bin` 的二进制：

```
$ zig-test version
/system/bin/sh: /data/user/0/com.atomcode.app/files/atomcode/bin/zig-test: Permission denied
rc=126
```

数据目录里 `execve` 被拒（同一个文件在 `run-as` 下能跑，那是 `runas_app` 域，是假阳性）。
所以进程模型运行时的唯一去处是**某个 APK 的 `nativeLibraryDir`** —— 也就是 helper 那套
机制的推广：一个包只当「文件挂架」，主应用在 `<product>/bin` 建链接。

**修法**：zig 搬进自己的 provider APK，helper 只留 `make`/`tcc`/`atomcode`。

| 位置 | 改动 |
|---|---|
| `android-zig/` | **新增**：`com.atomcode.tools.zig`，只带 `libzig.so`，无 Activity/服务 |
| `shims::PROVIDER_TOOLS` | `("zig","libzig.so")`，在**所有** provider 目录里查找 |
| `shims::link_helper_tools(&[PathBuf], …)` | 逐个 provider 找库；找不到就摘链接（卸载 provider 不留死链） |
| `HostOptions::helper_lib_dirs` | 单目录 → 列表；JNI 用 `
` 分隔的一个字符串传（签名不随 provider 数量增长） |
| `AgentService.HELPER_PACKAGES` + 清单 `<queries>` | 两个包都要声明，否则 Android 11+ 查不到 |
| `packages.rs::HELPER_COMMANDS` | 同步去掉 zig（wire contract） |
| `scripts/build-android-tools.sh` | 顺手删 jniLibs 里遗留的 `libzig.so` |

**真机结果**

```
I AtomCode: helperLibraryDirs: …/com.atomcode.tools-…/lib/arm64
I atomcode_android::host: helper APK tools are available tools=make,cc,tcc,atomcode,zig
$ ls -l files/atomcode/bin/make  → …/com.atomcode.tools-…/lib/arm64/libmake.so
$ ls -l files/atomcode/bin/zig   → …/com.atomcode.tools.zig-…/lib/arm64/libzig.so
$ zig version                    → 0.13.0        （真实 app 进程）
$ zig run hello.zig              → hello zig 42   （标准库走 $ZIG_LIB_DIR）
```

helper APK：**82 MB → 33 MB**（剩下的主体是 `libatomcode.so`，即设备上的
`atomcode daemon` CLI，见 `docs/external-daemon.md`）。

**面板安装已实现**（见下一节「八」）：「运行时插件」页按市场清单逐行列出 provider，各带
自己的安装按钮与已安装判定。

**仍未做（仓库外）**：把 `com.atomcode.tools.zig` 与重新瘦身后的 `com.atomcode.tools`
**发布到市场仓库**（清单里的两条 `[[helpers]]` + 两个 APK）。发布之前，设备上只能靠
`adb install` 或本地市场夹具。

#### 七、二进制 MCP 服务器：pluginhost 插件（真机验证 2026-10-30）

**症状**：随包的 MCP 示例是 Python 脚本（靠内置 `python3`）。真要做「MCPbin」，得回答
一个问题：**二进制 MCP 服务器在 Android 上怎么活**？

**答案**：MCP 传输本来就是「拉起一个进程、说 stdio」——而这台设备只允许执行 APK 的
`nativeLibraryDir`，同时**允许 `dlopen` 数据目录里的 `.so`**（P5.7 实测）。所以把 MCP
服务器编成 pluginhost 的插件：`pluginhost`（唯一需要可执行的部分，在 APK 里）`dlopen`
它，插件的 stdin/stdout 就是协议管道，不需要 fork/exec 任何东西。

| 部件 | 位置 |
|---|---|
| 服务器源码 | `crates/atomcode-capabilities/assets/setup-seeds/mcp/example_server.c`（纯 C、无依赖；NDJSON JSON-RPC，工具 `now` / `repeat`） |
| 构建 | `scripts/build-android-mcp-plugin.sh`（Windows/Linux 两种 NDK 布局都支持）→ `dist/mcp-example-1.0.0.tar.gz` |
| 包清单 | `{name: mcp-example, entry: lib/libmcp_example.so, symbol: plugin_main, aliases:[mcp-example]}` |
| 配置 | `mcp.json` 里 `command` 指向别名 `<product>/bin/mcp-example`（安装时自动建，指向 `pluginhost`） |

**真机实测**

```
POST /packages/install {"path":"/sdcard/Download/mcp-example-1.0.0.tar.gz","sha256":"99a2…"}
→ {"package":{"name":"mcp-example","commands":["mcp-example"],"entry":"lib/libmcp_example.so",…},
   "success":true}

$ mcp-example <<< '<initialize>…<tools/list>…<tools/call now>'   # 直接测插件链
→ 初始化/工具列表/时间 三帧 NDJSON 正常，rc=0

GET /mcp/status
→ {"servers":[{"name":"example-bin","status":"connected","tool_count":2},
              {"name":"example-tools","status":"connected","tool_count":2}]}

模型侧一轮：mcp__example-bin__now → "2026-10-30 13:…"（tool_calls 计入）
```

**踩到的两个坑**

1. `adb install -r` 报 `Failure [-99]`，但 `adb shell pm install -t /data/local/tmp/x.apk`
   成功——APK 没问题，是 streaming 安装路径的问题；装大 APK 卡住时先换 `pm install`。
2. 改完 `mcp.json` 后 `/mcp/status` 一直是 `connecting`：MCP 服务器是在 **live runtime
   装配时**连接的，`/config/reload` 只重载 provider 配置。要么切会话、要么重启应用。

#### 八、市场 provider 列表与面板（真机验证 2026-10-30）

上一轮把「多个 provider APK」的市场清单、按名下载、面板逐行安装做完了，这一轮在 PLT140 上
逐条验证。

**夹具**：本地市场推到设备 `/sdcard/Download/atomcode-market/`（`market.json` +
`apk/tools.apk` 33.5 MB + `apk/zig.apk` 53.3 MB），基址用
`POST /packages/market/config {"base": "/sdcard/Download/atomcode-market"}` 写入。

**1. 清单返回列表**（`GET /packages/market?refresh=1`）

```
helpers:
  tools com.atomcode.tools     1.2.0 33534300 path=/sdcard/Download/atomcode-market/apk/tools.apk
  zig   com.atomcode.tools.zig 0.13.0 53306460 path=/sdcard/Download/atomcode-market/apk/zig.apk
legacy helper = tools
```

**2. 按名下载 + 哈希**（`GET /packages/market/helper?name=…`）

| 请求 | 结果 |
|---|---|
| `?name=zig` | `{"name":"zig","package":"com.atomcode.tools.zig","path":"…/helper/zig.apk","size_bytes":53306460}` |
| `?name=tools` | `{"name":"tools","package":"com.atomcode.tools","path":"…/helper/tools.apk","size_bytes":33534300}` |
| 不带 `name` | 返回 `tools`（老客户端的行为） |
| `?name=nope` | `{"success":false,"error":"the market does not publish a provider called `nope`"}` |

落盘文件与市场发布的 `sha256` 一致：

```
$ sha256sum files/atomcode/helper/*.apk
2bdee48825243edc2251a666e3a7c920519438d063a2373d99dba1ab339c3251  …/helper/zig.apk
d7b8fc5636bbc83a6c280ce2a2401c92317b203693f6e153f1fe7792ef10206f  …/helper/tools.apk
```

**3. 宿主桥**（CDP `Runtime.evaluate`）

```
window.AtomCodeHost.installedProviders()
→ ["com.atomcode.tools","com.atomcode.tools.zig"]
```

**4. 面板逐行渲染**（`.pkg-provider-row` 的真实 DOM 文本）

| 场景 | 面板显示 | 按钮 |
|---|---|---|
| 两个 provider 都装着 | `Build tools 1.2.0 | 已安装`、`zig 0.13.0 | 已安装` | 无（已安装不显示按钮） |
| `pm uninstall com.atomcode.tools.zig` 后重载面板 | `Build tools 1.2.0 | 已安装`、`zig 0.13.0 | 未安装 | 安装 zig` | 恰好一个：`安装 zig` |

**边界（这次没走完的一段）**：「安装 zig」按钮调 `installHelperApk(path)` →
`ACTION_INSTALL_PACKAGE`，**系统安装器界面需要人点确认**，自动化只能停在「按钮已出现」。

测试里为了验证「未安装」这一支卸掉了 `com.atomcode.tools.zig`，随后所有安装路径
（`pm install` / `adb install` / `adb install --no-streaming`）都长时间不返回。根因不是
Play Protect，也不是我们的代码：

```
W ContextImpl: Failed to ensure /data/user/0/com.android.packageinstaller/files:
               mkdir failed: errno 126 (Required key not available)
I PackageInstaller: SharedPreferences in credential encrypted storage are not available
                    until after user (id 0) is unlocked
$ dumpsys user  → State: RUNNING_LOCKED
$ dumpsys deviceidle → mScreenLocked=true
```

**重启后设备停在锁屏，`/data/user/0` 的凭证加密存储没解密，安装器一直在等**。解锁屏幕后
安装即恢复（或直接在面板里点「安装 zig」，那本来也需要人点）。排查同类问题时先看
`dumpsys user` 里的 `RUNNING_LOCKED`，别在安装器和 adb 上找原因。

**回退路径（同次真机确认）**：把 `[market] base` 还原为空后，`/packages/market` 返回
`market not configured`，面板的 `providers` 为空 → 不渲染 provider 行，回到
「辅助程序已安装：提供 make, cc, tcc, atomcode」这行聚合提示。这是设计内的降级（没有市场
就没人能说清有哪些 provider），不是渲染故障。

**顺带一个日志陷阱**：`helperLibraryDirs` 用 `
` 把多个目录拼进一条日志，logcat 里会
显示成两行，`grep | head -3` 只看到第一行会误判成「只认到一个 provider」。

## 6. 路线

| 阶段 | 内容 |
|---|---|
| ~~P1~~ | ~~真机跑通：WebView + 内嵌 webui + 本地 daemon~~ **已完成**（页面/资源/health/cookie 均实测） |
| ~~P2~~ | ~~打开真实目录~~ **已完成**（`MANAGE_EXTERNAL_STORAGE` + `/fs/list` + `/cd` 实测） |
| ~~P2.5~~ | ~~登录链路~~ **已完成**（`/auth/login/start` → `poll` → 取消 全链路实测；webui 自带登录入口，无需 `/login` 命令） |
| ~~P3~~ | ~~provider 打通~~ **已完成**（官方 overlay 接入 + 真机真实模型回复 + 工具调用 + 写文件 + bash 全通） |
| ~~P4~~ | ~~Git 互通~~ **已完成**：打包 git 2.47.1 进 `jniLibs`，clone / commit / push 真机实测通过；token 经 `POST /git/credentials` 落进 `~/.git-credentials`，git 自己的 `store` helper 实测读得到；webui Git 面板已实现（`git_ops.rs` 八个路由 + `SourceControl.tsx`），真机复核待做 |
| P5 | 审批 UI 的移动端适配（webui 已有 `PermissionCard`，需在窄屏复核）；**窄屏排版已修**（顶部安全区、下拉框溢出、composer 控件重排，见上面 P5 小节） |
| ~~P6~~ | ~~后台长任务的通知与进度~~ **大部分已完成**：回合开始/结束驱动唤醒锁与通知，结束时报告最后一轮的成败（见 P5.11）；多会话区分仍未做 |
| P7 | 量内存与 Doze 下的存活；长回合的压力测试 |
| P8 | 若要上架：SAF + `content://` 的 world 实现，替换全盘访问 |

## 7. 边界

- 不新增 runtime owner：`run_server` 仍是唯一入口，`CodingRuntime` 仍由 daemon 持有。
- 不把 Android 语义下沉到 kernel；平台差异只在 host 层与 `world` 的实现里。
- 不引入第二套前端；webui 的改动通过重建 `.so` 生效。
## 8. 验证来源：真机 vs 模拟器

模拟器不是"缩小版的真机"，在这个工程里它连装都装不上，这条边界必须写死，否则以后有人
拿模拟器结果填「真机验证」小节。

### 现状（2026-10-30 实测）

| 项 | 值 |
|---|---|
| 模拟器 | Android Studio 的 `emulator.exe` 可用；`VirtualizationFirmwareEnabled=True`、WHPX `Enabled` |
| 唯一系统镜像 | `system-images/android-37.0/google_apis_playstore/**x86_64**`（AVD `Medium_Phone` 的 `abi.type=x86_64`） |
| 本工程产物 | `lib/` 里只有 **arm64-v8a**（`build-android.sh` 只交叉编译 aarch64） |

结果：`adb install` 直接 `INSTALL_FAILED_NO_MATCHING_ABIS`。换 arm64 镜像这条路也堵死——
x86_64 主机上 `sdkmanager` 不提供 arm64 系统镜像（那是 Apple Silicon 主机才有的）。

### 即便补上 x86_64 变体，能验证什么

| 模拟器可以 | 必须真机（PLT140） |
|---|---|
| 应用安装/启动、WebView 加载、面板布局与窄屏 | **zig**：没有 x86_64 Android 构建，arm64 APK 装不上 |
| 包生命周期（装/卸载/回滚）、`/packages/*` 契约 | **执行语义**：`execve` 数据目录的 126 是 `untrusted_app` 的 SELinux 结论，模拟器是 userdebug + 可 `adb root`，在 root/shell 域能 exec，测出来是假答案（与 `run-as` 假阳性同源，见 P5.7） |
| MCP 插件握手（`mcp-example` 重编 x86_64 即可） | **相机**：OPPO 相机包与 FileProvider 授权行为 |
| 前端桥（`installedProviders`）、装完重启重链 | **通知外观**：InboxStyle 在 ColorOS 上的实际呈现 |
|  | **性能/Doze/内存**、真实存储与权限弹窗 |

### 规则

1. 本文件所有「真机验证」小节只接受**真实设备**数据；模拟器结果如出现，必须标注
   「模拟器（x86_64）」并单独成段，不得混进真机记录。
2. 关于「能不能执行某个二进制」「SELinux 允不允许」的结论，**只能在 app 进程里测**
   （用 agent 自己的 `bash` 工具），既不能用 `run-as`，也不能用模拟器的 root/shell。
3. 要真用模拟器做回归，需要补：Rust target `x86_64-linux-android`、NDK 的 x86_64 clang、
   gradle `abiFilters`、`pluginhost` 与 MCP 插件重编；`make`/`tcc`/`git` 还要各自的
   x86_64 产物，zig 则永远只能在真机上验证。
