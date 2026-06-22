# AtomCode for JetBrains

AtomCode for JetBrains is the IntelliJ Platform frontend for the local
`atomcode-daemon`.

## Development

Requirements:

- JDK 21
- Generated Gradle wrapper
- Kotlin Gradle Plugin 2.2.21

Useful commands:

```bash
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" test
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" verifyPlugin
```

Use IntelliJ IDEA Community Edition for local development and manual smoke
testing. It does not require a commercial JetBrains license. For `runIde`,
prefer the local Community Edition path or omit `platformLocalPath` so Gradle
downloads the configured Community IDE:

```bash
./gradlew runIde
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" runIde
```

Using `-PplatformLocalPath=/Applications/IntelliJ IDEA.app` points `runIde` at
IntelliJ IDEA Ultimate, which requires a valid Ultimate license even in the
sandbox IDE.

If Community Edition is not installed locally, the first `./gradlew runIde`
may spend a long time downloading and unpacking the configured Community IDE.
For a faster manual smoke test, build the zip and install it into an already
running IDE via `Install Plugin from Disk...`.

If a local non-IDEA verifier run is killed while building searchable options in
a headless environment, rerun that compatibility check with:

```bash
./gradlew "-PplatformLocalPath=/Applications/GoLand.app" "-PskipSearchableOptions=true" verifyPlugin
```

Keep searchable options enabled for normal release builds unless the local IDE
process is failing for environment reasons.

If IntelliJ IDEA Community Edition is already installed locally, pass
`platformLocalPath` to avoid downloading an IDE distribution during development
and to make `verifyPlugin` validate the local IDE instead of the default remote
recommended IDE matrix:

```bash
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" test
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" verifyPlugin
```

Use IntelliJ IDEA Ultimate only when the test machine has an active Ultimate
license. A license error such as `There are no valid licenses associated with
the account ...` is an IDE entitlement problem, not a plugin build failure.

The packaged plugin zip is written to `build/distributions/`.

For an official release containing the private signer and bundled daemons for
all supported platforms, run this from the repository root:

```bash
./build-official-jetbrains.sh [branch]
```

The script writes the plugin zip, daemon binaries, and checksums to
`dist/v<workspace-version>/`. Use `./build-official-jetbrains.sh clean` to
restore the public stub files after an interrupted build.

The build optionally bundles `atomcode-daemon` into `resources/bin/<platform>`.
Local development builds automatically include the current platform daemon from
`target/release` or `target/debug` when present. Marketplace/release builds can
provide explicit daemon binaries with:

```bash
ATOMCODE_DAEMON_DARWIN_ARM64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_DARWIN_X64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_LINUX_X64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_LINUX_ARM64=/path/to/atomcode-daemon \
ATOMCODE_DAEMON_WIN32_X64=/path/to/atomcode-daemon.exe \
./gradlew buildPlugin
```

At runtime, daemon discovery checks the user-configured path first, then the
bundled daemon, then `atomcode`/`atomcode-daemon` on PATH and common install
locations. Bundled daemon resources are extracted to a temporary executable path
before startup because JetBrains plugin resources live inside the plugin jar.
When the plugin is using a bundled daemon and finds an already-running
`atomcode-daemon` on the configured port, it compares `/health.version` with
`resources/bin/daemon-version.txt`. A mismatch triggers a graceful `/shutdown`
and restart into the bundled daemon; if the old daemon cannot be stopped, the
connection state reports an incompatible daemon instead of silently talking to
the wrong version.
Concurrent connection attempts inside one project share the same in-flight
startup future so IDE startup, status updates, and multiple chat tabs do not
spawn duplicate daemon processes.

The build uses Kotlin Gradle Plugin `2.2.21` with `-Xjvm-default=all`. This
combination can compile against the local IntelliJ IDEA Community Edition
2026.1 Kotlin metadata while avoiding generated ToolWindowFactory bridge
methods that older 2025.1 verifiers classify as internal API usage.

## v0.1 Scope

