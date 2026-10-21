# Review user instructions implementation plan

Goal: Preserve user review constraints across the TUI request, background delegation and read-only reviewer without removing the parent agent's normal coding capabilities.

Architecture: The review tool owns an optional `instructions` argument. TUI encodes the user's additional instructions as JSON; the delegate receives the same arguments and readable constraints; inline and deep reviewers receive the constraints in their shared task. No runtime ownership, session format or permission changes.

1. Add the optional argument to the tool schema and deserializer, preserving older callers.
2. Forward additional `/review` instructions into the tool arguments with JSON escaping.
3. Include the instructions in delegated and inline tasks. Clarify that report-only requests stop after reporting, while authorized follow-up work remains allowed.
4. Verify the TUI argument boundary, delegated task and actual reviewer provider request; run affected crate tests, formatting and the compile gate.

Limit: This change preserves instructions supplied with the review request. It does not automatically synchronize later foreground messages to an already-running background session, and prompt guidance is not a filesystem write prohibition on the parent agent. Result delivery remains an attributed session note, rather than a new user request.
