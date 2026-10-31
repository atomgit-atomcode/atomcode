# 手动验证报告

对 `manual-verification.md` 的逐项实测记录。

- 验证时间：2026-10-27
- 验证方式：入口 A（AtomCode 自带 shell，`PATH` / `HOME` / `TMPDIR` 由宿主设置）
- 设备：Android arm64，应用 uid `u0_a348`

## 结论摘要

命令可验证的检查点共 20 项：**17 项通过，3 项不符**。另有 **2 处清单本身写错**（数值算错、边界描述过时）。

| 类别 | 数量 | 明细 |
|---|---|---|
| 通过 | 17 | 第 0 步全部、第 1 步前 2 项、第 2 步命令行 3 项、第 3 步 2 项、第 4 步编译、第 6 步基础 2 项、第 7 步 2 项 |
| 不符 | 3 | spawn 缺 `_posixshmem`、`sqlite3` 无 `enable_load_extension`、`import numpy` 缺动态库 |
| 清单写错 | 2 | 第 4 步预期值应为 8 不是 10；第 6 步「禁止 exec」不成立 |

三个不符项中，两个（`_posixshmem`、`enable_load_extension`）和 numpy 的 `libpython3.12.so` 缺失，都能在 **cpython 安装包本身**里找到根因，重装无效。详见「缺陷」一节。

## 环境基线

| 项 | 值 |
|---|---|
| 工作目录 | `/storage/emulated/0/AtomCode` |
| `HOME` | `/data/user/0/com.atomcode.app/files/atomcode` |
| `TMPDIR` | `/data/user/0/com.atomcode.app/cache` |
| `LD_LIBRARY_PATH` / `LD_PRELOAD` / `PYTHONPATH` | 三者均为空（不是我人为清空的） |
| `bin/python3` | 软链到 `pluginhost`（即 `libpluginhost.so`，解释器以库形式嵌入） |
| `packages/` | `cheaders cpython lua node numpy sqlite3` |

`zig` 不在 `packages/` 下，按设计位于 `$HOME/zig`，`bin/zig` 为软链——与清单描述一致。

## 逐项结果

### 第 0 步：底座 —— 全部通过

| 命令 | 预期 | 实测 | 结果 |
|---|---|---|---|
| `python3 -V` | `Python 3.12.10` | `Python 3.12.10` | 通过 |
| `python3 -m pip -V` | `pip 25.0.1 from …/packages/cpython/…` | `pip 25.0.1 from /data/user/0/com.atomcode.app/files/atomcode/packages/cpython/lib/python3.12/site-packages/pip` | 通过 |
| `sqlite3 --version` | `3.50.4 …` | `3.50.4 2025-07-30 19:33:53 …(64-bit)` | 通过 |
| `lua -v` | `Lua 5.4.7` | `Lua 5.4.7` | 通过 |
| `ls packages` | 至少 `cpython lua sqlite3 numpy` | `cheaders cpython lua node numpy sqlite3` | 通过 |

注意：清单里的 `ls packages` 需在 `$HOME` 下执行；工作目录 `/storage/emulated/0/AtomCode` 下没有 `packages`。

### 第 1 步：Python —— 2 项通过，1 项失败

| 项 | 预期 | 实测 | 结果 |
|---|---|---|---|
| `sys.executable` | `'/data/.../files/atomcode/bin/python3'` | `'/data/user/0/com.atomcode.app/files/atomcode/bin/python3'` | 通过 |
| subprocess 自重执行 | `0 42` | `0 42` | 通过 |
| multiprocessing `spawn` | `exit 0 True` | `ModuleNotFoundError: No module named '_posixshmem'` | **不符** |

失败发生在 `p.start()`：`popen_spawn_posix` → `from . import resource_tracker` → `import _posixshmem` 直接导入失败。见缺陷 D1。

### 第 2 步：SQLite —— 命令行 3 项通过，Python 侧 1 项失败

| 项 | 预期 | 实测 | 结果 |
|---|---|---|---|
| fts5 全文检索 | `1` | `1` | 通过 |
| 数学函数 | `4.0\|1024.0` | `4.0\|1024.0` | 通过 |
| rtree | `1` | `1` | 通过 |
| `enable_load_extension` | `True` / `enabled` | `False` / `AttributeError: 'sqlite3.Connection' object has no attribute 'enable_load_extension'` | **不符** |

命令行 `sqlite3` 是完整构建（fts5、rtree、数学函数都在），坏的是 Python 侧那个 `_sqlite3` 扩展。见缺陷 D2。

### 第 3 步：Lua —— 全部通过

