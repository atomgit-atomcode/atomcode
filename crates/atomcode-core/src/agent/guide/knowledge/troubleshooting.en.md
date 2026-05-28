---
title: Troubleshooting
category: Help
keywords: [troubleshooting, fix, issue, problem, help, not working, failed, stuck, hang, error, bug, timeout, crash, panic, debug, log]
---

# Troubleshooting Guide

## Provider Connection Failed

**Symptoms**: API connection error or authentication failure.

**Steps**:
1. Check if API Key is correct: `/config` to view current configuration
2. Confirm `base_url` is accessible (corporate networks may need proxy)
3. Check Provider service status
4. Try switching Provider: `/model` to select other model

## Model Response Timeout

**Symptoms**: No response for a long time after sending message.

**Steps**:
1. Check if network connection is stable
2. Try switching to a faster model (like Haiku)
3. Use `/clear` to clear context and retry
4. Check if Provider has rate limits

## Command Not Responding

**Symptoms**: No reaction after typing slash command.

**Steps**:
1. Confirm command spelling is correct (type `/help` to see all commands)
2. Try `/clear` to reset session
3. Check if background tasks are using resources: `/bg list`
4. Restart AtomCode terminal

## MCP Connection Failed

**Symptoms**: MCP tools unavailable or connection error.

**Steps**:
1. Check `mcpServers` configuration: `/mcp` to view status
2. Confirm npx/node is installed and available
3. Check if MCP server process started normally
4. View logs: start with `RUST_LOG=debug`

## File Read/Write Permission

**Symptoms**: Cannot read or write project files.

**Steps**:
1. Check `.atomcode` directory permissions
2. Confirm project directory is not locked by other process
3. Check if disk space is sufficient

## Session Corrupted

**Symptoms**: Error when restoring session or abnormal content.

**Steps**:
1. Use `/session` to create new session
2. If cleanup needed: delete corrupted files under `.atomcode/sessions/`
3. Use `/resume` to try restoring other sessions

## LSP Not Working

**Symptoms**: Code completion, go to definition, etc. not available.

**Steps**:
1. Confirm language server is installed (like rust-analyzer, typescript-language-server)
2. Check `lsp.enabled` configuration: `/config`
3. View LSP log output

## Update Failed

**Symptoms**: Auto-update failed or version mismatch.

**Steps**:
1. Manually run `atomcode upgrade`
2. Check network connection and proxy settings
3. If permission issues: run with administrator privileges

## Getting More Help

If none of the above solutions work:
1. Use `/guide <specific problem description>` for targeted help
2. Visit documentation: https://atomcode.atomgit.com/docs/en/
3. Submit Issue: https://github.com/anthropics/claude-code/issues
