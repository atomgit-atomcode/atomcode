# Unattended Git authentication diagnostics

Goal: reuse external SSH agents and Git credential helpers without storing passphrases in AtomCode.

Ownership: the CLI owns local diagnostic probes. The common shell spawn path applies prompt policy; TUI-owned askpass remains interactive. No kernel, session or provider lifecycle changes. Existing unrelated Git-index work is preserved.

1. Add `git-auth status [--json]`: bounded local `ssh-add -l` and Git configuration-name probes. Discard child output, never call a credential helper or contact a remote. Report setup rather than successful authentication; missing sockets are not proof of failure on Windows.
2. Without TUI askpass, set SSH_ASKPASS_REQUIRE=never and GCM_INTERACTIVE=never on the shared shell spawn command. Preserve agent environment, helpers, SSH command overrides and configured identities. Arbitrary custom helpers still rely on existing cancellation/timeouts; TUI background operations share the TUI askpass policy.
3. Extend foreground/background authentication hints for SSH failures paired with Git's remote error. Do not retry, inject secrets, or bypass push approval. Cover failure classification and prompt policy with tests, then run affected crate tests and the compile gate.

Deferred: remote access checks, keyring storage, agent ownership, automatic key loading and passphrase retrieval. Agent availability cannot prove authorization, host trust, signing-key availability or absence of agent-side confirmation prompts.
