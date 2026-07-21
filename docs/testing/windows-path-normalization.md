# Windows 路径归一化 —— 真机测试清单

对应改动:中心化路径归一化(`atomcode-capabilities/src/pathnorm.rs`),修复
工具结果反斜杠打断 bash、`\\?\` 泄漏、`atomcode review` 在 Windows 全挂等问题。

> ⚠️ 本改动的效果**只在 Windows 上体现**(macOS/Linux 上 `to_display` 是 no-op,
> 单测覆盖不到真实的 Git Bash 交互 / `cmd start` / `\\?\` / webui↔TUI 会话对齐)。
> 必须在真机 Windows 上逐条验证。

## 环境准备

- [ ] Windows 当前版本
- [ ] 已安装 **Git Bash**(Windows 默认走它)
- [ ] 工作目录用**非 C: 盘**(如 `E:\cqy-service`)跑一遍
- [ ] 再用一个**含中文/空格**的目录(如 `C:\用户\我的 项目`)跑一遍
- [ ] (可选)找一台**没装 Git Bash**(回退 cmd.exe)的机器测 §6

---

## §1 核心链路 [P0] —— tool→model→bash 不再断

工具报出路径后,模型能真正找到 / 运行它(本次修复的根本目的)。

- [ ] 让模型 `write_file` 建 `scan.py`,**同一轮**让它 `python <刚建的路径>`
      → 正常运行(过去会 `python C:sers...` 找不到)
- [ ] `create_file` → `edit_file` → 再 `bash` 引用该文件 → 通
- [ ] **原始 bug 场景**:装一个带 `scripts/` + `references/` 的目录式 skill(如 wiki skill),
      `/<skill>` 触发 → 模型能读 `references/*.md`、能跑 `scripts/scan_project_structure.py`

## §2 各工具结果路径显示 [P1] —— 都应是正斜杠

逐个看返回给模型的路径是 `C:/Users/.../x` 而非 `C:\Users\...\x`:

- [ ] `write_file` / `create_file` → `Created C:/...` / `Overwrote ...`
- [ ] `edit_file` → `Edited C:/...`;以及 "old_string not found in C:/..." 错误
- [ ] `read_file` → "resolved to C:/..."
- [ ] `search_replace` → 每文件报告 + 根目录
- [ ] `grep` / `glob` / `list` → **相对结果** `src/main.rs`(不是 `src\main.rs`)
- [ ] `change_dir` → `Working directory changed to C:/...`
- [ ] **系统提示的 env 块** → `Working directory: C:/...`(与其下 `Shell: bash` 一致)

## §3 `\\?\` 剥离 [P0] —— 身份 / 落地正确

- [ ] **open_file**:让模型 `open_file` 打开一个 pdf/图片 → **真的打开**
      (过去 `\\?\C:\..` 传给 `cmd start` 打不开)
- [ ] **open_file**:开一个不存在的文件 → 提示里的路径干净、无 `\\?\`
- [ ] **change_dir**:`change_dir` 到某目录后,**再用相对路径读/写文件** → 落到正确位置;
      `/context` / footer 的 cwd 正常
- [ ] **webui ↔ TUI 会话对齐(重要)**:webui 里用目录选择器**新建目录**(触发 daemon
      `fs_mkdir`)→ 选它 → 该目录的会话在 **webui 和 TUI 两边是同一个**
      (过去 `\\?\` 会把 session hash 分桶、两端对不上)

## §4 review confine [P0 + 安全回归] —— 之前 Windows 全挂

- [ ] **修复验证**:`atomcode review`,让模型用**绝对仓库内路径**(`C:\repo\src\a.rs`)
      读 / grep → **不再被拒**(过去每个绝对路径都报 "outside the review repository")
- [ ] 相对路径照常可用
- [ ] **⚠️ 安全不能被削弱**(重点回归):
  - [ ] 仓库**外**的绝对路径(`C:\Windows\...`)→ 仍被拒
  - [ ] `..\..\escape` → 仍被拒
  - [ ] 指向仓库外的**符号链接** → 仍被拒(那道 canonicalize 双边比较)

## §5 落地正确性 / 不误伤 [P1]

- [ ] `to_display` 只改显示:文件**真的写到了原生位置**(显示 `C:/a/b`,文件在 `C:\a\b`)
- [ ] **中文 / 空格目录**:`C:\用户\我的 项目` → 显示 `C:/用户/我的 项目`,读写都对
- [ ] **不同盘符**(D:\、E:\)
- [ ] (若适用)**UNC 路径** `\\server\share\...` → 显示成 `//server/share/...`,操作正常

## §6 两种 shell [P1]

- [ ] **Git Bash**(默认):§1 全过
- [ ] **cmd.exe 回退**(没装 Git Bash 的机器):正斜杠 `C:/Users/x/file` 在 cmd 下大多命令
      也接受,但实测 `type` / `cd` / python 等是否 OK(若有问题需单独处理)

---

## 最小冒烟(时间紧只跑这些)

覆盖三个 P0/P1 修复点 + 一个安全回归:

- [ ] §1.1（write → run）
- [ ] §3.1（open_file 真打开）
- [ ] §3.3（webui / TUI 会话对齐）
- [ ] §4.1（review 能用）
- [ ] §4.3（review 安全没削弱）

---

## 备注

- 相关提交:`fix(win): centralize path normalization for the v2 stack`
- 未纳入本轮:入站 `from_shell`(`/c/…` → `C:/…`)归一化 —— 有意推迟(改的是路径*解析*,
  风险更高,单拆)。若测试中发现"模型/Git-Bash 用户发 `/c/Users/...` 路径解析不了",
  即为该缺口,属已知、待后续。
