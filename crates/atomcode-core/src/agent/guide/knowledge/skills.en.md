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
- In conversation: `use_skill my-skill` or `/my-skill`
- `/skills` — Browse installed skills

## Plugins

Plugin marketplace system for installing community skill packages.

### Commands
- `/plugin marketplace` — Browse marketplace
- `/plugin install <name>` — Install plugin
- `/plugin uninstall <name>` — Uninstall
- `/plugin list` — List installed

### Configuration
- `auto_install_default_skills` — Auto-install default skill package on first launch
- `auto_update_marketplaces` — Auto-update plugins after upgrade
