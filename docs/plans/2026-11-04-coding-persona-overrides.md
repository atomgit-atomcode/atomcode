# Coding persona overrides

Initial inspected baseline: release/v5.2.6 at 53b3e0eb2. Shared checkout changes to proxy/file-index/Windows dependencies belong to other tasks and are retained.

Use the existing persona-atomcode row, keeping every other prompt contribution and runtime owner. Resolved provider/model system_prompt supplies the configured persona. PrepareOptions owns explicit replacement and append text, so model changes and runtime rebuilds retain host policy. On model swap, preserve the existing row config and update only model and configured persona. Row patches replace whole config: never partially patch away product identity or explicit policy.

CLI exposes --system-prompt / --system-prompt-file and --append-system-prompt / --append-system-prompt-file. Each text/file pair conflicts. Read UTF-8 files once before startup, preserving bytes; reject unreadable or blank explicit values. These root flags apply to the main coding CLI/TUI, including its background runtimes; subcommands retain their own interfaces. No presets, dynamic hooks, stdin prompt-file syntax or persistence format changes.

Selection: explicit replacement > resolved model/provider system_prompt > built-in coding persona; append follows the selected persona. Project instructions, tool guidance and executable approval rules remain independent. Restart/resume uses the current invocation and config; CLI overrides are not persisted in session logs and must be supplied again on a later process invocation.

Validation: actual provider prompts retain project instructions, configured persona follows a model switch, explicit policy survives reprepare, CLI pair validation and file failures, configuration resolution, consumer compilation and affected crate tests.
