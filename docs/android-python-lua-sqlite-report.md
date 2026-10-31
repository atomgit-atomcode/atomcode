# Python / Lua / SQLite 环境检查报告

- **生成日期**：2026-10-26
- **工作目录**：`/storage/emulated/0/AtomCode`
- **探测方式**：全部为实际执行解释器、导入模块、调用功能，非依据文档或版本号推断
- **对象**：AtomCode 内置运行时（Python / Lua / SQLite）

---

## 一、总结论

**三个运行时都可用，可以正常承担脚本与数据处理工作，但均为"裁剪版"，各有一处硬伤。**

| 运行时 | 状态 | 硬伤 |
|---|---|---|
| Python 3.12.10 | ✅ 可用 | 无 `tkinter`；第三方包几乎为零（仅有 pip）、无 `numpy` / `requests` |
| Lua 5.4.7 | ⚠️ 基本可用 | 无 luarocks、无 luajit、**无任何 C 扩展模块**（socket / sqlite 均加载失败） |
| SQLite 3.50.4 | ⚠️ 受限 | **无 FTS3/4/5、无 RTree、无数学函数**，且 `OMIT_LOAD_EXTENSION` 禁止加载扩展 |

最值得注意的一条：**SQLite 无法做全文检索，也无法通过 `.load` 补装**，文本搜索只能退回到 `LIKE` 或在应用层自建索引。

---

## 二、工具与版本一览

所有额外工具均来自 AtomCode 私有 bin 目录：
`/data/user/0/com.atomcode.app/files/atomcode/bin/`

| 工具 | 路径 | 版本 |
|---|---|---|
| `python3` | `<bin>/python3` | Python 3.12.10 |
| `python` | `<bin>/python` | Python 3.12.10（同一份） |
| `cpython` | `<bin>/cpython` | Python 3.12.10（同一份） |
| `lua` | `<bin>/lua` | Lua 5.4.7 |
| `sqlite3` | `<bin>/sqlite3` | 3.50.4 (2025-07-30) |
| `tcc` | `<bin>/tcc` | 0.9.28rc (AArch64 Linux) |
| `git` | `<bin>/git` | 可用 |
| `jq` | `<bin>/jq` | 可用 |
| `curl` | `<bin>/curl` | 可用 |
| `rg` | `<bin>/rg` | 可用 |
| `cc` | `<bin>/cc` | 存在（实际后端未验证） |
| `make` | `<bin>/make` | 可用 |

**不存在**：`gcc`、`clang`、`node`、`npm`、`perl`、`wget`、`luajit`、`luarocks`。

---

## 三、Python 3.12.10 详细

### 3.1 基础信息

| 项目 | 值 |
|---|---|
| 版本 | 3.12.10 |
| `sys.executable` | `/data/user/0/com.atomcode.app/files/atomcode/bin/python3` |
| pip | **25.0.1 存在**（`.../packages/cpython/lib/python3.12/site-packages/pip`） |
| 内置 sqlite3 模块 | SQLite 3.50.4（与 CLI 同一份库） |
| SSL | OpenSSL 3.0.16 (2025-02-11) |

> ⚠️ 修正历史报告：旧的 `report.md`（2026-10-25）写"pip 不存在"，**该结论已过期**，当前 pip 25.0.1 可用。

### 3.2 标准库模块实测

| 模块 | 结果 |
|---|---|
| `ssl` | ✅ OK |
| `zlib` / `bz2` / `lzma` | ✅ OK（压缩全通） |
| `ctypes` | ✅ OK（可调用 C 库） |
| `sqlite3` | ✅ OK |
| `json` / `hashlib` / `decimal` | ✅ OK |
| `curses` / `readline` | ✅ OK |
| `multiprocessing` / `asyncio` | ✅ OK |
| `venv` / `ensurepip` | ✅ OK |
| **`tkinter`** | ❌ 缺失（`ModuleNotFoundError`） |

### 3.3 第三方包

- 已安装：**仅 `pip`**。
- 实测缺失：`numpy`、`requests`。

`site-packages` 目录顶层只有 `pip`，即这是一个近乎裸装的 CPython。

### 3.4 影响

- 无 GUI 能力（`tkinter` 缺失），只能写 CLI / 后台脚本。
- 无 `numpy`，数值计算只能靠纯 Python（注意本机总内存 5.75 GB、可用约 1.5 GB，大数组场景更受限）。
- 无 `requests`，网络请求需走 `urllib`，或改用 `curl` 命令。
- pip 在，但本报告**未验证联网安装是否可行**；若需要装包应先实际测试一次。

