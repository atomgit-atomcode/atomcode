# Android 后续可做项

本文件记录**尚未实现**的功能设想与环境扩展，供后续排期。已完成的事不在这里
重复（见 `docs/android-port.md` 与提交历史）。

写下来的目的：这些判断来自一轮真机核查（PLAT140 / Android 16，2026-10-29 / 10-30），
每一条都有具体的落点或阻塞原因，避免下次重新摸索。

---

## 一、功能：待做

### 1. 真正有用的 MCP 集成包（仓库内部分已完成，发布未做）

**现状**：MCP 的**交付形态**已经跑通两种，都是真机验证过的：

- `example-tools`：Python stdio 服务器（随包脚本 + `python3`），`connected, tool_count 2`；
- `mcp-example`：**二进制**版，纯 C 编译成 pluginhost 插件（11 KB bundle），
  `connected, tool_count 2`，模型调用 `mcp__example-bin__now` 拿到时间。

所以「MCP 服务器怎么在手机上跑」这个问题已经有答案（插件路线，`scripts/build-android-mcp-plugin.sh`
可复现）。

**已补的真实集成**：`mcp-filesystem`（`scripts/mcp-filesystem/plugin.c`）把「看文件」这件事
补上了，这是手机上没有 shell 时最缺的能力。只读工具 `list_dir` / `read_file` / `stat`，
根由 `ATOMCODE_MCP_FS_ROOT` 指定，参数解析到 realpath 后必须落在根内（`..` 和符号链接都
不能越界），二进制文件拒绝回显而不是当文本丢给模型。

- 打包：`scripts/build-android-mcp-filesystem.sh`（NDK aarch64，产物约 6.8 KB）；
- 协议自测：`bash scripts/mcp-filesystem/selftest.sh`，同一份源码用主机编译器跑
  initialize / tools/list / tools/call 与两条失败路径（越界、二进制），不需要真机；
- 真机安装路径与 `mcp-example` 相同（`POST /packages/install`，再让 `mcp.json` 指向别名）。

**仍未做（不在本仓库内）**：把 `mcp-filesystem` 与 `com.atomcode.tools.zig` 一样发布到
市场仓库并加目录条目——仓库推送权限还在等。产物已经可编、可自测，发布是纯搬运。

**落点**：`scripts/mcp-filesystem/`、`scripts/build-android-mcp-filesystem.sh`、
`docs/plugin-market.md`。


### 2. 包的版本更新提示（已实现，此处仅备注逻辑）

市场卡片的"可升级 → x.y.z"与一键升级**已经实现**：`webui/src/lib/marketUpdates.ts`
的 `compareVersions` / `upgradeFrom`（4 个测试覆盖），接在
`PackageMarket.tsx` 的 `upgrade` 判断与 `market.upgrade` 按钮上。版本号格式不统一
（`3.12.10` / `2025-09-13` / `1.0.0`），比较器按 `.` 与 `-` 切段、数字段按数值比。

无需再做；如果要扩展，方向是"自动检查 + 角标"，而不是重写比较逻辑。

---

## 二、环境：可加的运行时

### 先看一条实测硬约束（决定了所有交付形态）

2026-10-30 在真实 app 进程里（agent 自己的 `bash` 工具，`untrusted_app` 域）执行一份
复制到 `<data>/bin` 的二进制：

```
$ zig-test version
/system/bin/sh: /data/user/0/com.atomcode.app/files/atomcode/bin/zig-test: Permission denied
rc=126
```

**数据目录里的可执行文件跑不了**（同一份二进制在 `run-as` 下能跑，那是 `runas_app` 域，
是假阳性——`docs/android-port.md` §P5.7 记过这个坑）。所以「把运行时打成市场数据包、
里面放一个二进制」这条路对所有**进程模型**运行时都是死的。

只剩两条交付路线：

| 路线 | 机制 | 适合 | 不适合 |
|---|---|---|---|
| **A. provider APK** | 二进制放进某个 APK 的 `nativeLibraryDir`（平台允许执行别的包的 lib 目录），主应用在 `<product>/bin` 建链接 | 任何要 `exec` 自己或子进程的程序：zig、Go、clang、cmake、ninja、完整 CPython | 包体大（每个 APK 一个运行时） |
| **B. pluginhost 插件** | `.so` + `int plugin_main(argc, argv)`，`dlopen` 进 pluginhost（数据目录允许 `dlopen`），stdout/stdin 直接是协议管道 | 能被嵌进一个进程的东西：CPython/Lua/SQLite/quickjs/node(libnode)、**以及 MCP 服务器** | 需要 fork/exec 的工具链 |

### 现状（真机验证 2026-10-30）

| 运行时 | 形态 | 状态 |
|---|---|---|
| **zig 0.13.0** | **A**：独立 APK `com.atomcode.tools.zig`（`libzig.so` 141 MB），标准库仍是市场包 `zig-lib`（`$ZIG_LIB_DIR`） | **✓ 已实现并实测**：`bin/zig` 指向该 APK；`zig version` → `0.13.0`；`zig run hello.zig` → `hello zig 42` |
| make / tcc | A：`com.atomcode.tools` helper | ✓ 既有，helper 现在 33 MB（去掉 zig 前是 82 MB） |
| python3 | B（内置 RustPython） + 市场 CPython 插件 | ✓ 既有 |
| lua / sqlite3 / quickjs / node 18 | B（市场插件包） | ✓ 既有 |
| **MCP 服务器（二进制）** | **B**：`mcp-example` 插件包（纯 C，11 KB） | **✓ 已实现并实测**：`/mcp/status` `connected, tool_count 2`；模型调用 `mcp__example-bin__now` 拿到时间 |
| Go | A（`CGO_ENABLED=0 GOOS=linux GOARCH=arm64`，静态） | **未做**：本机没有 Go 工具链，且产物必须进 APK（A 路线） |
| node 20/22 | B（需要 libnode 的交叉构建） | **未做**：18.20.4 已 EOL，升级要重编 libnode（Linux + NDK） |
| clang + lld | A（NDK 的 clang/lld + sysroot 放进 provider APK；`clang` 的 `-cc1` 在同进程，`ld.lld` 同目录可 exec） | **未做**：产物数百 MB，要 Linux 侧的打包流程 |
| cmake / ninja / pkg-config | A | **未做**：同上（cmake 还会 exec 编译器，必须在 A 路线） |