| 项 | 预期 | 实测 |
|---|---|---|
| `package.cpath` 首项 | `…/files/atomcode/lua/?.so` | `/data/user/0/com.atomcode.app/files/atomcode/lua/?.so` |
| `require("cjson")` 编解码 | `{"nums":[1,2,3],"name":"atomcode"}` 与 `atomcode` | `{"name":"atomcode","nums":[1,2,3]}` 与 `atomcode` |

JSON 键序不同（Lua 表的遍历顺序），语义等价，属正常。

### 第 4 步：tcc + cheaders 编 C —— 编译通过，清单预期值算错

编译命令（`-nostdlib` 与 `-DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD` 两个参数都带上了）执行成功，`ctypes` 加载调用正常。

- 清单预期：`10（v=5 长度 3 + 5）`
- 实际值：**8**，因为 `"v=5"` 长度是 3，3+5=8，清单括号里的算式与自己写的预期值矛盾。

交叉验证确认函数行为正确（返回值恒等于 `strlen("v=%d") + x`）：

| 入参 | 实测 | 手算 | 结果 |
|---|---|---|---|
| 5 | 8 | `"v=5"`=3 → 3+5=8 | 一致 |
| 7 | 10 | `"v=7"`=3 → 3+7=10 | 一致 |
| 100 | 105 | `"v=100"`=5 → 5+100=105 | 一致 |

对照第 7 步：`"zig=5"` 长度 5，5+5=10，那份预期是对的——两节对照即可确认第 4 步是文档算错，不是功能坏。

### 第 5 步：numpy —— 软链正确，默认导入失败

软链两条，与预期完全一致：

```
numpy -> …/packages/numpy/lib/python/numpy
numpy-1.26.2.dist-info -> …/packages/numpy/lib/python/numpy-1.26.2.dist-info
```

但 `import numpy` 失败：

```
ImportError: dlopen failed: library "libpython3.12.so" not found:
needed by …/packages/numpy/lib/python/numpy/core/_multiarray_umath.so in namespace (default)
```

补上缺失的动态库之后，numpy 功能逐项对上预期：

| 项 | 预期 | 实测 |
|---|---|---|
| 版本 | `1.26.2` | `1.26.2` |
| `a @ (np.eye(3)*2)` | `[[0,2,4],[6,8,10],[12,14,16]]` | `[[0,2,4],[6,8,10],[12,14,16]]` |
| `det(a)` / `sum(a)` | `0.0 36.0` | `0.0 36.0` |

即：numpy 本身完好，坏的只是动态库解析。见缺陷 D3。

### 第 6 步：node —— 基础 2 项通过，「禁止 exec」不成立

| 项 | 预期 | 实测 | 结果 |
|---|---|---|---|
| 版本/平台/架构 | `v18.20.4 android arm64 2` | `v18.20.4 android arm64 2` | 通过 |
| map + fs | `[ 2, 4, 6 ] true` | `[ 2, 4, 6 ] true` | 通过 |
| exec 应失败 | 报错 | `execSync("id")` 成功返回 `uid=10348(u0_a348) …` | **清单写错** |

三种子进程方式全部成功：`execSync`、`spawnSync`、`execFileSync`。所以清单那句「`process.execPath` 是 `pluginhost`，且禁止 exec」错了一半。

`node` 包自带 README 的说法才是准确的，而且更窄：

- `process.execPath` = `/data/app/~~…/lib/arm64/libpluginhost.so`（实测确认）
- 后果是 **不能重新执行 node 自身**：`fork` 起的子进程 `exit 2`，`spawn(process.execPath, …)` 同样 `status: 2`
- 但执行系统里的其它可执行文件不受限制（`id`、`/system/bin/id` 都能跑）

也就是说边界是「不能自举 node」，不是「禁止 exec」。清单第 6 步后半段的描述需要改写。

### 第 7 步：Zig —— 全部通过

| 项 | 预期 | 实测 | 结果 |
|---|---|---|---|
| `zig version` | `0.13.0` | `0.13.0` | 通过 |
| `zig cc` 编译 + `ctypes` 调用 | `10` | `zigprobe(5) -> 10` | 通过 |

清单提到的两个陷阱**都仍然存在**，实测复核：

- 不设 `ZIG_LIB_DIR` 时：`zig cc` 报 `error: unable to find zig installation directory: FileNotFound`；而 `zig version` 照常输出版本号（所以只看版本号会误判环境没问题）。
- 即使设了 `ZIG_LIB_DIR`，编译仍会打印 `warning: Encountered error: FileNotFound, falling back to default ABI and dynamic linker.`，但产物正确、可加载——属良性警告，可忽略。

