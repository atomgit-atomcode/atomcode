[TOML] This batch contains TOML configuration; pay extra attention to:

#### correctness
- Duplicate keys / redefined tables (parse error or silent override); wrong value types (string vs number vs bool)
- Array-of-tables `[[...]]` vs table `[...]` confusion; quoting/escaping in strings and paths

#### other
- Secrets/credentials in plaintext; unpinned or inconsistent dependency versions