### 结论

- 想在手机上补运行时，先决定它属于 A 还是 B：**能嵌进程的走 B（小、快、可自动装）**，
  其余只能走 A（一个 APK 一个运行时）。
- 纯数据包（stdlib、wheel、字库、模型）不受这条约束影响，A/B 之外随时可装。
- 上一版文档里「把 zig 做成市场数据包（`links: {"zig": "bin"}`）」是**错的**，已按实测改成
  provider APK；`bin` 字段没有引入，因为它在 Android 上只会产出 126。

## 三、包体体积：helper 从 82 MB 降到 33 MB（已完成）

**原来的问题**：`android-tools`（helper）的 APK 是 **82 MB**，其中 `libzig.so` 解压后
141 MB，占了绝对多数；而 zig 的标准库（`zig-lib`，20.4 MB）在市场上本来就是独立数据包。

**做法**：zig 从 helper 里搬进**自己的 provider APK** `com.atomcode.tools.zig`
（`android-zig/` 工程，`libzig.so` 一个文件），主应用把 `<product>/bin/zig` 链接到它的
`nativeLibraryDir`。

| 位置 | 改动 |
|---|---|
| `android-zig/` | **新增**：只有 `jniLibs/arm64-v8a/libzig.so` 的 provider APK（无 Activity/服务） |
| `shims::PROVIDER_TOOLS` | `("zig", "libzig.so")`：在**所有** provider 目录里查找 |
| `shims::link_helper_tools` | 接收 `&[PathBuf]`，逐个 provider 找库；找不到就摘链接（卸载 provider 不留死链） |
| `HostOptions::helper_lib_dirs` | 单个目录 → 列表；JNI 用一个 `
` 分隔的字符串传（签名不再随 provider 数量增长） |
| `AgentService.HELPER_PACKAGES` | `["com.atomcode.tools", "com.atomcode.tools.zig"]`；清单 `<queries>` 两个都要声明 |
| `packages.rs::HELPER_COMMANDS` | 同步去掉 zig（wire contract，daemon 不依赖 android crate） |
| `scripts/build-android-tools.sh` | 顺手删掉 jniLibs 里遗留的 `libzig.so`，免得旧的树把它打回包里 |

**真机结果（2026-10-30）**

```
I AtomCode: helperLibraryDirs: …/com.atomcode.tools-…/lib/arm64
I atomcode_android::host: helper APK tools are available tools=make,cc,tcc,atomcode,zig
$ ls -l files/atomcode/bin/make  → …/com.atomcode.tools-…/lib/arm64/libmake.so
$ ls -l files/atomcode/bin/zig   → …/com.atomcode.tools.zig-…/lib/arm64/libzig.so   ← 新 provider
$ zig version                    → 0.13.0        （在真实 app 进程里）
$ zig run hello.zig              → hello zig 42   （标准库走 $ZIG_LIB_DIR）
```

helper APK：**82 MB → 33 MB**（剩下的主体是 `libatomcode.so`，即设备上的
`atomcode daemon` CLI，见 `docs/external-daemon.md`）。

**仍未做**：把 `com.atomcode.tools.zig` 这个 APK **发布到市场**（清单里一个 `helper`
条目），并在插件页加第二个「下载并安装」。目前它靠 `adb install` 装，代码侧全部就位。

**注意**：`shims::HELPER_TOOLS` / `PROVIDER_TOOLS` 与 `packages.rs::HELPER_COMMANDS`
是必须同步的清单。

---

## 四、已知边界（不是缺陷，但要知道）

- **gateway 鉴权默认开启**：`scripts/build-android.sh` 的 `ATOMCODE_ANDROID_CRYPTO`
  默认 `1`。关掉它（`ATOMCODE_ANDROID_CRYPTO=0`）会得到一个"看起来已登录、但每个
  回合都被网关拒绝"的包 —— 只在明确需要无签名构建时才关。
- **会话目录**：默认在共享存储 `/storage/emulated/0/AtomCode/.sessions`，任何有存储
  权限的应用都能读。插件页的"会话改为应用私有"可以切到私有目录，**重启后生效**
  （宿主在进程启动时钉死会话根）。
- **外部 daemon**：应用内可以一键启动（插件页），但它是一个**独立进程**，退出应用
  不会带走它；要停用同一个入口的"停止外部守护进程"。
- **示例资产是"种子"不是"资产"**：两个示例技能只在缺失时写入，删掉或改过之后
  更新应用也不会覆盖/复活（`example_skills::seed_missing`，台账
  `<data>/skills/.example-seeds`）。MCP 示例的 `mcp.json` 条目**只在文件不存在时**写，
  且只在 APK 里真有 `libpython3.so` 时写——既不改用户维护的配置，也不留一条跑不起来的
  条目（`example_mcp::seed_missing`）。
- **`Cargo.lock` 与私有 overlay**：本机存在私有实现版 overlay 时，构建会把
  `hkdf`/`hmac`/`zeroize_derive` 写进 lock。提交前必须确认这三项计数为 0
  （`scripts/` 下没有自动化门，靠 `git ls-files -v Cargo.lock` 与人工核对）。
