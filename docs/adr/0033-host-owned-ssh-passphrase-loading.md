# 0033: SSH passphrases stay in the OS store and load through a host-owned worker

Status: implemented for review; Linux/Windows native backend validation remains pending.

## Context

Unattended Git needs existing authentication capability, not a model-visible password. The current generic askpass protocol is interactive and does not prove that a password request came from SSH. Returning stored passwords through that endpoint would create a secret retrieval surface. Configuration rows and session facts must remain credential-free.

## Decision

- `atomcode-auth::git_ssh` owns private registration metadata, OS-store adapters and controlled key loading. CLI launchers and daemon startup invoke it; kernel commands and runtime rows receive no credential fields or additional agent lifecycle.
- Users opt in through a dedicated masked TUI field or hidden terminal input. The TUI field uses a zeroizing host-only channel, outside kernel questions, command history and session facts; cancellation cannot close a newer password request. macOS/Windows use native keyring backends; Linux uses libsecret's system `secret-tool`. An absent or locked store fails without a plaintext or mock-store fallback.
- Registrations bind canonical key paths and SHA-256 hashes to a product-home namespace and an optional requested agent lifetime. Pending/failed saves and deletions remain disabled but retain their cleanup identity. Moving metadata never silently grants another product home access to the original store entry.
- Startup uses isolated, bounded workers. A fixed system `ssh-add` receives one passphrase through a private PTY, with output discarded and startup/module injection variables removed. Agent ownership is checked against system OpenSSH, including macOS's protected launchd job and Windows's OpenSSH agent service. No raw-secret worker or general-purpose stored-password askpass is exposed.
- The system retains ownership of SSH agent lifetime. Authentication setup does not approve Git writes. Missing authentication does not prevent unrelated coding work from starting, and a warning is not a readiness claim.
- Forget/uninstall disable future loading before deleting the OS entry. Uncertain deletion preserves metadata for retry. Existing identities in any agent remain subject to their own lifetimes/removal; unrelated identities are never cleared.

## Limits and consequences

This protects application data flow, not against arbitrary code executing with equivalent OS credential-store privileges. Agent process checks are defense in depth, not protection against ptrace or a compromised account. Custom, proxied and forwarded agents are supported by ordinary external provisioning, but are not automatic destinations for stored credentials.

The operator must provision the service account's agent and unlocked OS store. OpenSSH agent lifetime support varies; explicit lifetime requests fail on unsupported combinations; the default passes no lifetime override and follows the agent policy. Locks, hardware confirmation, host trust and expiry can still prevent unattended Git. Remote access is not inferred from local setup checks.

Verification uses fake stores, an isolated real OpenSSH agent and encrypted test key, and subprocess CLI tests. Native Windows/Linux keyring and service behavior needs validation on those systems; independent module cross-compilation is not a substitute. See [operator instructions](../git-auth.md).
