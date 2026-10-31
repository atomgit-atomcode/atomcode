# 插件生态：目录与形态

运行时插件是在设备上按需安装的能力包，市场源见 [`plugin-market.md`](plugin-market.md)。
这里记录**当前有哪些插件、它们各自以什么形态存在、新增一个要动哪里**。

## 三种形态

| 形态 | 结构 | 执行方式 | 例子 |
|---|---|---|---|
| 命令型 | `lib/plugin.so` 导出 `plugin_main` | `pluginhost` dlopen 后调用，按名建 `<product>/bin/<alias>` 软链 | cpython、lua、node、quickjs、svg、sqlite3 |
| Python 载荷 | `lib/python/**` + `python_packages` | daemon 把 `lib/python/*` 软链进 `<product>/python-packages`，解释器的 `PYTHONPATH` 已含那里 | numpy、cffi |
| 数据包 | 纯数据（头文件、库） | 无入口、不建命令；`entry` 可为空 | cheaders |

命令型插件的关键是**不能 exec**：Android 10+ 禁止从应用数据目录执行文件，所以一切都走
`dlopen` + 函数调用。这也决定了「解释器」必须能作为库被嵌入，而不是装个二进制。

## 当前目录（10 个）

| 插件 | 分类 | 体积 | 提供什么 |
|---|---|---|---|
| `cpython` | python | 23.5 MB | 完整 CPython 3.12.10：pip、FTS5/数学函数、spawn、ctypes、cffi 底座 |
| `numpy` | python | 9.4 MB | NumPy 1.26.2（自带 BLAS）；载荷形态 |
| `cffi` | python | 249 KB | 在 Python 里调用 C 库；自带 `_cffi_backend` 与 `pycparser` |
| `node` | js | 17.2 MB | nodejs-mobile 的 `libnode` + 嵌入 shim（`fork` 不可用，见包内 README） |
| `quickjs` | js | 456 KB | ES2023 引擎 `qjs`：启动快、内存小，适合一次性脚本 |
| `lua` | lua | 756 KB | Lua 5.4.7 + lua-cjson + lsqlite3，可 `require` 设备上编译的 C 模块 |
| `sqlite3` | data | 960 KB | SQLite 3.50.4 命令行：FTS5、RTree、数学函数、扩展加载 |
| `cheaders` | toolchain | 2.7 MB | 给 tcc 用的 bionic/Python/Lua 头文件 + CRT 与 libc 存根 |
| `svg` | render | 52 KB | `svg2png`：把 SVG 光栅化成 PNG |
| `text` | render | 384 KB | `text2png`：把文字（含中文）渲染成 PNG，补 SVG 不支持 `<text>` 的缺口 |

另外 `zig` 不在插件里（它是静态 `ET_EXEC`，只能放在 `nativeLibraryDir` 执行），由
`com.atomcode.tools` helper APK 承载。

## 本批新增的三个

### quickjs（解释器）

`scripts/build-android-quickjs-plugin.sh`：把 `qjs.c` 编译时 `-Dmain=quickjs_main`，
插件入口只做转发；`qjs` 的 `-e`、脚本参数、`std`/`os` 模块因此全部保留。

**踩坑**：`qjs.c` 引用预编译的 REPL 字节码（`qjsc_repl` / `qjsc_repl_size`），而源码包只带
`repl.js`。不生成的话链接照样成功，运行时 `dlopen` 才报
`cannot locate symbol "qjsc_repl_size"`——装得上、跑不起来。构建脚本因此先用宿主 `cc`
编一个 `qjsc`，再 `qjsc -c -o qjsc_repl.c repl.js`，把生成的 C 一起编进 `plugin.so`。

体积 456 KB，对比 node 的 17.2 MB：需要 npm 生态才用 node，一次性脚本用 qjs 更划算。

### svg（渲染）

`scripts/build-android-svg-plugin.sh`：nanosvg + nanosvgrast + stb_image_write，三个库都是
「头文件实现」，随包放在 `scripts/svg-plugin/vendor/`，构建完全离线。

```sh
svg2png in.svg out.png          # 按 SVG 自身尺寸（96 dpi）
svg2png in.svg out.png 720      # 指定宽度，等比缩放
```

**为什么值得有**：agent 能写 SVG 却看不见自己画的。渲染成 PNG 后 `read_file` 就能显示，
「画 → 看 → 改」的回路闭合。真机验证：一张 320×180 的渐变山景渲染成 480×270 PNG，画面正确。

**踩坑**：三个头文件都带 include guard，先普通 include 再定义 `*_IMPLEMENTATION` 二次
include 只会命中 guard、拿不到实现——宏必须**先于唯一一次 include**。

已知边界：nanosvg 不支持 `<text>`（文字要字体引擎），需要文字时先用路径轮廓代替。

### cffi（Python 载荷）

`scripts/build-android-cffi-plugin.sh`：把 cffi wheel 与 pycparser wheel 一起解进
`lib/python/`，`python_packages` 暴露。

