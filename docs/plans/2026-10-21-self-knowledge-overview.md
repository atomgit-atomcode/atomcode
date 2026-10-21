# Compact self-knowledge overview

Goal: Reduce the context added by a simple identity question without changing tool availability, project instructions or startup hooks.

The self-knowledge row owns report rendering. Add `aspect: overview` as the default and typo fallback, returning live session information, service names, tool names and navigation to detailed aspects. Keep explicit `all`, `tools`, `operations`, `models`, `session`, `services` and `settings` queries available. Identity remains owned by the system persona; domain-specific explanations remain owned by their existing rows. Update the tool description and invariant prompt to select overview for simple identity questions.

Regression checks: 96 long single-line tool descriptions and verbose operations do not leak into default output; detailed calls still return them; live tool additions appear immediately. Run the harness tests, relevant coding runtime criteria, formatting and compile gate.

Limits: This reduces tool-result context, not the initial request's tool schemas or project instructions. An explicit `all` still returns the full report. Model adherence and actual token savings require a fresh real session; offline tests validate the payload structure and byte reduction only.
