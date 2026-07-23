---
name: setup
description: Analyze a codebase and recommend AtomCode automations (hooks, subagents, skills, plugins, MCP servers). Use when user asks for automation recommendations, wants to optimize their AtomCode setup, mentions improving AtomCode workflows, asks how to first set up AtomCode for a project, or wants to know what AtomCode features they should use.
user_invocable: true
argument_hint: "[focus area, e.g. hooks, mcp, skills, all]"
allowed_tools: Read, Glob, Grep, Bash, Write, Edit
---

# AtomCode Automation Recommender

Analyze codebase patterns to recommend tailored AtomCode automations, then **offer to install them**.

## Core Principles

1. **Recommend first, install on request** — output recommendations, then ask "要我帮你装上吗？"
2. **Online search first, local fallback** — try `npx skills find` for the latest community skills; fall back to reference files if Node.js unavailable
3. **Friendly error handling** — if tools are missing (no Node.js, no npx), explain clearly and continue with local recommendations
4. **1-2 per category** — don't overwhelm; surface the most valuable automations

## Automation Types

| Type | Best For | How to Install |
|------|----------|----------------|
| **Skills** | Packaged expertise, workflows, repeatable tasks | `npx skills add <pkg>` or create `.atomcode/skills/<name>/SKILL.md` |
| **Plugins** | Collections of skills, commands, agents, hooks | `/plugin marketplace add <url>` then `/plugin install <name>` |
| **MCP Servers** | External tool integrations (databases, APIs, docs) | Write to `.mcp.json` (project root) or `~/.atomcode/mcp.json` (global) |
| **Hooks** | Automatic actions on tool events (format, lint, block) | Write to `.atomcode/settings.json` |
| **Subagents** | Specialized reviewers (security, performance, a11y) | Create `.atomcode/skills/<name>/SKILL.md` with reviewer prompt |
| **Commands** | Quick slash commands (/test, /review, /deploy) | Create `.atomcode/commands/<name>.md` |

## Workflow

### Phase 1: Codebase Analysis

Gather project context:

```bash
# Detect project type and tools
ls -la package.json pyproject.toml Cargo.toml go.mod pom.xml build.gradle 2>/dev/null
cat package.json 2>/dev/null | head -50

# Check dependencies
cat package.json 2>/dev/null | grep -E '"(react|vue|angular|next|express|fastapi|django|prisma|supabase|stripe)"'

# Check existing AtomCode config
ls -la .atomcode/ CLAUDE.md .mcp.json 2>/dev/null

# Project structure
ls -la src/ app/ lib/ tests/ components/ pages/ api/ 2>/dev/null
```

**Key Indicators:**

| Category | What to Look For | Informs |
|----------|------------------|---------|
| Language/Framework | package.json, Cargo.toml, pyproject.toml | Skills, Hooks |
| Frontend stack | React, Vue, Angular, Next.js | Playwright MCP, frontend skills |
| Backend stack | Express, FastAPI, Django | API documentation tools |
| Database | Prisma, Supabase, raw SQL | Database MCP servers |
| External APIs | Stripe, OpenAI, AWS SDKs | context7 MCP for docs |
| Testing | Jest, pytest, Playwright configs | Testing hooks/skills |
| CI/CD | GitHub Actions, GitLab CI | VCS MCP server |
| Docs patterns | OpenAPI, JSDoc, docstrings | Documentation skills |

### Phase 2: Search for Skills (Online + Local)

**Search strategy by recommendation type:**

| Type | Online `npx skills find` | Local reference files |
|------|--------------------------|----------------------|
| **Skills** | ✅ **先搜在线** — 社区生态有数千个 skill | ✅ 补充自定义 skill 创建建议 |
| **MCP Servers** | ❌ 在线 registry 不提供 MCP | ✅ **只用本地** reference |
| **Hooks** | ❌ 在线 registry 不提供 hooks | ✅ **只用本地** reference |
| **Subagents** | ❌ 在线 registry 不提供 agents | ✅ **只用本地** reference |
| **Commands** | ❌ 在线 registry 不提供 commands | ✅ **只用本地** reference |

#### Step 2a: Online skill search

```bash
# Check if npx is available
npx --version 2>/dev/null
```

**If npx is available**, search for relevant skills based on detected project type:

```bash
npx skills find <detected-language>
npx skills find <detected-framework>
npx skills find <specific-library>
```

Examples:
- Rust project → `npx skills find rust`
- React + Next.js → `npx skills find react nextjs`
- Python + Django → `npx skills find django`
- Docker → `npx skills find docker`

Include the most relevant results (by install count) in your Skills recommendations.

**If npx is NOT available** (Node.js not installed), show this friendly message and continue:

