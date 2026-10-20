# Searching ignored files and directory links

A file being ignored by Git does not prohibit reading it. When the filename is known,
use `read_file` with that path directly. A literal Glob pattern without wildcards also uses an exact lookup without walking the tree. Glob wildcard patterns and directory Grep discover files and may omit
content under the active filters. An empty search is not proof that a file is absent.

Glob and Grep accept three independent boolean overrides:

- `respect_gitignore` (default `true`): honor ignore files and skip build/cache directories.
  Setting it to `false` includes ignored and build/dependency content. VCS metadata
  and the product's own internal directory remain excluded.
- `include_hidden` (default `false`): include dotfiles and hidden directories.
- `follow_symlinks` (default `false`): follow directory symbolic links, including
  Windows directory links supported by the filesystem. Cycles are not traversed.

For an ignored linked directory, both ignore bypass and link following may be needed:

```json
{"pattern":"**/*.toml","path":"linked-dir","respect_gitignore":false,"include_hidden":true,"follow_symlinks":true}
```

Always scope a retry to the directory the user named. Do not expand a failed project
search to an entire disk. Broken links and access failures are reported separately
from missing files. These options never bypass approval or filesystem containment.
Grep continues protecting credentials discovered during directory traversal, including
credentials reached through aliases; explicit sensitive paths retain their existing
permission checks.

User defaults in `~/.atomcode/config.toml` (also editable through `/config`):

```toml
[tools.search]
respect_gitignore = true
include_hidden = false
follow_symlinks = false
timeout_secs = 30
max_entries = 100000
```

Project overrides go in `.atomcode/search.toml`, using the same keys **without** a
`[tools.search]` header. Only the keys present override user defaults. For example:

```toml
respect_gitignore = false
follow_symlinks = true
```

Precedence: tool-call switches > project search.toml > user tools.search > built-in
defaults. Configuration changes apply when the runtime capabilities are rebuilt;
use `/reload` or reopen the session. A directory switch reads the new project's
search settings rather than carrying overrides from the old project.

Timeouts are bounded to 1–300 seconds; entry limits to 1–1,000,000. The default entry
limit counts visited files/directories, not just matches. Long scans report progress.
A stopped, timed-out or capped scan must not report a definitive absence. Partial
results include their scan status; a timeout interrupting a blocked read reports an
incomplete-scan error. Filesystem implementations that cannot honor alternate
filter rules reject those options explicitly.
