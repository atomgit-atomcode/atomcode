# Huawei Cloud sandbox preview (V1)

AtomCode includes an opt-in HuaweiCloud Devkit adapter. It is off until explicitly enabled; normal sessions do not install dependencies or start a Huawei Cloud process. It uses the existing MCP lifecycle, panel, tool search and approval mechanisms. This version supports sandbox preview and diagnostics, not unrestricted cloud administration.

## Enable and diagnose

Install Node.js **22.19 or newer** with npm, then run:

```sh
atomcode cloud enable
atomcode cloud doctor
atomcode cloud status
```

Enable installs a pinned `undici` dependency (npm network access required; lifecycle scripts disabled) into a versioned integration cache below the product's user directory, installs one routing skill and registers the `huaweicloud` server in the user's `mcp.json`. Existing unrelated settings are preserved. A pre-existing user-owned `huaweicloud` server or modified routing skill is not overwritten. No cloud API is called during enable or doctor.

Restart the session after enable. The server appears in the existing MCP panel. With the default MCP search policy, the model searches for related tools instead of loading the entire catalog on every request. Explicit `tools.mcp.tool_search=off` remains respected.

## Account setup

Configure credentials outside the conversation: use the upstream Devkit's terminal `auth init` workflow, or arrange `HW_ACCESS_KEY` / `HW_SECRET_KEY` (and optional session token) in the environment of the process launching AtomCode. Restart AtomCode after changing its launcher environment. Do not paste keys into chat or store them in project files or MCP JSON. Node inherits its host's environment; the adapter can also use the upstream Devkit's existing local credential resolution.

`doctor` checks Node, npm and the installed adapter/dependencies; it does not verify cloud account permissions or access credential values. Model-facing auth-init, credential-returning and unrestricted cloud-write tools are excluded. Cloud account registration, applicable agreements and permissions may still be required when first connecting.

## Preview a project

Example request:

> Inspect this project and deploy a temporary preview to a Huawei Cloud sandbox. Show the target and planned changes before uploading or executing commands, verify the service and return its public URL.

The routing skill uses `mcp_search`, loads the `huawei-sandbox` procedure through `huaweicloud_retrieve_skill`, checks the account, connects, uploads the project, builds and starts it, then uses `huaweicloud_sandbox_preview` to authenticate the remote preview tool, verify the public URL and return it. The preview tool requires DevBridge to be installed in the sandbox through an approved remote command. It uses configured AK/SK internally for remote DevBridge authentication, where the remote tool may retain authentication state; it does not return keys or raw terminal output. This preview-login path does not support STS session credentials and fails explicitly rather than ignoring a session token. No cloud preview was executed during local verification. A monorepo may require choosing the application first. Sandbox availability and preview lifetime depend on the service; this is not long-term production hosting.

Uploads exclude `.git`, `node_modules`, `.env`, `.env.*`, `.hcloud`, `.huaweicloud`, `.aws`, `.ssh`, `*.pem` and `*.key` by default, in addition to user exclusions. This is a basic exclusion list, not a guarantee that arbitrary projects contain no private data: review the upload target and content. Required application environment variables must be configured intentionally in the sandbox rather than transferred as a local secret file.

Remote execution, connection, upload and agreement-signing tools retain the normal non-read-only MCP approvals. Enabling does not set server trust or automatic-approval grants. Existing user-selected approval mode still applies. Agreement signing requires explicit consent. Do not bypass a denied operation by generating a shell command.

## Disable

```sh
atomcode cloud disable
```

This disables the user-level server for new sessions and removes the unmodified routing skill. For a running session, use the MCP panel to disable it or restart. It does not delete cloud resources, the integration cache, existing credentials or user-modified skills. Project MCP configuration can intentionally override the user-level server as with other MCP integrations.

The cache is content-addressed. `status` does not dump config or credentials. If `doctor` reports damaged dependencies, preserve any needed local changes, remove the reported integration cache directory and enable again. Cached source files are checked before reuse; modified files are not silently overwritten.

## Upstream and checks

The bundled runtime and skill references are from HuaweiCloud Devkit commit `3ab9162868c17534544183c667abbd37113eb32e` under Apache-2.0. License, notice and provenance are included under `crates/atomcode-cli/assets/huaweicloud`. The setup CLI requiring native SQLite is omitted. Only the small AtomCode routing skill is installed in the user's skill catalog; upstream service references remain on-demand MCP content.

Offline protocol tests:

```sh
node scripts/huaweicloud-assets.mjs --check
node --test crates/atomcode-cli/assets/huaweicloud/adapter.test.mjs
cargo nextest run -p atomcode -E 'test(cloud_cmd)'
```

A real cloud preview requires an authorized account and explicit approval of the intended upload/execution. Local installation and synthetic MCP tests do not establish that a real cloud deployment succeeded.

Optional install smoke test (requires npm registry access, runs under a throwaway product directory and performs no cloud operations):

```sh
node scripts/huaweicloud-smoke.mjs target/debug/atomcode
```
