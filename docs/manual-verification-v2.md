# 手动验证清单 v2

对 Android 运行时能力的逐项验证清单。**v2 的第一件事是核对产物版本**——上一轮报告的三项「不符」全部来自一个 10-26 19:59 的旧 `cpython` 包（`593e8926…`），而不是代码缺陷。

## v1 → v2 的变更

| 变更 | 原因 |
|---|---|
| 新增**第 0 步：产物核验**（`sha256` + 已装标记） | 上一轮用旧包验证，把「包装错」误判成「功能坏」 |
| 第 4 步预期值 `10` → **`8`** | v1 括号里写 `3+5=8`，却把结论写成 10（文档算错，功能正常） |
| 第 6 步 exec 边界改写 | v1 说「禁止 exec」不成立：`execSync`/`spawnSync` 执行**系统命令**是通的，失败的是**重执行 node 自身** |
| numpy 依赖归属 | 伴随库（`libopenblas`/`libc++_shared`）已从运行时包移到 numpy 包自带的 `lib/python/.libs/`，由 shim 预加载；运行时包不再代持 |
| 排查表 numpy 行改写 | 「重装 cpython」已不是正确处置 |

## 验证者须知

- 命令都在 AtomCode 自带 shell（入口 A）里执行；`PATH` / `HOME` / `TMPDIR` 由宿主设好。
- 需要 `cd "$HOME"` 的命令都已标注；工作目录 `/storage/emulated/0/AtomCode` 下没有 `packages/`。
- 每项都要如实记录**实测输出**，与预期不符时给出**证据链**（文件是否存在、符号是否存在、`sha256` 对比），不要只写「失败」。
- **临时文件只留在当前工作目录**（`tmp/…`）。往 `$HOME`、`/data` 写会被工作区门禁拦下并要求审批；无头运行（没人点批准）时整个回合会停在那里。
- 完成后把报告写到 `/storage/emulated/0/AtomCode/manual-verification-report-v2.md`，格式见文末。

## 第 0 步：产物版本核验（必过，否则后面结果无意义）

```sh
sha256sum /sdcard/Download/*.tar.gz
ls -l /sdcard/Download/*.tar.gz
```

> 这两条要在**能读共享存储的 shell** 里跑（应用自带 shell，或 `adb shell`）。`adb shell run-as com.atomcode.app …` 没有存储权限，会静默输出空——别把它当成「哈希不对」。

**哈希以仓库的 `market.json` 为准**，不要在这里抄第二份：抄一份就会漂移一次——包体重新
发布后，文档里的旧哈希会让人把「正常的包」误判成「包坏了」（这次就踩过：文档写着
`8ac028af…`，而市场里的 cpython 已经是 `b17c99d3…`）。

主机侧直接比对设备上的实际哈希与清单：

```sh
adb shell 'sha256sum /sdcard/Download/atomcode-plugins/packages/*.tar.gz' > /tmp/device.sha256
python3 - <<'COMPARE'
import json, pathlib
market = json.loads(pathlib.Path('atomcode-plugins/market.json').read_text(encoding='utf-8'))
expected = {p['artifact']['path'].rsplit('/', 1)[-1]: p['sha256'] for p in market['plugins']}
seen = {}
for line in pathlib.Path('/tmp/device.sha256').read_text().splitlines():
    digest, _, path = line.partition('  ')
    seen[path.rsplit('/', 1)[-1]] = digest.strip()
missing = [name for name in expected if name not in seen]
wrong = [name for name, digest in seen.items() if name in expected and expected[name] != digest]
print(f'清单 {len(expected)} 个包；设备缺 {len(missing)}，哈希不符 {len(wrong)}')
for name in missing:
    print('  缺', name)
for name in wrong:
    print('  不符', name)
COMPARE
```

已装内容的标记位（`cd "$HOME"` 后执行）：

```sh
ls packages/cpython/lib/python3.12/lib-dynload | grep -c shmem      # 期望 1（_posixshmem）
ls packages/cpython/lib | grep -Ec 'openblas|libc\+\+'              # 期望 0（运行时不再代持）
ls python-packages/.libs                                            # 期望 libc++_shared.so libopenblas.so
ls packages                    # cffi cheaders cpython lua node numpy quickjs sqlite3 svg text
```

任何一项不符：先从对应包重装（哈希必须与上表一致），再继续。

## 第 1 步：Python

```sh
python3 -V
python3 -c "import sys; print(repr(sys.executable))"
```

预期：`Python 3.12.10`；`'/data/user/0/com.atomcode.app/files/atomcode/bin/python3'`（不是空串）。

```sh
python3 -c "import subprocess,sys; r=subprocess.run([sys.executable,'-c','print(42)'],capture_output=True,text=True); print(r.returncode, r.stdout.strip())"
```

预期：`0 42`。

