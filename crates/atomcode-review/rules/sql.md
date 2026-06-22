[SQL] This batch contains SQL; pay extra attention to:

#### Security

- Injection: concatenating user input, dynamic SQL without parameterization (should use precompilation/parameter binding)
- Permissions: cross-tenant/unauthorized queries missing tenant or user dimension filters

#### Logical Correctness

- UPDATE/DELETE missing WHERE, or WHERE condition can be bypassed → accidental full-table modification/deletion
- JOIN condition using wrong fields or missing join condition → Cartesian product
- NULL comparison semantic errors (`= NULL` should be `IS NULL`), NOT IN containing NULL returns empty set
- Aggregate and GROUP BY field mismatch, HAVING and WHERE confusion

#### Performance (only report critical issues)

- Missing index causing full table scan, implicit type conversion / function wrapping fields invalidating indexes
- Large queries without LIMIT / pagination, N+1, repeated subqueries that could be extracted to CTE/temp tables
- `SELECT *` on large tables / high-frequency paths

#### Data Correctness

- Amount/precision using floating point (should use DECIMAL), transaction boundaries and rollback, missing locks or overly large lock scope under concurrency

Review principle: when context is unclear, prefer to skip rather than misreport; only report when there is clear evidence.
