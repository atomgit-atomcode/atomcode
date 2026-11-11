# Computer doctor permission identity

Baseline 29650ce32, clean isolated worktree. The local setup service currently deserializes only macOS permission flags and drops the experimental backend's runtime.permission_app/bundle_identifier. This hides the exact grant target on both CLI and TUI and contributes to granting a different computer-use App.

Keep the existing schema v1 and macOS platform/readiness checks. Parse optional reported App identity, never treat it as permission/lease authority. Legacy reports remain accepted and explicitly say the App is unidentified. A supplied permission App must be an absolute path; malformed typed identity fails parsing. Only known identity fields render, with control characters escaped. Permission failures include the reported App and existing fail-closed readiness result. CLI and /computer share this service, with no runtime/session ownership or configuration change.

Verify legacy reports, strict typing/path checks, failure/ready output, control escaping and omission of unrelated metadata. Exercise actual CLI startup with a controlled backend, assert doctor and failed enable do not create the product tree. Then run the actual corrected experimental App's doctor without UI and verify the exact grant path survives into AtomCode's failure output. Run affected crate suites, compile/format/diff/lock gates. Windows/Linux structured readiness remains separate; no target support is enabled here.

## Verification

15 focused Computer tests passed. Complete atomcode crate suite: 661 passed, 5 skipped, including the real CLI subprocess checks for reported App identity, readiness refusal, legacy compatibility and no product startup writes. Full compile gate (default/features/negative controls), cargo fmt check and diff check passed. Cargo.lock unchanged with H index flag; forbidden private package counts all zero. No new dependency or protocol writer.

The freshly built actual CLI was run against the corrected experimental App, executable SHA-256 3cbfd17e3d95410c44973ab8c4bd0946645a5ebe74d9a0776ec9ad39e0aa0f31. The backend reported Accessibility=false and Screen Recording=false. CLI returned exit 1 and retained the exact permission App path and com.ifuryst.opencomputeruse.dev in the diagnostic, created no product directory and dispatched no GUI actions. The owned helper was stopped. Evidence is outside Git at the acceptance scratch directory's cli-permission-identity.json. Host maps the shared service error without dropping its message; the TUI command emits that full message as CommandSaid. No new TUI state or permission inference is introduced.

English/Chinese MCP setup docs now explain reported identity, legacy unknown identity and grants not transferring between App copies. Windows/Linux structured readiness, platform effects and the actual macOS GUI test remain incomplete.