```sh
cat > tmp/check.py <<'PY'
import multiprocessing as mp, os
def work(p): open(p,"w").write(f"child {os.getpid()}")
if __name__ == "__main__":
    out = os.path.join(os.environ["HOME"], "tmp", "spawn-check.txt")
    ctx = mp.get_context("spawn"); p = ctx.Process(target=work, args=(out,))
    p.start(); p.join(10); print("exit", p.exitcode, os.path.exists(out))
PY
python3 tmp/check.py
```

预期：`exit 0 True`。必须存成文件跑（`spawn` 需重新导入 `__main__`）。

## 第 2 步：SQLite

命令行（完整构建）：

```sh
sqlite3 :memory: "create virtual table b using fts5(x); insert into b values('hello'); select count(*) from b;"
sqlite3 :memory: "select sqrt(16), pow(2,10);"
sqlite3 :memory: "create virtual table r using rtree(id,minx,maxx); insert into r values(1,0,10); select count(*) from r;"
```

预期：`1`、`4.0|1024.0`、`1`。

Python 侧（与命令行同一个库）：

```sh
python3 -c "import sqlite3; c=sqlite3.connect(':memory:'); print(hasattr(c,'enable_load_extension')); c.enable_load_extension(True); print('enabled'); print(sqlite3.sqlite_version)"
```

预期：`True` / `enabled` / `3.50.4`。

## 第 3 步：Lua

```sh
lua -e "print(package.cpath:match('^[^;]+'))"
lua -e 'local cjson=require("cjson"); local s=cjson.encode({name="atomcode",nums={1,2,3}}); print(s); print(cjson.decode(s).name)'
```

预期：`/data/user/0/com.atomcode.app/files/atomcode/lua/?.so`；JSON 键序可能不同（Lua 表遍历顺序），但解码后 `atomcode` 必须正确。

## 第 4 步：tcc + cheaders 编 C

```sh
P=$HOME/packages/cheaders
cat > tmp/probe.c <<'EOF'
#include <stdio.h>
#include <string.h>
int probe(int x){ char b[64]; snprintf(b,sizeof b,"v=%d",x); return (int)strlen(b)+x; }
EOF
tcc -shared -fPIC -nostdlib -DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD \
    -I$P/include -I$P/builtin -I$P/include/python3.12 -L$P/lib -lc \
    -o tmp/probe.so tmp/probe.c
python3 -c "import ctypes; l=ctypes.CDLL('tmp/probe.so'); l.probe.restype=ctypes.c_int; print([(x, l.probe(x)) for x in (5,7,100)])"
```

预期：`[(5, 8), (7, 10), (100, 105)]`（`"v=5"` 长度 3，3+5=8；`"v=100"` 长度 5，5+100=105）。

## 第 5 步：numpy（插件形态，UI 操作 + 命令核验）

1. 插件页 → 「本机插件包」应扫到 `numpy-1.26.2-arm64.tar.gz`。
2. 若已装，先卸载：观察 `python-packages` 里 `numpy`、`numpy-1.26.2.dist-info`、`.libs` 三个链接消失。
3. 重新安装：应看到进度条（读取 → 解包 x/y 文件 → 建立链接）。
4. 核验：

```sh
ls -la python-packages | grep -E 'numpy|\.libs'
python3 - <<'PY'
import numpy as np
print(np.__version__)
a = np.arange(9, dtype=float).reshape(3,3)
print(a @ (np.eye(3)*2))
print(float(np.linalg.det(a)), float(a.sum()))
import numpy.core._multiarray_umath as m
print("loaded from", m.__file__)
PY
```

预期：三个链接（`numpy`、`numpy-1.26.2.dist-info`、`.libs`）；`1.26.2`；`[[0,2,4],[6,8,10],[12,14,16]]`；`0.0 36.0`。

5. 再次卸载，`python3 -c "import numpy"` 应重新报 `ModuleNotFoundError`；`ls python-packages` 里 `numpy`/`.libs` 都应消失。

## 第 6 步：node（libnode 嵌入）

```sh
node -e 'console.log(process.version, process.platform, process.arch, 1+1)'
node -e 'console.log([1,2,3].map(x=>x*2), require("fs").existsSync("."))'
```

预期：`v18.20.4 android arm64 2`；`[ 2, 4, 6 ] true`。

边界（**这部分 v1 写错了，以本节为准**）：

```sh
node -e 'console.log(process.execPath)'
node -e 'console.log(require("child_process").execSync("id",{encoding:"utf8"}).split("\n")[0])'
node -e 'const cp=require("child_process"); const r=cp.spawnSync(process.execPath,["-e","1"]); console.log("status", r.status)'
node -e 'const cp=require("child_process"); const r=cp.fork("-e", [], {silent:true}); console.log("fork pid", r.pid)'
```

预期：

