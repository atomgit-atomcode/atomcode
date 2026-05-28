---
title: Skills & Plugins
category: Extensions
keywords: [skill, plugin, skills, plugins, extension, install, create, custom, template, how, use, marketplace]
---

# Skills & Plugins

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
