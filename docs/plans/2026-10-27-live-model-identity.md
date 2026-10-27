# Live model identity

Use the existing runtime presentation projection to retain the lead model name
reported by AgentDescription/ProviderChanged. It survives transcript rebasing,
never reads configuration as runtime truth, ignores members and stale generations,
and carries no credentials. The hub mirrors this display metadata with its
binding. Shared snapshots and provider events expose model additively; model-only
descriptions update existing subscribers. No lifecycle or persistence changes.

WebUI keeps authoritative shared identity separate from the configured model
catalog and optimistic selection. Config polling cannot overwrite a live
provider. Missing/edited catalog entries keep the actual model label and do not
borrow capability flags from another configured endpoint. Default config still
seeds unsynchronized chats. Validate same-provider model changes, reconnect after
turn completion/compaction, member/stale descriptions, unknown selections and
cross-window selection before formatting, affected tests and compile gate.

Initialization is also seeded from the runtime's mounted ProviderSlots, because
an initial agent description is not guaranteed to enter the live replay before
the first completed turn. Session/rebuild projection seeds carry the same model
alongside their snapshot and mode. The integration scenario deliberately gives
config a different alias than the mounted provider, then checks the name during
a running turn, after provider reassembly and after a fresh session.

Validation: 1,564 coding/daemon tests passed (9 skipped); the strengthened real
runtime alias scenario passed separately. All 344 frontend tests, typecheck,
production build, formatting and the workspace compilation gate passed.
A headless Chromium selector harness verifies polling/catalog removal cannot
replace the runtime name, same-alias model changes are rendered, the unrelated
default is not highlighted, and candidate selection/authoritative rollback work.
The only current `/live` consumer in this workspace is WebUI; IDE `/chat` consumers
keep their existing runtime_info contract. No running user task was restarted.

Pre-push audit also exercises a different configured alias during provider
reassembly. Both provider-reassembly paths now report the mounted provider's
model name in ProviderChanged, matching the initial/snapshot projection; config
selection and cost attribution remain unchanged. The existing generation test
continues to assert configured context identity while checking the mounted mock
name in the presentation event. The full 1,564 coding/daemon tests passed again
(9 skipped), including the strengthened alias scenario, and the final workspace
compilation gate passed. Frontend code is unchanged from its 344-test, typecheck,
build and Chromium validation above.
