+++
id = "060-sql-select-join"
description = "写一个 SQL JOIN 查询找 top spender"
timeout_secs = 180
tags = ["code-gen", "sql", "sqlite"]
+++

schema.sql 里有 users 和 orders 两张表，seed.sql 里插了几条数据。
请写一个 SQL 文件 `query.sql`，查询每个用户的总消费并取 top 3，输出列 `name`, `total`，
按 `total` 降序，如果 total 相同按 name 升序，limit 3。

完成后用 bash 验证：
```
sqlite3 :memory: < schema.sql
sqlite3 :memory: ".read schema.sql" ".read seed.sql" ".read query.sql"
```
（或者创建一个临时 db 文件，按顺序 `.read` 三个文件然后跑 query）

期望结果是 3 行，格式由 sqlite3 默认输出决定即可。
