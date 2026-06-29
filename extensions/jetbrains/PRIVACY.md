# AtomCode Privacy Policy

Last updated: June 23, 2026

AtomCode for JetBrains connects JetBrains IDEs to a local AtomCode daemon. This policy explains what data the JetBrains plugin handles, what it sends to the local daemon, and when data may leave your machine.

## Data handled by the plugin

The JetBrains plugin may process the following data when you use AtomCode features:

- Chat prompts and assistant responses.
- Selected code, attached files, current file context, and project metadata that you choose or configure AtomCode to include.
- Local project paths and session metadata used to keep AtomCode sessions associated with your project.
- Provider settings that you enter, including provider type, model name, base URL, and API key.
- Local diagnostics generated on request. Diagnostics are redacted before display or copying where possible.

## Local daemon communication

By default, the plugin connects to an AtomCode daemon at `127.0.0.1:13456`. The plugin can also start a bundled or configured daemon process on your machine. The plugin sends requests to this local daemon so it can run coding-agent workflows, manage sessions, communicate with model providers, and apply user-approved actions.

The JetBrains plugin does not intentionally send your code or project data directly to AtomCode servers. Data leaves the IDE through the local daemon only as needed for user-initiated actions, configured provider workflows, authentication, or telemetry described below.

## External model providers

If you configure providers such as OpenAI, Claude, Ollama, or a custom compatible endpoint, the local AtomCode daemon may send prompts, selected code, file context, project metadata, and related request data to that provider according to your configuration and the provider's terms.

API keys entered in the JetBrains plugin are sent to the local AtomCode daemon so the daemon can store or use them for provider requests. Do not enter API keys unless you trust the local daemon and the configured provider.

## Telemetry

The JetBrains plugin UI does not implement its own telemetry pipeline. The local AtomCode daemon shares AtomCode's anonymous usage telemetry system, which is enabled by default unless you opt out.

AtomCode telemetry is documented in `../../docs/telemetry.md`. It is designed to avoid collecting prompt text, response text, file contents, full file paths, git remote URLs, tool argument values, environment variable values, and local paths in panic backtraces. Telemetry may include anonymous product usage metadata such as event type, app version, operating system, architecture, locale, provider/model names, token counts, tool call counts, error status, and a generated device identifier stored under `~/.atomcode/`.

You can disable telemetry by using any supported opt-out mechanism, including:

- `ATOMCODE_TELEMETRY=0`
- `DO_NOT_TRACK=1`
- `atomcode telemetry disable`
- `[telemetry] enabled = false` in `~/.atomcode/config.toml`

The JetBrains plugin starts the daemon with the `jetbrains` client identifier so daemon-originated telemetry, when enabled, is attributed to IDE usage.

## User controls

The plugin includes settings that affect what context is sent to the daemon, including daemon host and port, daemon binary path, selected-text context, relative path sharing, automatic file saving before reads, and context level. You can review and adjust these settings from the AtomCode settings page in the IDE.

## Sensitive files

AtomCode classifies sensitive paths such as private keys, `.env` files, credentials, SSH configuration, AWS configuration, GnuPG data, Terraform state, logs, dumps, backups, and similar files. Some paths are blocked, and others require stronger confirmation before being used as context. This classification is best-effort and does not replace your own review before sending context to a model provider.

## Contact

For privacy questions, contact `atomcode@atomgit.com`.
