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
| **Go 1.27.3** | **A**：独立 APK `com.atomcode.tools.go`（`libgo.so` + 八个 `lib<tool>.so`），GOROOT 是数据包 `go-data`（127 MB，`src`/`pkg/include`/`VERSION`） | **✓ 已实现，编译通过**：`go version` → `go1.27.3 android/arm64`，`go build` 成功；**产物不能直接执行**——共享存储与数据目录都 noexec（exit 126），这是平台限制，不是包的问题 |
| **cmake 4.4.5** | **A**：独立 APK `com.atomcode.tools.cmake`（`libcmake.so`/`libctest.so`/`libcpack.so`），模块是数据包 `cmake-data` | **✓ 已实现并实测**：`cmake --version` → `4.4.5`，模块可被找到（见下方「三个坑」）；配置工程还要指定编译器，本机 clang provider 未装 |
| **ninja 1.13.2** | **A**：独立 APK `com.atomcode.tools.ninja`（`libninja.so`，648 KB） | **✓ 已实现并实测**：`ninja --version` → `1.13.2` |
| pkg-config | A | **未做**：同样需要 exec 自己，走 A 路线 |
| **node 24 LTS** | **A**：独立 APK `com.atomcode.tools.node`（Termux `nodejs-lts` 24.22.1，`libnode.so` 45 MB）；npm 树与头文件是市场包 `node-lib`，npm 用 `node <prefix>/lib/node_modules/npm/bin/npm-cli.js` 调（数据目录里的 shebang 脚本不可执行） | **已打包，未上真机**（APK 19 MB + 5.1 MB 数据包）。取代 `nodejs-mobile` 18 插件：它是最后能用的 libnode 版本（GitHub 不可达，npm 上仍指 18.20.4） |
| **clang 21 + lld 21** | **A**：独立 APK `com.atomcode.tools.clang`（Termux LLVM 21.1.8）；头文件、sysroot、resource dir 是市场包 `clang-sysroot`；`clang` 是启动器（`scripts/termux/launcher/clang.c`），注入 `-resource-dir`/`--sysroot` 后 exec 真 driver | **已打包，未上真机**（APK 114 MB + 30 MB 数据包）。NDK 的 clang 是宿主二进制，不能上设备；产物由 `scripts/build-android-clang-provider.sh` 从 Termux 包生成 |

### 结论

- 想在手机上补运行时，先决定它属于 A 还是 B：**能嵌进程的走 B（小、快、可自动装）**，
  其余只能走 A（一个 APK 一个运行时）。
- 纯数据包（stdlib、wheel、字库、模型）不受这条约束影响，A/B 之外随时可装。
- 上一版文档里「把 zig 做成市场数据包（`links: {"zig": "bin"}`）」是**错的**，已按实测改成
  provider APK；`bin` 字段没有引入，因为它在 Android 上只会产出 126。

### 六个坑（provider 打包时真机踩出来的）

1. **版本化 soname 会被 Gradle 静默丢掉。** Gradle 只从 `jniLibs` 打包 `lib*.so`，
   `libexpat.so.1`、`librhash.so.1`、`libzstd.so.1`、`libcrypto.so.3` 这类名字匹配不上——
   文件明明躺在 `jniLibs` 里，APK 里却没有，运行时报
   `CANNOT LINK EXECUTABLE …: library "librhash.so.1" not found`（clang 甚至因此起不来：
   `libLLVM.so` 需要 `libzstd.so.1`）。`scripts/termux/split.sh` 把这类库分流到数据包的
   `libs/`，宿主把 `<product>/libs` 与 `<product>/clang/libs` 加进 `LD_LIBRARY_PATH`
   （从 app 数据目录 dlopen 允许，exec 不允许）。
2. **cmake 的根目录不看 `CMAKE_ROOT` 环境变量。** 它从 `argv[0]` 推出
   `<dir>/../share/cmake-X.Y`；命令是 `<product>/bin/cmake`，所以它找的是
   `<product>/share/cmake-X.Y`。往环境里设 `CMAKE_ROOT` 无效（实测仍然
   `CMake Error: Could not find CMAKE_ROOT`），必须由宿主建这个软链接。
3. **Termux 把 `$PREFIX/bin/sh` 编进了 cmake 的 `CMAKE_SH`。** 生成的 Makefile 里
   `SHELL=` 就是这个不存在的路径，`make` 每条配方都以 127 失败（`-DCMAKE_SH` 覆盖不掉）。
   用 `scripts/termux/patch-terminux-sh.py` 做**等长**字节替换改成 `/bin/sh`（等长才不会
   破坏 ELF 偏移）。
4. **cmake 还读 `$ENV{PREFIX}`。** 它的 Android 探测打开
   `$ENV{PREFIX}/include/android/api-level.h`，`PREFIX` 为空时就是 `/include/...`，
   于是整个 configure 失败。宿主设 `PREFIX=<product>/clang` 并建 `include → usr/include`
   软链即可。
5. **工具链缺的不是编译器，是它旁边那几个名字。** clang 链接要 `ld`（否则 cgo/链接阶段
   `unable to execute command: No such file or directory`）、cmake 要 `ar`（否则
   `CMAKE_AR-NOTFOUND`）、静态库要 `ranlib`，C++/cgo 链接还要 `libunwind.a`。前三个由
   宿主从 `liblld.so`/`llvm-ar`/`llvm-ranlib` 供给；`libunwind.a` 是静态库，随数据包走，
   由启动器在**链接时**注入 `-L<root>/lib`（编译时注入会被 `-Werror` 的未用参数判死，
   cgo 正是这么挂的）。
6. **Go 能编译，产物不能直接执行。** 工具链跑得通（`go build`、`go build -buildmode=c-shared`
   都成功），但产物落在共享存储上，app 进程 exec 它是 `Permission denied`（126）——与
   「数据目录不可执行」同一条平台规则。要在手机上跑产物，只能放进某个 APK 的
   `nativeLibraryDir`，或走 `pluginhost` 的 `dlopen` 路线。

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

**注意**：`shims::HELPER_TOOLS` / `PROVIDER_TOOLS`、`packages.rs::HELPER_COMMANDS`、
`AgentService.HELPER_PACKAGES` 与 manifest 的 `<queries>` 是必须同步的四份清单。
现在是自动门而不是注释：`crates/atomcode-android/tests/list_sync.rs` 直接读
XML / Kotlin 源文件比对，少改一处就红（加 provider 时会先撞到这里）。

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
