---
title: Guide 使用指南
category: 帮助
keywords: [guide, 使用, 怎么用, 用法, 提问, 查询, 指南]
---

# /guide 使用指南

`/guide` 是 AtomCode 的内置问答功能，回答关于 AtomCode 的各种使用问题。

## 基本用法

```
/guide <你的问题>
```

直接输入问题即可，支持中英文。

## 使用技巧

- **问题越具体越好**：`/guide 怎么配置 Ollama` 比 `/guide 配置` 效果更好
- **使用自然语言**：`/guide How do I switch to a different model?` 完全没问题
- **查看常用话题**：直接输入 `/guide`（不带问题）显示常用话题菜单

## 常见问题示例

| 问题 | 说明 |
|------|------|
| `/guide 怎么开始使用` | 安装、登录、首次配置 |
| `/guide 怎么切换模型` | /model /provider 操作 |
| `/guide 怎么用 MCP` | MCP 服务器配置 |
| `/guide 快捷键有哪些` | 键盘快捷键参考 |
| `/guide 怎么用后台任务` | /bg 使用方法 |
| `/guide 怎么管理上下文` | /compact /context /cost |
| `/guide 记忆怎么用` | /remember /forget /memory |
| `/guide 报错了怎么办` | 故障排除 |

## 回答来源

Guide 的回答基于 AtomCode 本地知识库。如果知识库未覆盖你的问题，Guide 会尝试从在线文档获取答案。如果仍无法解决，建议：
1. 访问文档站：https://atomcode.atomgit.com/docs/zh/
2. 使用 `/issue` 提交问题

## 回答被截断时

如果回答末尾显示 `*(已截断)*`，说明回答较长被截断了。你可以：
- 用更具体的问题重新查询（缩小范围）
- 直接访问文档站获取完整内容
