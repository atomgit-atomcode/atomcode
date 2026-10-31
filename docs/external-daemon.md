# 外部守护进程（连接设备上的 `atomcode daemon`）

App 自带一个进程内 daemon（`NativeAgent`）。这个功能让它也能连上**在设备上另起的
`atomcode daemon`**——比如你在 `adb shell` 里跑一个，用来在电脑上调试、或让 CLI 与界面
共用同一个运行时。

## 为什么发现是「间接」的

Android 不允许应用枚举其它 uid 的进程（`/proc/<pid>` 读不到，`ps` / `ActivityManager`
也只给自己的进程）。所以「自动发现 atc 进程」不能靠进程列表，只能靠两条线索：

| 线索 | 给出什么 | 需要外部 daemon 做什么 |
|---|---|---|
| 汇合文件 `daemon-<port>.json`（`{pid, port, token}`，daemon 自己写，0600） | 端口 + token | 把 `ATOMCODE_HOME` 指到一个双方都读得到的目录 |
| `GET /health`（免鉴权）里的 `"service":"atomcode-daemon"` | 它确实在跑、而且是我们 | 什么都不用做（daemon 自带） |

两者必须合用：只认文件会把崩溃残留当成活的；只探端口拿不到 token（除非 `--no-auth`）。

## 怎么起一个外部 daemon

```sh
# 在 adb shell 里（shell uid 只能写共享存储）
ATOMCODE_HOME=/storage/emulated/0/AtomCode atomcode daemon --port 8787
```

`daemon` 只有 `--port`（默认 13456），**没有 `--host`** —— 它固定绑 loopback。

`atomcode` 这个命令来自 **helper APK**（`com.atomcode.tools`）：Android 只允许执行某个包的
`nativeLibraryDir` 里的二进制，所以 CLI 必须住在那儿。helper 装好后，App 会把
`atomcode` 链进 `<data>/bin`，于是：

```sh
# App 私有目录里的那份产品目录
run-as com.atomcode.app sh -c 'cd files/atomcode && ./bin/atomcode daemon --port 8787'
```

> **发现依赖汇合文件，而它只在带 token 的模式下写**。daemon 启动时把
> `{pid, port, token}` 写进 `$ATOMCODE_HOME/daemon-<port>.json`（0600）；`--no-auth`
> 没有 token，所以**不写这个文件**——App 拿不到端口线索，也就发现不了它。
> （真机实测：`--no-auth` 启动后目录里只有配置与日志，没有 `daemon-8787.json`，
> App 侧表现为根本不去探测；带 token 重启后立刻 `external daemon found on port 8787`。）

App 会扫描这两处：

- `/storage/emulated/0/AtomCode`（共享工作目录）与它下面的 `.daemon/`；
- App 私有产品目录 `<files>/atomcode` 与它下面的 `.daemon/` —— `run-as` 起的 daemon 只能写这里。

> 早期版本用 `scripts/stub-daemon.py` 做端到端验证（那时 CLI 还编不到 Android 上）。
> 现在 CLI 已经能交叉编译（`arboard` 在 TUI 侧有 Android 门槛，`crates/atomcode-tui/Cargo.toml`
> 用 `cfg(not(target_os = "android"))` 把它挡在外面），端到端验证可以直接用真的 CLI。

## 行为

| 场景 | 行为 |
|---|---|
| 启动时发现（端口与上次不同） | 弹一次「External daemon found / 端口 N，切换过去？本机 agent 会被停止」 |
| 上一个就是它（端口相同且还活着） | 不再询问，直接用 |
| 选了「Keep this one」 | 记住这个端口，不再打扰（端口变了会重新问） |
| 切换 | 停掉进程内 daemon（避免两份运行时抢同一份会话日志、白耗电），WebView 指到外部 URL，Toast 提示 |
| 外部消失（回前台时 `/health` 不通） | 清掉持久化记录，重起进程内 daemon，Toast 说明已切回 |
| token | 只放在内存里的 URL，不落 SharedPreferences（它会随 daemon 重启而变） |

## 会话目录与配置

