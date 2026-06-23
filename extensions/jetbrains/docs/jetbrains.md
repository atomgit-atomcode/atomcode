# AtomCode for JetBrains

AtomCode for JetBrains brings the local AtomCode coding agent into IntelliJ-based IDEs. It provides a native tool window, editor actions, and intentions for chat-based coding workflows.

## Requirements

- A JetBrains IDE compatible with the plugin version shown on JetBrains Marketplace.
- The AtomCode plugin installed from JetBrains Marketplace or from a signed plugin ZIP.
- A local AtomCode daemon. Marketplace builds may include a bundled daemon for supported platforms, and you can also configure a custom daemon binary path.

## Installation

### From JetBrains Marketplace

1. Open `Settings | Plugins`.
2. Search for `AtomCode`.
3. Install the plugin and restart the IDE if prompted.

### From a signed ZIP

1. Open `Settings | Plugins`.
2. Choose the gear menu.
3. Select `Install Plugin from Disk...`.
4. Select the signed `atomcode-jetbrains-<version>-signed.zip` file.
5. Restart the IDE if prompted.

## Open AtomCode

Use any of these entry points:

- `Tools | AtomCode: Open Chat`
- Search Everywhere and run `AtomCode: Open Chat`
- The `AtomCode` tool window
- Editor context menu actions such as `AtomCode: Explain Selection`
- Alt+Enter intentions for selected code

## Configure the daemon

Open `Settings | Tools | AtomCode` or run `AtomCode: Open Settings`.

Available settings include:

- Daemon binary path
- Host and port, defaulting to `127.0.0.1:13456`
- Request timeout
- Chat font size
- Context level
- Selected text context
- Relative path sharing
- Auto-save before AtomCode reads files
- Chat send shortcut behavior

By default, the plugin communicates with a local daemon on `127.0.0.1`. If you configure a different host, review the privacy and security implications before sending project context.

## Configure providers

Open the AtomCode tool window and use the provider controls to add or edit a provider.

Supported provider types include:

- OpenAI-compatible providers
- Claude
- Ollama
- Custom compatible endpoints through provider base URLs

Provider settings may include a provider name, model name, base URL, and API key. API keys entered in the JetBrains plugin are sent to the local AtomCode daemon so it can store or use them for provider requests.

## Context and privacy controls

AtomCode can use editor selection, attached files, current file context, and project metadata as coding context. You control this through the AtomCode settings and through explicit editor actions.

Important controls:

- Disable selected text context if you do not want selection-based code context to be sent to the daemon.
- Use minimal context when you want prompts to include less project information.
- Review attached files and selected code before sending prompts to external model providers.
- Sensitive paths such as private keys, `.env` files, credentials, SSH configuration, AWS configuration, GnuPG data, and Terraform state receive stronger handling or blocking.

Read the privacy policy before configuring external model providers:

`../PRIVACY.md`

Telemetry details are documented here:

`../../../docs/telemetry.md`

## Common workflows

### Explain selected code

1. Select code in the editor.
2. Run `AtomCode: Explain Selection` from the editor context menu or Search Everywhere.
3. Review the generated explanation in the AtomCode tool window.

### Fix or optimize selected code

1. Select code in the editor.
2. Run `AtomCode: Fix Selection` or `AtomCode: Optimize Selection`.
3. Review AtomCode's response and apply changes only after checking the diff.

### Attach a file as context

1. Open the AtomCode tool window.
2. Use the attach-file control or `AtomCode: Add Selection/File as Context`.
3. Send a prompt that refers to the attached context.

### Review local changes

Use `AtomCode: Open Changes` to inspect project changes that AtomCode can use during review workflows.

## Troubleshooting

- If AtomCode cannot connect, check the daemon host and port in settings.
- If the daemon fails to start, configure a daemon binary path or install AtomCode separately.
- If provider requests fail, verify the provider type, model, base URL, and API key.
- If context is missing, check the context level and selected-text context settings.
- If telemetry should be disabled, set `ATOMCODE_TELEMETRY=0`, `DO_NOT_TRACK=1`, or run `atomcode telemetry disable`.

## Support

Report issues at:

`https://atomgit.com/atomgit_atomcode/atomcode/issues`

Source code:

`https://atomgit.com/atomgit_atomcode/atomcode`
