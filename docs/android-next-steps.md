# Android 后续可做项

本文件记录**尚未实现**的功能设想与环境扩展，供后续排期。已完成的事不在这里
重复（见 `docs/android-port.md` 与提交历史）。

写下来的目的：这些判断来自一轮真机核查（PLAT140 / Android 16，2026-10-29 / 10-30），
每一条都有具体的落点或阻塞原因，避免下次重新摸索。

---

## 一、功能：待做

### 1. 真正有用的 MCP 集成包（已完成，两个包都已发布）

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

**发布状态（2026-11-02 更新）**：市场仓库已换成个人公开仓库
[`qiaoen_python/atomcode-android-plugins`](https://atomgit.com/qiaoen_python/atomcode-android-plugins)
（AtomGit），里面现在是 14 个 provider APK + 11 个数据包 + **两个 MCP 插件**：

- `mcp-filesystem`（6837 字节）、`mcp-example`（5174 字节），`category: "mcp"`，
  包体走 Release 附件（`v1.0`），`artifact.kind = "url"` 直链；
- 真机 `GET /packages/market?refresh=1` 返回 13 个 plugins（11 数据包 + 2 MCP）+ 14 个
  helpers，两个附件下载回来 sha256 与清单一致。

清单由该仓库的 `tools/write_market.py` 生成；二进制不在仓库里，大小与哈希记在
`packages/manifest.json`，所以重新生成清单不需要把几百 MB 拷回来。

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
| **cmake 4.4.5** | **A**：独立 APK `com.atomcode.tools.cmake`（`libcmake.so`/`libctest.so`/`libcpack.so`），模块是数据包 `cmake-data` | **✓ 已实现并实测**（2026-11-01）：`cmake --version` → `4.4.5`；`cmake -S … -B … -DCMAKE_C_COMPILER=clang` → `Configuring done`，`cmake --build` → `Built target hello`（7064 字节），见下方「六个坑」 |
| **ninja 1.13.2** | **A**：独立 APK `com.atomcode.tools.ninja`（`libninja.so`，648 KB） | **✓ 已实现并实测**：`ninja --version` → `1.13.2` |
| **pkg-config 0.29.2** | **A**：独立 APK `com.atomcode.tools.pkgconf`（`libpkg-config.so`），版本化依赖（`libglib-2.0.so.0`）走数据包 `pkgconf-data` | **✓ 已实现并实测**（2026-11-01）：`pkg-config --version` → `0.29.2`，`PKG_CONFIG_PATH` 下 `--modversion` / `--libs` 都对 |
| **unzip 6.0** | **A**：独立 APK `com.atomcode.tools.unzip`（`libunzip.so`），`libbz2.so.1.0` 走 `unzip-data` | **✓ 已实现并实测**（2026-11-01）：解包 CPython 生成的 zip 成功。平台 toybox 的 `unzip` 没有 `-P`、Zip64，Gradle 不打包的版本化库照旧走数据包 |
| **GNU tar 1.35** | **A**：独立 APK `com.atomcode.tools.tar`（`libtar.so`），无数据包（依赖全在 sysroot 里） | **✓ 已实现并实测**：`--format=posix --transform 's,^,pre,/'` 打包再解包，内容正确。toybox 的 `tar` 没有 `--transform`/`--exclude-from`/归档自动识别 |
| **perl 5.44.0** | **A**：独立 APK `com.atomcode.tools.perl`（`libperl-driver.so` + `libperl.so`），模块树 67 MB 走 `perl-data`；宿主设 `PERL5LIB` 指向数据包（内置 `@INC` 指 Termux 前缀） | **✓ 已实现并实测**：`perl -v` → v5.44.0，`perl -MJSON::PP` 加载核心模块成功。驱动刻意叫 `perl-driver`：`libperl.so` 是它链接的共享库，同名会在 jniLibs 里互相覆盖 |
| **bash 5.3.20** | **A**：独立 APK `com.atomcode.tools.bash`（`libbash.so`），`readline` 等版本化库走数据包 `bash-data` | **✓ 已实现并实测**（2026-11-01）：`bash --version` → `5.3.20(1)-release`；`#!/bin/bash` + `set -o pipefail` 的脚本跑通——平台自带的 `/system/bin/sh` 是 toybox，跑不了这些 |
| **rsync 3.5.1** | **A**：独立 APK `com.atomcode.tools.rsync`（`librsync.so`），`libpopt`/`libxxhash`/`libidn2` 等走 `rsync-data` | **✓ 已实现并实测**：`rsync --version` → `3.5.1`，`rsync -a src/ dst/` 同步成功。手机网络抖，`rsync --partial` 比 `git clone` 从头再来实在 |
| **OpenSSH 10.6p1** | **A**：独立 APK `com.atomcode.tools.openssh`（`libssh.so`/`libscp.so`/`libsftp.so`/`libssh-keygen.so`/`libssh-add.so`），库走 `openssh-data`；**只有客户端，不含 sshd** | **✓ 已实现并实测**：`ssh -V` → `OpenSSH_10.6p1, OpenSSL 3.6.5`；`ssh-keygen -t ed25519` 生成密钥、`sftp` 可用。`termux-auth` 依赖没有在运行时发作 |
| **node 24 LTS** | **A**：独立 APK `com.atomcode.tools.node`（Termux `nodejs-lts` 24.22.1，`libnode.so` 45 MB）；npm 树与头文件是市场包 `node-lib`，npm 用 `node <prefix>/lib/node_modules/npm/bin/npm-cli.js` 调（数据目录里的 shebang 脚本不可执行） | **✓ 已实现并实测**（2026-11-01，APK 37 MB + 22 MB 数据包）：`node --version` → `v26.12.1`，`node -e` 求值正常，`child_process.execSync` 也正常（补丁把 Termux 的 `$PREFIX/bin/sh` 换成 `/bin/sh`）。取代 `nodejs-mobile` 18 插件：它是最后能用的 libnode 版本（GitHub 不可达，npm 上仍指 18.20.4）。注意 deb 的 control 写 `24.22.1`，二进制实际报 `v26.12.1`——版本号以运行结果为准 |
| **clang 21 + lld 21** | **A**：独立 APK `com.atomcode.tools.clang`（Termux LLVM 21.1.8）；头文件、sysroot、resource dir 是市场包 `clang-sysroot`；`clang` 是启动器（`scripts/termux/launcher/clang.c`），注入 `-resource-dir`/`--sysroot` 后 exec 真 driver | **✓ 已实现并实测**（2026-11-01，APK 72 MB + 30 MB 数据包）：`clang --version` → `21.1.8`，能编译、能链接（`clang main.c -o hello` → 6040 字节），cgo 的 `-buildmode=c-shared` 也通。NDK 的 clang 是宿主二进制，不能上设备；产物由 `scripts/build-android-clang-provider.sh` 从 Termux 包生成 |

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

**已发布到市场**（2026-11-01）：`com.atomcode.tools.zig` 及其余 13 个 provider
（clang/node/ninja/cmake/go/pkgconf/bash/rsync/openssh/unzip/tar/perl/tools）全部
发布到新市场仓库 [`qiaoen_python/atomcode-android-plugins`](https://atomgit.com/qiaoen_python/atomcode-android-plugins)。
真机验证：`GET /packages/market` 返回 `helpers: 14`，`GET /packages/market/helper?name=unzip`
下载落地、sha256 校验通过。

接入方式——`config.toml` 或环境变量：

```toml
[market]
base = "https://api.atomgit.com/api/v5/repos/qiaoen_python/atomcode-android-plugins/raw"
```

**踩的坑**：AtomGit 网页版的 `raw/branch/main/...` 端点返回 SPA 的 HTML 壳，不是裸文件；
真正能用的是官方 API `GET /api/v5/repos/{owner}/{repo}/raw/{path}`（host 是
`api.atomgit.com`，不是 `atomgit.com`）。Git LFS 在这条 API 上只返回 133 字节的指针
文本（`version/oid/size`），不是真实字节——跟 `docs/plugin-market.md` 记录的
「LFS raw 链接返回指针不是包体」是同一个坑，这次在新仓库上又踩了一遍；二进制改回
普通 git blob 存储后才正常。`git lfs untrack` 不会把已提交的 LFS 对象转回普通 blob，
要 `git rm --cached` 再重新 `git add` 才生效。

**注意**：`shims::HELPER_TOOLS` / `PROVIDER_TOOLS`、`packages.rs::HELPER_COMMANDS`、
`AgentService.HELPER_PACKAGES` 与 manifest 的 `<queries>` 是必须同步的四份清单。
现在是自动门而不是注释：`crates/atomcode-android/tests/list_sync.rs` 直接读
XML / Kotlin 源文件比对，少改一处就红（加 provider 时会先撞到这里）。

---

## 四、正式包默认带哪些 provider（选型清单，2026-11-02 实测体积）

先说清楚这件事实际在决定什么：**APK 里一个 provider 都不带**，14 个 APK 与 11 个数据包
全部按需从市场下载（见 `docs/plugin-market.md`）。所以"默认带哪些"其实是三件事：

1. 首次启动引导安装哪几个（不装基本没法干活）；
2. 市场页默认置顶/推荐哪几个；
3. 哪些折叠进"按需下载"，让体积留给真的要用的人。

体积取自市场清单（provider APK + 它必须配套的数据包，**数据包不同装跑不起来**）：

| 档 | provider | 体积 | 为什么在这一档 |
|---|---|---|---|
| **A 基础**（建议首启就引导装） | `bash` `unzip` `tar` `pkgconf` | **4.3 MB** | 平台的 `/system/bin/sh` 是 toybox：没有真正的 bash 语法、unzip 不支持加密与 Zip64、没有 GNU tar；`pkgconf` 是 `configure` 与 `find_package` 的前提 |
| **B 构建**（建议同属首装） | `tools` `ninja` | **32.7 MB** | `tools` 给 `make/cc/tcc` 与 `atomcode` CLI，属于「进程模型」那套，不装就没有 C 构建；`ninja` 只有 0.8 MB |
| **C 常用**（置顶但不引导） | `openssh` `rsync` | **9.2 MB** | SSH 远端与断点续传，做真项目才会用到 |
| **C 常用** | `cmake` | **26.3 MB** | 只在 CMake 项目里有意义，且体积主要来自模块集 |
| **D 重型**（折叠，按需） | `clang` **101.9 MB** · `zig` **71.5 MB** · `node` **58.2 MB** · `go` **56.8 MB** · `perl` **14.8 MB** | **303 MB** | 单个就比前四档加起来还大；语言工具链是"用哪个装哪个"的典型 |
| **E MCP** | `mcp-filesystem` `mcp-example` | **~12 KB** | 小到可以无条件装 |

全装是 **375.7 MB**（provider 251.3 + 数据包 124.4）。

**建议**：首启引导装 A+B（**37 MB**），C 档置顶不引导，D 档折叠并支持搜索，E 档随首装。
理由是 A+B 之后"能跑 shell、能 `make`、能解压"就成立了，剩下都是具体项目才知道要不要。

**顺带一个事实**：重型档的体积大头在数据包不在 APK——`clang` 101.9 MB 里 30.7 MB 是头文件
与库、`zig` 71.5 里 20.7 是标准库、`go` 56.8 里 25.2 是 GOROOT 源码。真有瘦身需求，先看
数据包能不能按需切（例如只装 `libc++` 头），而不是去砍 APK。

## 五、已知边界（不是缺陷，但要知道）

- **`share/man` 里的 `::` 文件名会在解包时炸掉**：Termux 的 man 页叫 `App::Cpan.3.gz`，
  Windows 不允许 `:`，`scripts/termux/extract.py` 会中止并留下半截的树（perl 那次
  `bin/perl` 只有 6.8 KB 就是这个）。现在解包统一跳过 `share/man` 与 `share/doc`——
  设备上没人读文档，跳过比改名安全。
- **`adb install` 的流式安装会挂**：本机上有几个 provider APK（rsync、openssh）用默认的
  `adb install` 会卡在 `Performing Streamed Install` 直到超时，换成
  `adb install --no-streaming -r <apk>` 立刻成功。装不上时先换这个，不要去怀疑包本身。
- **产物点击的"没有查看器"提示在真机上基本不可达**：`MainActivity.viewerMimeType` 在
  resolver 与扩展名都认不出时回落到 `*/*`，而设备上总有应用声明 `*/*`（文本编辑器、
  文件管理器），于是永远弹「打开方式」而不是兜底 toast。这个取舍是有意的——`.patch`
  这类没有专用查看器的文件正是靠它才能打开；代价是 `no_viewer` toast 只留在理论上。
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
