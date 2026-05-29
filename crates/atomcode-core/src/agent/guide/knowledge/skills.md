---
title: 技能与插件
category: 扩展
keywords: [技能, 插件, 打包, use_skill, 扩展, marketplace, 安装, 创建, 自定义, 模板, 编写, 市场, 商店]
---

# Skill 与 Plugin

**关系说明**：Skill 是单个 AI 提示模板，Plugin 是 Skill 的打包分发形式。一个 Plugin 可以包含多个 Skill，通过插件市场安装后，其中的 Skill 会出现在 `/skills` 列表中。

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
