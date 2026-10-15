# Plugin hook entry points

Keep existing plugin hook-set authorization. Restrict plugin-contributed commands at the shared executor to plugin-relative script entry points, optionally launched with python, python3, node, sh or bash and literal argv. Support quoted paths/arguments and the existing plugin-root placeholders. Resolve and validate the script against the canonical plugin root and execute that exact path, never the original string through a shell. Reject shell expressions, absolute or parent paths, missing/non-file targets and symlink targets outside the root. Direct batch files are unsupported. On Windows, interpreters resolve to canonical .exe/.com paths from absolute PATH directories, avoiding implicit project-directory executable lookup.

User-authored hooks keep their existing shell semantics. Invalid plugin commands return an explicit block decision with exit 2 rather than None; PreToolUse blocks and UserPromptSubmit sees a deliberate rejection. Post/terminal events can report the rejection but cannot undo completed work. Existing timeouts and spawn-failure handling remain unchanged.

This is an intentional compatibility change: authorized inline pipelines, interpreter -c/-e code and unsupported launchers must migrate to script entry points. A path inside a plugin is not a sandbox. The hook-set trust hash still does not cover script contents, and filesystem mutation after validation remains a separate concern. Skill shell expansion and project-hook authorization are outside this change; do not claim issue #1274 fully resolved.

Tests cover user shell compatibility, interpreter/quoted arguments/root placeholders, expressions, traversal, missing paths, symlink escape, executing the plugin script despite a project namesake and a rejected PreToolUse gate. Windows path strings are checked on all platforms; native Windows execution requires field validation.

## Follow-up (2026-10-15)

The implementation now hashes plugin bundle contents, including imported helper
files, excluding Git metadata. Command-only trust is not grandfathered into
content trust. Invalid entry points are withheld at load, rejected by `plugin
trust`, and diagnosed with the plugin/event/command and migration guidance.
Loaded plugin configurations capture a content digest and reject later content
changes before invocation. This does not sandbox external dependencies or PATH,
and validation plus interpreter execution is not atomic against concurrent file
replacement. Bundle files must be readable regular files; file symlinks must stay
inside the root and directory symlinks are rejected. Python bytecode generation
is disabled so normal imports do not mutate the authorized bundle.

History mutation actions all leave recall mode; actual vertical recall refreshes
completion. Todo disclosure uses existing `/todo show|hide|toggle` commands, while
`/todo` still prints the complete plan. Its wheel bound is captured from the
already-folded view during painting, without replaying the event log per wheel.
