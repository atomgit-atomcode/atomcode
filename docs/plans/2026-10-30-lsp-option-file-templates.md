# LSP option-file path templates

Baseline release/v5.2.6 2a1fcdec4, clean worktree. LspManager remains the sole
owner of server roots, prepared initialization and pooled clients. Expand both
option-file paths in options::prepare before existing path resolution. Keep
relative paths based on the host config directory, tilde and literal absolute
paths compatible. Decode file URIs through url::Url::to_file_path; reject other
URI schemes and query/fragment suffixes. Do not fetch remote data.

Use the same variables as JSON keys/values, track environment values for trace
redaction, and redact expanded path diagnostics too. Keep init on/auto/off,
JSON byte limits, reload and missing/invalid-file semantics unchanged. An
explicit global file template opts into reading that project's JSON; this is
not automatic project config discovery or merging. Test two project roots,
URI escaping, both file options, errors, legacy paths and rereading changed JSON.

Validation: final capabilities suite with `--features lsp` passed 1302 tests;
legacy default-feature suite passed 1167 tests. Added Windows drive-path test
is conditional and has not run on this macOS host. Real Cangjie SDK acceptance
remains external. Formatting and diff checks passed; no project config merging
or public config fields were introduced.

Pre-push audit: workspace/all-target and feature compilation gate, including
negative controls, passed. Cargo.lock is unchanged with all three prohibited
package counts at zero. Concurrent WebUI long-session changes are excluded.