- AtomCode Tool Window with chat, setup status, model selection, and session controls
- Multiple AtomCode Tool Window chat tabs with isolated active sessions, matching VS Code's new-tab workflow with a JetBrains-native tabbed tool window
- JetBrains status bar widget for AtomCode connection state and quick chat access
- Project startup connection initialization and periodic daemon health checks
- Diagnostics dialog that copies redacted IDE, daemon, settings, setup, and queue state
- Multi-line chat input with configurable Enter/Ctrl+Enter send behavior and chat font size
- Bundled/local daemon discovery and optional auto-start hooks
- Auto-save files before AtomCode reads project content
- Privacy controls for selected text context and relative/absolute path display
- Daemon REST/SSE client for health, setup, providers, sessions, chat, stop, permissions, and file-change workflows
- Provider create/edit/delete, default model switching, and thinking/reasoning controls
- CodingPlan setup trigger
- Streaming chat with text, reasoning, tools, artifacts, tokens, stop, error, and permission events
- Queue another chat message while a response is generating
- Copy the last assistant response and preview/apply the last fenced code block to the active editor
- Session new/load/rename/delete
- Session History dialog with search, load, rename, single/bulk delete, and refresh
- IDE actions for Open Chat, Focus Input, New Conversation, Stop Generation, Open Changes, and Open Settings
- IDE action for Open Chat in New Tab
- Editor actions for Explain Selection, Fix Selection, Optimize Selection, and Add Selection/File as Context
- Alt+Enter intentions for Explain Selection, Fix Selection, Optimize Selection, and Add Selection/File as Context
- Context attachment queue for the next chat message, including IDE file chooser attachment
- Context level behavior: Minimal, CurrentFile automatic context, or ProjectContext metadata plus current file
- Git/Local Changes entry point for reviewing modified files

## JetBrains Actions

The plugin registers JetBrains-native actions so users can invoke AtomCode from
Search Everywhere, the Tools menu, the editor context menu, Alt+Enter
intentions, or custom keymaps:

- `AtomCode: Open Chat`
- `AtomCode: Open Chat in New Tab`
- `AtomCode: Focus Input` (`Ctrl+Alt+Shift+I`)
- `AtomCode: New Conversation` (`Ctrl+Alt+Shift+N`)
- `AtomCode: Stop Generation`
- `AtomCode: Open Changes`
- `AtomCode: Open Settings`
- `AtomCode: Explain Selection` (`Ctrl+Alt+Shift+E`)
- `AtomCode: Fix Selection`
- `AtomCode: Optimize Selection`
- `AtomCode: Add Selection/File as Context`

Provider rows in the tool window expose `Thinking` controls for the daemon's
`/providers/{name}/thinking` API, matching the VS Code provider settings
workflow for models that support reasoning budgets.

The chat input also supports VS Code-aligned slash commands:

- `/login`
- `/codingplan`
- `/explain`
- `/fix`
- `/test`
- `/refactor`
- `/docs`
- `/review`
- `/optimize`

## Install From Disk

1. Build the plugin:

   ```bash
   ./gradlew "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
   ```

2. In IntelliJ IDEA Community Edition, open `Settings | Plugins`.
3. Use the gear menu and choose `Install Plugin from Disk...`.
4. Select:

   ```text
   build/distributions/atomcode-jetbrains-0.1.0.zip
   ```

5. Restart the IDE if prompted.

## End-To-End Smoke Test

Run this checklist before considering the plugin build usable:

