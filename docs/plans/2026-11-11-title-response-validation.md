# Validate model-generated session titles

**Goal:** Prevent failed, incomplete or protocol-contaminated title responses from being committed as successful generated titles; keep bounded UTF-8 titles without excessive shortening.

**Baseline:** release/v5.2.7, 8966af165, initially clean worktree. Reviewed current ModelTitle, first-prompt title policy, Titled consumers, recent title history, and existing coding/session_title and mechanism_rows tests. Prior evidence includes an unexpected MiniMax chat-template token in the title helper's original content field.

**Owner and failure semantics:** The existing harness ModelTitle row owns accepting model output; the existing first-prompt policy remains the only automatic Titled writer, under the same native event log and human-rename precedence. Failed/unsafe candidates return None and use the existing first-prompt fallback. No retry, additional model call, hook, lifecycle owner, persistence format, default effort or output budget is added.

## Changes

1. Share the provider's exact known native-token predicate as a small pure function, retaining the metadata-only streaming observer. Only generated title candidates use this validation; first-prompt/literal body text is preserved.
2. Require a non-truncated Done for model titles. Reject stream errors, malformed chunks, tool output, EOF without completion and candidates exceeding the existing 512-byte read allowance. Check the size before appending.
3. Yield between title stream events so continuously-ready metadata/reasoning cannot starve the existing outer 15-second timeout. No additional timer is introduced.
4. Truncate tidy's word-limited result at the largest UTF-8 boundary within max_bytes, replacing the old max_bytes/4 character approximation.
5. Test the actual ModelTitle service with scripted provider responses, including partial errors, truncation, marker splits, tools, Unicode limits, fallback and reasoning-before-title. Keep the existing human-name ownership tests.
6. Run affected crates/tests, compile gate, formatting/lock checks, and signed-release end-to-end tests with controlled SSE plus native persisted titles. Preserve public crypto stub/lock and clean temporary credentials/source clones.


The existing utility-title test exposed that the shared replay provider ended after Usage without emitting Done. Update that fixture provider to emit an explicit non-truncated Done, matching the real adapter contract; do not loosen title validation or re-record differential goldens. This terminal adds no new session fact and must leave differential logs unchanged.


## Final verification

- Complete capabilities+harness run: 2164 passed, 2 skipped. Complete coding run: 1325 passed, 11 skipped. All differential golden tests passed without changing golden files. Compile gate and formatter checks passed; root Cargo.lock is unmodified with private package counts still zero.
- Five focused ModelTitle tests exercise eleven invalid response combinations, valid reasoning/signed-block prefixes, literal XML, exact 512-byte allowance, fallback without retries, human-origin literal prefixes, UTF-8 limits, and the existing timeout against both pending and continuously-ready metadata streams.
- A signed release binary was built from the complete 8966af165 base plus this change. All four changed production sources match the tested build tree. SHA-256: bbdac881186bec45b334493ffd0a0f54a644afbbb814de31ceda5f99c5dd1699.
- Actual CLI + local controlled SSE injection: 11 cases passed (valid title, native token, truncation, provider error, missing terminal, malformed event, oversized title, tool fragment, Chinese byte limit, ASCII byte limit, literal marker in main body). Native `.events` contains exactly the expected Titled fact. Each session was resumed and kept the same title without another naming request. Main text remained intact; injected title-tool fragments never wrote a file.
- The UTF-8 native-persistence cases kept 80 ASCII bytes and 78 Chinese UTF-8 bytes respectively, instead of the old 20-character approximation. Limits never split a UTF-8 code point.
- These are deterministic protocol fault-injection tests against the release executable, not a new claim that the external gateway anomaly was reproduced or cured. No external model calls were made and no real user credentials were copied in this round. The known native-prefix check applies to generated title candidates; original body text and first-prompt fallback remain intact. Previously stored titles are not rewritten.
- Evidence directory: `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-title-check-bh1_eg8t` (verification-summary.json, sse-validation-summary.json, per-case JSONL/native event logs, build.log and regression logs). The controlled server used only loopback with a fake fixture key. Owned private build sources were removed; public crypto stub and root Cargo.lock were never overlaid.


## Latest complete-branch release refresh

Parallel browser commits landed while the first isolated build was running. Rebuilt the complete functional commit `97910188c895cd086e12fb50fface2465810cb5f` so the final executable includes both the browser changes and this title fix. Latest compile gate passed; repeated all eleven release SSE/native-persistence/resume cases successfully. No production-source changes followed that build snapshot. This is the final executable delivered at `target/release/atomcode`.

Final SHA-256: `1f85e20eb806877708370fd23fdd9065814049b0b5ab7f2323e27953cf16c0d9`; size: 40044096 bytes. Refreshed proof: `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-title-check-bh1_eg8t/latest-release/verification-summary.json`. The earlier binary digest remains historical evidence only. The repeated controlled tests again used no external model calls or real credentials; owned private sources were removed.
