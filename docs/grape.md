# Grape: authorize once, use native tools

This integration connects AtomCode to a deployed Grape service. Grape owns its account, repository access, remote workers and pipelines; AtomCode keeps the current development session and supplies typed tools. It does not reuse an AtomGit token, install Grape, upload the working directory or dispatch a task when connecting.

Both the Grape device-authorization endpoints and this AtomCode build are required. Deploy the Grape changes first; an older service returns an authorization-unavailable error rather than falling back to copying API keys.

## Connect

The TUI `/grape` entry is temporarily disabled until cloud device authorization is deployed. Its implementation is retained but is not mounted in the TUI command menu or dispatcher. The CLI remains available for services that already support the protocol.

The official cloud is in private beta. Sign in with GitCode at https://www.grapedev.cn/app/. If access is unavailable, use the existing “申请内测” entry; cancel the current AtomCode wait and reconnect after approval. Submitting an application does not grant access or complete authorization. An older cloud deployment without device authorization fails immediately with deployment guidance.

From a terminal:

```sh
atomcode grape connect
```

AtomCode opens the service's authorization page. If already signed in to Grape, compare the displayed code and approve. Otherwise sign in through Grape's existing login, then approve. The CLI waits for completion and securely saves a dedicated, revocable credential. No key is copied into chat or configuration.

For private deployments, use `atomcode grape connect --server https://your-grape.example/`. The `/app/` URL is the browser application, not the API base; the official API base is the site root.

For SSH/headless machines:

```sh
atomcode grape connect --server https://your-grape.example --no-browser
```

Open the printed URL on another device. Run this command on the machine, OS account and product directory that own the AtomCode runtime. Logging in on a laptop does not connect a daemon running under another account or host. HTTP is accepted only for literal loopback development services; deployed services require HTTPS.

The stored connection is read at request time, so the new CLI/TUI/WebUI runtimes share it without a session restart after authorization. There is no separate MCP server to install. Connection startup currently uses the human CLI command; this version does not add a WebUI connection button. A daemon under another OS account must be authorized on its own host.

```sh
atomcode grape status       # local saved identity; does not verify remote health
atomcode grape disconnect   # revoke this credential remotely, then remove locally
```

If remote revocation cannot be confirmed, disconnect retains the local record and reports failure. Existing Grape API-key management can also revoke the `AtomCode ...` entry. Grape logout/password-change invalidates the integration's account-version binding. Rejected, expired or failed authorization leaves an existing connection unchanged. Only one connect/disconnect operation can run per product directory. Successful reauthorization saves the new credential before revoking the old one; failed persistence revokes the newly issued credential when possible. Unconfirmed revocation is reported for manual cleanup.

## Use from a conversation

- “List the Grape scenes available for owner/repo.”
- “Hand Issue #123 and PR #456 to Grape's QA role. Review pushed commit abc only; then show me the event ID.”
- “Check that event's state and fetch the next ten log messages.”
- “Search this repository's Grape knowledge for architecture constraints.”

`grape_project` provides hosted repositories and scenes. `grape_task` provides event status, incremental logs, existing-Issue handoff and handoff verification. `grape_knowledge` provides knowledge search and detail. The host injects credentials; model-facing arguments never contain them. Query actions are read-only; handoff uses the normal risky-tool approval policy. Authorizing the connection does not preapprove dispatches. Existing user-selected approval modes remain in effect.

A remote handoff can trigger the selected scene's downstream pipeline, including deployment if configured by the repository. The adapter does not grant direct deployment, account administration, configuration editing or arbitrary REST access. Grape still enforces repository/organization access. Local tool grants do not automatically become worker permissions.

Handoff requires a real Issue number and a stable `idempotency_key`. A receipt with `seq`/`task_id` means submission, not successful completion. `seq` identifies an event; `task_id` identifies an execution. On ambiguous failure or cancellation, query `handoff_status` with the same repository, Issue and key before resending. Never switch keys to bypass uncertainty. Local uncommitted changes are not included: push the intended version deliberately and identify its branch/commit/PR in the task context.

## Validation

Python route tests cover explicit consent, pending/backoff, expiry, denial, one-time and concurrent exchange, account-version checks, scope enforcement, self-revocation and knowledge detail ACL. Rust tests cover origin validation, private storage, host-only credential injection, bounded reads, stable handoff payloads, risk classification and no automatic write retries. The cross-repository smoke test uses an isolated local Grape database, a real native CLI and a headless browser, with no model calls or remote work:

```sh
GRAPE_SOURCE=/path/to/grape \
GRAPE_PYTHON=/path/to/grape/.venv/bin/python \
node webui/scripts/grape-device-e2e.mjs /path/to/atomcode
```

The protocol follows the device authorization pattern in RFC 8628 using private JSON endpoints. This is not a claim of general OAuth-server interoperability. End-to-end testing against an actual deployed Grape instance still requires deployment and an authorized user login.
