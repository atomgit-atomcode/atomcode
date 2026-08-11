# Permission Model

## Overview

AtomCode uses a unified permission model to control access to files, directories, and shell commands.

The goals are:

- prevent unconfirmed access to paths outside the current working directory
- prevent common shell-based bypasses after a file tool is denied
- keep permission behavior explicit and traceable in code

This document describes the current behavior of the implementation.

## Design Goals

AtomCode's permission model is designed to:

- distinguish normal project work from external-system access
- distinguish low-risk reads from high-risk writes
- apply consistent path rules across file tools
- apply equivalent path rules to common shell file commands
- preserve a simple mental model for users and maintainers

## Non-Goals

The current model does not aim to:

- implement a full shell parser
- statically analyze interpreter code such as `python -c "open(...)"`
- infer all runtime-expanded paths from shell variables or substitutions
- replace sandboxing or OS-level security boundaries

Sandboxing and host-level permissions remain important defense layers.

## Core Concepts

### Working Directory Boundary

All path checks begin by resolving the requested path and determining whether it stays within the current working directory.

- paths inside the working directory are auto-approved
- paths outside the working directory are classified by action and sensitivity

### Access Actions

AtomCode currently models external path access with three actions:

- `Enumerate`
  directory listing, structural exploration, changing directories
- `Read`
  reading file contents or searching file contents
- `Write`
  creating, editing, overwriting, renaming, or otherwise mutating files

### Approval Results

Permission checks return one of three outcomes:

- `AutoApprove`
- `RequireApproval`
- `RequireApprovalAlways`

`RequireApprovalAlways` is the stronger form and is intended for high-risk operations such as sensitive reads or any write outside the workspace.

## Path Approval Rules

Current external-path behavior is:

| Scenario | Result |
|---|---|
| Path stays inside working directory | `AutoApprove` |
| Outside workspace, non-sensitive `Enumerate` | `AutoApprove` |
| Outside workspace, sensitive `Enumerate` | `RequireApprovalAlways` |
| Outside workspace, non-sensitive `Read` | `RequireApproval` |
| Outside workspace, sensitive `Read` | `RequireApprovalAlways` |
| Outside workspace, any `Write` | `RequireApprovalAlways` |

This means:

- normal external directory browsing is allowed by default
- normal external file reads require confirmation
- sensitive reads always require strong confirmation
- any write outside the workspace always requires strong confirmation

## Sensitive Path Classification

A path is currently treated as sensitive if it matches one of the built-in protected system prefixes, unless it also matches an exception prefix, or if it matches credential/config-like file rules.

### Built-in Protected Prefixes

Current built-in protected prefixes are:

- `/System`
- `/bin`
- `/sbin`
- `/usr`
- `/var`
- `/private/etc`
- `/private/var`
- `/etc`
- `/root`
- `/var/root`
- `/private/var/root`

### Built-in Exceptions

These paths are currently exempted from the protected-prefix rule:

- `/usr/local`
- `/private/usr/local`
- `/Applications`
- `/Library`
- `/var/folders`
- `/private/var/folders`
- `/var/tmp`
- `/private/var/tmp`

These exceptions exist to avoid over-classifying common writable or user-owned areas as sensitive.

### Home and Secret-Like Paths

AtomCode also treats the following as sensitive:

Sensitive home directories:

- `~/.ssh`
- `~/.aws`
- `~/.gnupg`
- `~/.config`

Sensitive filenames:

- `.bashrc`
- `.bash_profile`
- `.zshrc`
- `.zprofile`
- `.zshenv`
- `.npmrc`
- `.pypirc`
- `.env`
- `.env.local`
- `credentials`
- `config`
- `id_rsa`
- `id_dsa`
- `id_ecdsa`
- `id_ed25519`

Sensitive extensions:

- `.pem`
- `.key`
- `.p12`
- `.pfx`
- `.der`
- `.crt`
- `.cer`

## Tool Integration

Most built-in file and path tools already use the shared path approval model.

### File and Directory Tools

Current mappings are:

| Tool | Action |
|---|---|
| `read_file` | `Read` |
| `grep` | `Read` |
| `find_references` | `Read` |
| `list_symbols` | `Read` |
| `read_symbol` | `Read` |
| `lsp` (all phase-one operations) | `Read` |
| `list_directory` | `Enumerate` |
| `glob` | `Enumerate` |
| `cd` | `Enumerate` |
| `file_dependencies` | `Enumerate` |
| `blast_radius` | `Enumerate` |
| `edit_file` | `Write` |
| `write_file` | `Write` |
| `search_replace` | `Write` |

