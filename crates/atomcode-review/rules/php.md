[PHP] This batch contains PHP code; pay extra attention to:

#### Security
- SQL injection: concatenated SQL (should use PDO/mysqli prepared statements + parameter binding)
- XSS: unescaped output (`echo` user input, should use htmlspecialchars)
- Command injection: `exec`/`system`/`shell_exec`/`passthru` concatenating external input
- File inclusion / path traversal: `include`/`require`/file operations using external input
- Deserializing untrusted data (`unserialize`); unvalidated `$_GET`/`$_POST`/`$_REQUEST`

#### Error-Prone Semantics
- Weak type comparison `==` causing bypass (`"0e123" == "0e456"`), should use `===`
- Misuse of `isset` / `empty` / null coalescing `??` causing undefined variable warnings
- Array key implicit type conversion, floating-point comparison

#### Robustness
- Errors suppressed by `@` masking issues, uncaught exceptions
- Resources not released (file handles, DB connections)
- Database queries inside loops (N+1), string concatenation inside loops