---

## 四、Lua 5.4.7 详细

### 4.1 基础信息

| 项目 | 值 |
|---|---|
| 版本 | Lua 5.4.7 (`Lua 5.4`) |
| 解释器 | `/data/user/0/com.atomcode.app/files/atomcode/bin/lua` |
| luajit | ❌ 无 |
| luarocks（包管理器） | ❌ 无 |

### 4.2 模块搜索路径

`package.path`：

```
/usr/local/share/lua/5.4/?.lua
/usr/local/share/lua/5.4/?/init.lua
/usr/local/lib/lua/5.4/?.lua
/usr/local/lib/lua/5.4/?/init.lua
./?.lua
./?/init.lua
```

`package.cpath`：

```
/usr/local/lib/lua/5.4/?.so
/usr/local/lib/lua/5.4/loadall.so
./?.so
```

注意：路径里写的是 `/usr/local/...`（系统路径，本机基本不存在对应目录），实际能用的只有末尾的当前目录 `./?.lua` 与 `./?.so`。也就是说，**第三方 Lua 库只能放在脚本同目录下、且必须是纯 Lua 源码**。

### 4.3 模块实测

| 模块 | 结果 |
|---|---|
| `os` / `io` / `string` / `table` / `math` / `utf8` / `coroutine` | ✅ 全部 OK |
| **`socket`** | ❌ 缺失（无 C 扩展） |
| **`sqlite`**（luasqlite 等） | ❌ 缺失（无 C 扩展） |

### 4.4 影响

- 只能写**纯 Lua**脚本，标准库功能完整、`utf8` 库可用（可处理中文）。
- 无法联网、无法直接访问 SQLite —— 需要数据库操作时请改用 Python。
- 没有 luarocks 且 cpath 指向不存在的系统目录，**装 C 扩展基本无解**（且本机无 gcc/clang，只有 tcc，编译 Lua C 模块的前景不明）。
- 纯 Lua 文件（`./*.lua`）和放同目录的 `?.so` 仍可正常 `require`。

---

## 五、SQLite 3.50.4 详细

### 5.1 关键发现：CLI 与 Python 共用同一份库

- `sqlite3 --version` → `3.50.4 2025-07-30`
- Python `sqlite3.sqlite_version` → `3.50.4`

因此 **两边的能力完全一致**，不存在"CLI 能用、Python 不能用"的差异，下述限制对两者同时成立。

### 5.2 编译选项（共 38 项，完整清单）

```
ATOMIC_INTRINSICS=1
COMPILER=clang-17.0.2
DEFAULT_AUTOVACUUM
DEFAULT_CACHE_SIZE=-2000
DEFAULT_FILE_FORMAT=4
DEFAULT_JOURNAL_SIZE_LIMIT=-1
DEFAULT_MMAP_SIZE=0
DEFAULT_PAGE_SIZE=4096
DEFAULT_PCACHE_INITSZ=20
DEFAULT_RECURSIVE_TRIGGERS
DEFAULT_SECTOR_SIZE=4096
DEFAULT_SYNCHRONOUS=2
DEFAULT_WAL_AUTOCHECKPOINT=1000
DEFAULT_WAL_SYNCHRONOUS=2
DEFAULT_WORKER_THREADS=0
DIRECT_OVERFLOW_READ
MALLOC_SOFT_LIMIT=1024
MAX_ATTACHED=10
MAX_COLUMN=2000
MAX_COMPOUND_SELECT=500
MAX_DEFAULT_PAGE_SIZE=8192
MAX_EXPR_DEPTH=1000
MAX_FUNCTION_ARG=1000
MAX_LENGTH=1000000000
MAX_LIKE_PATTERN_LENGTH=50000
MAX_MMAP_SIZE=0x7fff0000
MAX_PAGE_COUNT=0xfffffffe
MAX_PAGE_SIZE=65536
MAX_SQL_LENGTH=1000000000
MAX_TRIGGER_DEPTH=1000
MAX_VARIABLE_NUMBER=32766
MAX_VDBE_OP=250000000
MAX_WORKER_THREADS=8
MUTEX_PTHREADS
OMIT_LOAD_EXTENSION
SYSTEM_MALLOC
TEMP_STORE=1
THREADSAFE=1
```

