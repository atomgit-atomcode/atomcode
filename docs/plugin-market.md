# 插件市场

Android 端「运行时插件」的市场源。目标是把「装什么」从翻 `Download` 目录里的压缩包，
变成按分类浏览——就像外卖平台用「疯狂星期四」「新人特惠」这类侧栏分区，而不是把所有店
铺平铺成一列。`numpy`、`cpython` 归在 `python` 下，`node` 归在 `js` 下。

**市场仓库与 `atomcode` 主仓相互独立**，本地位于 `E:/atomcode-plugins`（尚未推送：
组织下建仓的权限还在等）。这里只放元数据与包体，不含客户端代码。

## 三层元数据

| 层 | 位置 | 作用 |
|---|---|---|
| 市场清单 | `market.json` | 分类定义 + 每插件一条引用；客户端一次拉取即可渲染侧栏与分页 |
| 单插件元数据 | `plugins/<name>/index.json` | 该插件的分类、标签、版本、校验值 |
| 包内自述 | 包体里的 `index.json` | 与上面同一份内容，包自己说清它是什么，市场端不必先解包 |
| 包外校验 | `packages/<file>.sha256` | 独立文件，只含一行 sha256 |

## 存储后端是可切换字段

包体放哪儿由条目里的 `artifact.kind` 决定，**换方案不改客户端**：

| kind | 取法 | 什么时候用 |
|---|---|---|
| `repo-lfs` | 媒体基址 + 相对路径 | 默认。LFS 配额够用时 |
| `repo-plain` | 仓库基址 + 相对路径 | LFS 受限，或想让 raw 链接直接可下 |
| `url` | 条目自带的绝对地址 | 迁到 Release 附件或对象存储 |

踩过的一点：**LFS 文件的 raw 链接返回的是指针文本，不是包体**。所以 `repo-lfs` 必须配
媒体基址（`[market] media_base`）；没配时退回仓库基址，客户端下载后会因 sha256 不符而
明确失败——比静默装错安全。

## provider APK：不是插件，但和市场同源

有些工具**不能**打成插件包：它们要么得自己当程序运行、要么得 `execve` 子进程
（`make` 要跑编译器，`zig` 本身就是编译器）。而 Android 拒绝执行应用数据目录里的
任何文件——实测在真实 app 进程里执行复制到 `<data>/bin` 的二进制返回 **126**
（`/system/bin/sh: …: Permission denied`；`run-as` 下能跑是 `runas_app` 域的假阳性）。
平台允许的是**执行别的 APK 的 `nativeLibraryDir`**，所以这类工具只能装在独立 APK 里，
主应用再把 `<product>/bin/<tool>` 链接过去。

一个市场因此要发布**多个** provider。清单里写成列表，旧的单数写法仍然有效：

```toml
# 老仓库（仍然支持：会被当成只有一项的列表）
[helper]
name = "com.atomcode.tools"

# 新写法：一行一个 APK
[[helpers]]
name = "tools"
package = "com.atomcode.tools"
title = "构建工具"
summary = "make / tcc / 设备上的 atomcode CLI"
version = "1.2.0"
size_bytes = 33534300
sha256 = "..."
[helpers.artifact]
kind = "repo-plain"
path = "apk/com.atomcode.tools.apk"

[[helpers]]
name = "zig"
package = "com.atomcode.tools.zig"
title = "zig"
summary = "zig 0.13.0 编译器（标准库仍是 zig-lib 数据包）"
version = "0.13.0"
size_bytes = 53306460
sha256 = "..."
[helpers.artifact]
kind = "repo-plain"
path = "apk/com.atomcode.tools.zig.apk"
```

- `package` 是 APK 的 applicationId，面板靠它判断「这一行装了没有」；缺省等于 `name`
  （老清单里两者本来就是同一个值）。
- 主应用清单的 `<queries>` 必须声明每个 provider 包，否则 Android 11+ 查不到。
- 客户端取包：`GET /packages/market/helper?name=<name|package>`；不带 `name` 时取第一个，
  这正是只有单 provider 的老客户端的行为。下载后 daemon 校验 `sha256`，再把文件写进
  `<product>/helper/<name>.apk`，交给系统安装器。
- `/packages/market` 的响应里 `helpers[]` 是完整列表，`helper` 仍是第一项（向后兼容）。

**当前在用的两个 provider**：`com.atomcode.tools`（make / tcc / CLI，33 MB）与
`com.atomcode.tools.zig`（zig，53 MB APK；工程在 `android-zig/`）。把 zig 拆出去之前，
helper 是 82 MB。

### 真机验证（PLT140，2026-10-30）

本地市场夹具 `/sdcard/Download/atomcode-market/`（`market.json` + 两个 APK），基址用
`POST /packages/market/config` 写入，然后：