This gives AtomCode a single path policy for file tools instead of per-tool ad hoc logic.

## Bash Permission Model

`bash` uses a two-layer permission model.

### Layer 1: Dangerous Command Detection

Some commands are flagged because the command itself is risky, regardless of path.

Examples include:

- privileged execution such as `sudo`
- destructive deletion such as `rm -rf`
- dangerous networking or shell-tunneling patterns
- remote script execution piped into a shell
- force-push and other destructive VCS operations

These return `RequireApproval`.

### Layer 2: Common Shell File Command Path Checks

For common shell file commands, AtomCode extracts path arguments and maps them onto the same shared path approval model used by file tools.

Current categories:

Read-like commands:

- `cat`
- `head`
- `tail`
- `less`
- `more`
- `bat`
- `hexdump`
- `xxd`
- `strings`
- `file`
- `stat`
- `grep`
- `sed`
- `awk`
- `cut`
- `sort`
- `uniq`
- `wc`
- `diff`
- `patch`
- `tar`
- `unzip`
- `gunzip`
- `source`
- `.`

Enumerate-like commands:

- `ls`
- `dir`
- `tree`
- `find`

Write-like commands:

- `cp`
- `mv`
- `touch`
- `mkdir`
- `rmdir`
- `rm`
- `chmod`
- `chown`
- `tee`
- `install`

This layer exists to prevent common bypasses such as:

- using `cat` after `read_file` was denied
- using `ls` or `find` to inspect a sensitive external path
- using `cp`, `mv`, or redirection to mutate files outside the workspace

### Shell Wrappers and Redirection

The current implementation also handles:

- `bash -c ...`
- `bash -lc ...`
- input redirection with `<`
- output redirection with `>` and `>>`

This allows common wrapped shell patterns to inherit the same path checks.

## Explicit Boundary: Interpreter Code

AtomCode currently does not perform semantic inspection of interpreter code passed through shell commands.

For example:

```bash
cat /etc/hosts
```

and

```bash
python -c "print(open('/etc/hosts').read())"
```

are treated differently.

The first is checked by the shell file-command permission layer. The second is currently outside that layer.

## Why This Boundary Exists

This boundary keeps the model practical.

Without it, AtomCode would need to:

- parse many scripting languages
- understand nested quoting and runtime string construction
- infer file access from interpreter semantics
- maintain a much larger and less predictable security surface

The current implementation instead focuses on:

- file tools
- common shell file commands
- explicit path-bearing shell syntax
- dangerous command patterns

## User Experience Model

From the user's perspective, the current model should be understood like this:

- project-local work is usually frictionless
- external file reads ask for confirmation
- external sensitive reads ask more strongly
- any external write asks more strongly
- dangerous shell commands ask for confirmation
- common shell file-command bypasses are blocked by the same path rules
- interpreter-internal file access is currently outside this approval layer

## Strengths of the Current Model

The current model has several strengths:

- path policy is unified across tools
- common shell bypasses are covered
- approval strength is explicit
- workspace boundary and sensitivity boundary are modeled separately
- behavior is inspectable in code rather than hidden in scattered conditionals

## Known Limitations

The current model also has tradeoffs:

- shell command coverage is heuristic, not complete
- protected-path and exception lists require maintenance
- platform-specific filesystem behavior can cause edge cases
- users may still see some cases as inconsistent when shell commands are parsed but interpreter code is not

## Recommended Evolution

The recommended direction is to refine the current model rather than replace it.

### Keep

- shared path approval core
- `Read / Write / Enumerate` action model
- `RequireApproval` vs `RequireApprovalAlways`
- dangerous shell command detection
- common shell file-command protection

### Reduce

- breadth of shell command parsing
- pressure to model complex shell semantics
- reliance on ever-growing hardcoded shell heuristics

### Add Later

- configurable sensitive prefixes
- configurable sensitive globs
- configurable exceptions
- clearer user-facing documentation for what is and is not covered

## Summary

AtomCode currently uses a unified path-based approval model for file tools and common shell file commands.

It protects:

- workspace boundaries
- sensitive external reads
- all external writes
- common shell-based file access bypasses
- dangerous shell commands

It intentionally does not attempt to analyze interpreter code or become a full shell security engine.

This keeps the model strong on common cases, explicit in code, and maintainable enough to evolve.
