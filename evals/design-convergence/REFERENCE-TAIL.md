# Initial-reference tail experiment — 2026-10-27

## Why this intervention

Current native session had contradicted previously confirmed identifiers mid-conversation. Capture a fresh six-turn native run using the frozen60ef0b652binary to check actual visible requests. The initial “both tables independently store DeviceNo/PipingNo/TubeSeq” statement remains in model requests during the omission-audit stage and later. This fresh run did not repeat the original “no parsed columns” assertion. It is evidence of input presence on that run, not a retroactive capture of the earlier failing request or proof of the original user's root cause.

Candidate: append the initial user statement at the request tail, explicitly labeled historical reference, later corrections authoritative, old tasks not to be re-executed. No extra model call, fact extractor, persistent state, compaction change, or production hook. This tests recency/exposure rather than another generic prose rule or a second-pass reviewer.

## Controlled direct experiment

Reconstruct omission-audit history using newly captured system/reminder messages and the previously failing run's visible first2assistant replies. Provider reasoning fields and native tool declarations are not replayed; this is not byte-exact native reproduction. The reverse-control uses an earlier frozen persona and legitimate BootId evidence. Comparisons are within each case; do not pool case persona differences as a uniform product run.

Two cases × baseline/candidate ×3repetitions =12completed direct responses. Same gateway/model, effortmax and cap32768. For each pair the only message difference is the appended reference; its quoted content is checked against the actual first user message. No production configuration changed.

## Outcome

Do not promote. Candidate omission-audit sample2 says “事件流水只保存原始报文”, although the user explicitly confirms independent identifier columns in both tables, both in its original position and repeated at the tail. The adjacent “noT1/T2columns” statement is not itself the defect (those are detail times); claiming the event table **only** stores raw messages is the defect. The other answers discuss preserving raw-message text without necessarily denying identifier columns, so they are not all scored as the same error.

All new-evidence outputs in targeted review retain BootId and the required four-column identity; the reference did not obviously freeze the old key in these samples. This does not rescue the failed main target or establish full correctness: unsupported timing/aggregation/protocol assumptions and follow-up questions remain.

Conclusion: initial factual input loss is not necessary for this class of error, and merely repeating those facts with high recency does not reliably prevent it. No claim that all history/compaction paths are correct, no causal diagnosis of the unavailable original session, no model intelligence ranking, and no stable automatic repair demonstrated. Keep the narrow approved-document/minimal-edit workflow as an existing opt-in mitigation, not a chat-wide fix.

Artifacts: ignored results/current-history-capture-20261027 and results/reference-tail-controls-20261027 retain sanitized requests, full visible responses, paired inputs and counterexample. Credentials absent from saved JSON.26offline tool tests, Python compilation and diff checks passed. No production code change, commit or push.
