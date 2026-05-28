---
title: 技能与插件
category: 扩展
keywords: [技能, 插件, 打包, use_skill, 扩展, marketplace, 安装, 怎么, 如何, 创建, 自定义, 模板, 咋用, 咋办]
---

# Skill 与 Plugin

## Skill (技能)

可复用的 AI 提示模板，封装特定领域知识。

### 目录结构
```
.atomcode/skills/
  my-skill/
    SKILL.md          # 技能定义 (YAML frontmatter + Markdown 正文)
```

### SKILL.md 格式
```markdown
---
name: my-skill
description: 我的自定义技能
---

# 技能内容
...
```

### 使用
- 对话中: `use_skill my-skill` 或 `/my-skill`
- `/skills` — 浏览已安装技能

## Plugin (插件)

插件市场系统，安装社区技能包。

### 命令
- `/plugin marketplace` — 浏览市场
- `/plugin install <name>` — 安装插件
- `/plugin uninstall <name>` — 卸载
- `/plugin list` — 列出已安装

### 配置
- `auto_install_default_skills` — 首次启动自动安装默认技能包
- `auto_update_marketplaces` — 升级后自动更新插件
