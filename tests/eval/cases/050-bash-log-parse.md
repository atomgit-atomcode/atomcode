+++
id = "050-bash-log-parse"
description = "用 awk/grep 从日志里抽取 error 行并分组计数"
timeout_secs = 180
tags = ["code-gen", "bash", "awk"]

[seed_files]
"app.log" = """2026-04-07T10:00:00Z INFO  startup ok
2026-04-07T10:00:01Z WARN  disk 80%
2026-04-07T10:00:02Z ERROR db connect timeout
2026-04-07T10:00:03Z INFO  request id=1 ok
2026-04-07T10:00:04Z ERROR db connect timeout
2026-04-07T10:00:05Z ERROR auth invalid token
2026-04-07T10:00:06Z INFO  shutdown
"""
+++

写一个 bash 脚本 errors.sh，从 app.log 中提取所有 ERROR 行，
按 "ERROR 之后的消息（去掉时间戳和等级）" 分组，统计每种消息出现次数，
按出现次数从高到低排序，然后字典序。输出格式：`<count> <message>`。

期望输出（对当前 app.log）：
```
2 db connect timeout
1 auth invalid token
```

完成后用 `bash errors.sh` 验证输出匹配。
