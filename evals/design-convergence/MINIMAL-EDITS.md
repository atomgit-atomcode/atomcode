# Edit-only verifier experiment

## Concrete change tested

Implemented an evaluation-only verifier returning at most5exact excerpt replacements instead of a rewritten final answer. Program checks each source excerpt is unique, validates cited history quotes/indices, rejects overlaps, and applies all edits atomically against original offsets. Invalid output, ambiguous quotes, timeout or other request failure preserves the original byte-for-byte and marks it unverified. No second runtime owner, hook, persistent fact registry or production final-output interceptor.

Same predeclared6recorded drafts and2synthetic controls from the earlier verifier. Same DeepSeek endpoint. Output cap8192, requested efforthigh, socket timeout45seconds; changed from full-rewrite/max/32768/300. This is a combined engineering candidate; differences cannot be attributed to output format alone. Seed20261027 shuffles jobs. No retry of failed samples.

## Observations

-8attempts completed bookkeeping:4returned structurally valid edit lists;4retained original as unverified.
- Recorded drafts:3/6valid lists (2empty/unchanged,1modified);3/6unverified (one bad citation, two socket timeouts).
- Synthetic correct control was edited: reviewer marked “各字段独立” a contradiction and changed it to a uniqueness-scope explanation. The original phrase describes separate fields, not independent uniqueness domains. The source does not establish a contradiction; at minimum this is unnecessary clarification rather than demonstrated repair. Therefore correct-control preservation criterion failed.
- Injected-bad control produced suggested repairs but one referenced the wrong history index; the entire edit set was rejected. Do not count detected-looking suggestions as an applied successful correction.
- One recorded draft's unsupported clock-reset paragraph was changed to conditional wording, but its table description became “是否同BootId内可比需确认”. The original uncertainty is about cross-boot behavior, so this replacement widens uncertainty to within-boot comparison. This is not a clean semantic pass just because the edit list validates.
- An additional recorded draft also had plausible edits, but a history index was out of range; no partial prefix was applied.

Observed request times roughly10.5–45.4seconds, versus35–155seconds for successful full-rewrite reviews previously. Different effort/budgets and censored failures prevent a controlled speedup claim. Even this extra delay must be weighed against benefit; no user-facing SLA claim.

## Decision

Do not auto-apply this verifier in production. The program can enforce edit mechanics and preserve originals on mechanical failure; it cannot establish that proposed edits are correct. This run demonstrates both a targeted repair attempt and an unwanted change to a correct answer. Human/independent adjudication and held-out regression are still absent. No quality score or corrected-real-error rate claimed.

Artifacts: ignored results/minimal-edits-verifier-20261026 contains prompt, complete cohort, raw visible verifier responses, applied/unverified text and timings, including failures. Real credential absent from saved JSON; private reasoning not saved.26offline tests passed, including overlap/ambiguity rejection, exact evidence validation, no partial application, unchanged-text preservation and Unicode original-offset application. Python compilation and diff checks passed. No production changes, commit or push.
