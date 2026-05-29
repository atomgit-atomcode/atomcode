---
title: Skills & Plugins
category: Extensions
keywords: [skill, plugin, skills, plugins, extension, install, create, custom, template, how, use, marketplace, write]
---

# Skills & Plugins

**Relationship**: A Skill is a single AI prompt template. A Plugin is a packaged distribution of one or more Skills. After installing a Plugin from the marketplace, its Skills appear in the `/skills` list.

## Skills

Reusable AI prompt templates that encapsulate domain-specific knowledge.

### Directory Structure
```
.atomcode/skills/
  my-skill/
    SKILL.md          # Skill definition (YAML frontmatter + Markdown body)
```

### SKILL.md Format
```markdown
---
name: my-skill
description: My custom skill
---

# Skill content
...
```

### Usage
- Use `/skills` to browse installed skills, select one to expand its template
- `/skills <name>` to directly expand a specific skill, e.g., `/skills brainstorm`
- In conversation, ask the AI to use a skill (AI invokes it via the `use_skill` tool), e.g., "review my code with code-reviewer"
- Note: Direct `/my-skill` invocation only works if the skill is loaded and user-invocable; it won't appear in the top-level `/` command menu

## Plugins

Plugin marketplace system for installing community skill packages.

### Commands
- `/plugin marketplace list` — List registered marketplaces
- `/plugin marketplace add <url>` — Add a marketplace
- `/plugin marketplace remove <name>` — Remove a marketplace
- `/plugin install <name>@<marketplace>` — Install plugin from a marketplace
- `/plugin uninstall <name>@<marketplace>` — Uninstall plugin
- `/plugin list` — List installed plugins

### Configuration
- `auto_install_default_skills` — Auto-install default skill package on first launch
- `auto_update_marketplaces` — Auto-update plugins after upgrade
