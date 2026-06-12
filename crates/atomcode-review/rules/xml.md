[XML] This batch contains XML configuration; pay extra attention to:

#### correctness
- Malformed structure: unclosed/mismatched tags, unescaped `&` / `<` / `>` in text, duplicate keys/ids
- Wrong namespace/schema; encoding-declaration mismatch

#### security
- XXE risk indicators (external entity / `DOCTYPE` definitions); secrets/credentials in plaintext
- Misconfigured permissions/endpoints in framework config
