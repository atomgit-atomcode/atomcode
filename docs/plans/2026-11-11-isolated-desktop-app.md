# Isolated macOS lease App activation

Baseline: AtomCode 99fd18f2e plus the pending host-scope integration; OpenComputerUse v1.3.0 e551c13e7082ce4c097234137582fbf4a1bc647a plus the separately verified lease patch. This is the next delivery phase, not a completed implementation.

## Current boundary and hazards

Inspection of `MacOSAppAgentProxy.swift` confirms that the proxy uses one legacy default socket, terminates a mismatching agent and unlinks its socket, and the listener unlinks before binding. Listener stop compares `fstat(socketFD)` against `lstat(path)` rather than retaining the filesystem socket identity obtained after bind. None of these operations may be used against the installed user's App during experimental activation.

`PermissionSupport.currentAppBundleURL()` prefers a recognized running App bundle, but a raw development executable can discover the installed release App. Merely passing a custom executable to AtomCode is therefore insufficient. Keep the verified protocol-only direct tests separate from real App activation.

## Implementation requirements

1. Package an explicit scratch `.app` using the supported development bundle identity and a distinguishable AtomCode Lease display name. Include provenance and the exact binary; ad-hoc signing must not impersonate the installed release signer. Do not overwrite/register as the user's default App or edit TCC. Validate discovery resolves the exact scratch bundle before any proxy test.
2. Derive a short socket namespace from the exact App/binary identity, inside a private per-user directory, independently of AtomCode product homes. Never use the legacy default socket for this variant. Check socket pathname length and reject symlink/unsafe-owner/mode paths.
3. A separate agent-lifetime OS file lock belongs to the actual listener and is distinct from the per-turn desktop lease. Only its owner may reclaim a verified stale filesystem socket before bind. A competing listener must fail without unlinking the live socket; never unlink lock files.
4. A proxy validates the connected helper's exact App/executable identity both before use and after launch. A mismatch or occupied/hung namespace returns a diagnostic error, never a global terminate or unlink. Normal cleanup is limited to owned test connections and helpers.
5. After bind, retain the `lstat` identity of the filesystem socket. Unlink on cleanup only if that same owned socket still occupies the pathname. Dispose descriptors on all bind/listen/permission failures.
6. Test two proxies, concurrent listener startup, stale socket recovery, mismatched helper refusal, symlink and wrong-owner/mode rejection, binary/variant namespace separation and owned cleanup. Keep a sentinel listener alive throughout to prove the installed/neighbor namespace is untouched.
7. Only after a concrete isolated App and diagnostics are reviewable, request any missing OS permission for that exact App. Then run real GUI observe/action, in-flight cancel/proxy death, busy neighboring turn and fresh screenshot/window reference handover. Host disposal alone is not proof that the shared App's accepted GUI work ended; the native lease must remain held until it does.

The final artifact remains optional and separately installed. No native executable or browser is added to the Rust release binary by default. Windows/Linux/HarmonyOS execution and GUI acceptance remain separate requirements, not consequences of adding configuration fields.
