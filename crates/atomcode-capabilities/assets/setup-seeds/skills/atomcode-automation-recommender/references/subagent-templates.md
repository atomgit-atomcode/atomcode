# Subagent Recommendations

Subagents are specialized AtomCode instances that run in parallel, each with their own context window and tool access. They're ideal for focused reviews, analysis, or generation tasks.

**Note**: These are common patterns. Design custom subagents based on the codebase's specific review and analysis needs.

## Code Review Agents

### code-reviewer
**Best for**: Automated code quality checks on large codebases

| Recommend When | Detection |
|----------------|-----------|
| Large codebase (>500 files) | File count |
| Frequent code changes | Active development |
| Team wants consistent review | Quality focus |

**Value**: Runs code review in parallel while you continue working
**Permission**: explore
**Difficulty**: hard
**Tools**: `read_file`, `grep`, `glob`

---

### security-reviewer
**Best for**: Security-focused code review

| Recommend When | Detection |
|----------------|-----------|
| Auth code present | `auth/`, `login`, `session` patterns |
| Payment processing | `stripe`, `payment`, `billing` patterns |
| User data handling | `user`, `profile`, `pii` patterns |
| API keys in code | Environment variable patterns |

**Value**: Catches OWASP vulnerabilities, auth issues, data exposure
**Permission**: explore
**Difficulty**: hard
**Tools**: `read_file`, `grep`, `glob` (read-only for safety)

---

### test-writer
**Best for**: Generating comprehensive test coverage

| Recommend When | Detection |
|----------------|-----------|
| Low test coverage | Few test files vs source files |
| Test suite exists | `tests/`, `__tests__/` present |
| Testing framework configured | jest, pytest, vitest in deps |

**Value**: Generates tests matching project conventions
**Permission**: worker
**Difficulty**: hard
**Tools**: `read_file`, `write_file`, `edit_file`, `grep`, `glob`

---

## Specialized Agents

### api-documenter
**Best for**: API documentation generation

| Recommend When | Detection |
|----------------|-----------|
| REST endpoints | Express routes, FastAPI paths |
| GraphQL schema | `.graphql` files |
| OpenAPI exists | `openapi.yaml`, `swagger.json` |
| Undocumented APIs | Routes without docs |

**Value**: Generates OpenAPI specs, endpoint documentation
**Permission**: worker
**Difficulty**: hard
**Tools**: `read_file`, `write_file`, `edit_file`, `grep`, `glob`

---

### performance-analyzer
**Best for**: Finding performance bottlenecks

| Recommend When | Detection |
|----------------|-----------|
| Database queries | ORM usage, raw SQL |
| High-traffic code | API endpoints, hot paths |
| Performance complaints | User reports slowness |
| Complex algorithms | Nested loops, recursion |

**Value**: Finds N+1 queries, O(n^2) algorithms, memory leaks
**Permission**: explore
**Difficulty**: hard
**Tools**: `read_file`, `grep`, `glob`

---

### ui-reviewer
**Best for**: Frontend accessibility and UX review

| Recommend When | Detection |
|----------------|-----------|
| React/Vue/Angular | Frontend framework detected |
| Component library | `components/` directory |
| User-facing UI | Not just API project |

**Value**: Catches accessibility issues, UX problems, responsive design gaps
**Permission**: explore
**Difficulty**: hard
**Tools**: `read_file`, `grep`, `glob`

---

## Utility Agents

### dependency-updater
**Best for**: Safe dependency updates

| Recommend When | Detection |
|----------------|-----------|
| Outdated deps | `npm outdated` has results |
| Security advisories | `npm audit` warnings |
| Major version behind | Significant version gaps |

**Value**: Prepares incremental dependency edits; the lead runs package commands and tests
**Permission**: worker
**Difficulty**: hard
**Tools**: `read_file`, `write_file`, `edit_file`, `grep`

---

### migration-helper
**Best for**: Framework/version migrations

| Recommend When | Detection |
|----------------|-----------|
| Major upgrade needed | Framework version very old |
| Breaking changes coming | Deprecation warnings |
| Refactoring planned | Architectural changes |

**Value**: Plans and prepares incremental migration edits; the lead runs builds and tests
**Permission**: worker
**Difficulty**: hard
**Tools**: `read_file`, `write_file`, `edit_file`, `grep`, `glob`

---

## Quick Reference: Detection -> Recommendation

| If You See | Recommend Subagent |
|------------|-------------------|
| Large codebase | code-reviewer |
| Auth/payment code | security-reviewer |
| Few tests | test-writer |
| API routes | api-documenter |
| Database heavy | performance-analyzer |
| Frontend components | ui-reviewer |
| Outdated packages | dependency-updater |
| Old framework version | migration-helper |

---

## Subagent Placement

A **team role** goes in `.atomcode/agents/` (this project) or
`~/.atomcode/agents/` (every project). The file name without `.md` is the
role's name, and the `team` tool offers it by that name.

The file is a header of fields between `---` fences, then the role's
instructions:

```
---
permission: explore      # required: explore (reads, changes nothing) | worker (may edit files)
difficulty: hard         # required: simple (the fast model) | hard (the strong one)
when: reviewing auth     # optional: one line on when to hand it work
---
You review code for security risks. You report each finding with the file
and why it matters.
```

**`permission` and `difficulty` are required.** A file without them is left
out and the person is told so — the role is then simply not on offer, which
reads as "the agent has no reviewer" rather than "the file is malformed".

`permission` decides the tools, and a member never gets a shell:
- `explore` — read-only: read, grep, glob, list symbols, web search
- `worker` — the above plus `write_file`, `edit_file`, `search_replace`

The Claude Code agent-file fields (`name`, `description`, `allowed_tools`,
`user_invocable`) are **not** read here: the role's name is the file's name,
and the tools come from `permission` (or a `tools:` line naming a subset of
what `permission` allows).

UTF-8 files with a Windows byte-order mark (BOM) are accepted too.

---

## Model Selection Guide

| Difficulty | Best For | Model |
|------------|----------|-------|
| `simple` | Small, focused reading or documentation tasks | Configured utility model when available |
| `hard` | Reviews, implementation, migrations, architecture | Conversation model |

In a role file, the model tier is `difficulty` (`simple` or `hard`); a
`model:` line names a selection id outright and overrides it.

---

## Tool Access Guide

| Access Level | Tools | Use Case |
|--------------|-------|----------|
| `explore` | `read_file`, `list_directory`, `grep`, `glob`, `list_symbols`, `read_symbol`, available web tools | Reviews, analysis |
| `worker` | The above plus `write_file`, `edit_file`, `search_replace` | Scoped code and documentation edits |

A team member never gets a shell, whatever the role says: `permission:
worker` means scoped file edits, not command execution.
