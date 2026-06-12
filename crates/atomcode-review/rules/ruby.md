[Ruby] This batch contains Ruby code; pay extra attention to:

#### Security
- SQL injection: string interpolation into queries (should use parameterization / ActiveRecord safe syntax, avoid `where("name = #{x}")`)
- Command injection: `system`/backticks/`%x` concatenating external input
- Deserializing untrusted data (YAML.load, Marshal.load); mass assignment without filtered parameters

#### Exceptions and nil
- Exceptions swallowed by `rescue`, overly broad rescue scope (bare `rescue` catches StandardError and above)
- nil dereference, missing safe navigation `&.`
- `rescue nil` masking errors

#### Rails / Performance
- N+1 queries (should use `includes`/`preload`), database queries inside loops
- Callback hell, triggering save/validation inside loops

#### Error-Prone Semantics
- Truthiness judgment (only `nil`/`false` are falsy, `0`/`""` are truthy)
- Mutable constants being modified, `||=` ambiguity with false/nil