| 验证点 | 真机结果 |
|---|---|
| `GET /packages/market?refresh=1` | `helpers` 两条：`tools 1.2.0 33534300`、`zig 0.13.0 53306460`，`download_path` 已解析；`helper` = tools |
| `?name=zig` / `?name=tools` | 分别写出 `…/helper/zig.apk`（53,306,460 B）与 `…/helper/tools.apk`（33,534,300 B） |
| 不带 `name` | 返回 tools，与老客户端行为一致 |
| `?name=nope` | 明确报错，不静默退化成「第一个」 |
| 落盘哈希 | 与清单 `sha256` 逐字节一致（`2bdee488…` / `d7b8fc56…`） |
| 面板 provider 行 | `Build tools 1.2.0 \| 已安装`、`zig 0.13.0 \| 已安装`，无按钮 |
| 卸载 zig 后重载 | `zig 0.13.0 \| 未安装 \| 安装 zig`，恰好一个按钮 |

**没走完的一段**：「安装 zig」按钮走 `ACTION_INSTALL_PACKAGE`，系统安装器要人点确认，
自动化只能验证到「按钮出现 / 下载与哈希正确」。验证「未安装」分支时卸掉了
`com.atomcode.tools.zig`，之后 `pm install` / `adb install` 都长时间不返回——根因是设备
重启后停在锁屏（`dumpsys user` → `RUNNING_LOCKED`，`mScreenLocked=true`），
`/data/user/0` 的凭证加密存储未解密，安装器报
`errno 126 (Required key not available)`。解锁屏幕即可恢复，与代码无关。

## 适合发布成插件的 MCP 服务器

MCP 服务器不必是独立进程：`pluginhost` 是 APK 里唯一需要可执行的程序，它 `dlopen`
插件后把 stdin/stdout 直接交给插件，协议就跑在这对管道上（数据目录允许 `dlopen`）。
所以市场里可以发**二进制** MCP 服务器，形式与其它插件一样：

```toml
[[plugins]]
name = "mcp-example"
title = "MCP 示例服务器"
summary = "纯 C 实现的 MCP stdio 服务器（工具：now / repeat）"
category = "mcp"
version = "1.0.0"
```

```json
{
  "name": "mcp-example",
  "version": "1.0.0",
  "entry": "lib/libmcp_example.so",
  "symbol": "plugin_main",
  "aliases": ["mcp-example"]
}
```

装完之后 `mcp.json` 指向别名即可：

```json
{ "mcpServers": { "example-bin": { "command": "<product>/bin/mcp-example", "args": [] } } }
```

构建：`bash scripts/build-android-mcp-plugin.sh`（Windows/Linux 的 NDK 布局都支持）。

**真实用途的那一个**：`mcp-filesystem` 同形，但工具是只读的文件访问
（`list_dir` / `read_file` / `stat`）：

```toml
[[plugins]]
name = "mcp-filesystem"
title = "文件系统 MCP 服务器"
summary = "只读文件访问：列目录、读文本、看 stat；越界路径（含符号链接）拒绝"
category = "mcp"
version = "1.0.0"
```

```json
{
  "name": "mcp-filesystem",
  "version": "1.0.0",
  "entry": "lib/libmcp_filesystem.so",
  "symbol": "plugin_main",
  "aliases": ["mcp-filesystem", "filesystem"]
}
```

它必须知道**能读哪里**，所以 `mcp.json` 的条目带一个根：

```json
{ "mcpServers": { "filesystem": {
  "command": "<product>/bin/mcp-filesystem", "args": [],
  "env": { "ATOMCODE_MCP_FS_ROOT": "/storage/emulated/0" } } } }
```

参数先解析成 realpath 再与根比较，`../` 和指向根外的符号链接都返回错误结果；二进制文件
（含 NUL）拒绝回显。构建：`bash scripts/build-android-mcp-filesystem.sh`；协议自测不需要
设备：`bash scripts/mcp-filesystem/selftest.sh`（主机编译器跑同一份源码）。

**发布状态**：两个 MCP 包（`mcp-example`、`mcp-filesystem`）都只在本仓库里可编，尚未
发布到市场仓库——市场仓库还等组织建仓权限。
注意 MCP 服务器是在 **live runtime 装配时**连接的，改完 `mcp.json` 要切会话或重启应用，
`/config/reload` 只重载 provider 配置。

## 发布一个插件

```sh
# 1) 在 atomcode 里构建出 dist/<name>-<version>-arm64.tar.gz
# 2) 在 catalog.tsv 加一行：name / title / category / tags / summary
# 3) 发布
cd E:/atomcode-plugins
tools/publish.sh                 # 默认 STORAGE=repo-lfs
STORAGE=repo-plain tools/publish.sh   # 换后端
# 4) 提交推送
git add . && git commit -m "feat(plugins): add <name>" && git push
```

`publish.sh` 会：把 `index.json` 注入包体（保留软链等成员）→ 生成包外 `.sha256` →
写 `plugins/<name>/index.json` → 重算 `market.json`。

## 客户端契约

```
GET /packages/market[?refresh=1]
```

返回 `success`、`base`、`storage`、`generated_at`、`from_cache`、`categories[]`、
`plugins[]`、`helpers[]`。每个插件带已解析的 `download_url` 或 `download_path`（本地基址时
是文件路径）、`sha256`、`size_bytes`、`installed` / `installed_version`。

