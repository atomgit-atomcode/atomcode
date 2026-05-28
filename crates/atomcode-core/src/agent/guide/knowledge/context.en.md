---
title: Context & Cost Management
category: Session
keywords: [compact, context, cost, budget, compress, usage, manage, limit, long, overflow, token, cache]
---

# Context & Cost Management

## /context — Context Budget

View the current session's context usage, including the proportion of each part.

| Command | Description |
|---------|-------------|
| `/context` | Show context budget breakdown |
| `/context prompt` | Show full system prompt content |

Context includes: system prompt + project instructions + tool definitions + conversation history. Compress when conversation gets too long.

## /compact — Compress Conversation

Compress conversation history to free up context space. AI retains key information and discards redundant details.

| Command | Description |
|---------|-------------|
| `/compact` | Auto compress |
| `/compact <hint>` | Compress with specific direction (e.g., "keep database discussion") |

Use when: conversation gets long, model responses slow down, approaching context limit.

## /cost — Token Usage

View current session's token usage and estimated cost.

Display includes:
- Prompt tokens (input)
- Completion tokens (output)
- Cached tokens (cache hits)
- Cache hit rate
- Estimated cost (USD)

## Best Practices

- Regularly `/compact` during long tasks to keep context lean
- Check `/context` to see if approaching limit
- Use `/session` to start new session instead of continuing in overly long ones