**踩坑**：cffi 的 `_cffi_backend` 是早前编的，DT_NEEDED 还写着长名
`libpython3.12.so.1.0`，而运行时已把 SONAME 统一成短名 `libpython3.12.so`——bionic 只按
SONAME 匹配已加载的库，长名既匹配不上预加载那份、文件系统里也找不到。处置是打包时
**等长改写**：`scripts/plugin_payload_fixup.py` 把载荷里所有扩展的长名改成短名。
任何第三方 wheel 都会遇到这个，所以做成了公用步骤。

### text（渲染，补 `<text>` 缺口）

`scripts/build-android-text-plugin.sh` + `scripts/build-android-freetype.sh`：FreeType 交叉编译成
静态库（关掉 zlib/png/bzip2/harfbuzz/brotli，只留轮廓渲染，产物 1 MB），插件用
`FT_Load_Char` 逐字光栅化、自己算换行，再用 stb 写 PNG。

```sh
text2png out.png "AtomCode 插件生态" --size 36 --width 640 --fg 1b2b4b --bg f8f2d8
```

字体取设备系统字体（默认 `/system/fonts/NotoSansCJK-Regular.ttc`），**不随包携带**——包里只有
384 KB 的 .so。中文能显示是因为用了系统 CJK 字体；换成没有 CJK 字形的字体会渲染成空白方框。
真机验证：640×292、5 行，中英文混排与自动换行都正确。

### lsqlite3（不是新包，是 lua 包的模块）

按 lua 包的既有通道（`lib/lua/*.so` + shim 把该目录加进 `LUA_CPATH`）加一个模块，不新建包：

1. `sqlite3.c` 合并源码 + `lsqlite3.c` 编成 `lsqlite3.so`，放进 `dist/lua-modules/`；
2. `scripts/build-android-plugins.sh lua` 只重打 lua 包（脚本现在支持挑目标，避免顺手换掉
   sqlite3 的 sha）；
3. 真机：`require("lsqlite3")` → `3.50.4`，建表/插入/按序查询 3 行、总计 892 KB 全部正确。

**坑**：宿主直连 `sqlite.org` 下 2.8 MB 的源码包会超时，改由设备下载再拉回；`lsqlite3.c`
需要 `lua.h`/`lauxlib.h`，从 `lua.tgz` 里解出来即可。

## 构建输入从哪来

这些源码包**不入库**（`/e/atomcode/.gitignore` 里已忽略），按需下载：

| 输入 | 地址 | 谁用 |
|---|---|---|
| `lua.tgz` | https://www.lua.org/ftp/lua-5.4.7.tar.gz | lua 包、lsqlite3 的头文件 |
| `sqlite-amalgamation-3500400.zip` | https://www.sqlite.org/2025/sqlite-amalgamation-3500400.zip | sqlite3 CLI、lsqlite3、cpython 的 `_sqlite3` |
| `quickjs.tar.xz` | https://bellard.org/quickjs/quickjs-2025-09-13-2.tar.xz | quickjs 插件 |
| `freetype-2.13.3.tar.gz` | https://download.savannah.gnu.org/releases/freetype/freetype-2.13.3.tar.gz | text 插件 |
| `lsqlite3` 源码 | https://github.com/slanska/lsqlite3（master tarball） | lua 包里的 lsqlite3 模块 |

下载渠道的实测差异（本机网络）：`bellard.org` 与 `savannah` 宿主可直连；`raw.githubusercontent.com`
宿主不通（`000`），要经 `gh-proxy.com` 前置；`sqlite.org` 的 2.8 MB 包宿主会超时，改由设备下载再拉回。
`nanosvg` / `stb_image_write` 已经 vendor 进 `scripts/svg-plugin/vendor/`，不需要联网。

## 新增一个插件要动哪里

1. `scripts/build-android-<name>-plugin.sh`：产出 `dist/<name>-<version>-arm64.tar.gz`，
   内含 `package.json`（`entry`/`symbol`/`aliases` 或 `python_packages`）与 `README.md`；
2. `atomcode-plugins/catalog.tsv` 加一行（名称 / 标题 / 分类 / 标签 / 摘要）；
3. `atomcode-plugins/tools/publish.sh`：注入包内 `index.json`、生成包外 `.sha256`、
   重算 `market.json`；包体是**可复现**的（成员时间戳、属主、gzip mtime 全部归零，
   重复发布不会换哈希）；
4. 提交并推送仓库；客户端 `[market] base` 指向它。

## 市场侧的验证记录

- 市场页显示 9 个插件、6 个分类（`Python 3 / JavaScript 2 / 渲染 1 / Lua 1 / 数据与存储 1 / 工具链 1`）；
- 从市场卡片卸载 cffi → 卡片变「安装」→ 点安装 → 提示「已安装 cffi 2.1.1」，
  随后 `import cffi` + `cos(0.0)` 返回 `1.0`；
- quickjs：`qjs -e 'console.log(1+2, typeof BigInt)'` → `3 function`；
- svg：`svg2png` 渲染出可查看的 PNG。

## 下一步候选

| 候选 | 形态 | 说明 |
|---|---|---|
| ffmpeg | helper APK | 体积大且要 exec，只能走 helper，不进插件 |
| harfbuzz（复杂文字排版） | 命令型 | 与 text 插件配合可做阿拉伯文/天城文的正确连写；体积 ~2 MB |
| perl / ruby | 命令型 | 都能编成 `.so` 嵌入，但体积与维护成本高，优先级低 |