- `process.execPath` 指向 APK 里的 `…/lib/arm64/libpluginhost.so`。
- `execSync("id")` **成功**，输出 `uid=10348(u0_a348) …`——执行系统命令不受限制。
- `spawnSync(process.execPath, …)` 返回 `status 2`（node 不能重新执行自己）。
- `fork` 同样失败（子进程以 2 退出）。

即边界是「不能自举 node」，不是「禁止 exec」。

## 第 7 步：Zig

```sh
export ZIG_LIB_DIR=$HOME/zig/lib
zig version
```

预期：`0.13.0`。

```sh
cat > tmp/zigprobe.c <<'EOF'
#include <stdio.h>
#include <string.h>
int zigprobe(int x){ char b[64]; snprintf(b,sizeof b,"zig=%d",x); return (int)strlen(b)+x; }
EOF
zig cc -shared -fPIC -o tmp/zigprobe.so tmp/zigprobe.c
python3 -c "import ctypes; l=ctypes.CDLL('tmp/zigprobe.so'); l.zigprobe.restype=ctypes.c_int; print(l.zigprobe(5))"
```

预期：`10`（`"zig=5"` 长度 5，5+5=10）。

陷阱复核（两项都要实测并记录）：

```sh
(unset ZIG_LIB_DIR; zig cc -shared -fPIC -o tmp/x.so tmp/zigprobe.c)   # 期望报 FileNotFound
zig cc -shared -fPIC -o tmp/y.so tmp/zigprobe.c                        # 期望一条 "falling back to default ABI" 警告，但产物可用
```

注意：`zig version` 在缺 `ZIG_LIB_DIR` 时**照常输出版本号**，所以只测版本号会误判环境正常。

## 第 8 步：Doze（熄屏长回合）

先看豁免状态：设置 → 应用 → AtomCode → 电池 → 「不限制 / 允许后台活动」；或 `adb shell cmd deviceidle whitelist`。

两种状态各做一遍：给 agent 发「先 `date +%H:%M:%S`，再 `sleep 120`，再 `date +%H:%M:%S`，把两行给我」，它一开始跑就锁屏，等 3 分钟看结果。

| 状态 | 预期 |
|---|---|
| 已豁免 | 两行时间相差约 120 秒，回合正常收尾 |
| 未豁免 | 可能卡住（厂商冻结器）；这是平台边界，记录现象即可 |

## 第 9 步：清理（可选，属用户取舍）

- 插件页卸载 `node`（约 17 MB）、`numpy`（约 9.5 MB）如不需要。
- `$HOME/zig`（约 210 MB）与 `bin/zig` 软链如不需要可删。
- `/sdcard/Download/*.tar.gz` 可删。
- 电池豁免不想保留就在设置里改回。

## 排查表

| 现象 | 先查这个 |
|---|---|
| 某项功能与预期不符 | **先做第 0 步**：`/sdcard/Download` 里包的 `sha256` 是否与表一致；不一致就重新拿到正确包再装 |
| `import numpy` 报 `No module named` | 通过 agent shell 跑（`HOME` 必须是产品目录）；`ls -l $HOME/python-packages` 有没有 `numpy` 链接 |
| numpy 报 `libopenblas.so` / `libc++_shared.so` not found | `ls python-packages/.libs`（numpy 包自带）；缺就重装 numpy 包 |
| `import _posixshmem` 失败 | `ls packages/cpython/lib/python3.12/lib-dynload \| grep shmem`；缺说明 cpython 包是旧的（对照第 0 步哈希重装） |
| `enable_load_extension` 为 False | 同上，cpython 包过旧 |
| `tcc` 找不到头文件 | 三个 `-I` 与 `-nostdlib`、`-DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD` 是否都带了 |
| `zig cc` 报 `unable to find zig installation directory` | `ZIG_LIB_DIR` 没设（`zig version` 不报错，别被骗） |
| `zig cc` 报 `unable to make temporary file` | `TMPDIR` 不可写；agent shell 里已设好，纯 adb shell 要自己 `export TMPDIR=$HOME/tmp` |
| 插件安装报 `already exists in python-packages` | 那个名字被真实目录占用（不是软链），手工删掉再装 |

## 报告格式要求

写到 `/storage/emulated/0/AtomCode/manual-verification-report-v2.md`，至少包含：

1. **产物核验结果**：六个包的 `sha256` 与字节数，逐项标注是否与第 0 步的表一致。
2. **逐项结果表**：`步骤 / 命令 / 预期 / 实测 / 结论（通过|不符|清单写错）`。
3. **缺陷节**：每个「不符」给出证据链（哪个文件不存在、哪个符号缺失、`sha256` 对比、`readelf`/`strings` 输出），并区分「包装错」与「代码缺陷」。
4. **清单勘误节**：文档自身写错的地方（预期值算错、边界描述不准）。
5. **未能验证项**：说明原因（需要人工 UI 操作 / 需要锁屏等待 / 无 adb 等）。
6. **复现命令**：能让人照着重跑。
