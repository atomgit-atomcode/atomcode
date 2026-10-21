# Generation speed formula correction

Baseline: main 07e4253a9. Preserve unrelated i18n edits.

Historical reference: f465a43ec^, atomcode-tuix state::phase_output_token_estimate and event_loop spinner. Formula is floor(phase output characters / 4) / fractional phase seconds, rounded, with a one-second minimum. The phase includes model waiting and excludes earlier tools. It is a display estimate, not billing throughput.

The previous visibility change mixed provider completion totals with first-visible-chunk duration and admitted subsecond completed samples. Replace this with the tuix character estimate and request-phase timing; retry resets both output and its phase clock at the retry notice, including the subsequent retry wait. Retain the last valid reading during tools and subsequent model waits, before cache share. No usage calibration or arbitrary maximum. New turns and sessions still reset. No persisted/runtime protocol changes.

Regression coverage: buffered delivery after a long wait, invisible usage tokens, short finished requests, mixed reasoning/tool arguments, retry reset, stale events, frozen duration and display ordering. Existing replay without streaming facts produces no speed.

Validation: all 1796 TUI tests passed; cargo fmt --all -- --check, git diff --check and bash gates/compile.sh passed. Interactive provider reproduction remains unverified. This supersedes the usage-calibrated numerator and first-chunk timing in the earlier visibility plan.
