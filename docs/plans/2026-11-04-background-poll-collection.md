# Background poll bounded collection

Baseline: release/v5.2.6, 8a69534a5. Existing unrelated TUI/WebUI edits are preserved.

The capabilities background job store remains the process and unread-output owner. Runtime completion notification registration and defaults remain unchanged. `bash_poll` defaults to collecting output during its existing bounded wait (10 seconds default, 60 seconds maximum), returning on exit, status change, timeout or cancellation. `wait_until: "output"` explicitly retains early return on ordinary output. `wait_ms: 0` remains immediate.

Waiting does not consume output, stop the job or hold the store lock. Cancellation retains unread output. Existing bounded output storage and truncation reporting remain in effect; this change does not promise unlimited retention. PowerShell jobs use the same polling tool. Prompt descriptions must match the new default.

Verification covers explicit output mode, default accumulation through exit, a running job at deadline, incremental collection, existing cancellation and missing-job handling. No UI grouping or new lifecycle hooks are introduced.