`helpers[]` 是 provider APK 列表（每项带 `name`、`package`、`version`、`size_bytes`、
`sha256`、已解析的下载位置）；`helper` 仍是第一项，给只有单 provider 时代的老客户端用。

```
GET /packages/market/helper?name=<name|package>    # 取指定 provider，缺省取第一个
```

响应：`{"success":true,"name":"zig","package":"com.atomcode.tools.zig","version":"0.13.0",
"path":"<product>/helper/zig.apk","size_bytes":53306460}`；未知名字返回
`{"success":false,"error":"the market does not publish a provider called `nope`"}`。

**客户端装的是什么，谁说了算**：`/packages/market` 只说市场发布什么。某一行「装没装」由
**宿主**回答（`AtomCodeHost.installedProviders()` 返回已安装的包 id 数组）——daemon 只看
得见 `<data>/bin` 的链接，看不出某个工具来自哪个 APK。

配置写在产品 `config.toml`：

```toml
[market]
base = "https://raw.githubusercontent.com/<owner>/<repo>/main"   # 或本地目录
# media_base = "https://media.githubusercontent.com/media/<owner>/<repo>/main"   # LFS 用
```

环境变量 `ATOMCODE_MARKET_BASE` / `ATOMCODE_MARKET_MEDIA_BASE` 覆盖配置（测试与 adb
调试用）。基址可以是 `http(s)://…`，也可以是本地目录或 `market.json` 文件路径——断网时
把整个仓库推到 `/sdcard/Download/atomcode-plugins` 就能照常用。

缓存：内存 + `<product>/cache/market.json`，TTL 10 分钟；`refresh=1` 跳过缓存；拉取失败时
退回磁盘缓存（断网仍能看到上次的清单）。

## 界面

插件页顶部新增「插件市场」：

- **侧栏**：`全部` + 各分类（带数量），点分类即筛选；手机上转成横向 chips，不占首屏。
- **折叠**：每个分类是一个可折叠分组，点头部收起/展开（`aria-expanded` 跟着变）。
- **分页**：每页 12 个，底部「上一页 / 第 x / y 页 / 下一页」；客户端分页——整份索引只有
  几 KB，服务端分页只会多一次往返。
- **卡片**：名称、版本、体积、摘要、标签；已装显示「已装 x.y」并给卸载，未装给安装。
  安装走既有的流式接口（读取 → 校验 → 解包 x/y 文件 → 建链接），进度条与已安装区共用一套。

## 验证记录（2026-10-27，真机）

| 项 | 结果 |
|---|---|
| `GET /packages/market` | `success:true`，6 分类、10 插件，`download_path` 解析正确，`installed` 全为 true |
| 市场区块渲染 | 侧栏（全部 10 / Python 3 / JavaScript 2 / 渲染 2 / Lua 1 / 数据与存储 1 / 工具链 1）、搜索框、来源行均正常 |
| 分类筛选 | 点 `Python` 后只显示 Python 组（cpython、numpy），chip 高亮 |
| 分组折叠 | 点 `Python` 组头后卡片收起，页面回流 |
| 分页 | 数学部分单测 6 项；真机确认：20 条目时底部出现「上一页 / 第 1 / 2 页 / 下一页」，点下一页到「第 2 / 2 页」且下一页置灰 |
| 从市场安装 | 选源与校验值单测 4 项；真机确认：卡片卸载 → 卡片安装 → 提示「已安装 numpy 1.26.2」、卡片变回「已装 1.26.2」，随后 `import numpy` 与矩阵运算正常，`python-packages` 三个链接齐（含 `.libs`）。本地包体 9.4 MB 秒装，进度条未截到；同一组件的分阶段进度早已在 cpython 大包上验过 |

真机验证时临时替换过 `/sdcard/Download/atomcode-plugins/market.json`（20 条目的分页用例），
已用同目录备份 `market.real.json` 换回，设备上现在跑的是真实 6 条目索引。

## 单元测试

```sh
# 分页数学（6 项）与安装源选择（4 项）
cd webui
npx tsc src/lib/marketPaging.ts src/lib/marketPaging.test.ts         src/lib/marketSource.ts src/lib/marketSource.test.ts         --outDir /tmp/out --module esnext --target es2022 --moduleResolution bundler --skipLibCheck
# 把产物里的 './x.ts' 改成 './x.js' 后：
node --test /tmp/out
```

本机 Node 20 不支持 `--experimental-strip-types`，所以走「tsc 转译 + node --test」。

生态与各插件形态见 [`plugin-ecosystem.md`](plugin-ecosystem.md)。

## 已知边界

- 仓库还没推送，客户端目前指向设备本地目录；等组织建仓权限到位，把 `base` 换成 raw 链接即可，
  代码不用改。
- LFS 的媒体基址在 AtomGit 上还没确认（GitHub 是 `media.githubusercontent.com/media/...`）；
  未确认前建议先用 `repo-plain`，或保持本地目录。
- 分类以 `categories.json` 为准，插件只声明 `category` 单值；要「一个插件进多个分区」时再扩。
