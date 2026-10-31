# 手动验证报告 v2（实测）

对 `manual-verification-v2.md` 的逐项实测。本轮**由 adb 直接执行**（入口 B，`run-as` + 显式 `HOME`），不经过前端 UI。

- 验证时间：2026-10-27
- 设备：OnePlus PLT140 / Android 16（SDK 36），应用 uid `u0_a348`
- 执行方式：`run-as com.atomcode.app sh /data/local/tmp/checks_v2.sh`（脚本内容见文末），哈希一步用 `adb shell` 跑（`run-as` 读不到 `/sdcard`）

## 结论摘要

命令可核验的部分**全部通过**。上一轮报告（`manual-verification-report.md`）的 3 项「不符」全部消失，根因是当时装的是 **10-26 19:59 的旧 `cpython` 包**（`593e8926…`），不是代码缺陷。

| 步骤 | 项数 | 通过 | 不符 |
|---|---|---|---|
| 第 0 步 产物核验（6 哈希 + 4 标记） | 10 | 10 | 0 |
| 第 1 步 Python | 4 | 4 | 0 |
| 第 2 步 SQLite | 4 | 4 | 0 |
| 第 3 步 Lua | 2 | 2 | 0 |
| 第 4 步 tcc + cheaders | 2 | 2 | 0 |
| 第 5 步 numpy（仅命令核验部分） | 3 | 3 | 0 |
| 第 6 步 node | 5 | 5 | 0 |
| 第 7 步 Zig | 4 | 4 | 0 |
| **合计** | **34** | **34** | **0** |

## 第 0 步：产物核验

`adb shell sha256sum /sdcard/Download/*.tar.gz`，与清单第 0 步表逐项比对：

| 包 | 清单 sha256（前 16 位） | 实测 | 结论 |
|---|---|---|---|
| `cpython-3.12.10-arm64.tar.gz` | `8ac028af0c96dd71` | 同左 | 通过 |
| `numpy-1.26.2-arm64.tar.gz` | `68c02d93643ef95e` | 同左 | 通过 |
| `node-18.20.4-arm64.tar.gz` | `33ee442b45a0b54b` | 同左 | 通过 |
| `cheaders-1.0.0-arm64.tar.gz` | `d086bcfa3f6804c0` | 同左 | 通过 |
| `lua-5.4.7-arm64.tar.gz` | `ab166cf5811f1857` | 同左 | 通过 |
| `sqlite3-3.50.4-arm64.tar.gz` | `e6e03ffef83d992c` | 同左 | 通过 |

`/sdcard/Download` 另有 `numpy-plugin.tar.gz`（`866f2b61…`），是上一轮遗留的旧命名副本，不在清单表内，不影响结论；建议删除以免混淆。

已装标记（`cd "$HOME"` 后执行）：

| 检查 | 预期 | 实测 |
|---|---|---|
| `lib-dynload` 里 `shmem` 计数 | 1 | `1` |
| 运行时 `lib/` 里 `openblas`/`libc++` 计数 | 0 | `0` |
| `python-packages/.libs` | `libc++_shared.so libopenblas.so` | 同左 |
| `ls packages` | `cheaders cpython lua node numpy sqlite3` | 同左 |

## 第 1 步：Python

| 命令 | 预期 | 实测 |
|---|---|---|
| `python3 -V` | `Python 3.12.10` | `Python 3.12.10` |
| `python3 -c "import sys; print(repr(sys.executable))"` | `/data/.../bin/python3` | `/data/user/0/com.atomcode.app/files/atomcode/bin/python3` |
| `subprocess.run([sys.executable,"-c","print(42)"])` | `0 42` | `re-exec 0 42` |
| `multiprocessing` spawn（脚本方式） | `exit 0 True` | `spawn 0 True` |

## 第 2 步：SQLite

| 命令 | 预期 | 实测 |
|---|---|---|
| fts5 建表+查询 | `1` | `1` |
| `select sqrt(16), pow(2,10)` | `4.0\|1024.0` | `4.0\|1024.0` |
| rtree 建表+查询 | `1` | `1` |
| `hasattr(c,'enable_load_extension')` + 启用 | `True` / `enabled` / `3.50.4` | `enable_load_extension True` / `enabled 3.50.4` |

## 第 3 步：Lua

| 检查 | 实测 |
|---|---|
| `package.cpath` 首项 | `/data/user/0/com.atomcode.app/files/atomcode/lua/?.so` |
| cjson 编解码 | `{"nums":[1,2,3],"name":"atomcode"}` → 解码 `atomcode` |

键序与清单示例不同（Lua 表遍历顺序），语义一致。

## 第 4 步：tcc + cheaders

