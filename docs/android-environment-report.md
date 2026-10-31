# 环境报告

- **生成时间**：2026-10-26
- **工作目录**：`/storage/emulated/0/AtomCode`
- **系统**：Linux 5.15.197-android13-8 (aarch64, Toybox)
- **Shell**：`/system/bin/sh`（mksh，非 bash）

所有额外工具均来自 AtomCode 私有 bin 目录：`/data/user/0/com.atomcode.app/files/atomcode/bin/`

---

## 运行时

| 工具 | 版本 | 路径 |
|---|---|---|
| Python | **3.12.10** | `.../atomcode/bin/python3`（`python`、`cpython` 为同一份） |
| pip | 25.0.1 | 已可用，site-packages 在 `.local/lib/python3.12` |
| Lua | **5.4.7** | `.../atomcode/bin/lua` |
| SQLite | **3.50.4** (2025-07-30, 64-bit) | `.../atomcode/bin/sqlite3` |
| C 编译器 | **tcc 0.9.28rc** | `cc` 与 `tcc` 均指向它 |
| make | GNU Make 4.4.1 | 为 `aarch64-unknown-linux-android` 构建 |
| git | 2.47.1 | 含 http 远程助手，可 clone/push |

## 辅助工具

| 工具 | 版本 | 工具 | 版本 |
|---|---|---|---|
| jq | 1.7.1 | curl | 8.11.1（OpenSSL 3.0.16 + zlib 1.3.0.1） |
| rg (ripgrep) | 15.2.0 | tar / unzip / gzip / xxd / strings | 系统自带 |

## 缺失的工具

`node`、`npm`、`cargo`、`perl`、`wget`、`gcc`、`clang`

- 均不可用，且无包管理器，无法安装。
- C 代码只能走 **tcc**：不支持完整 GCC 扩展与部分 `-O2` 优化。

## Python 模块实测

### 标准库

| 模块 | 状态 |
|---|---|
| sqlite3 | ✅ |
| ssl | ✅ |
| hashlib | ✅ |
| json | ✅ |
| re | ✅ |
| ctypes | ✅ |
| zlib | ✅ |
| urllib.request | ✅ |
| http.client | ✅ |
| asyncio | ✅ |
| multiprocessing | ✅ |
| tkinter | ❌ ModuleNotFoundError（无 GUI） |
| lua | ❌ ModuleNotFoundError |

### 第三方包（pip list）

| 包 | 版本 |
|---|---|
| requests | 2.34.2 |
| certifi | 2026.10.23 |
| urllib3 | 2.8.0 |
| charset-normalizer | 3.5.2 |
| idna | 3.20 |
| attrs | 26.1.0 |
| packaging | 26.3 |
| six | 1.17.0 |
| tzdata | 2026.5 |
| setuptools | 84.2.1 |
| wheel | 0.48.0 |
| pip | 25.0.1 |

### 缺失的第三方包

`numpy`、`cffi`、`lupa`（Lua-Python 桥接）

---

## 使用注意

1. **共享存储挂载为 noexec**
   脚本必须用 `sh script.sh` 运行；直接 `./script.sh` 会 `Permission denied (exit 126)`，即使已 `chmod +x`。

2. **Shell 是 mksh 不是 bash**
   - 支持：`[[ ]]`、`set -o pipefail`、数组、brace expansion、heredoc、herestring `<<<`、`local`、`${v:1:3}`、`${v/a/b}`、`case`、`trap`、`$(( ))`、函数、子 shell
   - 不支持：`[[ ... =~ ... ]]`（用 `case` 做 glob 匹配）、进程替换 `<( )` / `>( )`、`select`

3. **非交互式 Shell**
   不能启动 REPL（python/node/irb）、编辑器（vim/nano）、分页器（less/more）或全屏程序（top/htop）——它们收不到输入，会阻塞到超时。改用 `python3 -c '…'` 或脚本文件。

---

## 总体结论

Python + SQLite + Lua + tcc 的组合足够支撑：脚本编写、数据查询、C 小工具编译、HTTP 请求。

限制项：

- **无 numpy** — 数值计算需用纯 Python 或另寻方案
- **无 node/npm** — 前端工具链不可用
