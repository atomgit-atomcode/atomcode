# Skills Recommendations

Skills are packaged expertise with workflows, reference materials, and best practices. Create them in `.atomcode/skills/<name>/SKILL.md`. Skills can be invoked by AtomCode automatically when relevant, or by users directly with `/skill-name`.

Some pre-built skills are available through the setup installer (`atomcode setup`).

**Note**: These are common patterns. Use web search to find skill ideas specific to the codebase's tools and frameworks.

---

## Available from Setup Seeds

### Built-in Skills (installed via `atomcode setup`)

| Skill | Best For |
|-------|----------|
| **rust-best-practices** | Idiomatic Rust development |
| **vue-best-practices** | Vue 3 Composition API |
| **react-native-best-practices** | React Native performance |
| **spring-boot-engineer** | Spring Boot 3.x apps |
| **code-review-excellence** | Code review practices |
| **kubernetes** | K8s operations |
| **docker** | Containerization |
| **api-design-patterns** | REST/GraphQL API design |
| **sql-optimization-patterns** | Query optimization |
| **frontend-testing** | Vitest + RTL tests |

### Built-in Commands (installed via `atomcode setup`)

| Command | Best For |
|---------|----------|
| **/review** | Code review workflow |
| **/refactor** | Code refactoring |
| **/lint** | Linting workflow |
| **/clippy** | Rust clippy checks |
| **/cargo-test** | Rust test runner |
| **/cargo-run** | Rust build & run |
| **/pytest** | Python test runner |
| **/tsc-check** | TypeScript type checking |
| **/changelog** | Changelog generation |
| **/fixissue** | Issue fixing workflow |

---

## Plugin Skills

Skills distributed as plugins — install with `/plugin marketplace add <url>` then `/plugin install <name>`.

| Skill | Plugin | Install | Best For |
|-------|--------|---------|----------|
| **atomcode-workflows** | atomcode-workflows | `/plugin marketplace add https://atomgit.com/atomgit_atomcode/atomcode-plugins-official` then `/plugin install atomcode-workflows@atomcode` | Plan / review / debug / brainstorm workflow skills |
| **commit-craft** | commit-craft | (same marketplace) `/plugin install commit-craft@atomcode` | Conventional commit messages, PR descriptions, changelogs |
| **git-worktree** | git-worktree | (same marketplace) `/plugin install git-worktree@atomcode` | `/worktree` — isolated worktrees for parallel work |

> **AtomCode usage & docs Q&A** (installation, config, slash commands,
> troubleshooting) is now the **built-in `/guide`** subagent — run
> `/guide <question>`, no plugin install needed.

---

## Custom Project Skills

Create project-specific skills in `.atomcode/skills/<name>/SKILL.md`.

### Skill Structure

```
.atomcode/skills/
  my-skill/
    SKILL.md           # Main instructions (required)
    template.yaml      # Template to apply
    scripts/
      validate.sh    # Script to run
    examples/          # Reference examples
```

### Frontmatter Reference

```yaml
---
name: skill-name
description: What this skill does and when to use it
disable-model-invocation: true  # Only user can invoke (for side effects)
user-invocable: false           # Only AtomCode can invoke (for background knowledge)
allowed-tools: Read, Grep, Glob # Restrict tool access
---
```

### Invocation Control

| Setting | User | AtomCode | Use for |
|---------|------|----------|---------|
| (default) | Yes | Yes | General-purpose skills |
| `disable-model-invocation: true` | Yes | No | Side effects (deploy, send) |
| `user-invocable: false` | No | Yes | Background knowledge |

---

## Custom Skill Examples

### API Documentation with OpenAPI Template

Apply a YAML template to generate consistent API docs:

```
.atomcode/skills/api-doc/
  SKILL.md
  openapi-template.yaml
```

**SKILL.md:**
```yaml
---
name: api-doc
description: Generate OpenAPI documentation for an endpoint. Use when documenting API routes.
---

Generate OpenAPI documentation for the endpoint at $ARGUMENTS.

Use the template in [openapi-template.yaml](openapi-template.yaml) as the structure.

1. Read the endpoint code
2. Extract path, method, parameters, request/response schemas
3. Fill in the template with actual values
4. Output the completed YAML
```

**openapi-template.yaml:**
```yaml
paths:
  /{path}:
    {method}:
      summary: ""
      description: ""
      parameters: []
      requestBody:
        content:
          application/json:
            schema: {}
      responses:
        "200":
          description: ""
          content:
            application/json:
              schema: {}
```

---

### Database Migration Generator with Script

Generate and validate migrations using a bundled script:

```
.atomcode/skills/create-migration/
  SKILL.md
  scripts/
    validate-migration.sh
```