1. Open the `AtomCode` Tool Window.
2. Run `AtomCode: Open Chat in New Tab` or click `New Tab`, confirm a second closeable chat tab appears, and confirm each tab keeps its own loaded/new session while editor/context actions target the selected tab.
3. Click `Start` and confirm the status becomes connected.
4. Confirm the IDE status bar shows `AtomCode` connected state and clicking it focuses the selected chat tab.
5. Click `Settings` or run `AtomCode: Open Settings`, adjust a harmless setting, and confirm the AtomCode settings page opens.
6. Click `Provider`, create an OpenAI/Claude/Ollama provider, and set it as default.
7. Confirm setup status shows provider count and the model dropdown lists the provider model.
8. Send a simple chat prompt and confirm streaming output appears.
9. Type a multi-line prompt and confirm Enter/Ctrl+Enter behavior follows the AtomCode settings.
10. Change `Chat font size` in settings and reopen/focus the tool window; confirm chat/input text size follows it.
11. Confirm `Auto-save files before AtomCode reads them` is enabled, edit a file without saving, attach/send, and confirm saved content is used.
12. While a response is streaming, type another prompt, click `Queue`, and confirm it sends automatically after the current response finishes.
13. Ask for a code block, click `Copy Last`, and confirm the last assistant response is copied.
14. With an editor open, click `Apply Code`, inspect the JetBrains diff preview, confirm, and check that the last fenced code block inserts at the caret or replaces the selection.
15. Click `Stop` during a long response and confirm generation stops.
16. Open an editor file, right-click `AtomCode: Add Selection/File as Context`, then send a prompt and confirm the context is shown and used.
17. Disable `Allow selected text context` in settings and confirm selection actions are disabled while whole-file context still works.
18. Toggle `Send relative path with selection` and confirm attached context labels use relative or absolute paths accordingly.
19. Set `Context level` to `CurrentFile`, send a prompt with an editor open, and confirm the current file is included automatically.
20. Set `Context level` to `ProjectContext`, send a prompt, and confirm project metadata plus current file context are included.
21. Click `Attach File`, select a project file, then send a prompt and confirm the file context is shown and used.
22. Select code and run `AtomCode: Explain Selection`.
23. Create a new session, send a message, refresh sessions, reload that session, rename it, and delete it.
24. Click `History`, search for a session, load it, rename it, select multiple sessions, bulk delete them, and refresh the list.
25. Click `Diagnostics`, confirm a redacted diagnostics report opens, and confirm it is copied to the clipboard.
26. Edit a file in the project and click `Changes`; confirm Local Changes opens and changed files are opened.

## Verification Status

Known license-free local checks:

```bash
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" test
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" buildPlugin
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA CE.app" verifyPlugin
```

The latest local Community Edition verifier run passed against
`IC-261.25134.95` and wrote its HTML report to
`build/reports/pluginVerifier/IC-261.25134.95/report.html`.

Ultimate compatibility regression checks also pass locally without launching the
licensed IDE UI:

```bash
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA.app" test
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA.app" buildPlugin
./gradlew --no-daemon "-PplatformLocalPath=/Applications/IntelliJ IDEA.app" verifyPlugin
```

The latest local Ultimate verifier run passed against `IU-251.26094.121` and
wrote its HTML report to
`build/reports/pluginVerifier/IU-251.26094.121/report.html`.

Additional local compatibility checks passed against:

- `PY-251.26927.74` (`/Applications/PyCharm.app`)
- `GO-251.26094.127` (`/Applications/GoLand.app`, verifier run with `-PskipSearchableOptions=true` after the local searchable-options IDE process exited 137)

Daemon preflight smoke:

```bash
cd webui && npm ci --cache .npm-cache && npm run build
cargo check -p atomcode-daemon
cargo build -p atomcode-daemon
./target/debug/atomcode-daemon --host 127.0.0.1 --port 13456 --idle-timeout 0 --no-telemetry --client jetbrains
curl -sS http://127.0.0.1:13456/health
curl -sS -X POST http://127.0.0.1:13456/cd -H "Content-Type: application/json" -d '{"path":"/path/to/project"}'
curl -sS http://127.0.0.1:13456/auth/status
curl -sS http://127.0.0.1:13456/providers
curl -sS http://127.0.0.1:13456/models
curl -sS http://127.0.0.1:13456/sessions
curl -sS http://127.0.0.1:13456/config
```

The latest local daemon smoke returned `service=atomcode-daemon`,
`version=4.25.0`, successfully changed the project directory, and returned
auth/provider/model/session/config payloads. This verifies the HTTP endpoints
used by the JetBrains plugin before a full IDE install smoke.

## Security Notes

The plugin defaults daemon host to `127.0.0.1`, uses the daemon's HTTP API, and
does not collect plugin telemetry. Sensitive path classification is applied
before sending editor selections or files as chat context.
