---
title: Skill 打包与使用
category: 扩展
keywords: [skill, 技能, 打包, package, use_skill]
---

# Skill 打包与使用

Skill 是可复用的提示模板，帮助 AI 执行特定领域的任务。

## 目录结构
```
.atomcode/skills/
  my-skill/
    SKILL.md          # 技能定义文件
```

## SKILL.md 格式
```markdown
---
name: my-skill
description: 我的自定义技能
---

# 技能内容
...
```

## 使用方法
- 在对话中直接说 "use my-skill" 或 "/my-skill"
- AI 会自动加载技能模板作为系统提示
- 使用 `use_skill` 工具按需加载
