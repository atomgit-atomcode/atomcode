# 手动验证清单

真机上逐项验证当前 Android 运行时能力的顺序清单。每项都写了「预期结果」——不符时先看最后一节的排查表。

前提：插件包已放在 `/sdcard/Download/`（`cpython-3.12.10`、`lua-5.4.7`、`sqlite3-3.50.4`、`cheaders-1.0.0`、`numpy-1.26.2`、`node-18.20.4`）。

## 运行命令的两个入口

**入口 A（推荐，不用电脑）**：在 AtomCode 里直接让 agent 跑，或在它的终端下手敲。这个 shell 的 `PATH` / `HOME` / `TMPDIR` 都由宿主设好了，`python3`、`lua`、`sqlite3`、`node`、`zig` 直接可用。

**入口 B（用电脑 + adb）**，注意 `HOME` 必须指到产品目录，否则 `python-packages` 找不到：

```bash
adb shell 'run-as com.atomcode.app sh -c "cd files/atomcode; export HOME=\$PWD; unset LD_PRELOAD; <你的命令>"'
```

---

## 第 0 步：确认底座

| 命令 | 预期 |
|---|---|
| `python3 -V` | `Python 3.12.10` |
| `python3 -m pip -V` | `pip 25.0.1 from …/packages/cpython/…` |
| `sqlite3 --version` | `3.50.4 …` |
| `lua -v` | `Lua 5.4.7` |
| `ls packages` | 至少 `cpython lua sqlite3 numpy`（还可能有 `cheaders`、`node`） |

若 `python3` 显示 RustPython（`Python 3.14.0.alpha`），说明 `cpython` 包没装：在插件页装 `cpython-3.12.10`。

## 第 1 步：Python —— 三项以前坏掉的能力

```sh
python3 -c "import sys; print(repr(sys.executable))"
```

预期：`'/data/user/0/com.atomcode.app/files/atomcode/bin/python3'`（以前是空串）。

```sh
python3 -c "import subprocess,sys; r=subprocess.run([sys.executable,'-c','print(42)'],capture_output=True,text=True); print(r.returncode, r.stdout.strip())"
```

预期：`0 42`（解释器能重新执行自己）。

```sh
python3 - <<'PY'
import multiprocessing as mp, os
def work(p): open(p,"w").write(f"child {os.getpid()}")
if __name__ == "__main__":
    out = os.path.expanduser("tmp/spawn-check.txt")
    ctx = mp.get_context("spawn"); p = ctx.Process(target=work, args=(out,))
    p.start(); p.join(10); print("exit", p.exitcode, os.path.exists(out))
PY
```

预期：`exit 0 True`（`spawn` 以前因缺 `_posixshmem` 直接导入失败）。

这段必须存成文件跑（`python3 tmp/check.py`），不能 `-c`：`spawn` 要重新导入 `__main__`。

## 第 2 步：SQLite —— 全文检索与数学函数

```sh
sqlite3 :memory: "create virtual table b using fts5(x); insert into b values('hello'); select count(*) from b;"
sqlite3 :memory: "select sqrt(16), pow(2,10);"
sqlite3 :memory: "create virtual table r using rtree(id,minx,maxx); insert into r values(1,0,10); select count(*) from r;"
```

预期：`1`、`4.0|1024.0`、`1`（以前分别是 `no such module: fts5`、`no such function: sqrt`、`no such module: rtree`）。

```sh
python3 -c "import sqlite3; c=sqlite3.connect(':memory:'); print(hasattr(c,'enable_load_extension')); c.enable_load_extension(True); print('enabled')"
```

预期：`True` / `enabled`（可加载扩展以前被编译期禁掉）。

## 第 3 步：Lua —— C 模块能 `require`

```sh
lua -e "print(package.cpath:match('^[^;]+'))"
lua -e 'local cjson=require("cjson"); local s=cjson.encode({name="atomcode",nums={1,2,3}}); print(s); print(cjson.decode(s).name)'
```

预期：`/data/user/0/com.atomcode.app/files/atomcode/lua/?.so`（以前是 `/usr/local/...`）；`{"nums":[1,2,3],"name":"atomcode"}` 与 `atomcode`。

## 第 4 步：在设备上编 C（tcc + cheaders 包）

