+++
id = "061-sql-window-func"
description = "用 window function 计算每个账户的 running total"
timeout_secs = 180
tags = ["code-gen", "sql", "window-function", "sqlite"]
+++

schema.sql + seed.sql 建立一张 `txns(id, account, ts, amount)` 表，里面有多个账户的若干笔流水。

请写 `query.sql`：对每个账户按 ts 升序计算 running total，输出列
`account`, `ts`, `amount`, `running_total`。使用 SQL window function（SUM OVER (PARTITION BY ... ORDER BY ...)）。

验证：
```
sqlite3 :memory: ".read schema.sql" ".read seed.sql" ".read query.sql"
```
应该每行正确显示每个账户的递增累计金额。
