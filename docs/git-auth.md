# Unattended Git authentication

AtomCode uses the SSH agent and Git credential helpers available to its process. By default it does not store SSH passphrases or start an agent. Explicit opt-in registrations below use the OS credential store and controlled agent loading; no command returns a password to the model.

## Inspect local setup

Run this from the repository in the same environment that starts AtomCode:

```sh
atomcode git-auth status
atomcode git-auth status --json
```

The check runs `ssh-add -l` and queries Git credential-helper setting names. Each probe has a five-second deadline. It discards key identities, configuration names/values and error output, and never contacts a remote or invokes a credential helper. It does not prove push access: a helper declaration can be empty/reset or scoped to another remote, and loaded keys can be wrong, expired or require confirmation. A missing `SSH_AUTH_SOCK` is informational; Windows can use its agent service without this variable.

## Prepare before leaving

In an external terminal, connect to your existing SSH agent and load the intended key (for example, `ssh-add -t 8h ~/.ssh/id_ed25519` on OpenSSH versions supporting key lifetimes). Enter the passphrase there. Start AtomCode from an environment that can access the same agent. A daemon, service or container can have a different environment and user; a successful check in your desktop terminal does not establish its access.

For HTTPS, configure and unlock the remote's Git credential helper externally. Do not put passwords or tokens in a chat message, command argument, repository URL or AtomCode configuration. AtomCode's provider/AtomGit login is not proof that this repository's Git transport is authenticated.

Verify the intended remote and host key before unattended work. Do not disable SSH host-key verification to work around a failure. Hardware keys, agent confirmation policies, signed commits and host trust can still require human action.

## Prompt behavior

The shared local process launcher keeps `SSH_AUTH_SOCK`, credential-helper settings and SSH command overrides intact. Git cannot prompt in the terminal (`GIT_TERMINAL_PROMPT=0`). When there is no TUI askpass service, it also sets `SSH_ASKPASS_REQUIRE=never` and `GCM_INTERACTIVE=never`: OpenSSH will not launch askpass and Git Credential Manager cannot request interactive sign-in. Already available agent identities and stored helper credentials can still work.

TUI askpass remains interactive, including background commands launched by that TUI process. Arbitrary custom helpers or SSH wrappers may have their own UI; the existing process timeout/cancellation remains the limit for them. These settings do not universally guarantee that every external program is non-interactive. Other execution worlds must implement their own transport policy.

A failed Git command receives diagnostic advice for recognized HTTPS errors or SSH authentication/host-key failures paired with Git's remote-repository error. Foreground and background calls share this classification. Commands are not retried, remotes are not rewritten, and authentication never bypasses operation approval.

