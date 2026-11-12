# Controlled browser uploads

Baseline `release/v5.2.7`, `f7636549d5500f3fe129c25d94bffe00346967ee`, initially clean. Reviewed current origin policy, controlled downloads, owned artifact storage and recent integration history. Local reference remains `/Users/theo/Documents/workspace/codex`: `codex-rs/config/src/browser_use.rs` defines access/downloads/uploads/full_cdp_access vocabulary; the local tree does not supply a reusable origin-enforcing upload executor. This is an implementation task, not grounds to advertise configuration vocabulary as completed policy.

The target is explicit, backend-owned HTTP upload of host-selected bytes, with per-origin access/upload grants, ordinary MCP approval, bounded transfer/cancellation and no automatic replay or redirect resubmission. Normal browser file-input uploads and arbitrary CDP remain denied until their respective execution boundaries are implemented and verified. They must not inherit an allow merely because a controlled HTTP path exists.

## Host-selected byte snapshots

First implementation adds `upload-store.cjs` and an explicit host-only `stage-upload.cjs` utility. This utility is not an MCP tool, not a slash command, and does not broaden a managed entry's permissions. The host supplies one absolute regular file and the backend-owned output directory. Staging uses the existing output ownership marker, a separate private uploads directory and exclusive generated IDs. No caller-selected destination filename is accepted.

Source selection refuses nonregular files, final symlinks and files over 64 MiB. Opening uses nonblocking/no-follow flags where supported and validates descriptor identity; it does not claim path confinement or isolation against other same-user processes. The read descriptor, size and modification metadata are checked across copying. Selected bytes are captured independently of later changes to the source. Metadata contains only ID, basename, byte count, SHA-256 and fixed fifteen-minute validity, not source path or content. Staging cancellation cleans up its own unpublished files. The manifest publication is synchronous and exclusive, with no post-commit abort check misreporting the selected snapshot.

Readers accept only opaque v4 UUIDs, bounded exact metadata and private, owned, single-link regular files. Expired selections fail closed. Metadata and data reads are bounded before allocation. Size, descriptor stability and digest must match; the result is a bounded in-memory buffer so a future sender uses the verified bytes rather than reopening a mutable path. Expiry revokes use through this API; it does not erase old private snapshots from disk. Retention/revocation and consumption policy must be integrated before network capability delivery.

Current explicit host utility:

```sh
node integrations/browser-origin-scope/stage-upload.cjs \
  --output-dir /absolute/backend-owned-output \
  --file /absolute/explicitly-selected-file
```

It prints a metadata-only selection record. This does not send anything. An ID identifies selected bytes; it is not standalone authorization or an unforgeable grant against a process that can modify this user’s filesystem. Upload policy and normal per-call approval must independently authorize transmission. Do not put private user data into acceptance fixtures; the implemented tests use disposable synthetic bytes.

Validation: ten new Node tests and all existing policy/transfer/cookie tests pass together, 71/71 (`/tmp/atomcode-upload-store-all-final.log`). Coverage includes exact binary bytes/digest/private permissions, immutable buffer after disk changes, modified source after selection, empty/Unicode-named files, traversal/unknown IDs, malformed/oversized/linked metadata, nonregular/symlink/private-store refusal, oversize refusal, actual filesystem-observed mid-stage cancellation, expiry and the standalone host command's clean metadata-only output/refusal of relative selection. This is recorded macOS filesystem evidence, not Windows/Linux desktop acceptance. No model, browser profile or network was used by these tests.

Remaining delivery: controlled HTTP sender and truthful unknown-result semantics; per-origin upload grants and capability negotiation; managed CLI/slash staging and policy setup; approval-visible file identity; actual local HTTP(S)/MCP/runtime acceptance, including redirect refusal, denied origins, TLS/authentication, cancellation, EOF and no replay; retention/revocation; browser file-input and full-CDP policy remain separate incomplete work. Until those are implemented, v1/v2 continue to refuse upload allows and advertise no upload tool.

## Rendered WebUI revalidation

The CUA inventory now lists a Chrome extension browser, unlike the earlier empty inventory. That alone was insufficient: create/navigate, DOM snapshot, documented read-only evaluate and explicit close each failed in the computer-use interface with `Cannot read properties of undefined (reading 'timeoutMs')`; native Chrome binding timed out and reset the interface. Documentation and its troubleshooting path were read before alternatives were attempted. No hidden API, raw CDP, injected navigation or other UI mechanism was substituted.

The current WebUI and daemon builds passed. `/tmp/atomcode-webui-rendered-acceptance` uses a private local daemon, a disposable HTTP fixture and copies of two synthetic image sessions. The original user/test logs remain untouched; copied event log hashes also stayed unchanged, with zero allowed/forbidden requests and zero submissions. No page was rendered or image visually accepted. Only a new blank agent tab was created; its explicit close also failed, and it was left unmarked for automatic turn cleanup. The owned daemon shut down via authenticated POST /shutdown (200, exit zero); fixture and unused one-use handoff server were stopped. `/tmp/atomcode-webui-rendered-acceptance/ui-blocker.json` records this failure, not success. Full UI/live acceptance remains open.

Concurrent unrelated reasoning/recovery work appeared in the shared workspace during this stage. It is preserved and excluded from this change's commits. Only the upload-store sources/tests and this stage's documentation belong to this task.

Submission checks: `cargo fmt --all` and its check passed; `bash gates/compile.sh` passed workspace/all-target, feature-target and both negative-control checks (`/tmp/atomcode-upload-store-compile.log`). Cargo.lock remains unchanged with `hkdf`, `hmac` and `zeroize_derive` counts zero. These workspace checks observed the concurrent reasoning edits but do not claim that unrelated feature's behavioral acceptance; none of its files are staged here. Frontend build succeeded with its existing large-chunk warning; it is not counted as rendered UI acceptance.