## 缺陷

### D1：cpython 包缺 `_posixshmem` → `multiprocessing` 的 spawn 不可用

证据链：

- `packages/cpython/lib/python3.12/lib-dynload/` 内无任何 `*shmem*` 文件（只有 `resource.cpython-312.so`）
- `sys.builtin_module_names` 里也没有 `_posixshmem`（只有 `posix`），即既不是动态模块也不是内置模块
- `find $HOME -name '*posixshmem*'` 全产品目录无结果
- `/sdcard/Download/cpython-3.12.10-arm64.tar.gz` 内同样不含 `posixshmem` 条目

影响：`multiprocessing` 的 `spawn` / `forkserver` 起点直接导入失败，`fork` 起点不受影响。清单第 1 步把这项列为「已修好」，与实测相反。

### D2：Python 的 `_sqlite3` 编译期禁用了扩展加载

证据链：

- `sqlite3.connect(...)` 返回对象上 `hasattr(c, 'enable_load_extension')` 为 `False`
- `strings _sqlite3.cpython-312.so | grep -c enable_load_extension` → **0**
- 同一文件里既没有 `SQLITE_OMIT_LOAD_EXTENSION` 字面量，也没有 `sqlite3_enable_load_extension` 符号 → 构建时即被裁掉，不是运行时被禁
- 已装文件与 Download 安装包内同名文件 sha256 完全相同（`329e787835e5bd2e8405094b71cc90d136f2cd28aa233f549a3eae598aad9168`）

影响：Python 侧无法加载 SQLite 扩展；命令行 `sqlite3` 不受影响。清单第 2 步最后一项预期 `True / enabled`，实测为 `False / AttributeError`。

### D3：numpy 的动态库解析三重问题

三种独立原因叠加，导致默认 `import numpy` 必失败：

1. **`libpython3.12.so` SONAME 不匹配**
   - numpy 的 `_multiarray_umath.so`：`NEEDED  libpython3.12.so`
   - cpython 的 `packages/cpython/lib/libpython3.12.so.1.0`：`SONAME  libpython3.12.so.1.0`
   - 差一个 `.1.0` 后缀，按名字查不到。`LD_LIBRARY_PATH` 加一个同名软链后该错误消失 —— 说明只要文件名对上就能解决。

2. **`libopenblas.so` / `libc++_shared.so` 位置错误**
   - 两者只存在于 `packages/node/lib/`（`libopenblas.so` 10.3 MB、`libc++_shared.so` 1.4 MB）
   - `packages/numpy/` 内不含这两个库，`numpy-1.26.2-arm64.tar.gz` 里也不含（`grep openblas|libc++` 无命中）
   - `packages/cpython/lib/` 内也不含
   - 修好第 1 点后错误立刻变成 `dlopen failed: library "libopenblas.so" not found`，说明这是下一个拦路点

3. **cpython 的 shim 没做它该做的 preload**
   - `packages/numpy/README.md` 写明：「the `cpython` runtime package. Its shim preloads `libopenblas` and `libc++_shared` with `RTLD_GLOBAL`」
   - 实测 `packages/cpython/lib/plugin.so` 里引用的库名只有 `libpython3.12.so.1.0`（外加 `libc.so` / `libdl.so`），没有任何 `libopenblas`、`libc++_shared` 或 `dlopen` 字样
   - 即 cpython 包的 shim 与 numpy 的 README 承诺不符

清单排查表给的处置是「numpy 报 `libopenblas.so not found` → 插件页重装一次 cpython」。按本报告的证据，**重装无效**：安装包内既没有这两个库，shim 里也没有对应 preload 逻辑。

## 清单本身需要改的地方

| 位置 | 现状 | 建议 |
|---|---|---|
| 第 4 步预期 | `10（v=5 长度 3 + 5）` | 改为 `8`（3+5=8）；入参换成一个能让答案等于 10 的值也可以 |
| 第 6 步 exec 边界 | 「报错（`process.execPath` 是 `pluginhost`，且禁止 exec）」 | 改为「外部命令 `execSync`/`spawnSync` 可正常执行；失败的是重执行 node 自身：`fork` / `spawn(process.execPath)` 以 exit code 2 失败」 |
| 排查表 numpy 行 | 「重装 cpython」 | 重装无效，需按 D3 修包（补齐 SONAME 与两个库、或让 shim 真的 preload） |
| 第 1 步 spawn | 描述为已修好 | 当前安装包仍未修好（D1） |
| 第 2 步 load_extension | 描述为已修好 | 当前安装包仍未修好（D2） |

