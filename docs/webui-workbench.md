# WebUI workspace panel

Open **Workspace** at the top right of WebUI. Files and terminals belong to the
selected conversation. Opening it on the initial landing page creates an empty
session without sending a model request.

## Files

Browse folders on demand and select a file to open a read-only tab. Source code,
UTF-8 text, JSON and HTML source use a text view; Markdown uses the existing
sanitized renderer; PNG/JPEG/GIF/WebP use an image view. Preview is limited to
2 MiB and a directory listing to 1,000 entries. Unsupported binary files report
an error. Previews do not execute HTML or enter the model context. Text editing is a separate explicit action.
Paths are resolved against the native session's workspace. Parent traversal and
symlinks resolving outside that workspace are refused. This panel deliberately
has a narrower reach than the existing host GUI file opener.

Open tabs, the current folder and selected view are saved per session in browser
tab storage. Switching conversations restores that conversation's panel state.
On narrow screens the panel becomes a full-width overlay; close it to return to
chat. Panel state is UI state, not a new conversation persistence format.

## Editing and draft protection

Choose **Edit** on an existing UTF-8 file to use the text editor, then **Save**
(or Ctrl/Cmd+S). A dot marks an unsaved tab. Switching files/sessions or closing
the panel keeps its draft, caret, scroll position and panel width per session.
Closing a dirty file asks before discarding it. Closing the panel warns while
retaining drafts; leaving/refreshing the page warns if any session has unsaved
work. Drafts use memory and tab-scoped sessionStorage, not cloud/session-log
storage. Browser storage failures are reported; save files before closing the tab.

Reads and saves are bounded to 2 MiB. UTF-8 BOM and uniform CRLF line endings are
preserved. Saving compares a canonical-path-bound content revision and returns
409 if the file changed or a former workspace revision targets another file.
The rejected draft is retained. **Reload** asks before discarding the draft.
Typing while a save is in flight remains unsaved work after that save completes.

Browser saves are serialized within the daemon, written to a same-directory temp
file, checked again and atomically renamed. Existing permission bits are copied;
read-only files are refused. This detects observed conflicts but is not a
cross-process atomic compare-and-swap against arbitrary concurrent writers.
Extended ACL/ownership behavior and Windows saves still need platform validation.
Creating files, binary editing and rename/delete operations are not provided.

## Read-only Git changes

The **Git** tab shows changed files and separate **Working tree** / **Staged**
diffs, plus previews of untracked text files. Refresh reads the latest changes;
**Open file** returns to the file pane. There are no stage, commit, discard,
checkout or fetch controls. Git queries are workspace-scoped with literal paths,
limited to 1,000 status entries and 1 MiB output, with a ten-second command limit.
Custom clean/process filters, external diff, textconv and fsmonitor are disabled
so viewing changes does not execute these helpers. For repositories using such
filters (for example LFS), raw comparisons can differ from a normal Git view.
Submodule contents are not inspected. A directory outside Git reports that it
is not a Git repository.

## Terminals

**New terminal** starts an interactive shell on the machine running AtomCode,
in that session's working directory. It uses a Unix PTY or Windows ConPTY.
Commands are direct user actions, not model tool calls; the agent's Plan mode
and tool approval choices do not constrain what you type into this shell.

Closing a terminal tab ends its shell/process group. Closing the panel, changing
sessions or refreshing the browser disconnects the view without immediately
killing the shell. Reopening reconnects to its bounded output buffer. An
unattended terminal expires after 15 minutes (checked once per minute), and
daemon shutdown ends its terminals. A daemon restart does not restore processes.
Expired/missing terminal tabs can be closed and recreated.

Limits: four terminals per session, sixteen per daemon; 256 KiB retained output
per terminal, up to 64 KiB per read, and 16 KiB per input request. Old output is
explicitly marked as expired. Input is ordered and is not automatically replayed
when delivery fails, avoiding duplicate commands. Output, input, resize and close
use the existing authenticated HTTP transport rather than a separate WebSocket,
so no new relay upgrade route is required. Remote relay and Windows operation
still require platform validation beyond the local macOS test.

## Validation

Build the UI and daemon, then run the real-browser smoke test:

```sh
npm --prefix webui run build
cargo build -p atomcode-daemon --bin atomcode-daemon
npm --prefix webui run test:workbench
```

The script uses Playwright with a real isolated daemon, temporary workspaces and
real PTYs. It does not send prompts to a model. It checks authentication,
workspace containment, preview limits, Markdown/text/image previews, session
isolation, reconnection, terminal limits, resize and process cleanup, and captures
screenshots. It also verifies editing, save conflicts, drafts and close warnings,
CRLF preservation, positions/width, and read-only Git diffs. `WORKBENCH_BROWSER` selects a Chrome/Chromium executable (default is
the macOS Chrome installation); `WORKBENCH_DAEMON` selects the daemon binary.

Not included: Git mutation, task management inside this panel, or side conversations.

## Refresh and conversation links

While the workbench is open and the page visible, the current file listing and
mounted Git view refresh every ten seconds and on window focus/visibility return.
This does not reload editor contents or replace drafts. Conversation artifact
buttons open the file in the current session's workbench instead of launching a
host GUI application. Workspace containment and preview limits still apply.

Completed terminals display the shell exit code and offer **Restart terminal**.
Restart removes that terminal and creates a new shell in the session workspace;
it does not replay commands or input.
