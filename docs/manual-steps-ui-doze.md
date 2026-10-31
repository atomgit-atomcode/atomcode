# 手动验证步骤：插件页 UI 与 Doze

这两项只能人工做（要点按钮 / 要锁屏等时间）。每步都写了「该看到什么」，照着走即可。

当前设备状态（做完清理之后）：

- `packages`：`cheaders cpython lua sqlite3`
- `python-packages`：只有 `bin`（numpy 已卸载）
- `$HOME/zig` 与 `bin/zig` 已删除；zig 二进制在 helper APK（`com.atomcode.tools`）里
- 插件包都在 `/sdcard/Download/`：`numpy-1.26.2-arm64.tar.gz`（9.5 MB）、`cpython-3.12.10-arm64.tar.gz`（24.8 MB）等

---

## A. 插件页 UI 流程（约 5 分钟）

### A1. 扫包与列表

1. 打开 AtomCode → 侧栏/设置里的「插件」页。
2. 找到「本机插件包」区域，等扫描结束。

**该看到**：`numpy 1.26.2` 一行（来自 `/sdcard/Download/numpy-1.26.2-arm64.tar.gz`），并按包名分组；已装的 `cpython`、`lua`、`sqlite3`、`cheaders` 出现在「已安装」区域，带版本号与体积。

如果扫不到 numpy：确认 `/sdcard/Download/numpy-1.26.2-arm64.tar.gz` 还在（`adb shell ls -l /sdcard/Download`），再点「重新扫描」。

### A2. 安装并观察进度条

1. 在 `numpy 1.26.2` 那行点「安装」。
2. 盯着页面顶部（列表**上方**）出现的进度条。

**该看到**（这是上一轮专门修好的「黑箱安装」）：

| 阶段文字 | 说明 |
|---|---|
| 读取安装包 | 从 `/sdcard/Download` 读文件 |
| 校验 SHA256 | 校验通过才继续 |
| 解包 | 显示 `x/y 个文件` 且**百分比在动**（按压缩字节算，不是空转） |
| 建立命令链接 | 最后一步，很快 |

装完应提示「已安装 numpy 1.26.2」。如果进度条一直不动且没有阶段文字，记录当时的阶段名。

### A3. 装完的痕迹

在 AtomCode 的 shell 里跑：

```sh
ls -la python-packages | grep -E 'numpy|\.libs'
python3 -c "import numpy as np; print(np.__version__)"
```

**该看到**：三个软链（`numpy`、`numpy-1.26.2.dist-info`、`.libs`），以及 `1.26.2`。

### A4. 卸载与「清理精确」

1. 回到插件页，「已安装」里点 numpy 的「卸载」。
2. 再跑：

```sh
ls -la python-packages | grep -E 'numpy|\.libs' || echo "已清干净"
python3 -c "import numpy" 2>&1 | tail -1
```

**该看到**：`已清干净`，且第二行报 `ModuleNotFoundError: No module named 'numpy'`。这正是「卸载按记录精确摘除、不留死文件」。

### A5. （可选）大包进度条

`numpy` 只有 9.5 MB，进度条闪得快。想看清阶段变化：把 `cpython` 卸载再装一次（24.8 MB，阶段停留更久）。

⚠️ 卸载 cpython 会让 `python3` 暂时退回内置的 RustPython（`Python 3.14.0.alpha`），**装回来之前 numpy/node 都用不了**。不熟就别做这一步。

---

## B. Doze：熄屏长回合（约 15 分钟，两种状态）

### B1. 先确认豁免状态

系统设置路径（OnePlus/ColorOS 大致是）：**设置 → 应用 → 应用管理 → AtomCode → 电池（耗电管理）→ 选「不限制 / 允许后台活动」**。

也可以在电脑上看：

```sh
adb shell cmd deviceidle whitelist
```

列表里有 `com.atomcode.app` = 已豁免；没有 = 未豁免。

### B2. 已豁免状态下的测试

1. 回到 AtomCode，发这条消息：

   > 请先执行 `date +%H:%M:%S` 并把时间告诉我，然后 `sleep 120`，再执行一次 `date +%H:%M:%S` 并告诉我。两条命令分开执行，最后把两个时间都给我。

2. 它开始跑命令后，**立刻锁屏**（电源键），期间不要解锁、不要碰手机。
3. 顺手看一眼通知栏：**该看到**「Working on 1 conversation(s)」（或类似进行中的文案）。
4. 等 3 分钟后解锁查看。

**该看到**：两个时间相差约 **120 秒**，并且回合正常收尾（有总结文字，不是卡在半截）。
再确认通知回到空闲态（`Idle — the agent runs on this device`）。

### B3. 未豁免状态下的对照（可选）

1. 把电池设置改回「智能限制 / 优化」，或用 adb 撤销：
   ```sh
   adb shell cmd deviceidle whitelist -com.atomcode.app
   ```
2. 重复 B2 的步骤。

**可能的两种结果**（都算「记录到了」，不算失败）：

| 结果 | 含义 |
|---|---|
| 两个时间仍相差约 120 秒 | 该状态也能撑住，记下来即可 |
| 第二个时间不出现 / 回合卡住 | 厂商冻结器把进程冻了（系统日志里会出现 `LocationFreezeProc … com.atomcode.app`）。这是平台边界：应用的唤醒锁挡不住 OEM 冻结，只有豁免能避免 |

### B4. 记录格式

把结果写成一行，例如：

```
已豁免：A 10:02:11 / B 10:04:13（差 122s），回合正常收尾，通知显示进行中 ✅
未豁免：A 10:20:05 / B 未出现，回合卡住，logcat 有 LocationFreezeProc ⚠️
```

---

## C. 做完之后

- 想留着 numpy：不用管；想清掉：插件页卸载。
- 想删 zig 的 jar 级残留：`$HOME/zig` 已删除，helper APK 里的二进制留着也无妨（不占运行内存，只在执行 zig 时加载）。
- 想恢复 zig（需要它的标准库）：

```sh
# 1) 解出标准库（电脑侧）
adb push dist/zig-lib.tar.gz /data/local/tmp/
adb shell 'run-as com.atomcode.app sh -c "cd files/atomcode; mkdir -p zig; tar xzf /data/local/tmp/zig-lib.tar.gz -C zig"'

# 2) 把 bin/zig 指向 helper APK 里的二进制
adb shell 'pm path com.atomcode.tools'   # 形如 /data/app/~~xxx/com.atomcode.tools-yyy/base.apk
# 把上面路径里的 base.apk 换成 lib/arm64/libzig.so，例如：
adb shell 'run-as com.atomcode.app sh -c "cd files/atomcode; ln -sf /data/app/~~xxx/com.atomcode.tools-yyy/lib/arm64/libzig.so bin/zig"'

# 3) 用的时候必须带这两个环境变量
export ZIG_LIB_DIR=$HOME/zig/lib
export TMPDIR=$HOME/tmp
zig version
```
