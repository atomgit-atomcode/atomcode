# AtomGit PR argument reliability

Baseline: dd63bfc20, branch fix/atomgit-pr-arguments. Main checkout contains unrelated work.

The grape session had repeated local failures from string-valued limits ("30", "40", "25"). Its PR creation calls passed target_branch=weekly-1102-zrr, but the typed tool used base=master because target_branch was only a list_mine filter. A later raw API request succeeded with the same upstream repository and fork-owner-qualified head, using the actual target branch. This proves a dropped-argument defect; an HTTP 404 alone does not prove the exact server-side cause. Other failures included missing owner/repo/title, mixed-up title/head, permission refusals and guessed endpoint fields.

The existing capabilities tool owns parsing and guidance; AtomgitClient remains the request/auth owner. No runtime, persistence or approval boundary changes:

- PR integer arguments accept exact unsigned decimal strings, retaining integer schemas and rejecting fractions, negative values and overflow.
- create accepts source_branch/target_branch as action-specific head/base aliases. Explicit canonical arguments retain precedence only when aliases agree. Conflicts fail locally before any write; fork-qualified head may agree with an unqualified source branch.
- Existing master fallback remains when neither base nor target_branch is supplied. Empty or owner-prefixed target branches are rejected instead of silently producing the wrong request.
- Tool-owned descriptions distinguish the upstream target repository, target branch and fork source. Generic REST guidance distinguishes schema errors, permission refusals and ambiguous 404s; it does not invent fallback endpoints or retry writes.
- No production PR, branch or file is created during verification. HTTP tests replay the observed successful payload against a local mock and preserve the Chinese body in the initial request.

79 AtomGit tests passed, including branch alias/fork payload replay, no-write conflict rejection, numeric argument boundaries, permission-denial no-retry behavior and existing endpoint/client coverage. Validation also covers the compile gate, formatting and Cargo.lock checks. These do not establish that every account has permission or eliminate failures from unsupported server endpoints.
