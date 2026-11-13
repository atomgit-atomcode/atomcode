# Unattended Git authentication

AtomCode uses the SSH agent and Git credential helpers available to its process. It does not store SSH passphrases, start an agent, load keys automatically, or retrieve a password for the model.

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
