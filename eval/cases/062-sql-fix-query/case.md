+++
id = "062-sql-fix-query"
description = "修一个返回重复行的 JOIN 查询"
timeout_secs = 180
tags = ["debug-fix", "sql", "sqlite"]
+++

bad_query.sql 尝试查询"每个产品及它的总销量"，但因为 JOIN 漏了 GROUP BY / 使用错了聚合方式，
返回了重复行。schema.sql + seed.sql 建立了 `products` 和 `sales` 两张表。

请创建 `fixed_query.sql`，修正查询使其：
- 每个 product 只出现一次
- 列 `product_name`, `total_qty`（总销量），按 `total_qty` 降序
- 没有销量的产品 total_qty 显示为 0（不要丢掉）

验证：
```
sqlite3 :memory: ".read schema.sql" ".read seed.sql" ".read fixed_query.sql"
```
应看到所有产品（含零销量的）恰好一行。
