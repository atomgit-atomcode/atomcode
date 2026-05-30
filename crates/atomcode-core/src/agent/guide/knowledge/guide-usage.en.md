---
title: Guide Usage
category: Help
keywords: [guide, usage, how, use, ask, query, help]
---

# /guide Usage Guide

`/guide` is AtomCode's built-in Q&A feature that answers usage questions about AtomCode.

## Basic Usage

```
/guide <your question>
```

Type your question directly. Supports both English and Chinese.

## Tips

- **Be specific**: `/guide How to configure Ollama` works better than `/guide config`
- **Use natural language**: `/guide How do I switch to a different model?` is perfectly fine
- **View common topics**: Type `/guide` (without a question) to see the topic menu

## Example Questions

| Question | Description |
|----------|-------------|
| `/guide Getting started` | Installation, login, initial setup |
| `/guide How to switch models` | /model /provider usage |
| `/guide How to use MCP` | MCP server configuration |
| `/guide Keyboard shortcuts` | Shortcut reference |
| `/guide Background tasks` | /bg usage |
| `/guide Context management` | /compact /context /cost |
| `/guide How to use memory` | /remember /forget /memory |
| `/guide Troubleshooting errors` | Error resolution |

## Answer Sources

Guide answers are based on AtomCode's local knowledge base. If the knowledge base doesn't cover your question, Guide will try to fetch the answer from online documentation. If that still doesn't help:
1. Visit the documentation: https://atomcode.atomgit.com/docs/en/
2. Use `/issue` to report the problem

## When Answer Is Truncated

If the answer ends with `*(truncated)*`, it was cut short. You can:
- Re-query with a more specific question (narrow the scope)
- Visit the documentation site for the full content