```
💡 提示：安装 Node.js 后可以使用在线 skill 搜索功能，获取社区最新推荐。
   下载：https://nodejs.org/
   目前使用内置推荐列表为您分析，功能不受影响。
```

#### Step 2b: Local reference lookup (always runs)

For **all recommendation types** (MCP / Hooks / Subagents / Commands + skill supplements), use the local reference files. These are always available regardless of Node.js:

### Phase 3: Generate Recommendations

Combine online search results + local reference knowledge to generate recommendations.

#### A. Skills Recommendations

**From online registry** (if available):
- Include skills found via `npx skills find` with high install counts
- Provide the exact install command: `npx skills add <owner/repo@skill> -g -y`

**From reference files** (always available):
See [references/skills-reference.md](references/skills-reference.md) for built-in patterns.

**Plugin skills to recommend installing:**

| Codebase Signal | Skill | Install Command | Invocation |
|-----------------|-------|-----------------|------------|
| Any project (AtomCode Q&A) | **/guide** | Built in — no install. Run `/guide <question>` (also auto-dispatches). | Both |

> The official plugin marketplace (`/plugin marketplace add https://atomgit.com/atomgit_atomcode/atomcode-plugins-official`) ships workflow plugins like `atomcode-workflows`, `commit-craft`, and `git-worktree`. AtomCode usage Q&A is now the built-in `/guide` subagent, so it no longer needs a plugin install.

**Custom skills to suggest creating:**

| Codebase Signal | Skill to Create | Invocation |
|-----------------|-----------------|------------|
| API routes | **api-doc** (OpenAPI template) | Both |
| Database project | **create-migration** | User-only |
| Test suite | **gen-test** (example tests) | User-only |
| Component library | **new-component** (templates) | User-only |
| PR workflow | **pr-check** (checklist) | User-only |
| Code style | **project-conventions** | AtomCode-only |

#### B. MCP Server Recommendations

See [references/mcp-servers.md](references/mcp-servers.md) for detailed patterns.

| Codebase Signal | Recommended MCP Server |
|-----------------|------------------------|
| Uses popular libraries | **context7** - Live documentation lookup |
| Frontend with UI testing | **Playwright** - Browser automation |
| Uses Supabase | **Supabase MCP** - Database operations |
| PostgreSQL/MySQL | **Database MCP** - Query and schema |
| GitHub/GitLab repository | **VCS MCP** - Issues, PRs/MRs |
| Docker containers | **Docker MCP** - Container management |

#### C. Hooks Recommendations

See [references/hooks-patterns.md](references/hooks-patterns.md) for configurations.

| Codebase Signal | Recommended Hook |
|-----------------|------------------|
| Prettier configured | PostToolUse: auto-format on edit |
| ESLint/Ruff configured | PostToolUse: auto-lint on edit |
| TypeScript project | PostToolUse: type-check on edit |
| Tests directory exists | PostToolUse: run related tests |
| `.env` files present | PreToolUse: block `.env` edits |
| Lock files present | PreToolUse: block lock file edits |

#### D. Subagent Recommendations

See [references/subagent-templates.md](references/subagent-templates.md) for templates.

In AtomCode, subagents are implemented as skills with specialized reviewer prompts:

| Codebase Signal | Recommended Subagent |
|-----------------|---------------------|
| Large codebase (>500 files) | **code-reviewer** |
| Auth/payments code | **security-reviewer** |
| API project | **api-documenter** |
| Performance critical | **performance-analyzer** |
| Frontend heavy | **ui-reviewer** (accessibility) |

### Phase 4: Output Report + Offer Installation

Format recommendations clearly, then **offer to install**.

```markdown
## AtomCode Automation Recommendations

### Codebase Profile
- **Type**: [detected language/runtime]
- **Framework**: [detected framework]
- **Key Libraries**: [relevant libraries]

---

### 🎯 Skills

#### [skill name]
**Why**: [specific reason]
**Install**: `npx skills add <owner/repo@skill> -g -y`
**Source**: [online registry / custom creation]

---

### 🔌 MCP Servers

#### [server name]
**Why**: [specific reason]
**Config**:
```json
{
  "mcpServers": {
    "[name]": {
      "command": "npx",
      "args": ["-y", "@package/name"]
    }
  }
}
```

---

### ⚡ Hooks

#### [hook name]
**Why**: [specific reason]
**Config**: Add to `.atomcode/settings.json`

---

### 🤖 Subagents

#### [agent name]
**Why**: [specific reason]
**Create**: `.atomcode/skills/[name]/SKILL.md`

---

**要我帮你安装这些推荐吗？** 你可以说：
- "全部装上" — 安装所有推荐
- "装 skills" — 只装推荐的 skills
- "装 [具体名称]" — 装指定项
- "先不装" — 只看推荐，稍后自行安装
```

