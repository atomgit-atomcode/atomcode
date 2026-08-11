---
name: session-backup
description: 会话全量备份与恢复 — 创建带校验的时间戳备份，支持一键恢复。触发词：备份 / backup / 数据安全 / 预防丢失 / archive / 永久保存 / 全量备份 / 恢复备份 / restore / 会话备份 / 备份会话 / 保存会话 / 备份恢复
argument-hint: ""
---

# Session Backup — 备份与会话存档

创建带完整性校验的全量会话备份，防患于未然。

## 为什么需要备份

最常见的会话丢失场景：

| 场景 | 原因 | 概率 |
|------|------|------|
| meta 丢失 | 升级/迁移/误删 | 高 |
| snapshot 损坏 | 磁盘错误 | 低 |
| 格式不兼容 | daemon 版本升级 | 中 |

**备份是最后的防线**。即使 PR #810 修复了格式容错，备份能覆盖所有意外场景。

## 使用方式

### 交互式

```
/session-backup
```

创建一个带 MD5 校验的时间戳全量备份。

### 手动备份

```bash
python3 plugins/session-keeper/scripts/session-keeper.py backup
```

### 从备份恢复

```bash
python3 plugins/session-keeper/scripts/session-keeper.py list-backups

# 恢复最新备份
python3 plugins/session-keeper/scripts/session-keeper.py restore

# 恢复指定备份（backup_name 来自 list-backups 的输出）
python3 plugins/session-keeper/scripts/session-keeper.py restore BACKUP_20260724_112342
```

## 备份内容

每次备份包含：

- `sessions/` — 全部会话文件（meta + snapshot + ui.json）
- `config.toml` — AtomCode 配置
- `memory.md` — 永久记忆文件
- `BACKUP_CHECKSUM.md5` — MD5 完整性校验文件

## 自动化

### 自动备份（AtomCode Hook）

在 `~/.atomcode/hooks.json` 中配置：

```json
{
  "hooks": {
    "auto-backup-sessions": {
      "event": "session_end",
      "command": "python3 /path/to/session-keeper.py backup"
    }
  }
}
```

### Windows 计划任务

```powershell
$action = New-ScheduledTaskAction -Execute "python" -Argument "C:\path\to\session-keeper.py backup"
$trigger = New-ScheduledTaskTrigger -Daily -At 02:00AM
Register-ScheduledTask -TaskName "AtomCode Session Backup" -Action $action -Trigger $trigger
```

### cron (Linux/macOS)

```cron
0 2 * * * python3 /path/to/session-keeper.py backup
```