**SKILL.md:**
```yaml
---
name: create-migration
description: Create a database migration file
disable-model-invocation: true
allowed-tools: Read, Write, Bash
---

Create a migration for: $ARGUMENTS

1. Generate migration file in `migrations/` with timestamp prefix
2. Include up and down functions
3. Run validation: `bash ~/.atomcode/skills/create-migration/scripts/validate-migration.sh`
4. Report any issues found
```

**scripts/validate-migration.sh:**
```bash
#!/bin/bash
# Validate migration syntax
npx prisma validate 2>&1 || echo "Validation failed"
```

---

### Test Generator with Examples

Generate tests following project patterns:

```
.atomcode/skills/gen-test/
  SKILL.md
  examples/
    unit-test.ts
    integration-test.ts
```

**SKILL.md:**
```yaml
---
name: gen-test
description: Generate tests for a file following project conventions
disable-model-invocation: true
---

Generate tests for: $ARGUMENTS

Reference these examples for the expected patterns:
- Unit tests: [examples/unit-test.ts](examples/unit-test.ts)
- Integration tests: [examples/integration-test.ts](examples/integration-test.ts)

1. Analyze the source file
2. Identify functions/methods to test
3. Generate tests matching project conventions
4. Place in appropriate test directory
```

---

### Component Generator with Template

Scaffold new components from a template:

```
.atomcode/skills/new-component/
  SKILL.md
  templates/
    component.tsx.template
    component.test.tsx.template
    component.stories.tsx.template
```

**SKILL.md:**
```yaml
---
name: new-component
description: Scaffold a new React component with tests and stories
disable-model-invocation: true
---

Create component: $ARGUMENTS

Use templates in [templates/](templates/) directory:
1. Generate component from component.tsx.template
2. Generate tests from component.test.tsx.template
3. Generate Storybook story from component.stories.tsx.template

Replace {{ComponentName}} with the PascalCase name.
Replace {{component-name}} with the kebab-case name.
```

---

### PR Review with Checklist

Review PRs against a project-specific checklist:

```
.atomcode/skills/pr-check/
  SKILL.md
  checklist.md
```

**SKILL.md:**
```yaml
---
name: pr-check
description: Review PR against project checklist
disable-model-invocation: true
---

## PR Context
- Diff: !`git diff HEAD~1`
- Description: !`git log -1 --format=%B`

Review against [checklist.md](checklist.md).

For each item, mark pass or fail with explanation.
```

**checklist.md:**
```markdown
## PR Checklist

- [ ] Tests added for new functionality
- [ ] No console.log statements
- [ ] Error handling includes user-facing messages
- [ ] API changes are backwards compatible
- [ ] Database migrations are reversible
```

---

### Release Notes Generator

Generate release notes from git history:

**SKILL.md:**
```yaml
---
name: release-notes
description: Generate release notes from commits since last tag
disable-model-invocation: true
---

## Recent Changes
- Commits since last tag: !`git log $(git describe --tags --abbrev=0)..HEAD --oneline`
- Last tag: !`git describe --tags --abbrev=0`

Generate release notes:
1. Group commits by type (feat, fix, docs, etc.)
2. Write user-friendly descriptions
3. Highlight breaking changes
4. Format as markdown
```

---

### Project Conventions (AtomCode-only)

Background knowledge AtomCode applies automatically:

**SKILL.md:**
```yaml
---
name: project-conventions
description: Code style and patterns for this project. Apply when writing or reviewing code.
user-invocable: false
---

## Naming Conventions
- React components: PascalCase
- Utilities: camelCase
- Constants: UPPER_SNAKE_CASE
- Files: kebab-case

## Patterns
- Use `Result<T, E>` for fallible operations, not exceptions
- Prefer composition over inheritance
- All API responses use `{ data, error, meta }` shape

## Forbidden
- No `any` types
- No `console.log` in production code
- No synchronous file I/O
```

---

### Environment Setup

Onboard new developers with setup script:

```
.atomcode/skills/setup-dev/
  SKILL.md
  scripts/
    check-prerequisites.sh
```

**SKILL.md:**
```yaml
---
name: setup-dev
description: Set up development environment for new contributors
disable-model-invocation: true
---

Set up development environment:

1. Check prerequisites: `bash scripts/check-prerequisites.sh`
2. Install dependencies: `npm install`
3. Copy environment template: `cp .env.example .env`
4. Set up database: `npm run db:setup`
5. Verify setup: `npm test`

Report any issues encountered.
```

---

## Argument Patterns

| Pattern | Meaning | Example |
|---------|---------|---------|
| `$ARGUMENTS` | All args as string | `/deploy staging` -> "staging" |

Arguments are appended as `ARGUMENTS: <value>` if `$ARGUMENTS` isn't in the skill.

## Dynamic Context Injection

Use `` !`command` `` to inject live data before the skill runs:

```yaml
## Current State
- Branch: !`git branch --show-current`
- Status: !`git status --short`
```

The command output replaces the placeholder before AtomCode sees the skill content.
