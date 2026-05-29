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
- 通过 `/skills` 浏览已安装技能，选择后自动展开技能模板
- `/skills <name>` 直接展开指定技能，如 `/skills brainstorm`
- 在对话中告诉 AI 使用技能（AI 通过 `use_skill` 工具调用），如"用 code-reviewer 审查我的代码"
- 注意：`/my-skill` 形式的直接调用仅在技能已加载且支持用户调用时生效，不会出现在顶层 `/` 命令菜单中

## Plugin (插件)

插件市场系统，安装社区技能包。

### 命令
- `/plugin marketplace list` — 列出已注册的市场
- `/plugin marketplace add <url>` — 添加市场
- `/plugin marketplace remove <name>` — 移除市场
- `/plugin install <name>@<marketplace>` — 从指定市场安装插件
- `/plugin uninstall <name>@<marketplace>` — 卸载插件
- `/plugin list` — 列出已安装

### 配置
- `auto_install_default_skills` — 首次启动自动安装默认技能包
- `auto_update_marketplaces` — 升级后自动更新插件