```
tcc compile ok
probe [(5, 8), (7, 10), (100, 105)]
```

与 v2 修正后的预期完全一致（`"v=5"`=3 → 8；`"v=100"`=5 → 105）。v1 把这一项写成 `10` 是文档算错。

## 第 5 步：numpy（仅命令核验）

| 检查 | 实测 |
|---|---|
| 软链 | `numpy`、`numpy-1.26.2.dist-info`（`.libs` 存在，但 `ls -l` 不显示隐藏目录，需 `ls -la`） |
| 版本 | `1.26.2` |
| `a @ (np.eye(3)*2)` | `[[0.0,2.0,4.0],[6.0,8.0,10.0],[12.0,14.0,16.0]]` |
| `det` / `sum` | `0.0` / `36.0` |

上一轮报的 `libpython3.12.so not found` 已消失：运行时的 SONAME 已统一为短名，numpy 自带的 `lib/python/.libs/{libopenblas,libc++_shared}.so` 由 CPython shim 以 `RTLD_GLOBAL` 预加载。

## 第 6 步：node

| 检查 | 实测 |
|---|---|
| 版本/平台/架构 | `v18.20.4 android arm64 2` |
| map + fs | `[ 2, 4, 6 ] true` |
| `process.execPath` | `/data/app/~~…/lib/arm64/libpluginhost.so` |
| `execSync("id")` | **成功**：`uid=10348(u0_a348) …` |
| `spawnSync(process.execPath, ["-e","1"])` | `status 2` |

即边界是「不能自举 node」，不是「禁止 exec」——v1 那句描述已按此改写。

## 第 7 步：Zig

| 检查 | 实测 |
|---|---|
| `zig version` | `0.13.0` |
| `zig cc` + ctypes | `zigprobe(5) 10`（编译时有一条 `falling back to default ABI` 警告，产物可用） |
| 不设 `ZIG_LIB_DIR` | `error: unable to find zig installation directory: FileNotFound`（而 `zig version` 照常输出，所以只测版本号会误判） |

## 缺陷

本轮**没有发现代码缺陷**。上一轮的三项「不符」与两项清单错误，处理如下：

| 上一轮项 | 本轮结论 |
|---|---|
| D1 `_posixshmem` 缺失 → spawn 不可用 | 旧包所致。当前包 `lib-dynload/_posixshmem.cpython-312.so` 存在，`spawn 0 True` |
| D2 `_sqlite3` 无 `enable_load_extension` | 旧包所致。当前 `hasattr` 为 True 且可启用 |
| D3 numpy 缺 `libpython3.12.so` / `libopenblas` / shim 未 preload | 旧包 + 依赖归属问题。现：SONAME 统一短名、numpy 自带 `.libs`、shim 扫描 `python-packages/.libs` 预加载 |
| 第 4 步预期值算错 | 已改为 `8`（并给出 5/7/100 交叉验证） |
| 第 6 步「禁止 exec」 | 已改为「外部命令可用，失败的是重执行 node 自身」 |

本轮另外暴露两处**流程**问题，已写进清单 v2：

1. `adb shell run-as …` 读不到 `/sdcard`，`sha256sum /sdcard/Download/*.tar.gz` 会静默输出空 → 清单已注明要用能读共享存储的 shell。
2. `ls -l python-packages` 不显示 `.libs`（隐藏目录）→ 清单已改为 `ls -la`。
3. 无头运行时，往 `$HOME` 写文件会触发审批并让回合挂住 → 清单已要求临时文件只留在工作目录。

## 未能验证的项

| 项 | 原因 |
|---|---|
| 第 5 步的 UI 流程（插件页扫包、卸载看软链消失、安装进度条） | 需要人工在前端点击；本轮只做了命令侧核验 |
| 第 8 步 Doze 熄屏长回合（已豁免 / 未豁免） | 需要锁屏静置并等待 3 分钟 |
| 第 9 步 清理 | 属用户取舍 |

## 复现命令

```sh
# 全部命令（入口 B：adb + run-as，显式 HOME）
adb push scripts/checks_v2.sh /data/local/tmp/checks_v2.sh
adb shell 'run-as com.atomcode.app sh /data/local/tmp/checks_v2.sh'

# 哈希（需要能读共享存储的 shell）
adb shell 'sha256sum /sdcard/Download/*.tar.gz'

# 单独复核 D3 的依赖归属
adb shell 'run-as com.atomcode.app sh -c "cd files/atomcode; ls packages/cpython/lib; ls python-packages/.libs"'
```

`checks_v2.sh` 随仓库保存于 `scripts/checks_v2.sh`，内容就是上文逐条命令的串联。
