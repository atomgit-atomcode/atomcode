# Stored SSH passphrases and controlled agent loading

Goal: explicitly remember SSH key passphrases in the OS credential store, and load registered keys into an existing agent at host startup across macOS, Linux and Windows.

Ownership: atomcode-auth owns the OS store adapter, private registration index and controlled OpenSSH loader. CLI owns explicit remember/unlock/forget commands and the TUI authentication panel; the TUI owns the masked, zeroizing input channel. CLI/daemon launchers own startup invocation; no kernel command, row configuration or model-facing secret API is added.

- Native keyring on macOS/Windows. Linux uses the system Secret Service via libsecret's secret-tool; no plaintext or mock backend fallback. Noninteractive retrieval refuses system unlock UI. Missing/locked backend returns failure.
- Private metadata under the existing protected auth directory binds each registration to a canonical file and its SHA-256; each operation rechecks it. Secrets never enter that JSON, argv, environment or transcript.
- A host-spawned worker has a bounded lifetime. It feeds one passphrase only to a fixed system ssh-add process through a private PTY. No generic askpass endpoint or raw-secret helper is added. Existing agent sockets are inherited; no new agent owner is created.
- Registration requires human input in the dedicated TUI field or a real external terminal, and successful key loading. Loading follows the system agent policy by default, with an optional configured lifetime; missing agent, wrong passphrase, changed key or timeout fails explicitly. Forget disables startup first, then deletes storage; already-loaded identities in other agents remain outside its revocation scope.
- Test metadata isolation, rollback, changed keys, locked/missing stores and actual encrypted-key loading against an isolated test agent. Test CLI safe failures; run affected crates and the compile/format gates. Platform-specific native-store integration requires corresponding host verification and is reported separately.

First stage is committed separately as 63d7e9e1d. Existing Git-index work is separate and must be preserved.

## TUI setup

The `/git-auth` panel lists candidate key paths and safe registration state, with reload/status and confirmed forget actions. Command arguments are excluded from echo/history. Passwords use a TUI-only protected service, outside kernel questions and session facts. The field refuses busy input and its cancellation token cannot dismiss another request. The launcher supplies product directories directly to the plugin instance.

A single operation owns cancellation and completion. Input cancellation drops the zeroizing value; cancellation around native storage writes either cleans up or leaves a disabled cleanup record. Blocking work retains ownership until completion. UI unload aborts its waiter and requests cooperative cancellation; it cannot revoke a key already loaded into the external agent.

Default lifetime: no ssh-add -t argument. TUI offers system default, one hour and 24 hours; CLI accepts optional --lifetime seconds. Existing numeric registrations retain their explicit lifetime. No renewal scheduler or agent lifecycle owner is added.