App 启动时把**会话目录**钉到共享工作目录：`/storage/emulated/0/AtomCode/.sessions`
（宿主日志：`sessions live in the shared workspace`）。机制是 capability 层的一个覆盖：

```rust
atomcode_capabilities::session::set_sessions_root(path)   // 进程内一次，先设先得
```

外部 daemon 用同一个旋钮就能读到同一份历史：

```sh
ATOMCODE_SESSIONS_DIR=/storage/emulated/0/AtomCode/.sessions   atomcode daemon --port 8787
```

`config.toml` 与其中的密钥**留在 App 私有目录**——搬走的只有会话日志。代价是会话内容落在
共享存储上，任何有存储权限的应用都能读；不能接受就设 `ATOMCODE_SESSIONS_DIR` 指回私有
目录（那样两边历史又各一份）。

## token 的来源边界

| 目录 | 能提供什么 | 理由 |
|---|---|---|
| App 私有产品目录 `<files>/atomcode`（及 `.daemon/`） | 端口 + token | 别的应用读不到，可以当凭据 |
| 共享工作目录 `/storage/emulated/0/AtomCode`（及 `.daemon/`） | **只有端口** | 谁都能读；把它当凭据等于把 token 交出去 |

共享目录里发现一个要 token 的 daemon 时，App 不自动连，而是弹「This daemon needs a token」：
可以手输 token，或者让 daemon 以 `--no-auth` 重起。`--no-auth` 的 daemon 会被自动认出来
（首页 200 即免鉴权），无需任何 token。

## 前台服务

切到外部后**进程内 daemon 停掉，前台服务留着**：通知、唤醒锁和忙碌统计继续工作，通知文案变成
「Connected to the external daemon on port N」；外部掉线时变成「…stopped answering」（但不会
替你重启外部进程——回落由界面回前台时决定）。这样既没有两份运行时抢会话日志，也不会因为
服务被停而让系统更容易冻结这个应用。

## 已知边界

- **会话在共享存储上**：见上，可以用 `ATOMCODE_SESSIONS_DIR` 改回私有。
- **外部 daemon 的 busy 统计**来自它的 `/health`；`/health` 答 200 但缺 `active_chats`
  时按“空闲”算（早期实现会误判成“掉线”，已修）。
- **CLI 需要 helper APK**：CLI 已经在 helper 的 `nativeLibraryDir` 里（`libatomcode.so`，
  由 `scripts/build-android.sh` 交叉编译并 strip），但没装 helper 就没有这个命令。
  下面的验证记录是**用桩**跑的（那时 CLI 还编不到 Android 上）；换成真 CLI 的端到端
  验证尚未在真机上做。

## 验证记录（2026-10-28，真机）

| 项 | 结果 |
|---|---|
| 发现 | 桩写 `daemon-8787.json` 后启动 App，日志 `external daemon found on port 8787`，界面弹「External daemon found … port 8787」 |
| 切换 | 点「SWITCH TO IT」后 WebView 加载桩页面；`/proc/net/tcp` 里只剩 `8788`（本机 daemon 已停） |
| 记忆 | 换端口 8788 再启动 → 重新询问（记住的是 8787）；同端口再启动 → 静默直连 |
| 回落 | 杀掉桩并回前台，出现 `agent server listening at …:35445`（本机 daemon 重起），`external_port` 持久化被清空 |
| 失败可辨 | 汇合文件在、端口没人答时日志明确写 `points at 8787 but nothing answers`，而不是误判为可用 |
| 会话共享 | 宿主日志 `sessions live in the shared workspace dir=/storage/emulated/0/AtomCode/.sessions`，目录确实被创建 |
| 外部模式前台服务 | 切换后 `dumpsys notification` 里 AtomCode 的通知仍在（`FOREGROUND_SERVICE`），正文「Connected to the external daemon on port 8787」；`/proc/net/tcp` 只剩 8787 |
| token 边界 | 共享目录放 `daemon-8787.json` + 要令牌的桩：日志 `needs a token; not using daemon-8787.json`，界面弹「This daemon needs a token」；换成 `--no-auth` 桩则日志 `accepts unauthenticated access` 并正常切换 |