先确认 `cheaders` 已装（插件页装 `cheaders-1.0.0`），然后：

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
python3 -c "import ctypes; l=ctypes.CDLL('tmp/probe.so'); l.probe.restype=ctypes.c_int; print(l.probe(5))"
```

预期：`10`（`v=5` 长度 3 + 5）。

那两个参数不是可选的：不带 `-nostdlib` 会去找 `/usr/local/lib/tcc`，不带宏会在 `bits/ioctl.h` 报 `ioctl` 重定义。

## 第 5 步：numpy 作为插件（在 UI 里做）

1. 打开插件页 → 「本机插件包」应扫到 `numpy-1.26.2-arm64.tar.gz`。
2. 先点已装的 `numpy` **卸载**，观察 `python-packages` 里 `numpy` 链接消失。
3. 在插件页安装 `numpy 1.26.2`，过程中应看到进度条（读取 → 解包 x/y 文件 → 建立链接）。
4. 验证：

```sh
ls -l python-packages | grep numpy
python3 - <<'PY'
import numpy as np
print(np.__version__)
a = np.arange(9, dtype=float).reshape(3,3)
print(a @ (np.eye(3)*2))
print(float(np.linalg.det(a)), float(a.sum()))
PY
```

预期：软链两条（`numpy`、`numpy-1.26.2.dist-info`）；`1.26.2`；矩阵 `[[0,2,4],[6,8,10],[12,14,16]]`；`0.0 36.0`。

5. 再卸载一次，`python3 -c "import numpy"` 应重新报 `ModuleNotFoundError`——这就是「清理精确」的意思。

## 第 6 步：node（libnode 嵌入）

```sh
node -e 'console.log(process.version, process.platform, process.arch, 1+1)'
node -e 'console.log([1,2,3].map(x=>x*2), require("fs").existsSync("."))'
```

预期：`v18.20.4 android arm64 2` 与 `[2,4,6] true`。

想确认已知边界（应**失败**，失败才是对的）：

```sh
node -e 'require("child_process").execSync("id")'
```

预期：报错（`process.execPath` 是 `pluginhost`，且禁止 exec）。这不算 bug，是设计边界。

## 第 7 步：Zig（能编共享库）

```sh
export ZIG_LIB_DIR=$HOME/zig/lib
zig version
cat > tmp/zigprobe.c <<'EOF'
#include <stdio.h>
#include <string.h>
int zigprobe(int x){ char b[64]; snprintf(b,sizeof b,"zig=%d",x); return (int)strlen(b)+x; }
EOF
zig cc -shared -fPIC -o tmp/zigprobe.so tmp/zigprobe.c
python3 -c "import ctypes; l=ctypes.CDLL('tmp/zigprobe.so'); l.zigprobe.restype=ctypes.c_int; print(l.zigprobe(5))"
```

预期：`0.13.0` 与 `10`。

两个陷阱：`ZIG_LIB_DIR` 必须设（二进制在 APK 里，`../lib` 不存在）；`TMPDIR` 在 agent 的 shell 里已由宿主设好，若你在纯 `adb` shell 里跑，要自己 `export TMPDIR=$HOME/tmp`，否则报 `unable to make temporary file`。

## 第 8 步：Doze —— 熄屏长回合

先看豁免状态：设置 → 应用 → AtomCode → 电池 → 选「不限制 / 允许后台活动」（不同 OEM 措辞不同）。或用 adb：`adb shell cmd deviceidle whitelist` 里有没有 `com.atomcode.app`。

测试动作（两种状态各做一遍）：

1. 给 agent 发：`请先用 date +%H:%M:%S 报一个时间，然后 sleep 120，再 date +%H:%M:%S 报第二个时间，把两行给我`。
2. 它一开始跑，**立刻锁屏**，别再操作手机。
3. 等 3 分钟，点开看两行时间。

| 状态 | 预期 |
|---|---|
| 已豁免 | 两个时间差约 120 秒，回合正常收尾；运行中通知显示「Working on 1 conversation(s)」 |
| 未豁免 | 可能卡住：第二个时间不出现。这是厂商冻结器（系统日志里能看到 `LocationFreezeProc … com.atomcode.app`），应用的唤醒锁挡不住它 |

不点对话框也能切换豁免：`adb shell cmd deviceidle whitelist -com.atomcode.app`（撤销）/ `+com.atomcode.app`（授予）。

## 第 9 步：验完后的清理

- 在插件页卸载体积大的插件：`node` 约 22 MB、`numpy` 约 17 MB。
- `zig` 的二进制在 APK 里（APK 因此到 112 MB），`$HOME/zig`（库 + sysroot）约 210 MB：不需要就 `rm -rf zig` 并删掉 `bin/zig` 软链。
- `/sdcard/Download/*.tar.gz` 那几份插件包可以删。
- 电池优化豁免不想保留，就在设置里改回「智能限制 / 优化」。

## 出问题怎么定位

| 现象 | 先查这个 |
|---|---|
| `import numpy` 报 `No module named` | 是否通过 agent 的 shell 跑（`HOME` 必须是产品目录）；`ls -l $HOME/python-packages` 有没有软链 |
| numpy 报 `libopenblas.so not found` | 运行时包是不是旧的：插件页重装一次 `cpython` |
| `tcc` 报头文件找不到 | 是否带了那三个 `-I` 与 `-nostdlib`、`-DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD` |
| `zig` 报 `unable to make temporary file` | `TMPDIR` 指到 `$HOME/tmp` |
| 插件安装报 `already exists in python-packages` | 那个名字被真实目录占了（不是软链），先手工删掉再装 |
