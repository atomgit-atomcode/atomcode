---
name: session-check
description: 会话健康诊断与修复 — 自动扫描 orphan snapshot、修复损坏 meta、验证格式兼容性。触发词：会话检查 / 会话不见了 / session disappeared / corrupt session / 检查会话 / 修复会话 / 诊断会话 / 健康检查 / session recovery / session health / 对话丢失 / 元数据损坏
argument-hint: ""
---

# Session Health Check — 诊断并修复会话数据

## 问题背景

AtomCode 将会话存储在 `~/.atomcode/sessions/<项目哈希>/` 目录下，每个会话由一组侧车文件组成：

- `{uuid}.snapshot` — 实际对话消息
- `{uuid}.meta` — 元数据（名称、时间戳、import_info）
- `{uuid}.ui.json` — UI 状态
- `{uuid}.lease` / `{uuid}.meta.lock` — 运行时锁

当 `.meta` 文件丢失、格式损坏（如 daemon 升级后反序列化失败），或 `.snapshot` 文件成为孤儿时，会话会从 `/resume` 列表中消失。

**关键事实**：`.snapshot` 存有全部对话数据，极少损坏。绝大多数"会话不见了"问题都是 `.meta` 文件的问题，数据本身完好。

## 使用方式

### 交互式（在 TUI 中）

在 AtomCode TUI 中输入：

```
/plugin install session-keeper@atomcode
/session-check
```

脚本自动执行 `diagnose → fix → verify` 流程。

### 手动运行

```bash
python3 plugins/session-keeper/scripts/session-keeper.py diagnose
python3 plugins/session-keeper/scripts/session-keeper.py fix
python3 plugins/session-keeper/scripts/session-keeper.py verify
```

## 命令参考

| 命令 | 作用 |
|------|------|
| `diagnose` | 扫描 orphan snapshot、损坏 meta、格式不兼容 |
| `fix` | 修复损坏 meta（重置非法 import_info）、为 orphan snapshot 生成 meta 与 .ui.json、清理 stale lease |
| `verify` | 验证全部会话的结构完整性 |
| `full` | 依次执行 diagnose → fix → verify |

## 典型输出

```
============================================================
  AtomCode Session Health Check
============================================================

  Sessions directory:  /home/user/.atomcode/sessions
  Project directories: 2
  Total .snapshot:     142
  Total .meta:         127
  Total .ui.json:      127
  Total .lease:        5

  ✅  No orphan snapshots
  ✅  All sessions have .ui.json
  ✅  No stale lease files
  ✅  All meta files have matching snapshots
  ✅  All meta files are valid

  🎉  All sessions healthy!

============================================================
```