### 5.3 扩展能力实测

| 能力 | 结果 | 说明 |
|---|---|---|
| JSON（`json_extract` 等） | ✅ 可用 | 3.38+ 已并入核心，无需 `ENABLE_JSON1` 标志 |
| FTS3 / FTS4 | ❌ 不可用 | `no such module: fts4` |
| **FTS5** | ❌ 不可用 | `no such module: fts5` |
| RTree（空间索引） | ❌ 不可用 | `no such module: rtree` |
| 数学函数 `sqrt` / `pow` / `log` | ❌ 不可用 | 未编 `ENABLE_MATH_FUNCTIONS` |
| ICU / 中文分词 | ❌ 不可用 | 未编 `ENABLE_ICU` |
| 加载外部 `.so` 扩展 | ❌ 禁止 | `OMIT_LOAD_EXTENSION`，`.load` 不可用 |

### 5.4 正常可用的部分

- 普通建表、索引、事务、触发器、视图。
- WAL 模式、`THREADSAFE=1`（多线程安全）。
- JSON 函数族完整可用。
- 容量上限宽松：单条 SQL / 单值最大 **1 GB**（`MAX_LENGTH`、`MAX_SQL_LENGTH`），页大小可配到 64 KB，最多挂载 10 个数据库。

### 5.5 影响与替代方案

- **不要设计依赖 FTS5 全文检索的方案**，本机跑不起来。
- 也不能通过 `.load ./fts5.so` 补救（扩展加载被编译期禁用）。
- 中文分词（ICU）同样缺失，即便有 FTS 也无法按词切分。
- 需要文本搜索时的替代做法：
  1. 小数据量：`LIKE '%词%'` + 索引调优；
  2. 中等数据量：在 Python 侧构建倒排表（`token -> rowid`）自己维护；
  3. 对外部工具：先导出后交给 `rg`（本机可用）做正则检索。

---

## 六、系统层面信息

### 6.1 硬件与内核

| 项目 | 值 |
|---|---|
| 系统 | Android 13，`Linux 5.15.197-android13-8` |
| 架构 | `aarch64` |
| 用户态工具集 | Toybox |
| 内存总量 | 5,751,288 kB ≈ **5.49 GiB** |
| 内存可用 | 1,600,704 kB ≈ **1.53 GiB** |
| 磁盘（`/storage/emulated`） | 106 G 总量，77 G 已用，**28 G 可用（74% 已用）** |

### 6.2 挂载与执行限制

`/storage/emulated` 为 `/dev/fuse` 挂载，且带 **noexec**：

- `./script.sh` 直接执行会报 `Permission denied`（exit 126）。
- 正确做法：`sh script.sh`，或 `bash <script>`。

### 6.3 Shell 差异（重要）

本机 shell 为 `/system/bin/sh`，实际是 **mksh**，不是 bash。

**已确认支持**：`[[ ]]`、`set -o pipefail`、数组（`a=(x y)` 与 `set -A`）、花括号展开、heredoc、herestring、`local`、子串 `${v:1:3}`、模式替换 `${v/a/b}`、`case`、`trap`、`$(( ))`。

**确认不支持**：

| 特性 | 说明 |
|---|---|
| `[[ ... =~ ... ]]` | 无正则匹配，需用 `case` 做通配 |
| `<(cmd)` / `>(cmd)` | 无进程替换 |
| `select` | 无交互选择结构 |
| `\x27` 等 ANSI-C 转义 | **不被解析** |

最后一条是实际踩过的坑：本次检查中曾用
`sqlite3 :memory: 'select json_extract(\x27{"a":5}\x27,"\$.a")'`
导致 `unrecognized token: "\"` —— **不是 SQLite 的问题**，而是 mksh 不解析 `\x27`。
正确写法是使用 heredoc（见附录），或改用单引号嵌套拼接。

---

## 七、实践建议（速查）

| 需求 | 推荐选择 | 原因 |
|---|---|---|
| 数据库读写 / JSON 处理 | **Python** | `sqlite3` + `json` 完整可用 |
| 全文检索 | Python 自建倒排，或导出后 `rg` | 无 FTS5 |
| 数值计算 | 纯 Python（谨慎控制内存） | 无 `numpy`；可用内存仅 1.5 GiB |
| HTTP 请求 | `curl` 命令 或 `urllib` | 无 `requests` |
| GUI | **不可行** | 无 `tkinter` |
| 轻量脚本 / 数据清洗 | **Lua**（纯 Lua） | 标准库齐全、启动快 |
| Lua 访问数据库或网络 | **不可行**，改用 Python | 无 C 扩展 |
| 编写可执行脚本 | 一律 `sh xxx.sh` | `noexec` 挂载 |

