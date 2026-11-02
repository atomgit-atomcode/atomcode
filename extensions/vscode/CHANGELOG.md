# Changelog

本文件记录 AtomCode for VS Code 的用户可见变化。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号与 `package.json` 保持一致。

## [5.2.5] - 2026-11-02

### 新增

- **斜杠命令与技能**：输入 `/` 打开命令菜单，内置 `/login`、`/logout`、`/whoami`、`/status`、`/config`、`/reload`，并列出当前环境安装的技能（`/<skill-name>`）；支持前缀 + 子序列过滤，`↑/↓` 选择，`Enter`/`Tab` 补全，`Esc` 收起。
- **模型选择器**：输入区直接切换模型，档位跟随服务端 `/models` 返回的清单，默认模型带 `default` 标记。
- **推理强度（Effort）**：按模型声明的档位提供 `low / medium / high / xhigh / max`，未声明档位的模型自动隐藏该选项。
- **四种运行模式**：`Build` / `Accept Edits` / `Auto` / `Plan`，未手动选择时沿用 `config.toml` 的 `default_mode`，新建与切换会话时重新对齐。
- **@ 文件提及**：输入 `@` 检索工作区路径，把文件/目录作为上下文投喂给模型。
- **图片输入**：支持粘贴图片并点击放大查看。
- **Todo 面板**：在对话上方展示当前回合的待办与进度。
- **IDE 语言服务桥接**：新增启用/停用、状态查看与复制窗口绑定四个命令，把 VS Code 的定义、引用、符号与工作区符号查询能力提供给 AtomCode。

### 改进

- 历史会话支持搜索、恢复、重命名与删除；会话记录落盘，重开窗口可继续。
- 代码变更统一以 diff 预览，确认后才写入编辑器；模型产出的代码、表格与 artifact 可在对话内展开查看。
- 本地服务的端口选择与多窗口复用更稳：首选端口被其他版本占用时保留已有进程并改用稳定备用端口，状态栏显示实际连接端口。

### 文档

- README 补齐命令表（14 条）、设置项（含 `atomcode.chat.fontFamily`）、斜杠命令与技能说明。
- 扩展分类加入 `Machine Learning`，关键词扩充，便于在 Marketplace 检索到。

## [5.2.4] - 2026-10-28

### 修复

- 版本号与 daemon 构建基线对齐；修复打包时可能混入 macOS AppleDouble（`._*`）边车文件的问题。

> 更早的版本变更见仓库 [CHANGELOG.md](https://atomgit.com/atomgit_atomcode/atomcode/blob/main/CHANGELOG.md)。