### Phase 5: Execute Installation (on user request)

When the user agrees to install, execute the appropriate action for each type:

**Skills (from online registry):**
```bash
npx skills add <owner/repo@skill> -g -y
```

If `npx` fails, show:
```
⚠️ 安装失败。可能原因：
1. 未安装 Node.js — 下载：https://nodejs.org/
2. 网络问题 — 检查网络连接
3. 包名错误 — 在 https://skills.sh/ 搜索确认

你也可以手动安装：
1. 访问 https://skills.sh/[skill-name]
2. 复制 SKILL.md 内容
3. 创建 ~/.atomcode/skills/[name]/SKILL.md
```

**Skills (custom creation):**
Use the Write tool to create `.atomcode/skills/<name>/SKILL.md` with the recommended content.

**MCP Servers:**
Use the Write/Edit tool to add the server config to `.mcp.json` in the project root (NOT `.atomcode/mcp.json` — the loader does not read that) or the global `~/.atomcode/mcp.json`:
```bash
# Read existing config
cat .mcp.json 2>/dev/null || echo '{}'
# Merge new server config
```

**Hooks:**
Use the Write/Edit tool to add hook config to `.atomcode/settings.json`:
```bash
cat .atomcode/settings.json 2>/dev/null || echo '{}'
```

**Subagents (as skills):**
Use the Write tool to create `.atomcode/skills/<name>/SKILL.md` with the reviewer prompt from the subagent template.

**Commands:**
Use the Write tool to create `.atomcode/commands/<name>.md`.

**After installation**, confirm with the user:
```
✅ 安装完成！

已安装：
  ✓ skill: [name] — [source]
  ✓ mcp: [name] — 写入 .mcp.json（重启 AtomCode 后生效）
  ✓ hook: [name] — 写入 .atomcode/settings.json
  ✓ subagent: [name] — 创建 .atomcode/skills/[name]/SKILL.md

💡 MCP 服务需要重启 AtomCode 后生效。
💡 输入 /help 查看新增的 slash commands。
```

## Decision Framework

### When to Recommend MCP Servers
- External service integration needed (databases, APIs)
- Documentation lookup for libraries/SDKs
- Browser automation or testing
- Team tool integration (GitHub, GitLab, Linear, Slack)

### When to Recommend Skills
- Frequently repeated prompts or workflows
- Project-specific tasks with arguments
- Applying templates or scripts to tasks
- Quick actions invoked with `/skill-name`

**Invocation control:**
- `disable_model_invocation: true` — User-only (for side effects: deploy, commit)
- `user_invocable: false` — AtomCode-only (for background knowledge)
- Default — Both can invoke

### When to Recommend Hooks
- Repetitive post-edit actions (formatting, linting)
- Protection rules (block sensitive file edits)
- Validation checks (tests, type checks)

### When to Recommend Subagents
- Specialized expertise needed (security, performance)
- Review workflows
- Background quality checks

## Error Handling

### Node.js / npx not available
```
💡 提示：当前环境未安装 Node.js，在线 skill 搜索功能不可用。
   安装 Node.js 后可使用 `npx skills find` 搜索社区 skill。
   下载：https://nodejs.org/
   
   目前使用内置推荐列表为您分析，功能不受影响。
```

### npx skills find returns no results
```
ℹ️ 在线 skill 库中暂无 [keyword] 相关的社区 skill。
   已使用内置推荐列表为您分析。
   你也可以稍后在 https://skills.sh/ 浏览所有可用 skill。
```

### npx skills add installation fails
```
⚠️ skill 安装失败：[error message]

可能原因：
1. 网络连接问题 — 检查代理设置
2. 权限不足 — 尝试 sudo 或检查目录权限
3. 包已下架 — 在 https://skills.sh/ 确认包是否可用

替代方案：手动创建 skill 文件
  mkdir -p ~/.atomcode/skills/[name]
  # 然后将 SKILL.md 内容粘贴进去
```

### .atomcode directory not writable
```
⚠️ 无法写入 .atomcode/ 目录。

请检查目录权限：
  ls -la .atomcode/
  
或手动创建：
  mkdir -p .atomcode/skills .atomcode/commands
```

## Configuration Tips

### MCP Server Setup
- **Team sharing**: Check `.mcp.json` into repo so the entire team gets same MCP servers
- **Debugging**: Use `--mcp-debug` flag to identify configuration issues

### Permissions for Hooks
Configure allowed tools in `.atomcode/settings.json`:
```json
{
  "permissions": {
    "allow": ["Edit", "Write", "Bash(npm test:*)", "Bash(git commit:*)"]
  }
}
```
