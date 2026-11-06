---
name: huaweicloud-preview
description: Use when the user wants Huawei Cloud sandbox preview, remote builds, or cloud setup diagnostics. Discover the enabled Huawei Cloud MCP tools on demand.
---

# Huawei Cloud preview in AtomCode

Use `mcp_search` to discover tools from the `huaweicloud` MCP server. Start with `huaweicloud_auth_status`, `huaweicloud_sandbox_check_user` and `huaweicloud_retrieve_skill` (skill `huawei-sandbox`). Read relevant references before executing a workflow. If not enabled, tell the user to run `atomcode cloud enable` in a terminal and restart the session. `atomcode cloud doctor` checks local prerequisites.

For a web preview: inspect the local project's entry point and scripts, agree on the sandbox target, connect, upload the project, build, start the server, check its health and return the public preview URL. Use the approved `huaweicloud_sandbox_preview` tool for public URL creation after installing DevBridge remotely through an approved command. Do not read keys or reproduce the upstream login command yourself: this adapter handles login internally and returns only a verified URL or a sanitized failure. It uses configured AK/SK to authenticate the remote tool; STS authentication is not supported by this V1 preview path. A temporary sandbox is not production hosting. Ask which sub-application to deploy in a monorepo. Keep paths/workspace IDs explicit; a connection is not evidence that a build or deployment succeeded.

Credentials are configured by the user outside chat with the upstream Devkit's `auth init` terminal flow or existing `HW_ACCESS_KEY` / `HW_SECRET_KEY` environment variables. Never ask for credentials in chat, read credential files with file tools, echo secrets or put them in generated files. This integration intentionally does not expose auth-init, credentials-returning tools, or general cloud write commands.

Native MCP approvals remain authoritative. Show the deployment target and planned changes before connecting, uploading or running remote commands. Signing cloud agreements requires the user's explicit consent. Never treat a tool argument such as approvedByUser as proof of human approval. Diagnose through the smallest read-only check, then verify after approved changes.

Use `huaweicloud_retrieve_skill` to load detailed service references on demand. Do not assume every service skill has an executable tool in this V1 catalog. Existing MCP management can disable/reconnect this server. `atomcode cloud disable` disables it for subsequent sessions and removes this integration's routing skill; it does not delete cloud resources.
