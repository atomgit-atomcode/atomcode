---
title: 键盘快捷键
category: 参考
keywords: [快捷键, 键盘, hotkey, keybinding, 键位, 按键, 快捷, shortcut, 按啥, 怎么按]
---

# 键盘快捷键

AtomCode 的终端快捷键，基于 readline/emacs 风格。

## 输入

| 快捷键 | 功能 |
|--------|------|
| `Enter` | 发送消息 |
| `Ctrl+J` | 插入换行（所有终端通用） |
| `\` 后接 `Enter` | 插入换行（atomcode 兜底，所有终端通用） |
| `Alt+Enter` | 插入换行 * |
| `Shift+Enter` | 插入换行 ** |
| `/` | 打开斜杠命令菜单 |
| `Tab` | 自动补全 |
| `Backspace` / `Ctrl+H` | 删除上一个字符 |
| `Delete` / `Ctrl+?` | 删除下一个字符 |
| `Ctrl+W` | 删除前一个单词 |
| `Ctrl+U` | 清空当前行 |
| `Ctrl+K` | 删除到行尾 |
| `Ctrl+A` / `Home` | 跳到行首 |
| `Ctrl+E` / `End` | 跳到行尾 |
| `Left` / `Right` | 光标左右移动 |

## 历史

| 快捷键 | 功能 |
|--------|------|
| `Up` | 上一条输入 |
| `Down` | 下一条输入 |

## 翻看输出

使用终端原生 scrollback：`Cmd+↑/↓`、鼠标滚轮、tmux copy-mode 等都生效。
鼠标拖选 + `Ctrl+C` 复制（atomcode 不接管鼠标）。

## 会话控制

| 快捷键 | 功能 |
|--------|------|
| `Ctrl+C` | 取消当前轮 / 关闭弹层 |
| `Ctrl+D` | 退出 atomcode |
| `Ctrl+L` | 清屏 |

* `Alt+Enter`：部分终端（如 Windows Terminal）会拦截此快捷键用于全屏切换，此时请改用 `Ctrl+J` 或 `\` + `Enter`。
** `Shift+Enter`：部分终端（如原生 macOS Terminal）不支持，此时请改用 `Ctrl+J` 或 `\` + `Enter`。