## 建议修复

按包归口，最小改动：

**cpython 包**
- `lib-dynload` 补 `_posixshmem.cpython-312.so`（或构建时不要裁掉该模块）
- `_sqlite3` 构建去掉 `SQLITE_OMIT_LOAD_EXTENSION`
- 库命名统一：要么让 `libpython3.12.so` 这个名字存在（软链或 SONAME 直接改成不带版本号），要么把 numpy 的 `NEEDED` 改成 `.1.0`；`package.json` 的 `preload` 与之一致
- shim 按 numpy README 的承诺，把 `libopenblas.so`、`libc++_shared.so` 以 `RTLD_GLOBAL` 预加载（并保证这两个库在 `packages/cpython/lib/` 内），或在 `packages/numpy/package.json` 里用 `preload` 声明它们

**numpy 包**
- 把 `libopenblas.so`、`libc++_shared.so` 收进自己的 `lib/`，并在 `package.json` 加 `preload` 字段，不要依赖它们偶然出现在 node 包里

**node 包**
- `packages/node/lib/` 里的 `libopenblas.so` 属误放（node 的 `package.json` 只 preload `libnode.so` 与 `libc++_shared.so`），确认后应移走，避免掩盖 cpython 侧的缺失

## 未能验证的项

| 项 | 原因 |
|---|---|
| 第 5 步 UI 流程（插件页扫包、卸载看软链消失、安装进度条、再卸载后 `ModuleNotFoundError`） | 需要人工在插件页操作 |
| 第 8 步 Doze 熄屏长回合（已豁免 / 未豁免两种状态） | 需要锁屏等待 3 分钟 |
| 入口 B（adb + `run-as`） | 本机无 adb 连接；入口 A 与入口 B 的 `HOME` 处理不同，结论以入口 A 为准 |
| 第 9 步清理（卸载 node / numpy、删 zig、删 Download 包） | 属用户取舍，未执行 |

## 复现命令

```sh
cd "$HOME"

# D1：spawn 缺 _posixshmem
cp /storage/emulated/0/AtomCode/tmp/check.py "$HOME/tmp/check.py" && python3 tmp/check.py
ls packages/cpython/lib/python3.12/lib-dynload | grep -Ei 'shmem|resource'
find "$HOME" -name '*posixshmem*'

# D2：sqlite3 无 enable_load_extension
python3 -c "import sqlite3; c=sqlite3.connect(':memory:'); print(hasattr(c,'enable_load_extension'))"
strings packages/cpython/lib/python3.12/lib-dynload/_sqlite3.cpython-312.so | grep -c enable_load_extension

# D3：numpy 的库解析
python3 -c "import numpy"            # -> libpython3.12.so not found
readelf -d packages/numpy/lib/python/numpy/core/_multiarray_umath.so | grep NEEDED
readelf -d packages/cpython/lib/libpython3.12.so.1.0 | grep SONAME
find "$HOME" -name 'libopenblas.so*' -o -name 'libc++_shared.so'
strings packages/cpython/lib/plugin.so | grep -Ei 'openblas|libc\+\+_shared'

# 第 4 步的真实预期值
tcc -shared -fPIC -nostdlib -DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD \
    -I$HOME/packages/cheaders/include -I$HOME/packages/cheaders/builtin \
    -I$HOME/packages/cheaders/include/python3.12 -L$HOME/packages/cheaders/lib -lc \
    -o tmp/probe.so tmp/probe.c
python3 -c "import ctypes; l=ctypes.CDLL('tmp/probe.so'); l.probe.restype=ctypes.c_int; print([ (x,l.probe(x)) for x in (5,7,100)])"

# 第 6 步的真实边界
node -e 'console.log(process.execPath)'
node -e 'console.log(require("child_process").execSync("id",{encoding:"utf8"}).split("\n")[0])'   # 成功
node -e 'const cp=require("child_process");const r=cp.spawnSync(process.execPath,["-e","1"]);console.log("status",r.status)'  # 2

# 第 7 步的 ZIG_LIB_DIR 陷阱
(unset ZIG_LIB_DIR; zig cc -shared -fPIC -o tmp/x.so tmp/zigprobe.c)   # FileNotFound
```

## 本次留下的临时文件

在 `$HOME/tmp/` 下（均由清单流程生成，未清理）：`check.py`、`probe.c`、`probe.so`、`zigprobe.c`、`zigprobe3.so`。

诊断过程中临时建立的 `$HOME/tmp/libcompat/`（兼容软链）与 `$HOME/tmp/cpchk/`（解包对照）已删除，未改动任何已安装插件。