---

## 八、与历史报告的差异

工作目录中已存在三份更早的报告，本报告不覆盖它们，但需指出其中已过期的结论：

| 文件 | 日期 | 过期/需修正之处 |
|---|---|---|
| `report.md` | 2026-10-25 | 声称 **"pip 不存在"** —— 现已不成立，pip 25.0.1 可用 |
| `python-env-report.md` | 2026-10-25 | 仅覆盖 Python，未涉及 Lua 与 SQLite 编译选项 |
| `environment-report.md` | 2026-10-26 | 通用环境报告，未深入 SQLite 扩展能力（FTS/RTree/math 缺失） |

本次检查的核心增量：**SQLite 缺少 FTS5/RTree/数学函数且禁止加载扩展**，这一限制在之前的报告中未被记录。

---

## 九、附录：复现命令

所有命令均在本机实测通过（除标注"预期失败"者）。

### 9.1 版本与基础

```sh
python3 -V
python -V
lua -v
sqlite3 --version
tcc -v
```

### 9.2 Python 模块与 pip

```sh
python3 -c 'import sys, ssl, sqlite3; print(sys.version.split()[0], sqlite3.sqlite_version, ssl.OPENSSL_VERSION)'

python3 -m pip --version

# 逐项探测标准库
python3 - <<'PY'
import importlib
for m in ("ssl","zlib","bz2","lzma","ctypes","sqlite3","json","hashlib",
          "decimal","curses","readline","multiprocessing","asyncio",
          "venv","ensurepip","tkinter","numpy","requests"):
    try:
        importlib.import_module(m); print(f"{m:16} OK")
    except Exception as e:
        print(f"{m:16} MISSING ({type(e).__name__})")
PY
```

### 9.3 Lua 模块

```sh
lua -e 'for p in package.path:gmatch("[^;]+") do print(p) end'
lua -e 'for p in package.cpath:gmatch("[^;]+") do print(p) end'

for m in os io string table math utf8 coroutine socket sqlite; do
  lua -e "local ok=pcall(require,'$m'); print('$m', ok and 'OK' or 'MISSING')"
done
```

### 9.4 SQLite 编译选项与扩展能力

```sh
# 完整编译选项
sqlite3 :memory: 'pragma compile_options;' | sort

# 扩展能力实测（注意：必须用 heredoc，mksh 不解析 \x27）
sqlite3 :memory: <<'SQL'
SELECT sqlite_version(), 1+1;
SELECT json_extract('{"a":5}', '$.a');
SELECT 'fts5', sqlite_compileoption_used('ENABLE_FTS5');
SELECT 'rtree', sqlite_compileoption_used('ENABLE_RTREE');
SELECT 'math', sqlite_compileoption_used('ENABLE_MATH_FUNCTIONS');
SQL
```

预期失败示例（用于确认限制真实存在）：

```sh
sqlite3 :memory: 'create virtual table b using fts5(x);'
# Error: no such module: fts5

python3 -c "import sqlite3; sqlite3.connect(':memory:').execute('create virtual table b using fts5(x)')"
# sqlite3.OperationalError: no such module: fts5

python3 -c "import sqlite3; print(sqlite3.connect(':memory:').execute('select sqrt(4)').fetchone())"
# sqlite3.OperationalError: no such function: sqrt
```

### 9.5 系统信息

```sh
uname -a
df -h /storage/emulated/0/AtomCode
grep -E 'MemTotal|MemAvailable' /proc/meminfo
```

---

## 十、未验证事项

以下内容本次**未实测**，如需使用请先自行验证：

1. **pip 联网安装是否可行**（网络连通性、镜像源、能否编译 C 扩展）。
2. **`cc` 的实际后端**（`<bin>/cc` 存在，但未确认指向 tcc 还是其他）。
3. **tcc 能否编译出可被 Lua `require` 的 `.so`**（noexec 挂载与工具链兼容性均需实测）。
4. **Lua 的 `./?.so` 路径实际是否可加载**（cpath 中存在，但本机无可用 `.so` 可测）。