References: [OpenSSH ssh-add](https://man.openbsd.org/ssh-add), [OpenSSH ssh](https://man.openbsd.org/ssh), [Git Credential Manager environment settings](https://github.com/git-ecosystem/git-credential-manager/blob/main/docs/environment.md).

## Remember a passphrase in the OS credential store

In the TUI, run `/git-auth` and choose **选择 SSH 密钥并记住口令**. Select a key and choose its loaded lifetime (system default, one hour or 24 hours), then enter its passphrase in the masked input field. Enter saves it; Esc cancels. By default AtomCode passes no lifetime override to ssh-add: the system agent governs the loaded identity, which may remain available until removal or agent exit. An optional lifetime limits the loaded identity, not how long the passphrase stays in OS storage. Use Forget to remove the stored passphrase. The panel also offers local status, explicit reload and confirmed deletion of saved credentials. `/git-auth cancel` requests cancellation of an active operation; an in-progress OS call can finish before cancellation takes effect. Already-loaded agent identities remain until removed or expired.

The picker lists private-key paths with matching `.pub` files in `~/.ssh`; it does not read private-key contents. For another location, use `/git-auth remember /absolute/path/to/key`. Never put the passphrase in command arguments. The dedicated input passes a zeroizing value directly to the host-owned operation, outside conversation facts and command history. A busy SSH/sudo password field is not displaced.

With the intended agent running, the standalone CLI remains available:

```sh
atomcode git-auth remember --key ~/.ssh/id_ed25519
atomcode git-auth status
```

`remember` requires a real terminal and hidden password input; there is no password argument or stdin import. It first validates the passphrase by loading the specified key into the current agent. On macOS/Windows the passphrase is stored in the native credential store. Linux requires the system `secret-tool` utility (libsecret) and an available Secret Service session. No backend falls back to a file or a mock store. A store can require explicit OS confirmation during registration.

New CLI coding sessions (TUI, headless, background, scheduled execution and ACP) and daemon/WebUI hosts attempt to load explicitly registered keys at startup. The daemon's OS identity, agent and keyring session must be configured too. The OS owns the agent lifecycle (including macOS socket activation); AtomCode does not spawn a separate agent, wait for a password, or retry. An existing long-running host does not reload newly registered credentials automatically; either `remember` has already loaded its shared agent or restart that host in the intended environment. Optional lifetimes (`--lifetime SECONDS`) are requested through OpenSSH `ssh-add -t` and must be between 60 seconds and 24 hours; support depends on the installed OpenSSH/agent implementation. Keys are not renewed silently when they expire.

You can explicitly reload after unlocking your OS store yourself:

```sh
atomcode git-auth unlock
```

Loading runs in an isolated worker, limited to 35 seconds per key, with at most eight registrations. Native Keychain UI is disabled in the worker; on Linux it searches Secret Service without requesting item unlock. Locked/unavailable stores, absent agents, changed key files, rejected passphrases and timeouts fail without an interactive fallback. Other coding work may still start after a startup warning; authentication has not been declared ready. Windows uses system OpenSSH; configure Git to use a compatible agent/client rather than assuming a Git Bash agent and the Windows agent are interchangeable.

The private `auth/ssh-keys.json` index stores only canonical paths, file hashes, product-home namespace bindings, optional lifetimes and enabled/cleanup state. It is outside the printable runtime configuration tree, under the existing credential-path protection. Passphrases go only from OS storage to a fixed system `ssh-add` process through a private terminal, never into argv, environment, tool responses, logs or session facts. PTY output is discarded. No general-purpose secret-returning askpass endpoint is added.

This is an interface/data-flow guarantee, not OS isolation from arbitrary same-user code. In particular, a process with sufficient user-level OS credential-store access can independently query that store; protecting against hostile arbitrary code requires an OS sandbox or a separate security principal.

## Forget and uninstall

```sh
atomcode git-auth forget --key ~/.ssh/id_ed25519
```

Forget first disables automatic loading, then removes the OS-store entry. Failed deletion leaves a disabled cleanup record so the command can be retried; `status` reports such records. Use the original absolute path if the key was removed or replaced. Already-loaded identities remain usable in each agent until removed or expired. For immediate revocation remove that identity from every relevant agent; AtomCode never runs `ssh-add -D` or deletes unrelated keys.

Choosing credential removal during uninstall also attempts OS-store cleanup. If cleanup fails, the registration index is retained and the failure is reported instead of silently leaving an untracked credential. The saved key itself is never deleted. Registration/loading failures or cancellation can leave a successfully loaded identity in the agent for its requested lifetime even when persistent setup fails; the error reports this partial outcome.

Registrations are bound to their original product home. Moving/copying the home does not silently grant the new installation access: loading fails until re-registration. The original stored namespace is retained for credential removal.

Stored-key loading verifies that the agent endpoint belongs to system OpenSSH (including the protected macOS launchd job and Windows OpenSSH service), rather than trusting an arbitrary `SSH_AUTH_SOCK`. Loading uses a scrubbed child environment and fixed system executables, not PATH-selected programs. Custom, forwarded and proxied agents remain usable through the ordinary external setup path but are not automatic destinations for saved credentials. This verification is defense in depth, not protection against same-user ptrace, injected code or a compromised OS account.
