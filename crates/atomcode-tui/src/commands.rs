//! The commands this build ships, grouped by what they are about.
//!
//! Split into sets on purpose: the screen's own verbs, the session's, and
//! `/help`. Removing a set removes its commands, and a capability that wants a
//! command of its own contributes a set rather than editing anything here.
//!
//! There are no commands that read or rewrite the agent's config tree: the
//! agent is in the host's App, and what a person may change about it is what
//! host control offers (`docs/adr/0022` §7).

use crate::i18n::{t, Msg};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_host_api::{HostCommand, HostError, HostReply};
use atomcode_kernel::message::Role;
use atomcode_kernel::provider::ReasoningEffort;
use atomcode_kernel::session::{derive_messages, SessionEvent};
use atomcode_plexus::Context;

use crate::command::{Command, CommandOption, CommandSet, Commands, Outcome};
use crate::host::ToolOutput;
use crate::keymap::Action;

/// Quit, clear, fold — things the screen itself owns.
pub struct ScreenCommands;

/// Built per call rather than held in a `const`, because what each line says
/// depends on the language in force and `/language` changes that mid-session.
fn screen_catalogue() -> Vec<Command> {
    vec![
        // `/exit` keeps working as an alias — one row, not two.
        Command::said("quit", t(Msg::CmdAboutQuit)).with_aliases(&["exit"]),
        Command::said("reasoning", t(Msg::CmdAboutReasoning)),
        Command::said_taking(
            "tools",
            "[full|head|each|group]".into(),
            t(Msg::CmdAboutTools),
        ),
        Command::said("showinject", t(Msg::CmdAboutShowInject)),
        Command::said("mouse", t(Msg::CmdAboutMouse)),
        Command::said("keys", t(Msg::CmdAboutKeys)),
        Command::said("todo", t(Msg::CmdAboutTodo)),
        Command::said("team", t(Msg::CmdAboutTeam)),
        Command::said_taking("paste", t(Msg::CmdTakesPath), t(Msg::CmdAboutPaste)),
        Command::said("config", t(Msg::CmdAboutConfig)),
        Command::said("provider", t(Msg::CmdAboutProviderPanel)),
    ]
}

#[async_trait]
impl CommandSet for ScreenCommands {
    fn id(&self) -> &'static str {
        "cmd-screen"
    }
    fn commands(&self) -> Vec<Command> {
        screen_catalogue()
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        match name {
            "quit" | "exit" => Outcome::Do(Action::Quit),
            "reasoning" => Outcome::Do(Action::ToggleFold("reasoning")),
            "tools" => match tool_output(&args.to_ascii_lowercase()) {
                Ok(action) => Outcome::Do(action),
                Err(why) => Outcome::Refused(why),
            },
            "showinject" => match showinject(&args.to_ascii_lowercase()) {
                Ok(action) => Outcome::Do(action),
                Err(why) => Outcome::Refused(why),
            },
            "mouse" => Outcome::Do(Action::ToggleMouse),
            // The two panels a person toggles by name. Same gesture the fold
            // keys are, so a command and a key share one implementation.
            "todo" => Outcome::Do(Action::ToggleFold("todo")),
            "team" => Outcome::Do(Action::ToggleFold("team")),
            // A typed way in to the thing ctrl-v does, because ctrl-v does not
            // always arrive: Windows terminals hand the paste to the key layer
            // as a keystroke, and some platforms have no clipboard this process
            // can read at all. With a path it does not need one.
            "paste" => match args.trim() {
                "" => {
                    let Some(surface) = ctx.service::<crate::plugin::SurfaceSvc>() else {
                        return Outcome::Refused(t(Msg::NoClipboard).into_owned());
                    };
                    match surface.clipboard_text() {
                        Some(text) if !text.is_empty() => Outcome::Do(Action::Paste(text)),
                        // Not an error. "There is nothing in it" is how a person
                        // finds out there is nothing in it.
                        _ => Outcome::Refused(t(Msg::ClipboardHasNoText).into_owned()),
                    }
                }
                path => match std::fs::read_to_string(path) {
                    Ok(text) if text.is_empty() => {
                        Outcome::Refused(t(Msg::FileIsEmpty { path }).into_owned())
                    }
                    Ok(text) => Outcome::Do(Action::Paste(text)),
                    Err(error) => Outcome::Refused(
                        t(Msg::FileUnreadable {
                            path,
                            error: &error.to_string(),
                        })
                        .into_owned(),
                    ),
                },
            },
            "config" => Outcome::Do(Action::ToggleSettings),
            "provider" => Outcome::Do(Action::ToggleProviders),
            "keys" => Outcome::Said(t(Msg::KeysHelp).into_owned()),
            _ => Outcome::Quiet,
        }
    }
}

/// The word `/mode` takes for one of the four.
///
/// Beside the arm that reads them, and paired with [`mode_named`] by a
/// round-trip test: the cycle key asks for "the next one" and has to say it in
/// the same vocabulary a person types, so a second table here would be the
/// second place for the two to disagree.
pub fn mode_word(mode: atomcode_host_api::Mode) -> &'static str {
    use atomcode_host_api::Mode;
    match mode {
        Mode::Plan => "plan",
        Mode::Ask => "ask",
        Mode::AcceptEdits => "edits",
        Mode::Auto => "auto",
    }
}

/// The mode a word names, or `None` for a word that names none.
///
/// `accept-edits` is accepted as well as `edits`: the contract calls the mode
/// `AcceptEdits` and the shorter word is what the badge and the help text use,
/// so both spellings are the one mode rather than two.
pub fn mode_named(word: &str) -> Option<atomcode_host_api::Mode> {
    use atomcode_host_api::Mode;
    match word {
        "plan" => Some(Mode::Plan),
        "ask" => Some(Mode::Ask),
        "edits" | "accept-edits" => Some(Mode::AcceptEdits),
        "auto" => Some(Mode::Auto),
        _ => None,
    }
}

/// Turn `/showinject <what>` into the one action it means.
///
/// Split out from the dispatch because the interesting part is the refusal, and
/// a refusal that has to be written to be tested is a refusal that says what the
/// alternatives were. `/showinject` with nothing after it is the group — every
/// environmental injection at once — since that is the thing a person forms an
/// opinion about, not any one of them.
///
/// A named one is the same `Hidden → Folded → Open → Hidden` cycle `/reasoning`
/// is, with `Folded` standing in the label `[reminder]` alone: the useful middle
/// state for something that is off the screen because it is noise but is not
/// hidden from anybody who goes looking.
fn tool_output(what: &str) -> Result<Action, String> {
    // Bare: the cycle, which is what the key does too. One gesture, one
    // implementation — a press of ctrl-t and a bare `/tools` are the same act.
    if what.is_empty() {
        return Ok(Action::ToggleFold("tool_call"));
    }
    match what {
        "full" | "all" => Ok(Action::SetToolOutput(ToolOutput::Full)),
        "head" | "preview" => Ok(Action::SetToolOutput(ToolOutput::Head)),
        "each" | "one" => Ok(Action::SetToolOutput(ToolOutput::Each)),
        "group" | "run" => Ok(Action::SetToolOutput(ToolOutput::Group)),
        _ => Err(t(Msg::ToolOutputUnknown { what }).into_owned()),
    }
}

fn showinject(what: &str) -> Result<Action, String> {
    if what.is_empty() {
        return Ok(Action::ToggleFolds(
            crate::content::ENVIRONMENTAL_INJECTIONS.to_vec(),
        ));
    }
    // `all` rather than a fourth name: every kind in the table, peers included,
    // because "show me the injections" is a question about the screen and a
    // teammate's report is an injection flatly.
    if what == "all" {
        return Ok(Action::ToggleFolds(
            crate::content::INJECTIONS
                .iter()
                .map(|(_, kind)| *kind)
                .collect(),
        ));
    }
    match crate::content::injected_kind(what) {
        Some(kind) => Ok(Action::ToggleFold(kind)),
        None => Err(t(Msg::InjectionUnknown {
            what,
            names: &crate::content::INJECTIONS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(" / "),
        })
        .into_owned()),
    }
}

/// Taking the conversation out of the terminal: onto the clipboard, onto disk.
///
/// Its own set rather than two more arms in [`SessionCommands`], because it is
/// the one part of the command surface a downstream build is most likely to
/// have an opinion about — a house that saves to its own wiki drops this row
/// and mounts its own, or keeps it and overrides `save` alone
/// ([`CommandSet::overrides`]).
pub struct TakeAwayCommands;

/// The most of a file `/view` will read into memory.
///
/// A cap and not a preference: without one, `/view` on a multi-gigabyte log
/// reads the whole thing into a `String` before anyone can press anything.
/// The three caps below are the ones `atomcode-tuix` settled on, kept because
/// their job is to be large enough that nobody meets them by accident.
const VIEW_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The most lines it will show. Past this the file is a haystack, not a read.
const VIEW_MAX_LINES: usize = 1000;
/// The most characters kept from one line. A minified bundle is one line of
/// two million; wrapping it fills the screen with a single row of the file.
const VIEW_MAX_LINE: usize = 2000;

/// Which file `/view <typed>` means.
///
/// Its own function because it is a decision with three inputs and one right
/// answer, and the alternative is judging it through a command dispatch that
/// would have to own the machine's home directory to say anything.
///
/// `~/…` is expanded **before** the absolute test, not after: an unexpanded
/// `~/notes.md` is a relative path, so it would be joined onto the working
/// directory and the refusal would name a file nobody meant.
fn view_path(typed: &str, root: &str, home: Option<&std::path::Path>) -> std::path::PathBuf {
    let expanded = crate::text::expand_home_with(typed, home);
    let path = std::path::Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::Path::new(root).join(path)
    }
}

/// A file as `/view` will show it, and what had to be left out to show it.
struct Viewed {
    body: String,
    /// The file was longer than [`VIEW_MAX_BYTES`], so this is its opening.
    at_byte_cap: bool,
    /// Lines past [`VIEW_MAX_LINES`] were dropped.
    at_line_cap: bool,
    /// How many lines were cut at [`VIEW_MAX_LINE`].
    long_lines: usize,
}

/// Read a file for `/view`, bounded on all three axes.
///
/// Returns `Ok(None)` for a file that is not text. A binary opened in a text
/// viewer is not a degraded read — it is a screenful of garbage plus whatever
/// escape sequences happened to be in it, so it is refused by name instead.
/// (`crate::text::for_screen` would strip those on the way out; this refuses
/// earlier because "here are 8MB of nothing" is not worth drawing.)
///
/// **NUL first, then lossy.** The byte cap can land mid-character, and a file
/// that is merely not-UTF-8 (a latin-1 README) still reads fine with
/// replacement characters — so invalid UTF-8 alone is not the test. An embedded
/// NUL is: no text file has one, every binary does.
fn view_file(path: &std::path::Path) -> std::io::Result<Option<Viewed>> {
    view_file_within(path, VIEW_MAX_BYTES, VIEW_MAX_LINES, VIEW_MAX_LINE)
}

/// [`view_file`] with the three caps passed in, so each one can be judged
/// against a file of a few bytes instead of one of eight megabytes.
fn view_file_within(
    path: &std::path::Path,
    max_bytes: u64,
    max_lines: usize,
    max_line: usize,
) -> std::io::Result<Option<Viewed>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    // One past the cap, so "exactly at the cap" and "longer than the cap" are
    // distinguishable without a second trip to the filesystem.
    std::io::Read::take(file, max_bytes + 1).read_to_end(&mut bytes)?;
    let at_byte_cap = bytes.len() as u64 > max_bytes;
    if at_byte_cap {
        bytes.truncate(max_bytes as usize);
    }
    if bytes.contains(&0) {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut body = String::new();
    let mut long_lines = 0;
    let mut lines = text.lines();
    for line in lines.by_ref().take(max_lines) {
        if line.chars().count() > max_line {
            long_lines += 1;
            let keep: String = line.chars().take(max_line).collect();
            body.push_str(&keep);
        } else {
            body.push_str(line);
        }
        body.push('\n');
    }
    Ok(Some(Viewed {
        body,
        at_byte_cap,
        at_line_cap: lines.next().is_some(),
        long_lines,
    }))
}

fn take_away_catalogue() -> Vec<Command> {
    vec![
        Command::said_taking("copy", "[N|all|msg]".into(), t(Msg::CmdAboutCopy)),
        Command::said_taking("save", t(Msg::CmdTakesFilename), t(Msg::CmdAboutSave)),
        Command::said_taking("view", t(Msg::CmdTakesPathRequired), t(Msg::CmdAboutView))
            .requiring(),
    ]
}

#[async_trait]
impl CommandSet for TakeAwayCommands {
    fn id(&self) -> &'static str {
        "cmd-take-away"
    }
    fn commands(&self) -> Vec<Command> {
        take_away_catalogue()
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        let Some(client) = ctx.service::<crate::plugin::AgentClientSvc>() else {
            return Outcome::Refused(t(Msg::NoAgent).into_owned());
        };
        match name {
            // Copying a code block is the one thing people do with an answer
            // that the answer itself cannot do: the model wrote it to be run,
            // and dragging across a wrapped terminal is how it ends up with
            // line numbers and gutters in it.
            "copy" => {
                let answer = last_answer(&client.events());
                // `msg` takes the whole reply, prose and all — the other half of
                // what people do with an answer. A block is for running; the
                // whole message is for pasting into an issue or a review, and
                // that is exactly the case where dragging across a wrapped
                // terminal picks up gutters and fold marks.
                if args.trim() == "msg" {
                    if answer.trim().is_empty() {
                        return Outcome::Refused(t(Msg::CopyNoBlocks).into_owned());
                    }
                    let Some(surface) = ctx.service::<crate::plugin::SurfaceSvc>() else {
                        return Outcome::Refused(t(Msg::NoClipboard).into_owned());
                    };
                    let lines = answer.lines().count();
                    surface.copy(&answer);
                    return Outcome::Said(t(Msg::CopiedLines { lines }).into_owned());
                }
                let blocks = code_blocks(&answer);
                if blocks.is_empty() {
                    return Outcome::Refused(t(Msg::CopyNoBlocks).into_owned());
                }
                let text = match args.trim() {
                    "" if blocks.len() == 1 => blocks[0].clone(),
                    "" => {
                        return Outcome::Refused(
                            t(Msg::CopyWhichBlock {
                                count: blocks.len(),
                            })
                            .into_owned(),
                        )
                    }
                    "all" => blocks.join("\n\n"),
                    n => match n.parse::<usize>().ok().filter(|n| *n >= 1) {
                        Some(n) if n <= blocks.len() => blocks[n - 1].clone(),
                        _ => {
                            return Outcome::Refused(
                                t(Msg::CopyNoSuchBlock {
                                    count: blocks.len(),
                                    asked: n,
                                })
                                .into_owned(),
                            )
                        }
                    },
                };
                let Some(surface) = ctx.service::<crate::plugin::SurfaceSvc>() else {
                    return Outcome::Refused(t(Msg::NoClipboard).into_owned());
                };
                let lines = text.lines().count();
                surface.copy(&text);
                Outcome::Said(t(Msg::CopiedLines { lines }).into_owned())
            }
            // Markdown rather than the screen's own rendering: what is saved is
            // read elsewhere — in an editor, in a review, in an issue — and the
            // gutters and the fold marks belong to this screen.
            "save" => {
                let text = as_markdown(&client.events());
                if text.trim().is_empty() {
                    return Outcome::Refused(t(Msg::SaveNothingYet).into_owned());
                }
                let name = match args.trim() {
                    "" => format!("atomcode-{}.md", client.session().replace('/', "-")),
                    given => given.to_string(),
                };
                // Relative to where the session is working, not to wherever the
                // process happened to be started: a person saying `/save` means
                // "beside the code I am looking at".
                let path = std::path::Path::new(&name);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    std::path::Path::new(&client.root()).join(path)
                };
                // **An existing file that this command did not write is not
                // overwritten.** `/save` produces markdown; a `.md` target is
                // therefore a previous save being replaced, which is what a
                // person means by saving again. Any other extension is a file
                // that came from somewhere else — `/save Cargo.toml` would
                // destroy it, silently, with a transcript. Refusing costs one
                // retype; the other way round costs the file.
                let markdown = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
                if path.exists() && !markdown {
                    return Outcome::Refused(
                        t(Msg::SaveWouldOverwrite {
                            path: &crate::text::collapse_home(&path.display().to_string()),
                        })
                        .into_owned(),
                    );
                }
                match std::fs::write(&path, text) {
                    Ok(()) => Outcome::Said(
                        t(Msg::SavedTo {
                            path: &path.display().to_string(),
                        })
                        .into_owned(),
                    ),
                    Err(error) => Outcome::Refused(
                        t(Msg::SaveFailed {
                            error: &error.to_string(),
                        })
                        .into_owned(),
                    ),
                }
            }
            // Looking at a file costs a turn otherwise — and puts the whole
            // file in the conversation for good. This sends nothing and logs
            // nothing.
            "view" => {
                let path = args.trim();
                if path.is_empty() {
                    return Outcome::Refused(t(Msg::ViewWhichFile).into_owned());
                }
                let full = view_path(path, &client.root(), crate::text::home_dir().as_deref());
                let shown = crate::text::collapse_home(&full.display().to_string());
                match view_file(&full) {
                    // Read here rather than in the overlay: an overlay draws
                    // under the same rule a view module does — pure, no IO.
                    Ok(Some(seen)) => {
                        // What was left out rides in the title, not on the last
                        // line: the reader who needs to know is the one who
                        // never reaches the end.
                        let mut notes = Vec::new();
                        if seen.at_byte_cap {
                            notes.push(
                                t(Msg::ViewTooBig {
                                    mb: VIEW_MAX_BYTES / (1024 * 1024),
                                })
                                .into_owned(),
                            );
                        } else if seen.at_line_cap {
                            notes.push(
                                t(Msg::ViewOnlyFirstLines {
                                    lines: VIEW_MAX_LINES,
                                })
                                .into_owned(),
                            );
                        }
                        if seen.long_lines > 0 {
                            notes.push(
                                t(Msg::ViewLongLinesCut {
                                    lines: seen.long_lines,
                                })
                                .into_owned(),
                            );
                        }
                        let title = if notes.is_empty() {
                            shown
                        } else {
                            format!("{shown} ({})", notes.join(" · "))
                        };
                        Outcome::Open(crate::overlay::Reading::new(title, &seen.body))
                    }
                    Ok(None) => Outcome::Refused(t(Msg::ViewNotText { path: &shown }).into_owned()),
                    Err(error) => Outcome::Refused(
                        t(Msg::FileUnreadable {
                            path,
                            error: &error.to_string(),
                        })
                        .into_owned(),
                    ),
                }
            }
            _ => Outcome::Quiet,
        }
    }
}

/// The conversation: what is in it, what to do with it, and which one it is.
pub struct SessionCommands;

/// The reasoning-effort levels the slash menu offers inline, in place of a
/// modal: the closed set from the one place that defines it, plus `default`
/// (leave it to the endpoint). A pick dispatches `/effort <value>`, so this and
/// a typed `/effort high` reach one implementation.
fn effort_options() -> Vec<CommandOption> {
    let mut out: Vec<CommandOption> = atomcode_harness::REASONING_EFFORT_LEVELS
        .iter()
        .map(|level| CommandOption::new(*level, t(Msg::EffortAbout)))
        .collect();
    out.push(CommandOption::new("default", t(Msg::EffortDefaultAbout)));
    out
}

fn session_catalogue() -> Vec<Command> {
    vec![
        Command::said("compact", t(Msg::CmdAboutCompact)),
        Command::said("cancel-all", t(Msg::CmdAboutCancelAll)),
        Command::said("context", t(Msg::CmdAboutContext)),
        Command::said("agents", t(Msg::CmdAboutAgents)),
        Command::said("transcript", t(Msg::CmdAboutTranscript)),
        Command::said("clear", t(Msg::CmdAboutClear)),
        // `/session` is the same fresh start, named the way the reference does,
        // with `/new` as its memorable alias — one row, not two.
        Command::said("session", t(Msg::CmdAboutSession)).with_aliases(&["new"]),
        Command::said_taking("resume", t(Msg::CmdTakesSessionId), t(Msg::CmdAboutResume)),
        // A closed set of levels, so the menu offers them inline (one row each,
        // marked with the one in force) rather than a modal — the same way `/`
        // shows the commands themselves.
        Command::said("effort", t(Msg::CmdAboutEffort)).selecting(effort_options()),
        Command::said_taking("undo", t(Msg::CmdTakesTurn), t(Msg::CmdAboutUndo)),
        Command::said_taking("rewind", t(Msg::CmdTakesTurnScope), t(Msg::CmdAboutRewind)),
        Command::said_taking("model", t(Msg::CmdTakesModelId), t(Msg::CmdAboutModel)),
        Command::said("autonomy", t(Msg::CmdAboutAutonomy)),
        Command::said_taking("rename", t(Msg::CmdTakesName), t(Msg::CmdAboutRename)).requiring(),
        Command::said_taking("diff", t(Msg::CmdTakesFile), t(Msg::CmdAboutDiff)),
        Command::said_taking("mode", "[plan|ask|edits|auto]".into(), t(Msg::CmdAboutMode)),
        Command::said_taking("cd", t(Msg::CmdTakesDirectory), t(Msg::CmdAboutCd)),
        // The three modes people reach for by name. `/mode` is the one
        // implementation; these are the words tuix taught everyone to type.
        Command::said("plan", t(Msg::CmdAboutPlan)),
        Command::said("build", t(Msg::CmdAboutBuild)),
        Command::said("auto", t(Msg::CmdAboutAuto)),
        Command::said("status", t(Msg::CmdAboutStatus)),
        Command::said("cost", t(Msg::CmdAboutCost)),
        Command::said("usage", t(Msg::CmdAboutUsage)),
        Command::said_taking("mcp", t(Msg::CmdTakesMcp), t(Msg::CmdAboutMcp)),
        Command::said_taking(
            "language",
            t(Msg::CmdTakesLanguage),
            t(Msg::CmdAboutLanguage),
        ),
        Command::said("reload", t(Msg::CmdAboutReload)),
        Command::said("logout", t(Msg::CmdAboutLogout)),
        Command::said("login", t(Msg::CmdAboutLogin)),
        Command::said("whoami", t(Msg::CmdAboutWhoami)),
        Command::said_taking("think", "[on|off]".into(), t(Msg::CmdAboutThink)),
    ]
}

/// A host's refusal, in words a person can act on.
///
/// `pub(crate)` because a host command is not only a command's business: the
/// settings seam hands one to the runtime after writing a file, and its failure
/// has to read the same as every other host failure. One renderer, so one error
/// does not get two wordings depending on which path it came back along.
pub(crate) fn refusal(error: HostError) -> String {
    match error {
        HostError::Busy { reason } => t(Msg::HostBusy { reason: &reason }).into_owned(),
        HostError::NotFound => t(Msg::HostNotFound).into_owned(),
        HostError::SessionInUse { id } => t(Msg::HostSessionInUse { id: &id }).into_owned(),
        HostError::Unavailable => t(Msg::HostUnavailable).into_owned(),
        HostError::ProviderUnavailable { reason } => t(Msg::HostNoProvider {
            reason: &format!("{reason:?}"),
        })
        .into_owned(),
        HostError::Failed { message } => message,
        other => format!("{other:?}"),
    }
}

#[async_trait]
impl CommandSet for SessionCommands {
    fn id(&self) -> &'static str {
        "cmd-session"
    }
    fn commands(&self) -> Vec<Command> {
        session_catalogue()
    }
    /// What `/agents` dispatches when a row is picked. Not listed: nobody types
    /// it, and a session id in the menu would be noise (`CommandSet::hidden`).
    fn hidden(&self) -> Vec<Command> {
        vec![Command::said_taking(
            "look",
            t(Msg::CmdTakesSessionIdRequired),
            t(Msg::CmdAboutLook),
        )]
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        let Some(client) = ctx.service::<crate::plugin::AgentClientSvc>() else {
            return Outcome::Refused(t(Msg::NoAgent).into_owned());
        };
        // Asking is all a command may do here: which session is on screen is
        // screen state, and the loop writes it (`Action::LookAt`).
        if name == "look" {
            let session = args.trim();
            if session.is_empty() {
                return Outcome::Refused(t(Msg::LookWhichSession).into_owned());
            }
            return Outcome::Do(Action::LookAt(session.to_string()));
        }
        let control = client.control();
        // What host control acts on is the session this screen follows, whoever
        // is on screen.
        let root = client.root();
        let host = |control: Option<std::sync::Arc<dyn atomcode_host_api::HostControl>>| {
            control.ok_or_else(|| Outcome::Refused(t(Msg::NoHost).into_owned()))
        };
        match name {
            "cancel-all" => {
                let members = client.cancel_all();
                Outcome::Said(
                    if members == 0 {
                        t(Msg::CancelledTurn)
                    } else {
                        t(Msg::CancelledTurnAndMembers { members })
                    }
                    .into_owned(),
                )
            }
            "compact" => {
                if !client.described().is_some_and(|d| d.compaction) {
                    return Outcome::Refused(t(Msg::NoCompaction).into_owned());
                }
                // Over the handle, so it waits behind a running turn like every
                // other driver's `/compact`. The outcome comes back as an event
                // and is said then; saying "done" here would be saying it
                // before it is true.
                let focus = args.trim();
                client.compact((!focus.is_empty()).then(|| focus.to_string()));
                Outcome::Quiet
            }
            "context" => {
                let events = client.events();
                let turn = events
                    .iter()
                    .filter_map(|logged| match logged.event {
                        SessionEvent::TurnStart { turn } => Some(turn),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                let mut said = t(Msg::ContextCounts {
                    turn,
                    messages: derive_messages(&events).len(),
                    facts: events.len(),
                })
                .into_owned();
                // What the screen counted is not the budget: the host packs a
                // system prompt, instructions and tool definitions nobody here
                // ever saw. Ask it, and say both — the counts answer "what is
                // in this conversation", the budget answers "how much room is
                // left", and a person asking `/context` wants the second.
                if let Some(control) = control {
                    if let Ok(HostReply::Context {
                        window,
                        used,
                        model,
                        ..
                    }) = control.call(HostCommand::Context { session: root }).await
                    {
                        if window > 0 {
                            said.push_str(&format!(
                                "\n{used} / {window} tokens · {:.0}% · {model}",
                                used as f32 / window as f32 * 100.0
                            ));
                        }
                    }
                }
                Outcome::Said(said)
            }
            "transcript" => {
                let text = derive_messages(&client.events())
                    .iter()
                    .map(|m| format!("{:?}: {}", m.role, first_line(&m.text)))
                    .collect::<Vec<_>>()
                    .join("\n");
                Outcome::Said(if text.is_empty() {
                    t(Msg::NothingSaidYet).into_owned()
                } else {
                    text
                })
            }
            // The switch itself is not done here: the host announces the new
            // session, and the screen moves to it on that — the same way it
            // moves when something else replaced the session.
            //
            // `/clear` is this and not "empty the composer", which is what it
            // used to say. Emptying the line is a fact about the composer that
            // is already false by the time a command runs — `submit` clears the
            // field before it dispatches, so the old arm cleared nothing. What
            // people expect from the word (`/clear`, `/session` and Claude
            // Code's own) is a conversation that starts over, which is what the
            // host does here; ctrl-u is the gesture for the line.
            "clear" | "session" => {
                let Some(control) = control else {
                    return Outcome::Refused(t(Msg::NoHost).into_owned());
                };
                match control
                    .call(HostCommand::NewSession {
                        session: root.clone(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Quiet,
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // Who has been on this team, and the way to look at any of them.
            //
            // The team strip draws what is running now, so a stopped member has
            // no row there — but its log is kept, and `docs/adr/0023` §5 wants
            // it readable. This is that way in: the lead first, then every
            // member this screen has heard of, stopped ones included. The pick
            // is an [`Action::LookAt`] rather than a switch done here, because
            // which session is on screen is screen state (`docs/adr/0021`).
            "agents" => {
                let Some(roster) = ctx.service::<crate::plugin::TeamRosterSvc>() else {
                    return Outcome::Refused(t(Msg::NoRoster).into_owned());
                };
                let mut choices: Vec<crate::overlay::Choice> = Vec::new();
                if !root.is_empty() {
                    choices.push(
                        crate::overlay::Choice::new(
                            format!("/look {root}"),
                            t(Msg::AgentsLead).into_owned(),
                        )
                        .about(root.clone()),
                    );
                }
                for (name, session, gone) in roster.members() {
                    choices.push(
                        crate::overlay::Choice::new(
                            format!("/look {session}"),
                            // Where it stands is part of what the row is: a
                            // stopped member's conversation is still there and
                            // is not something to talk to.
                            if gone {
                                t(Msg::AgentsStopped { name: &name }).into_owned()
                            } else {
                                name.clone()
                            },
                        )
                        .about(session.clone()),
                    );
                }
                if choices.is_empty() {
                    return Outcome::Said(t(Msg::AgentsNoneYet).into_owned());
                }
                Outcome::Open(crate::overlay::Picker::new(
                    "agents",
                    t(Msg::AgentsPickerHint),
                    choices,
                ))
            }
            "resume" => {
                let Some(control) = control else {
                    return Outcome::Refused(t(Msg::NoHost).into_owned());
                };
                let target = args.trim();
                if target.is_empty() {
                    let working_dir = std::env::current_dir()
                        .ok()
                        .map(|dir| dir.display().to_string());
                    return match control
                        .call(HostCommand::ListSessions { working_dir })
                        .await
                    {
                        Ok(HostReply::Sessions { sessions }) => {
                            // Drop the one on screen — resuming the session you
                            // are already in is a no-op — and hand the rest to the
                            // panel, which rises over the composer like `/provider`
                            // rather than a pop-up. The metadata (`N 轮 · 时间 ·
                            // 目录`) is drawn by the panel from these fields.
                            let live = client.session();
                            let sessions: Vec<crate::resume::Session> = sessions
                                .into_iter()
                                .filter(|stored| stored.id != live)
                                .map(|stored| crate::resume::Session {
                                    id: stored.id,
                                    title: stored.title,
                                    working_dir: stored.working_dir,
                                    updated_at: stored.updated_at,
                                    turns: stored.turns,
                                    needs_newer_version: stored.needs_newer_version,
                                })
                                .collect();
                            if sessions.is_empty() {
                                Outcome::Said(t(Msg::ResumeNoOthers).into_owned())
                            } else {
                                Outcome::Do(Action::OpenResume(crate::resume::ResumeView::new(
                                    sessions,
                                )))
                            }
                        }
                        Ok(other) => Outcome::Refused(format!("{other:?}")),
                        Err(error) => Outcome::Refused(refusal(error)),
                    };
                }
                match control
                    .call(HostCommand::Resume {
                        session: root.clone(),
                        target: target.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Quiet,
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // A level is the session's, not the model route's, so this survives
            // a model switch. Whether a route can reason at all is the model's.
            "effort" => {
                let wanted = args.trim();
                // One vocabulary, taken from the place that defines it, so this
                // command cannot offer a level nothing parses.
                let levels = atomcode_harness::REASONING_EFFORT_LEVELS;
                // With nothing after it, the command reports rather than opens a
                // modal: the levels are offered inline in the slash menu (one row
                // each — see `effort_options`), so a bare `/effort` that reaches
                // here is the menu dismissed, and the honest answer is the level
                // in force and the closed set to type. A pick from the menu
                // arrives as `/effort <level>`, the branch below.
                if wanted.is_empty() {
                    let current = client
                        .described()
                        .and_then(|d| d.reasoning_effort)
                        .map(|level| level.as_str().to_string());
                    let now = current.as_deref().unwrap_or("default");
                    let mut all = levels.to_vec();
                    all.push("default");
                    return Outcome::Said(
                        t(Msg::EffortCurrent {
                            now,
                            levels: &all.join(", "),
                        })
                        .into_owned(),
                    );
                }
                let level = if wanted == "default" {
                    None
                } else if levels.contains(&wanted) {
                    ReasoningEffort::from_config(Some(wanted))
                } else {
                    return Outcome::Refused(
                        t(Msg::EffortUnknown {
                            wanted,
                            levels: &levels.join(", "),
                        })
                        .into_owned(),
                    );
                };
                let Some(control) = control else {
                    return Outcome::Refused(t(Msg::NoHost).into_owned());
                };
                match control
                    .call(HostCommand::SetReasoningEffort {
                        session: root.clone(),
                        level,
                    })
                    .await
                {
                    Ok(_) => {
                        client.chose_effort(level);
                        Outcome::Said(t(Msg::EffortSet { wanted }).into_owned())
                    }
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // The conversation goes back; the words the person said go back to
            // where they type, to change and send again (`docs/adr/0024` §17).
            "undo" | "rewind" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                if client.session() != root {
                    return Outcome::Refused(t(Msg::UndoLeadOnly).into_owned());
                }
                let mut words = args.split_whitespace();
                let turn = match words.next().map(str::parse::<u64>) {
                    None => None,
                    Some(Ok(turn)) => Some(turn),
                    Some(Err(_)) => {
                        return Outcome::Refused(
                            t(Msg::NotATurnNumber { what: args.trim() }).into_owned(),
                        )
                    }
                };
                let based_on = client.root_high();
                let reply = if name == "undo" {
                    control
                        .call(HostCommand::Undo {
                            session: root,
                            turn,
                            based_on,
                        })
                        .await
                } else {
                    // With no turn: the panel, which is where choosing one
                    // belongs (`crate::rewind`). It used to be a modal picker
                    // here — a list you picked from once and lost — and the scope
                    // could only be said by typing it. The panel is the same
                    // gesture a double-tap on Esc makes, so there is one rewind
                    // on screen rather than two.
                    let Some(turn) = turn else {
                        return Outcome::Do(Action::ToggleRewind);
                    };
                    let scope = match words.next() {
                        // Both spellings of each scope are taken, in either
                        // language: what a person typed last month must keep
                        // parsing after `/language`. The panel never dispatches
                        // a command at all — it carries a `Scope` — so this
                        // parser exists for what a person types, and only that.
                        None | Some("对话") | Some("conversation") => {
                            atomcode_kernel::session::RewindScope::Conversation
                        }
                        Some("代码") | Some("code") => {
                            atomcode_kernel::session::RewindScope::Code
                        }
                        Some("全部") | Some("both") => {
                            atomcode_kernel::session::RewindScope::Both
                        }
                        Some(other) => {
                            return Outcome::Refused(
                                t(Msg::RewindScopeUnknown { what: other }).into_owned(),
                            )
                        }
                    };
                    control
                        .call(HostCommand::Rewind {
                            session: root,
                            turn,
                            scope,
                            based_on,
                        })
                        .await
                };
                match reply {
                    Ok(HostReply::Undone {
                        prompt: Some(prompt),
                        ..
                    }) => Outcome::Do(Action::Paste(prompt)),
                    Ok(HostReply::Undone { restored_files, .. }) => Outcome::Said(
                        t(Msg::RewindRestored {
                            files: restored_files.len(),
                        })
                        .into_owned(),
                    ),
                    Ok(other) => Outcome::Refused(format!("{other:?}")),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "model" => {
                let wanted = args.trim();
                // With no argument: open the providers panel on its model list —
                // one surface for switching and editing models, the same panel
                // `/provider` opens on its 账号 tab. It replaced a models-only
                // popup so switching and editing a model are never two places.
                if wanted.is_empty() {
                    return Outcome::Do(Action::OpenModels);
                }
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::SwitchModel {
                        session: root,
                        model: wanted.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(t(Msg::ModelSet { wanted }).into_owned()),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "mode" => {
                let wanted = match args.trim() {
                    "" => return Outcome::Said(t(Msg::ModeWhatEachDoes).into_owned()),
                    other => match mode_named(other) {
                        Some(mode) => mode,
                        None => {
                            return Outcome::Refused(
                                t(Msg::ModeUnknown { what: other }).into_owned(),
                            )
                        }
                    },
                };
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::SetMode {
                        session: root,
                        mode: wanted,
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(t(Msg::ModeSet { mode: args.trim() }).into_owned()),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "cd" => {
                let directory = args.trim();
                // `pin` / `unpin`:标一个目录,或取消。带目录就是它,不带就是
                // 现在这个——人多半是干着干着决定「这地方以后还要来」。
                if let Some(rest) = directory
                    .strip_prefix("pin")
                    .or_else(|| directory.strip_prefix("unpin"))
                {
                    let pinning = directory.starts_with("pin");
                    let where_ = match rest.trim() {
                        "" => client.root(),
                        named => named.to_string(),
                    };
                    let Some(places) = ctx.service::<crate::plugin::PlacesSvc>() else {
                        return Outcome::Refused(t(Msg::NoPlaces).into_owned());
                    };
                    let done = match pinning {
                        true => places.pin(&where_).await,
                        false => places.unpin(&where_).await,
                    };
                    let shown = crate::text::collapse_home(&where_);
                    return match (done, pinning) {
                        (Ok(()), true) => {
                            Outcome::Said(t(Msg::CdPinned { dir: &shown }).into_owned())
                        }
                        (Ok(()), false) => {
                            Outcome::Said(t(Msg::CdUnpinned { dir: &shown }).into_owned())
                        }
                        (Err(why), _) => Outcome::Refused(why),
                    };
                }
                // Nothing typed, or a directory named but not the last word:
                // browse from there. tuix had a picker for this
                // (`modals/dir_picker.rs`); what a person needs of it is to see
                // what is under here and step into it, which is a list whose
                // picks are this command again.
                if directory.is_empty() || directory.ends_with('/') {
                    // 从哪儿开始浏览。**要问宿主要工作目录**:`client.root()` 是
                    // 这块屏幕跟着的**会话 id**,不是目录——裸 `/cd` 曾经拿它当路径
                    // 去读,于是只会报「读不了」,相对路径也拼在会话 id 上。
                    let from = if std::path::Path::new(directory).is_absolute() {
                        directory.to_string()
                    } else {
                        let Some(here) = working_dir(control.clone(), client.root()).await else {
                            return Outcome::Refused(t(Msg::NoHost).into_owned());
                        };
                        if directory.is_empty() {
                            here
                        } else {
                            std::path::Path::new(&here)
                                .join(directory)
                                .display()
                                .to_string()
                        }
                    };
                    // The trailing slash was the gesture ("browse here"), not
                    // part of the place. Left on, the row that says "stay here"
                    // would read as another "browse here" and the browser could
                    // not be stepped out of. Root keeps its one slash.
                    let from = {
                        let trimmed = from.trim_end_matches('/');
                        if trimmed.is_empty() {
                            "/".to_string()
                        } else {
                            trimmed.to_string()
                        }
                    };
                    let mut choices: Vec<crate::overlay::Choice> = Vec::new();
                    // 标下的地方排在最前,其次是最近干活的目录:这两样是「我要去
                    // 哪儿」的答案,而底下那半是「这儿有什么」。只在最外面那一屏
                    // 给——走进子目录之后再列一遍,等于每一层都把同样的东西说一遍。
                    if directory.is_empty() {
                        let pinned = match ctx.service::<crate::plugin::PlacesSvc>() {
                            Some(places) => places.bookmarks().await,
                            None => Vec::new(),
                        };
                        for dir in &pinned {
                            choices.push(
                                crate::overlay::Choice::new(
                                    format!("/cd {dir}"),
                                    crate::text::collapse_home(dir),
                                )
                                .about(t(Msg::CdBookmarked)),
                            );
                        }
                        for dir in recent_places(control.clone(), &from, &pinned).await {
                            choices.push(
                                crate::overlay::Choice::new(
                                    format!("/cd {dir}"),
                                    crate::text::collapse_home(&dir),
                                )
                                .about(t(Msg::CdRecent)),
                            );
                        }
                    }
                    // Up first: a browser you cannot back out of is a trap.
                    if let Some(up) = std::path::Path::new(&from).parent() {
                        choices.push(
                            crate::overlay::Choice::new(
                                format!("/cd {}/", up.display()),
                                "..".to_string(),
                            )
                            .about(t(Msg::CdUpOneLevel)),
                        );
                    }
                    match std::fs::read_dir(&from) {
                        Ok(entries) => {
                            let mut here: Vec<String> = entries
                                .flatten()
                                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                                .map(|e| e.file_name().to_string_lossy().into_owned())
                                .filter(|name| !name.starts_with('.'))
                                .collect();
                            here.sort();
                            for name in here {
                                let at = std::path::Path::new(&from).join(&name);
                                choices.push(
                                    crate::overlay::Choice::new(
                                        format!("/cd {}/", at.display()),
                                        name,
                                    )
                                    .about(t(Msg::CdStepInto)),
                                );
                            }
                        }
                        Err(error) => {
                            return Outcome::Refused(
                                t(Msg::FileUnreadable {
                                    path: &from,
                                    error: &error.to_string(),
                                })
                                .into_owned(),
                            )
                        }
                    }
                    // Staying is a choice too — and the only way to say "this
                    // one" once you have stepped into it.
                    choices.insert(
                        0,
                        crate::overlay::Choice::new(
                            format!("/cd {from}"),
                            t(Msg::CdStayHere).into_owned(),
                        )
                        .about(crate::text::collapse_home(&from)),
                    );
                    return Outcome::Open(crate::overlay::Picker::new(
                        "cd",
                        t(Msg::CdPickerHint {
                            here: &crate::text::collapse_home(&from),
                        }),
                        choices,
                    ));
                }
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::ChangeDirectory {
                        session: root,
                        directory: directory.to_string(),
                    })
                    .await
                {
                    // A new session: what was read and written belongs to where
                    // it ran, so the screen follows the new stream.
                    Ok(HostReply::SessionChanged { session }) => Outcome::Said(
                        t(Msg::CdMovedNewSession {
                            directory,
                            session: &session,
                        })
                        .into_owned(),
                    ),
                    Ok(_) => Outcome::Said(t(Msg::CdMoved { directory }).into_owned()),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // Two levels, one command: the list, then one file's diff. The
            // most-asked question of a coding session is "what did it do to my
            // code", and before this the only way to ask it was to leave for
            // another window or spend a turn asking the model — which answers
            // from what it remembers doing, not from the workspace.
            "diff" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                let wanted = args.trim();
                let file = (!wanted.is_empty()).then(|| wanted.to_string());
                match control
                    .call(HostCommand::Changes {
                        session: root.clone(),
                        file: file.clone(),
                    })
                    .await
                {
                    // "Cannot tell" and "nothing changed" are different answers
                    // and must read differently: one is a session without
                    // workspace snapshots, the other is a session that has not
                    // touched anything.
                    Ok(HostReply::Changes {
                        unavailable: Some(why),
                        ..
                    }) => Outcome::Refused(why),
                    Ok(HostReply::Changes {
                        diff: Some(text), ..
                    }) => {
                        let what = file.unwrap_or_default();
                        if text.trim().is_empty() {
                            return Outcome::Said(
                                t(Msg::DiffNoChangeIn { what: &what }).into_owned(),
                            );
                        }
                        Outcome::Open(crate::overlay::Reading::diff(what, &text))
                    }
                    Ok(HostReply::Changes { files, .. }) if files.is_empty() => {
                        Outcome::Said(t(Msg::DiffNothingChanged).into_owned())
                    }
                    Ok(HostReply::Changes { files, .. }) => {
                        let count = files.len();
                        let (added, removed): (u64, u64) = files
                            .iter()
                            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
                        let choices = files
                            .into_iter()
                            .map(|f| {
                                let about = if f.binary {
                                    t(Msg::DiffBinary).into_owned()
                                } else {
                                    format!("+{} -{}", f.added, f.removed)
                                };
                                // The value is the command that opens it, so a
                                // pick and a typed `/diff <path>` reach the same
                                // implementation.
                                crate::overlay::Choice::new(
                                    format!("/diff {}", f.path),
                                    f.path.clone(),
                                )
                                .about(about)
                            })
                            .collect();
                        Outcome::Open(crate::overlay::Picker::new(
                            "diff",
                            t(Msg::DiffPickerHint {
                                count,
                                added,
                                removed,
                            }),
                            choices,
                        ))
                    }
                    Ok(other) => Outcome::Refused(
                        t(Msg::HostSaidSomethingElse {
                            reply: &format!("{other:?}"),
                        })
                        .into_owned(),
                    ),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // A named way in to one setting, because it is the one people
            // look for by name. It is `/config language <x>` underneath — one
            // implementation, so the two cannot drift.
            "language" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                let settings = match control
                    .call(HostCommand::Settings {
                        session: root.clone(),
                    })
                    .await
                {
                    Ok(HostReply::Settings { settings }) => settings,
                    Ok(other) => {
                        return Outcome::Refused(
                            t(Msg::HostSaidSomethingElse {
                                reply: &format!("{other:?}"),
                            })
                            .into_owned(),
                        )
                    }
                    Err(error) => return Outcome::Refused(refusal(error)),
                };
                let Some(setting) = settings.into_iter().find(|s| s.id == "language") else {
                    return Outcome::Refused(t(Msg::NoLanguageSetting).into_owned());
                };
                let wanted = args.trim();
                // With nothing after it, say what it is and what it takes.
                // `/config` is the settings panel now — a screen command with
                // its own search and editing — and a session command cannot
                // open it for one row, so this names the row instead.
                if wanted.is_empty() {
                    return Outcome::Said(
                        t(Msg::LanguageNow {
                            value: &setting.value,
                            accepts: &setting.accepts,
                        })
                        .into_owned(),
                    );
                }
                match control
                    .call(HostCommand::SetSetting {
                        session: root.clone(),
                        id: "language".into(),
                        value: wanted.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(
                        t(Msg::LanguageSet {
                            wanted,
                            applies: &setting.applies,
                        })
                        .into_owned(),
                    ),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // Listing only. Adding, editing and removing a provider stays with
            // the configuration file on purpose: a provider entry carries an
            // `api_key`, and a screen that edited those tables would be a screen
            // that handles credentials.
            //
            // Switching is `/model <id>` — a provider and a model are resolved
            // by the same call, so the picked value is that command rather than
            // a second switch that would have to agree with it.
            // The runtime publishes `GoalChanged` every round, but that stream
            // is its own and this screen is not on it — so this asks. An
            // always-on status line would want the push instead; that is the
            // part still owed (B2-13's second half).
            // What `/cost` cannot answer: that one is this conversation's
            // token bill, this is the account's remaining allowance. Two
            // questions that sound alike and have different answers — a person
            // can be cheap this session and still be locked out until 14:30.
            "usage" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                match control
                    .call(HostCommand::Usage {
                        session: root,
                        windows_only: false,
                    })
                    .await
                {
                    Ok(HostReply::Usage { windows, .. }) if windows.is_empty() => {
                        Outcome::Said(t(Msg::UsageNotCounted).into_owned())
                    }
                    Ok(HostReply::Usage { windows, .. }) => Outcome::Said(
                        windows
                            .into_iter()
                            .map(|w| {
                                let cap = w
                                    .call_limit
                                    .map(|n| t(Msg::UsageCallLimit { n }).into_owned())
                                    .unwrap_or_default();
                                if w.exhausted {
                                    // The one line a person actually needs, and
                                    // the reason this is not `/cost`.
                                    let when = if w.resets_at.is_empty() {
                                        t(Msg::UsageResetsIn {
                                            duration: &crate::text::spoken_duration(
                                                w.resets_in_seconds.max(0) as u64,
                                            ),
                                        })
                                    } else {
                                        t(Msg::UsageResetsAt { at: &w.resets_at })
                                    };
                                    t(Msg::UsageExhausted {
                                        label: &w.label,
                                        when: &when,
                                        cap: &cap,
                                    })
                                    .into_owned()
                                } else {
                                    t(Msg::UsageLeft {
                                        label: &w.label,
                                        cap: &cap,
                                    })
                                    .into_owned()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Ok(other) => Outcome::Refused(
                        t(Msg::HostSaidSomethingElse {
                            reply: &format!("{other:?}"),
                        })
                        .into_owned(),
                    ),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "autonomy" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                match control
                    .call(HostCommand::Autonomy {
                        session: root.clone(),
                    })
                    .await
                {
                    Ok(HostReply::Autonomy { running: None }) => {
                        Outcome::Said(t(Msg::AutonomyIdle).into_owned())
                    }
                    Ok(HostReply::Autonomy {
                        running: Some(running),
                    }) => {
                        let what = if running.kind == "goal" {
                            t(Msg::AutonomyGoal {
                                what: &running.what,
                            })
                        } else {
                            t(Msg::AutonomyLoop {
                                what: &running.what,
                            })
                        };
                        let rounds = match running.of {
                            Some(of) => t(Msg::AutonomyRoundOf {
                                round: running.round,
                                of,
                            }),
                            None => t(Msg::AutonomyRound {
                                round: running.round,
                            }),
                        };
                        let took = crate::text::spoken_duration(running.elapsed_secs);
                        let line = t(Msg::AutonomyLine {
                            what: &what,
                            rounds: &rounds,
                            took: &took,
                        })
                        .into_owned();
                        Outcome::Said(match running.paused {
                            Some(why) => t(Msg::AutonomyHeld {
                                line: &line,
                                why: &why,
                            })
                            .into_owned(),
                            None => line,
                        })
                    }
                    Ok(other) => Outcome::Refused(
                        t(Msg::HostSaidSomethingElse {
                            reply: &format!("{other:?}"),
                        })
                        .into_owned(),
                    ),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // One implementation, three words. A person who types `/plan`
            // means the mode, and a second switch that had to agree with
            // `/mode` is the thing that eventually disagrees.
            "plan" | "build" | "auto" => {
                let wanted = match name {
                    "plan" => "plan",
                    "build" => "ask",
                    _ => "auto",
                };
                return Box::pin(self.run("mode", wanted, ctx)).await;
            }
            // `/cost` is `/context` under the name tuix taught. Same reason.
            "cost" => return Box::pin(self.run("context", "", ctx)).await,
            // What a person asks when they come back to a window and cannot
            // remember which one it is. Everything here is already on screen
            // somewhere — this is the one place that says it all at once.
            "status" => {
                let described = client.described();
                let model = described
                    .as_ref()
                    .and_then(|d| d.model.clone())
                    .unwrap_or_else(|| t(Msg::StatusNoModel).into_owned());
                let effort = described
                    .as_ref()
                    .and_then(|d| d.reasoning_effort)
                    .map(|level| level.as_str().to_string())
                    .unwrap_or_else(|| t(Msg::StatusEffortDefault).into_owned());
                let mut lines = vec![
                    t(Msg::StatusSessionLine {
                        session: &client.session(),
                    })
                    .into_owned(),
                    t(Msg::StatusModelLine {
                        model: &model,
                        effort: &effort,
                    })
                    .into_owned(),
                    t(Msg::StatusWhereLine {
                        where_: &crate::text::collapse_home(&client.root()),
                    })
                    .into_owned(),
                ];
                if let Some(control) = control {
                    if let Ok(HostReply::Autonomy {
                        running: Some(running),
                    }) = control
                        .call(HostCommand::Autonomy {
                            session: root.clone(),
                        })
                        .await
                    {
                        lines.push(
                            t(Msg::StatusAutonomyLine {
                                what: &running.what,
                                round: running.round,
                                took: &crate::text::spoken_duration(running.elapsed_secs),
                            })
                            .into_owned(),
                        );
                    }
                }
                Outcome::Said(lines.join("\n"))
            }
            "whoami" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                match control
                    .call(HostCommand::WhoAmI {
                        session: root.clone(),
                    })
                    .await
                {
                    Ok(HostReply::Identity {
                        signed_in: true,
                        who,
                        detail,
                    }) => {
                        let who = who.unwrap_or_else(|| t(Msg::WhoAmIUnnamed).into_owned());
                        Outcome::Said(match detail {
                            Some(detail) => format!("{who} · {detail}"),
                            None => who,
                        })
                    }
                    Ok(HostReply::Identity { .. }) => {
                        Outcome::Said(t(Msg::WhoAmINobody).into_owned())
                    }
                    Ok(other) => Outcome::Refused(
                        t(Msg::HostSaidSomethingElse {
                            reply: &format!("{other:?}"),
                        })
                        .into_owned(),
                    ),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // Two knobs, not one: `/effort` is how hard, this is whether at all.
            "think" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                let wanted = args.trim().to_ascii_lowercase();
                let on = match wanted.as_str() {
                    "" => {
                        return match control
                            .call(HostCommand::Thinking {
                                session: root.clone(),
                            })
                            .await
                        {
                            Ok(HostReply::Settings { settings }) => match settings.first() {
                                Some(setting) => Outcome::Said(
                                    t(Msg::ThinkingNow {
                                        value: &setting.value,
                                    })
                                    .into_owned(),
                                ),
                                None => Outcome::Refused(t(Msg::NoThinkingSwitch).into_owned()),
                            },
                            Ok(other) => Outcome::Refused(
                                t(Msg::HostSaidSomethingElse {
                                    reply: &format!("{other:?}"),
                                })
                                .into_owned(),
                            ),
                            Err(error) => Outcome::Refused(refusal(error)),
                        }
                    }
                    "on" | "true" => true,
                    "off" | "false" => false,
                    other => {
                        return Outcome::Refused(t(Msg::NotOnOrOff { what: other }).into_owned());
                    }
                };
                match control
                    .call(HostCommand::SetThinking {
                        session: root.clone(),
                        on,
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(
                        t(Msg::ThinkingSet {
                            value: if on { "on" } else { "off" },
                        })
                        .into_owned(),
                    ),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "rename" => {
                let title = args.trim();
                if title.is_empty() {
                    return Outcome::Refused(t(Msg::RenameNeedsName).into_owned());
                }
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::Rename {
                        session: root,
                        title: title.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(t(Msg::RenamedTo { title }).into_owned()),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "mcp" => {
                // 无参要的是那块面板,不是一列文本(设计 §5.1):它读的端口是面板自己的,
                // 而命令这一层连不上宿主——所以举起动作,由插件那一侧升起它(`/toolbox`
                // 同形)。有参的那几支照旧,面板是给无参调用的人的。
                let rest = args.trim();
                if rest.is_empty() {
                    return Outcome::Do(Action::ToggleMcp);
                }
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match rest {
                    "withdraw" => match control
                        .call(HostCommand::WithdrawMcpTools { session: root })
                        .await
                    {
                        Ok(_) => Outcome::Said(t(Msg::McpWithdrawn).into_owned()),
                        Err(error) => Outcome::Refused(refusal(error)),
                    },
                    // `tools <server>`: which tools that server actually put on
                    // the model. The status line says a server is connected;
                    // this says what came of it.
                    rest if rest.starts_with("tools") => {
                        let server = rest.trim_start_matches("tools").trim();
                        if server.is_empty() {
                            return Outcome::Refused(t(Msg::McpNeedsServerName).into_owned());
                        }
                        match control
                            .call(HostCommand::McpTools {
                                session: root,
                                server: server.to_string(),
                            })
                            .await
                        {
                            Ok(HostReply::McpTools { tools }) if tools.is_empty() => {
                                Outcome::Said(t(Msg::McpServerHasNoTools { server }).into_owned())
                            }
                            Ok(HostReply::McpTools { tools }) => Outcome::Said(tools.join("\n")),
                            Ok(other) => Outcome::Refused(format!("{other:?}")),
                            Err(error) => Outcome::Refused(refusal(error)),
                        }
                    }
                    other => {
                        Outcome::Refused(t(Msg::McpUnknownSubcommand { what: other }).into_owned())
                    }
                }
            }
            "reload" | "logout" | "login" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                let (command, done) = match name {
                    "reload" => (HostCommand::Reload { session: root }, t(Msg::Reloaded)),
                    "logout" => (HostCommand::SignOut { session: root }, t(Msg::SignedOut)),
                    _ => (HostCommand::SignIn { session: root }, t(Msg::SignedIn)),
                };
                match control.call(command).await {
                    Ok(_) => Outcome::Said(done.into_owned()),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            _ => Outcome::Quiet,
        }
    }
}

/// 这个会话现在在哪个目录里干活。
///
/// 问宿主,不看屏幕自己记的东西:目录是运行中那棵树的事实,`/cd` 改的也是它
/// (`docs/adr/0022` §3)。
async fn working_dir(
    control: Option<std::sync::Arc<dyn atomcode_host_api::HostControl>>,
    session: String,
) -> Option<String> {
    let control = control?;
    match control.call(HostCommand::Context { session }).await {
        Ok(HostReply::Context { working_dir, .. }) => Some(working_dir),
        _ => None,
    }
}

/// 最近干活的那几个目录,最新的在前。
///
/// 从宿主的会话目录折出来,不另存一份:会话本来就记着它是在哪儿跑的,而「最近去过
/// 哪儿」正是这句话的另一种读法。现在这个目录和已经标下的目录不再重复出现——
/// 一份清单里同一个地方出现两次,人得先分辨它们是不是同一个。
async fn recent_places(
    control: Option<std::sync::Arc<dyn atomcode_host_api::HostControl>>,
    here: &str,
    pinned: &[String],
) -> Vec<String> {
    /// 列几个。多到要翻页的「最近」就不是最近了。
    const MOST: usize = 5;
    let Some(control) = control else {
        return Vec::new();
    };
    let Ok(HostReply::Sessions { sessions }) = control
        .call(HostCommand::ListSessions { working_dir: None })
        .await
    else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for session in sessions {
        let Some(dir) = session.working_dir else {
            continue;
        };
        if dir == here || pinned.iter().any(|already| already == &dir) {
            continue;
        }
        if out.iter().any(|already| already == &dir) {
            continue;
        }
        out.push(dir);
        if out.len() == MOST {
            break;
        }
    }
    out
}

/// `/help`, which has to know about everything, so it holds the registry.
pub struct HelpCommands {
    pub all: Arc<Commands>,
}

fn help_catalogue() -> Vec<Command> {
    // `/guide` is what the classic screen called "how do I use this": there it
    // was a hand-written menu of thirteen lines, plus a skill for a question
    // with an argument. The menu is what `/help` already is, and the question
    // half is the `ask` skill, which is a command of its own wherever it is
    // installed — so the name resolves here rather than growing a second help.
    vec![Command::said("help", t(Msg::CmdAboutHelp)).with_aliases(&["guide"])]
}

#[async_trait]
impl CommandSet for HelpCommands {
    fn id(&self) -> &'static str {
        "cmd-help"
    }
    fn commands(&self) -> Vec<Command> {
        help_catalogue()
    }
    async fn run(&self, _name: &str, _args: &str, _ctx: &Context) -> Outcome {
        let width = self
            .all
            .all()
            .iter()
            .map(|c| c.display_name().len() + c.takes.as_ref().map(|t| t.len() + 1).unwrap_or(0))
            .max()
            .unwrap_or(8);
        Outcome::Said(
            self.all
                .all()
                .iter()
                .map(|c| {
                    // Aliases are shown here too (`/session (new)`), so `/help`
                    // and the slash menu name a command the same way.
                    let head = match &c.takes {
                        Some(t) => format!("/{} {t}", c.display_name()),
                        None => format!("/{}", c.display_name()),
                    };
                    format!("{head:<w$}  {}", c.about, w = width + 2)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// `/setup`: put the seed skills on disk if this machine never has, then hand
/// the name to the agent — which owns what running it means.
///
/// **Why the screen owns this at all.** `/setup` is a skill
/// (`assets/setup-seeds/skills/atomcode-automation-recommender/SKILL.md` —
/// `name: setup`, `user_invocable: true`), and the `skills` row registers one
/// command per user-invocable skill, so on a machine where the seeds *are*
/// installed the agent's catalog already offers `/setup` and there is nothing to
/// add here. On a machine where they are not, that command does not exist yet —
/// and typing `/setup` is answered with "no such command" by the very command
/// that would have installed what it needs. Unpacking the seeds is the one step
/// the agent cannot take, because it has to have the skill before it can be
/// asked for one.
///
/// **Why it declares `overrides`.** Both machines have to end in the same place,
/// so this set claims the name unconditionally rather than only when the agent
/// lacks it. Who would notice the difference is the person, at the moment one of
/// the two worked and the other did not.
///
/// What is left of tuix's `/setup` (`event_loop/commands.rs:3947`) is exactly
/// this: install → reload → forward. The reload is not optional — writing the
/// files is not making them so, and the command about to be handed over is one
/// of the things that arrives by that rebuild.
pub struct SetupCommands;

fn setup_catalogue() -> Vec<Command> {
    vec![Command::said_taking(
        "setup",
        t(Msg::CmdTakesSetup),
        t(Msg::CmdAboutSetup),
    )]
}

#[async_trait]
impl CommandSet for SetupCommands {
    fn id(&self) -> &'static str {
        "cmd-setup"
    }
    fn commands(&self) -> Vec<Command> {
        setup_catalogue()
    }
    fn overrides(&self) -> Vec<&'static str> {
        vec!["setup"]
    }
    async fn run(&self, _name: &str, args: &str, ctx: &Context) -> Outcome {
        let Some(client) = ctx.service::<crate::plugin::AgentClientSvc>() else {
            return Outcome::Refused(t(Msg::NoAgent).into_owned());
        };
        let Some(port) = ctx.service::<crate::plugin::SetupSvc>() else {
            return Outcome::Refused(t(Msg::NoSetupPort).into_owned());
        };
        // The common case, and the whole reason this is a question rather than
        // an attempt: on a machine that has run `/setup` once, unfolding the
        // seeds, taking the file lock and rebuilding the graph are all work with
        // nothing behind it.
        if port.installed() {
            return run_setup_skill(client.as_ref(), args);
        }
        // Said before the work, not after: unpacking the embedded seeds is a
        // second of file I/O, and a command that says nothing until it is over
        // reads as one that did nothing.
        let ui = ctx.service::<atomcode_harness::seams::UiSvc>();
        let announce = |line: &str| {
            if let Some(ui) = ui.as_ref() {
                ui.say(line);
            }
        };
        announce(&t(Msg::SetupInstalling));
        let report = match port.install().await {
            Ok(report) => report,
            Err(why) => return Outcome::Refused(why),
        };
        if let Err(why) = reload(ctx).await {
            // The files really are on disk — saying only the failure would have
            // a person run `/setup` twice for nothing.
            return Outcome::Said(
                t(Msg::ReloadFailedAfter {
                    said: report.trim_end(),
                    why: &why,
                })
                .into_owned(),
            );
        }
        announce(report.trim_end());
        run_setup_skill(client.as_ref(), args)
    }
}

/// Hand the name to the agent, which is where the skill actually runs.
///
/// The skill expands into **the person's own message** (`RunSkill` in
/// `harness/plugins/capabilities.rs`): a skill is a prompt somebody wrote to
/// send, so what comes back is an ordinary turn — logged as theirs, answerable,
/// undoable. Asking to send it is all the screen does.
///
/// Not gated on the agent having said it offers `setup`: right after the reload
/// above, what the screen was last *described* as offering is the answer from
/// before it. The agent looks the name up in the catalog it has now and refuses
/// it out loud if it is not there — one honest refusal beats a fresh false one.
fn run_setup_skill(client: &crate::plugin::AgentClient, args: &str) -> Outcome {
    client.invoke("setup", args);
    Outcome::Said(t(Msg::SetupRunningSkill).into_owned())
}

/// The agent's own commands, as its description lists them (`docs/adr/0021`
/// §10): whatever the rows in its tree registered — stopping a team member, say.
/// Run by name through the connection; what one produced comes back on screen.
///
/// Read when asked rather than copied at mount, so what is listed is what the
/// agent on screen was last described as offering.
pub struct AgentCatalogCommands {
    pub client: Arc<crate::plugin::AgentClient>,
}

#[async_trait]
impl CommandSet for AgentCatalogCommands {
    fn id(&self) -> &'static str {
        "cmd-agent-catalog"
    }
    fn commands(&self) -> Vec<Command> {
        self.client
            .described()
            .map(|d| d.commands)
            .unwrap_or_default()
            .into_iter()
            .map(|c| Command {
                name: c.name.into(),
                about: c.summary.into(),
                takes: c.usage.map(Into::into),
                // The agent's own commands carry no aliases.
                aliases: &[],
                // Nor a closed set of values to pick from inline.
                options: Vec::new(),
                // The agent owns what its own command does with no argument, so
                // this screen dispatches it bare rather than deciding for it.
                require_arg: false,
            })
            .collect()
    }
    async fn run(&self, name: &str, args: &str, _ctx: &Context) -> Outcome {
        self.client.invoke(name, args);
        Outcome::Quiet
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// The last thing the model said, as text. Empty when it has not said anything
/// yet — a session that has only been typed into.
fn last_answer(events: &[atomcode_kernel::session::LoggedEvent]) -> String {
    derive_messages(events)
        .into_iter()
        .rfind(|m| m.role == Role::Assistant)
        .map(|m| m.text)
        .unwrap_or_default()
}

/// The fenced code blocks in `text`, in the order they appear, without their
/// fences. An unclosed fence still counts: a model that stopped mid-block wrote
/// the part a person wants to run, and refusing to copy it because the closing
/// line never arrived is the wrong answer.
fn code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        match (&mut current, fence) {
            (None, true) => current = Some(Vec::new()),
            (Some(_), true) => {
                let lines = current.take().unwrap_or_default();
                blocks.push(lines.join("\n"));
            }
            (Some(lines), false) => lines.push(line),
            (None, false) => {}
        }
    }
    if let Some(lines) = current {
        blocks.push(lines.join("\n"));
    }
    blocks.retain(|b| !b.trim().is_empty());
    blocks
}

/// The conversation as markdown: who said what, in order, with tool traffic
/// left out. What is saved is read somewhere else — an editor, a review, an
/// issue — so it is the conversation, not this screen's rendering of it.
fn as_markdown(events: &[atomcode_kernel::session::LoggedEvent]) -> String {
    let mut out = String::new();
    for message in derive_messages(events) {
        let who = match message.role {
            Role::User => t(Msg::MarkdownUser),
            Role::Assistant => t(Msg::MarkdownAssistant),
            Role::System | Role::Tool => continue,
        };
        if message.text.trim().is_empty() {
            continue;
        }
        out.push_str(&who);
        out.push_str("\n\n");
        out.push_str(message.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

// `builtin()` used to live here and mount all five sets at once. It is gone on
// purpose: with each set a row, a function that mounted "the usual five" would
// be a second answer to "what commands does a screen have", and the second
// answer is the one that goes stale. See `crate::rows::SCREEN`.

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that answers from a script and keeps what it was asked.
    #[derive(Default)]
    struct Recording {
        asked: std::sync::Mutex<Vec<HostCommand>>,
        replies: std::sync::Mutex<std::collections::VecDeque<Result<HostReply, HostError>>>,
    }

    #[async_trait]
    impl atomcode_host_api::HostControl for Recording {
        async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
            self.asked.lock().unwrap().push(command);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(HostReply::Done))
        }
        fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_host_api::HostEvent> {
            tokio::sync::mpsc::unbounded_channel().1
        }
    }

    /// A screen following session `lead`, whose last fact it saw is number 7.
    fn following(host: &Arc<Recording>) -> (App, Arc<crate::plugin::AgentClient>, Arc<Commands>) {
        let app = bare();
        let client = Arc::new(crate::plugin::AgentClient::default());
        let (commands, _agent) = tokio::sync::mpsc::unbounded_channel();
        client.connect(commands, host.clone());
        client.follow("lead");
        client.keep(&atomcode_kernel::session::Committed {
            session: "lead".into(),
            seq: 7,
            at: 0,
            event: SessionEvent::TurnStart { turn: 1 },
        });
        let _ = app
            .context()
            .provide::<crate::plugin::AgentClientSvc>(client.clone());
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(SessionCommands));
        (app, client, all)
    }

    /// `/undo` asks the host about the session this screen follows, based on the
    /// last fact it saw, and puts the words it hands back where the person
    /// types (`docs/adr/0024` §17).
    #[tokio::test]
    async fn undo_is_asked_of_the_host_and_the_words_come_back_to_the_composer() {
        let host = Arc::new(Recording::default());
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Undone {
                prompt: Some("fix the parser".into()),
                restored_files: Vec::new(),
            }));
        let (app, client, all) = following(&host);
        assert_eq!(
            all.dispatch("/undo", &app.context()).await,
            Outcome::Do(Action::Paste("fix the parser".into()))
        );
        let _ = all.dispatch("/undo 3", &app.context()).await;
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::Undo {
                    session: "lead".into(),
                    turn: None,
                    based_on: 7,
                },
                HostCommand::Undo {
                    session: "lead".into(),
                    turn: Some(3),
                    based_on: 7,
                },
            ]
        );

        // With a member on screen, the lead's conversation is not what is shown.
        let _ = client.look_at("lead/scout");
        assert!(matches!(
            all.dispatch("/undo", &app.context()).await,
            Outcome::Refused(_)
        ));
        assert_eq!(
            host.asked.lock().unwrap().len(),
            2,
            "nothing more was asked"
        );
    }

    /// `/rewind` with nothing after it asks for the panel — the same thing a
    /// double-tap on Esc asks for, so there is one rewind on screen rather than
    /// two. With a turn and a scope it goes back without opening anything.
    #[tokio::test]
    async fn rewind_opens_the_panel_and_goes_back_with_a_scope() {
        let host = Arc::new(Recording::default());
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Undone {
                prompt: None,
                restored_files: vec!["src/a.rs".into()],
            }));
        let (app, _client, all) = following(&host);
        assert_eq!(
            all.dispatch("/rewind", &app.context()).await,
            Outcome::Do(Action::ToggleRewind),
            "无参的 /rewind 不问宿主,它要的是那块面板"
        );
        assert!(
            host.asked.lock().unwrap().is_empty(),
            "而且一趟往返都没发出去"
        );
        assert_eq!(
            all.dispatch("/rewind 2 代码", &app.context()).await,
            Outcome::Said("已还原 1 个文件".into())
        );
        assert_eq!(
            host.asked.lock().unwrap().last(),
            Some(&HostCommand::Rewind {
                session: "lead".into(),
                turn: 2,
                scope: atomcode_kernel::session::RewindScope::Code,
                based_on: 7,
            })
        );
    }

    /// The rest of host control over a session is a command each, for the
    /// session this screen follows even while a member is on screen.
    #[tokio::test]
    async fn model_mcp_reload_and_signing_in_and_out_are_asked_of_the_host() {
        let host = Arc::new(Recording::default());
        let (app, client, all) = following(&host);
        host.replies.lock().unwrap().extend([Ok(HostReply::Done)]);
        let _ = client.look_at("lead/scout");
        for line in [
            "/model glm-5",
            "/mcp withdraw",
            "/reload",
            "/logout",
            "/login",
        ] {
            assert!(
                !matches!(
                    all.dispatch(line, &app.context()).await,
                    Outcome::Refused(_)
                ),
                "{line}"
            );
        }
        let lead = || "lead".to_string();
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::SwitchModel {
                    session: lead(),
                    model: "glm-5".into(),
                },
                HostCommand::WithdrawMcpTools { session: lead() },
                HostCommand::Reload { session: lead() },
                HostCommand::SignOut { session: lead() },
                HostCommand::SignIn { session: lead() },
            ]
        );
    }

    /// `/mcp` with nothing after it raises the panel — it prints no list of
    /// servers, and it asks the host for nothing: the directory arrives over the
    /// panel's own port.
    ///
    /// The two states that list used to spell out are `McpState::about()`'s words,
    /// and the drawing layer's tests pin them where they are drawn. The half that
    /// says the panel really is up lives in `plugin.rs` — a command holds no host,
    /// so it cannot raise one itself.
    #[tokio::test]
    async fn mcp_with_no_argument_asks_for_the_panel() {
        let host = Arc::new(Recording::default());
        let (app, _client, all) = following(&host);
        assert!(
            matches!(
                all.dispatch("/mcp", &app.context()).await,
                Outcome::Do(Action::ToggleMcp)
            ),
            "/mcp with no argument routes to the panel"
        );
        assert!(
            host.asked.lock().unwrap().is_empty(),
            "opening the panel asks the host for nothing"
        );
    }

    /// `/model` with no argument opens the providers panel on its model list —
    /// one surface for switching and editing — instead of a models-only popup.
    /// It asks the host for nothing: opening a panel is screen state.
    #[tokio::test]
    async fn bare_model_opens_the_providers_model_list() {
        let host = Arc::new(Recording::default());
        let (app, _client, all) = following(&host);
        assert!(
            matches!(
                all.dispatch("/model", &app.context()).await,
                Outcome::Do(Action::OpenModels)
            ),
            "/model with no arg routes to the providers model list"
        );
        assert!(
            host.asked.lock().unwrap().is_empty(),
            "opening the panel asks the host for nothing"
        );
    }

    /// `/context` says both numbers, because they answer different questions.
    ///
    /// What the screen can count is what it was shown. The budget is the host's
    /// — it packs a system prompt, instructions and tool definitions that never
    /// reach a front end — so a `/context` that only counted would be reporting
    /// the smaller half of the answer and calling it the answer.
    #[tokio::test]
    async fn context_says_what_is_in_the_conversation_and_how_much_room_is_left() {
        let host = Arc::new(Recording::default());
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Context {
                window: 200_000,
                used: 50_000,
                model: "glm-5".into(),
                working_dir: "/w".into(),
            }));
        let (app, _client, all) = following(&host);

        match all.dispatch("/context", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("条事实"), "the counted half: {text}");
                assert!(text.contains("50000 / 200000"), "the budget half: {text}");
                assert!(text.contains("25%"), "and how full that is: {text}");
                assert!(
                    text.contains("glm-5"),
                    "a window belongs to a model: {text}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![HostCommand::Context {
                session: "lead".into()
            }]
        );
    }

    /// A host that cannot say what the window is says nothing rather than
    /// `0 / 0`, and the counted half still reaches the person.
    #[tokio::test]
    async fn context_without_a_known_window_still_says_what_it_knows() {
        let host = Arc::new(Recording::default());
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Context {
                window: 0,
                used: 0,
                model: String::new(),
                working_dir: "/w".into(),
            }));
        let (app, _client, all) = following(&host);
        match all.dispatch("/context", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("条事实"), "{text}");
                assert!(
                    !text.contains("0 / 0"),
                    "a window nobody knows is not a number: {text}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// `/usage` answers what `/cost` cannot: not what this conversation spent,
    /// but what the account may still do and when a spent window comes back.
    ///
    /// The distinction is the point of the command — a person can be cheap this
    /// session and still be locked out — so the criterion checks the exhausted
    /// window says *when*, which is the only part they can act on.
    #[tokio::test]
    async fn usage_says_what_is_left_and_when_a_spent_window_comes_back() {
        let host = Arc::new(Recording::default());
        host.replies.lock().unwrap().extend([
            Ok(HostReply::Usage {
                plan: None,
                stats: None,
                windows: vec![
                    atomcode_host_api::UsageWindow {
                        label: "5 小时".into(),
                        exhausted: true,
                        resets_at: "14:30".into(),
                        resets_in_seconds: 3600,
                        call_limit: Some(1000),
                        window_seconds: 0,
                        used_percent: None,
                        calls_used: None,
                    },
                    atomcode_host_api::UsageWindow {
                        label: "每周".into(),
                        exhausted: false,
                        resets_at: String::new(),
                        resets_in_seconds: 0,
                        call_limit: None,
                        window_seconds: 0,
                        used_percent: None,
                        calls_used: None,
                    },
                ],
            }),
            Ok(HostReply::Usage {
                plan: None,
                stats: None,
                windows: Vec::new(),
            }),
        ]);
        let (app, _client, all) = following(&host);

        match all.dispatch("/usage", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("5 小时") && text.contains("用完了"), "{text}");
                // When it comes back is the only actionable part.
                assert!(text.contains("14:30"), "{text}");
                assert!(text.contains("1000"), "{text}");
                assert!(text.contains("每周") && text.contains("还有"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        // A host that meters nothing says so. Not a refusal: there is nothing
        // wrong, there is just no meter.
        match all.dispatch("/usage", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("不计额度"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::Usage {
                    session: "lead".into(),
                    windows_only: false,
                },
                HostCommand::Usage {
                    session: "lead".into(),
                    windows_only: false,
                },
            ]
        );
    }

    /// `/cd` browses. Before this it took a path a person had to already know,
    /// and tuix had a picker for exactly that reason (`modals/dir_picker.rs`).
    ///
    /// What the picks are is the point: stepping in is `/cd <path>/` and
    /// staying is `/cd <path>` — this same command — so the browser cannot
    /// drift away from the typed form, because it is the typed form.
    #[tokio::test]
    async fn cd_browses_rather_than_demanding_a_path_already_known() {
        let host = Arc::new(Recording::default());
        let (app, _client, all) = following(&host);
        let dir = tempfile::tempdir().expect("tempdir");
        // Names that a random temp path cannot accidentally contain, so the
        // assertions below are about the listing and not about luck.
        std::fs::create_dir_all(dir.path().join("src-alpha")).expect("dir");
        std::fs::create_dir_all(dir.path().join("docs-beta")).expect("dir");
        std::fs::create_dir_all(dir.path().join(".hidden-gamma")).expect("dir");
        std::fs::write(dir.path().join("alpha-file.txt"), "x").expect("file");

        // The trailing slash is "browse from here", which is what picking a row
        // sends back in.
        let at = format!("/cd {}/", dir.path().display());
        let picker = match all.dispatch(&at, &app.context()).await {
            Outcome::Open(picker) => picker,
            other => panic!("{other:?}"),
        };
        assert_eq!(picker.id(), "cd");
        let text = picker
            .render(&crate::moment::Viewport::new(
                crate::frame::Rect::sized(80, 20),
                &crate::moment::Moment::default(),
            ))
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("src-alpha"), "{text}");
        assert!(text.contains("docs-beta"), "{text}");
        // A browser you cannot back out of is a trap.
        assert!(text.contains("上一层"), "{text}");
        // And one you cannot stop in is useless: stepping in has to be able to
        // end somewhere.
        assert!(text.contains("就在这儿干活"), "{text}");
        assert!(
            !text.contains("alpha-file"),
            "files are not directories: {text}"
        );
        assert!(
            !text.contains("hidden-gamma"),
            "dot directories stay out: {text}"
        );
        // Nothing was asked of the host: browsing is looking, not moving.
        assert!(
            host.asked.lock().unwrap().is_empty(),
            "looking at a directory must not move the session into it"
        );

        // And the way out. The "stay here" row sends `/cd <from>` with no
        // trailing slash, which is this — so a browser that could be stepped
        // into but never out of would fail here. It did: `from` kept the slash
        // it was browsed with, and the row read as "browse here" again.
        let stay = format!("/cd {}", dir.path().display());
        match all.dispatch(&stay, &app.context()).await {
            Outcome::Said(_) => {}
            other => panic!("picking a directory must move into it, not reopen: {other:?}"),
        }
        assert!(matches!(
            host.asked.lock().unwrap().first(),
            Some(HostCommand::ChangeDirectory { .. })
        ));
    }

    /// The words people type for a mode reach the one mode switch — they are
    /// not a second implementation that would drift from it.
    #[tokio::test]
    async fn plan_build_and_auto_are_the_one_mode_switch_under_other_names() {
        let host = Arc::new(Recording::default());
        let (app, _client, all) = following(&host);
        for (typed, wanted) in [
            ("/plan", atomcode_host_api::Mode::Plan),
            ("/build", atomcode_host_api::Mode::Ask),
            ("/auto", atomcode_host_api::Mode::Auto),
        ] {
            let _ = all.dispatch(typed, &app.context()).await;
            assert_eq!(
                host.asked.lock().unwrap().last(),
                Some(&HostCommand::SetMode {
                    session: "lead".into(),
                    mode: wanted,
                }),
                "{typed}"
            );
        }
        // And `/mode plan` still reaches the same place, so the two doors agree.
        let _ = all.dispatch("/mode plan", &app.context()).await;
        assert_eq!(
            host.asked.lock().unwrap().last(),
            Some(&HostCommand::SetMode {
                session: "lead".into(),
                mode: atomcode_host_api::Mode::Plan,
            })
        );
    }

    /// The two tables over the four modes agree, and both spellings of the one
    /// mode mean it.
    ///
    /// The cycle key builds a `/mode <word>` line from [`mode_word`], and the
    /// command reads it back with [`mode_named`]. A pair that disagreed — a word
    /// the command does not take, or a word that named a different mode — is the
    /// drift this pins, and it would show up as a key that says "no such mode"
    /// rather than as a wrong mode.
    #[test]
    fn every_mode_has_one_word_both_ways_and_the_cycle_visits_them_all() {
        use atomcode_host_api::Mode;
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits, Mode::Auto] {
            let word = mode_word(mode);
            assert_eq!(mode_named(word), Some(mode), "`{word}` does not round-trip");
        }
        // The long spelling is the one mode, not a fifth.
        assert_eq!(mode_named("accept-edits"), Some(Mode::AcceptEdits));
        assert_eq!(mode_named("nonsense"), None);

        // Four steps from anywhere and the cycle is back where it started,
        // visiting each mode once — the property that makes the key usable
        // without looking.
        let mut seen = Vec::new();
        let mut mode = Mode::Ask;
        for _ in 0..4 {
            seen.push(mode);
            mode = mode.next();
        }
        assert_eq!(mode, Mode::Ask, "the cycle does not close");
        seen.sort_by_key(|m| mode_word(*m));
        seen.dedup();
        assert_eq!(seen.len(), 4, "the cycle skips a mode: {seen:?}");
    }

    /// `/autonomy` says whether the session is driving itself, and how far it
    /// has got — the thing the runtime publishes every round to a stream this
    /// screen is not on.
    #[tokio::test]
    async fn autonomy_says_what_the_session_is_doing_on_its_own() {
        let host = Arc::new(Recording::default());
        host.replies.lock().unwrap().extend([
            Ok(HostReply::Autonomy {
                running: Some(atomcode_host_api::Running {
                    kind: "goal".into(),
                    what: "测试全过".into(),
                    round: 3,
                    of: Some(20),
                    elapsed_secs: 252,
                    paused: None,
                }),
            }),
            Ok(HostReply::Autonomy {
                running: Some(atomcode_host_api::Running {
                    kind: "loop".into(),
                    what: "再看一遍".into(),
                    round: 9,
                    of: None,
                    elapsed_secs: 40,
                    paused: Some("PausedAtCap".into()),
                }),
            }),
            Ok(HostReply::Autonomy { running: None }),
        ]);
        let (app, _client, all) = following(&host);

        match all.dispatch("/autonomy", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("测试全过"), "{text}");
                assert!(text.contains("3/20"), "with a cap it says the cap: {text}");
                assert!(text.contains("4 分 12 秒"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        match all.dispatch("/autonomy", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("循环") && text.contains("第 9 轮"), "{text}");
                assert!(!text.contains('/'), "no cap, no slash: {text}");
                assert!(text.contains("停着"), "a paused one says so: {text}");
            }
            other => panic!("{other:?}"),
        }
        // Idle is said, not refused.
        match all.dispatch("/autonomy", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("没有在自己干"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(host.asked.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn language_reads_and_writes_the_one_setting_it_names() {
        let host = Arc::new(Recording::default());
        let language = || atomcode_host_api::Setting {
            id: "language".into(),
            label: "语言".into(),
            value: "zh".into(),
            accepts: "zh | en".into(),
            applies: "下一回合".into(),
        };
        // Two readings, then the write, then a host that has no such setting.
        host.replies.lock().unwrap().extend([
            Ok(HostReply::Settings {
                settings: vec![language()],
            }),
            Ok(HostReply::Settings {
                settings: vec![language()],
            }),
            Ok(HostReply::Done),
            Ok(HostReply::Settings {
                settings: Vec::new(),
            }),
        ]);
        let (app, _client, all) = following(&host);

        // With nothing after it, `/language` says what it is and what it
        // takes. It used to open `/config`'s value picker; `/config` is the
        // screen's settings panel now, and a session command cannot open a
        // screen panel for one row.
        match all.dispatch("/language", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("zh") && text.contains("zh | en"), "{text}")
            }
            other => panic!("{other:?}"),
        }
        match all.dispatch("/language en", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("en"), "{text}"),
            other => panic!("{other:?}"),
        }
        // A host with no such setting says so rather than pretending.
        assert!(matches!(
            all.dispatch("/language en", &app.context()).await,
            Outcome::Refused(_)
        ));

        let lead = || "lead".to_string();
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::Settings { session: lead() },
                HostCommand::Settings { session: lead() },
                HostCommand::SetSetting {
                    session: lead(),
                    id: "language".into(),
                    value: "en".into(),
                },
                HostCommand::Settings { session: lead() },
            ],
            "the named door reads the setting, then writes it"
        );
    }

    /// `/diff` answers the most-asked question of a coding session at two
    /// depths: which files, then what changed in one.
    ///
    /// "Cannot tell" and "nothing changed" are different answers — one is a
    /// session with no workspace snapshots, the other a session that has not
    /// touched anything — and a screen that said the same for both would send
    /// somebody looking for a bug that is not there.
    #[tokio::test]
    async fn diff_lists_what_changed_and_then_shows_one_of_them() {
        let host = Arc::new(Recording::default());
        host.replies.lock().unwrap().extend([
            Ok(HostReply::Changes {
                files: vec![
                    atomcode_host_api::ChangedFile {
                        path: "src/parser.rs".into(),
                        added: 12,
                        removed: 3,
                        binary: false,
                    },
                    atomcode_host_api::ChangedFile {
                        path: "logo.png".into(),
                        added: 0,
                        removed: 0,
                        binary: true,
                    },
                ],
                diff: None,
                unavailable: None,
            }),
            Ok(HostReply::Changes {
                files: Vec::new(),
                diff: Some("@@ -1 +1 @@\n-a\n+b\n".into()),
                unavailable: None,
            }),
            Ok(HostReply::Changes {
                files: Vec::new(),
                diff: None,
                unavailable: None,
            }),
            Ok(HostReply::Changes {
                files: Vec::new(),
                diff: None,
                unavailable: Some("这个会话不做工作区快照".into()),
            }),
        ]);
        let (app, _client, all) = following(&host);

        match all.dispatch("/diff", &app.context()).await {
            Outcome::Open(picker) => assert_eq!(picker.id(), "diff"),
            other => panic!("{other:?}"),
        }
        match all.dispatch("/diff src/parser.rs", &app.context()).await {
            Outcome::Open(reader) => assert_eq!(reader.id(), "view"),
            other => panic!("{other:?}"),
        }
        // Changed nothing: said, not refused.
        match all.dispatch("/diff", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("还没有改过"), "{text}"),
            other => panic!("{other:?}"),
        }
        // Cannot tell: refused, with the host's own reason.
        match all.dispatch("/diff", &app.context()).await {
            Outcome::Refused(why) => assert!(why.contains("工作区快照"), "{why}"),
            other => panic!("{other:?}"),
        }

        let lead = || "lead".to_string();
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::Changes {
                    session: lead(),
                    file: None,
                },
                HostCommand::Changes {
                    session: lead(),
                    file: Some("src/parser.rs".into()),
                },
                HostCommand::Changes {
                    session: lead(),
                    file: None,
                },
                HostCommand::Changes {
                    session: lead(),
                    file: None,
                },
            ]
        );
    }

    /// Who is signed in, and the other thinking knob.
    ///
    /// `/think` is not `/effort`: one says whether the model thinks at all, the
    /// other how hard. Both are asked of the host against the session this
    /// screen follows.
    #[tokio::test]
    async fn who_is_signed_in_and_whether_the_model_thinks_at_all() {
        let host = Arc::new(Recording::default());
        host.replies.lock().unwrap().extend([
            Ok(HostReply::Identity {
                signed_in: true,
                who: Some("lichao".into()),
                detail: Some("li@example.com".into()),
            }),
            Ok(HostReply::Identity {
                signed_in: false,
                who: None,
                detail: None,
            }),
            Ok(HostReply::Settings {
                settings: vec![atomcode_host_api::Setting {
                    id: "thinking".into(),
                    label: "思考".into(),
                    value: "off".into(),
                    accepts: "on | off".into(),
                    applies: "下一回合".into(),
                }],
            }),
        ]);
        let (app, _client, all) = following(&host);
        assert_eq!(
            all.dispatch("/whoami", &app.context()).await,
            Outcome::Said("lichao · li@example.com".into())
        );
        // Nobody signed in is an answer, not a refusal.
        match all.dispatch("/whoami", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("没有人登录"), "{text}"),
            other => panic!("{other:?}"),
        }
        match all.dispatch("/think", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("off"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            all.dispatch("/think on", &app.context()).await,
            Outcome::Said("思考:on".into())
        );
        assert!(matches!(
            all.dispatch("/think 一点点", &app.context()).await,
            Outcome::Refused(_)
        ));
        let lead = || "lead".to_string();
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::WhoAmI { session: lead() },
                HostCommand::WhoAmI { session: lead() },
                HostCommand::Thinking { session: lead() },
                HostCommand::SetThinking {
                    session: lead(),
                    on: true,
                },
            ],
            "the refused one asked nothing"
        );
    }

    /// A screen following a session the model has answered in, with `answer` as
    /// its last reply.
    fn answered(answer: &str) -> (App, Arc<Commands>, Arc<crate::surface::Headless>) {
        let app = bare();
        let client = Arc::new(crate::plugin::AgentClient::default());
        let (commands, _agent) = tokio::sync::mpsc::unbounded_channel();
        client.connect(commands, Arc::new(Recording::default()));
        client.follow("lead");
        for (seq, event) in [
            SessionEvent::UserMessage {
                turn: 1,
                text: "写个 hello".into(),
                images: Vec::new(),
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: answer.into(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            client.keep(&atomcode_kernel::session::Committed {
                session: "lead".into(),
                seq: seq as u64 + 1,
                at: 0,
                event,
            });
        }
        let surface = crate::surface::Headless::new(80, 24);
        let ctx = app.context();
        let _ = ctx.provide::<crate::plugin::AgentClientSvc>(client);
        let _ = ctx.provide::<crate::plugin::SurfaceSvc>(surface.clone());
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(TakeAwayCommands));
        (app, all, surface)
    }

    /// `/copy` takes the code out of the last answer and nothing else — not the
    /// prose around it, not the fences. With more than one block it asks which,
    /// rather than guessing.
    #[tokio::test]
    async fn copy_takes_the_code_out_of_the_last_answer() {
        let (app, all, surface) =
            answered("这样写:\n\n```rust\nfn main() {}\n```\n\n或者:\n\n```sh\necho hi\n```\n");
        match all.dispatch("/copy", &app.context()).await {
            Outcome::Refused(why) => assert!(why.contains("2"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(surface.clipboard_text(), None, "nothing was copied yet");
        let _ = all.dispatch("/copy 2", &app.context()).await;
        assert_eq!(surface.clipboard_text(), Some("echo hi".into()));
        let _ = all.dispatch("/copy all", &app.context()).await;
        assert_eq!(
            surface.clipboard_text(),
            Some("fn main() {}\n\necho hi".into())
        );
        assert!(matches!(
            all.dispatch("/copy 9", &app.context()).await,
            Outcome::Refused(_)
        ));

        // An answer with no code in it says so rather than copying the prose.
        let (app, all, surface) = answered("没有代码,就这么说说");
        assert!(matches!(
            all.dispatch("/copy", &app.context()).await,
            Outcome::Refused(_)
        ));
        assert_eq!(surface.clipboard_text(), None);
    }

    /// `/view` opens the file beside the code, without sending anything.
    #[tokio::test]
    async fn view_opens_a_file_without_putting_it_in_the_conversation() {
        let (app, all, _surface) = answered("好了");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write");
        match all
            .dispatch(&format!("/view {}", file.display()), &app.context())
            .await
        {
            Outcome::Open(overlay) => assert_eq!(overlay.id(), "view"),
            other => panic!("{other:?}"),
        }
        // Nothing was said to the model and nothing was written.
        assert!(matches!(
            all.dispatch("/view", &app.context()).await,
            Outcome::Refused(_)
        ));
        assert!(matches!(
            all.dispatch("/view /nowhere/at/all", &app.context()).await,
            Outcome::Refused(_)
        ));
    }

    /// A file too long to show is shown as far as it goes — **and the title
    /// says so**. A viewer that silently stops at line 1000 is a viewer that
    /// tells you the file ends there.
    #[tokio::test]
    async fn view_clips_a_long_file_and_the_title_says_how_far_it_got() {
        let (app, all, _surface) = answered("好了");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("long.log");
        let body: String = (0..VIEW_MAX_LINES + 500)
            .map(|i| format!("line {i}\n"))
            .collect();
        std::fs::write(&file, body).expect("write");
        match all
            .dispatch(&format!("/view {}", file.display()), &app.context())
            .await
        {
            Outcome::Open(overlay) => {
                let title = overlay.title();
                assert!(
                    title.contains(&VIEW_MAX_LINES.to_string()),
                    "the title must say how much is missing: {title}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// `/view ~/notes.md` means the file in the home directory — the path a
    /// person types is the path this screen printed at them.
    ///
    /// Judged here rather than through a dispatch, because a dispatch would
    /// have to own the machine's `HOME` to have an opinion.
    #[test]
    fn view_resolves_a_typed_tilde_before_deciding_it_is_relative() {
        let home = std::path::Path::new("/home/me");
        assert_eq!(
            view_path("~/notes.md", "/work/proj", Some(home)),
            std::path::PathBuf::from("/home/me/notes.md"),
            "an unexpanded ~ is relative, and would land under the working dir"
        );
        // The two paths that were already right stay right.
        assert_eq!(
            view_path("/etc/hosts", "/work/proj", Some(home)),
            std::path::PathBuf::from("/etc/hosts")
        );
        assert_eq!(
            view_path("src/main.rs", "/work/proj", Some(home)),
            std::path::PathBuf::from("/work/proj/src/main.rs")
        );
    }

    /// The three caps, each judged against a file of a few bytes rather than
    /// one of eight megabytes — which is what `view_file_within` is for.
    #[test]
    fn each_cap_leaves_its_own_mark() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Lines: two kept of four, and it admits there were more.
        let lines = dir.path().join("lines.txt");
        std::fs::write(&lines, "a\nb\nc\nd\n").expect("write");
        let seen = view_file_within(&lines, 1024, 2, 100)
            .expect("read")
            .expect("text");
        assert_eq!(seen.body, "a\nb\n");
        assert!(seen.at_line_cap, "it stopped early and must say so");
        assert!(!seen.at_byte_cap);

        // Bytes: the read stops, and that is a different notice from the line
        // cap because the count of what is missing is unknown, not merely large.
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "0123456789abcdef").expect("write");
        let seen = view_file_within(&big, 8, 100, 100)
            .expect("read")
            .expect("text");
        assert_eq!(seen.body, "01234567\n");
        assert!(seen.at_byte_cap);

        // Columns: the long line is cut, kept, and counted — one line of a
        // minified bundle must not become the whole screen.
        let wide = dir.path().join("wide.js");
        std::fs::write(&wide, "short\n".to_string() + &"x".repeat(50)).expect("write");
        let seen = view_file_within(&wide, 1024, 100, 10)
            .expect("read")
            .expect("text");
        assert_eq!(seen.long_lines, 1, "the cut lines are counted");
        assert_eq!(
            seen.body.lines().last().map(str::len),
            Some(10),
            "cut to the cap, not dropped"
        );
    }

    /// A binary is refused by name, not drawn. Opening one in a text viewer
    /// fills the screen with nothing and whatever escapes happened to be in it.
    #[tokio::test]
    async fn view_refuses_a_file_that_is_not_text() {
        let (app, all, _surface) = answered("好了");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.png");
        std::fs::write(&file, [0x89, b'P', b'N', b'G', 0x00, 0x1a, 0x0a]).expect("write");
        match all
            .dispatch(&format!("/view {}", file.display()), &app.context())
            .await
        {
            Outcome::Refused(_) => {}
            other => panic!("a binary must not be drawn: {other:?}"),
        }
    }

    /// `/save` writes the conversation as markdown, beside the code the session
    /// is working on.
    #[tokio::test]
    async fn save_writes_the_conversation_as_markdown() {
        let (app, all, _surface) = answered("写好了");
        let dir = tempfile::tempdir().expect("tempdir");
        let into = dir.path().join("聊天.md");
        match all
            .dispatch(&format!("/save {}", into.display()), &app.context())
            .await
        {
            Outcome::Said(text) => assert!(text.contains("聊天.md"), "{text}"),
            other => panic!("{other:?}"),
        }
        let written = std::fs::read_to_string(&into).expect("written");
        assert!(
            written.contains("## 我") && written.contains("写个 hello"),
            "{written}"
        );
        assert!(
            written.contains("## 模型") && written.contains("写好了"),
            "{written}"
        );
    }

    /// `/save` does not write over a file it did not write.
    ///
    /// The failure this rules out is losing work to a typo: `/save Cargo.toml`
    /// replaces a source file with a transcript, silently, and the only notice
    /// is the success line. A `.md` target is a previous save being replaced,
    /// which is what saving again means.
    #[tokio::test]
    async fn save_refuses_to_overwrite_a_file_it_did_not_write() {
        let (app, all, _surface) = answered("写好了");
        let dir = tempfile::tempdir().expect("tempdir");

        let source = dir.path().join("Cargo.toml");
        std::fs::write(&source, "[package]\nname = \"mine\"\n").expect("write");
        match all
            .dispatch(&format!("/save {}", source.display()), &app.context())
            .await
        {
            Outcome::Refused(_) => {}
            other => panic!("a source file must survive a typo: {other:?}"),
        }
        assert!(
            std::fs::read_to_string(&source)
                .expect("still there")
                .contains("name = \"mine\""),
            "and it is untouched"
        );

        // Saving again over a previous save is the ordinary case and goes
        // through — refusing that would make the command usable once.
        let again = dir.path().join("notes.md");
        std::fs::write(&again, "old").expect("write");
        match all
            .dispatch(&format!("/save {}", again.display()), &app.context())
            .await
        {
            Outcome::Said(_) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            !std::fs::read_to_string(&again)
                .expect("read")
                .contains("old"),
            "the previous save was replaced"
        );

        // A path that is not there yet is written, whatever its extension.
        let fresh = dir.path().join("fresh.txt");
        match all
            .dispatch(&format!("/save {}", fresh.display()), &app.context())
            .await
        {
            Outcome::Said(_) => {}
            other => panic!("{other:?}"),
        }
        assert!(fresh.exists());
    }

    /// `/copy msg` takes the whole reply, not just the code in it.
    ///
    /// The other half of what people do with an answer: a block is for
    /// running, the message is for pasting into an issue or a review — and
    /// that is exactly when dragging across a wrapped terminal picks up
    /// gutters and fold marks.
    #[tokio::test]
    async fn copy_msg_takes_the_whole_reply_prose_and_all() {
        let (app, all, surface) = answered("先说一句,然后:\n\n```rs\nfn main() {}\n```\n");
        match all.dispatch("/copy msg", &app.context()).await {
            Outcome::Said(_) => {}
            other => panic!("{other:?}"),
        }
        let copied = surface.clipboard_text().expect("something was copied");
        assert!(copied.contains("先说一句"), "the prose is in it: {copied}");
        assert!(copied.contains("fn main"), "and the code: {copied}");

        // And the block form still copies only the block, or the two would be
        // one command with a confusing argument.
        match all.dispatch("/copy", &app.context()).await {
            Outcome::Said(_) => {}
            other => panic!("{other:?}"),
        }
        let block = surface.clipboard_text().expect("copied");
        assert!(!block.contains("先说一句"), "{block}");
    }

    /// `/paste` is the typed way in to what ctrl-v does, for the terminals and
    /// the platforms where ctrl-v never arrives. With a path it does not need a
    /// clipboard at all.
    #[tokio::test]
    async fn paste_reaches_the_composer_from_the_clipboard_or_from_a_file() {
        let app = bare();
        let surface = crate::surface::Headless::new(80, 24);
        let _ = app
            .context()
            .provide::<crate::plugin::SurfaceSvc>(surface.clone());
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(ScreenCommands));

        // Nothing in it is an answer, not a failure — and it says what else to
        // try.
        match all.dispatch("/paste", &app.context()).await {
            Outcome::Refused(why) => assert!(why.contains("路径"), "{why}"),
            other => panic!("{other:?}"),
        }

        {
            use crate::surface::Surface as _;
            surface.copy("从剪贴板来的");
        }
        assert_eq!(
            all.dispatch("/paste", &app.context()).await,
            Outcome::Do(Action::Paste("从剪贴板来的".into()))
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "从文件来的").expect("write");
        assert_eq!(
            all.dispatch(&format!("/paste {}", file.display()), &app.context())
                .await,
            Outcome::Do(Action::Paste("从文件来的".into()))
        );
        assert!(matches!(
            all.dispatch("/paste /nowhere/at/all", &app.context()).await,
            Outcome::Refused(_)
        ));
    }

    use atomcode_plexus::{App, ConfigTree, PluginRegistry};

    fn bare() -> App {
        App::new(PluginRegistry::new(), ConfigTree::default())
    }

    /// The five shipped sets, assembled directly.
    ///
    /// Whether these are the sets a real screen gets is not this test's job any
    /// more — `crate::rows::SCREEN` decides that, and `rows`' own tests check
    /// that every row it names exists. What is tested here is the property that
    /// survives either way: the shipped sets do not collide, and `/help`
    /// renders them.
    fn builtin_for_test() -> Arc<Commands> {
        let c = Arc::new(Commands::new());
        let _ = c.add(Arc::new(ScreenCommands));
        let _ = c.add(Arc::new(SessionCommands));
        let _ = c.add(Arc::new(TakeAwayCommands));
        let _ = c.add(Arc::new(HelpCommands { all: c.clone() }));
        c
    }

    #[test]
    fn the_shipped_set_mounts_without_conflicting_with_itself() {
        let c = builtin_for_test();
        let names: Vec<_> = c.all().iter().map(|x| x.name.to_string()).collect();
        assert!(["help", "compact", "effort"]
            .iter()
            .all(|n| names.contains(&n.to_string())));
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicates: {names:?}");
    }

    #[test]
    fn effort_offers_its_levels_inline_not_a_modal() {
        // The levels are a closed set, so the slash menu expands `/effort` into
        // one row per level (the way `/` shows commands) instead of a modal. The
        // registry entry carries them as options — every level plus `default` —
        // and no `takes` sentence, which is what makes the menu expand rather
        // than complete-with-a-space.
        let c = builtin_for_test();
        let effort = c.find("effort").expect("effort is a command");
        assert!(
            effort.takes.is_none(),
            "no arg sentence: the menu expands the levels instead"
        );
        let values: Vec<&str> = effort.options.iter().map(|o| o.value.as_ref()).collect();
        for level in atomcode_harness::REASONING_EFFORT_LEVELS {
            assert!(values.contains(&level), "offers `{level}`: {values:?}");
        }
        assert_eq!(
            values.last(),
            Some(&"default"),
            "`default` is the last row: {values:?}"
        );
    }

    #[tokio::test]
    async fn help_lists_everything_including_itself() {
        let c = builtin_for_test();
        let app = bare();
        match c.dispatch("/help", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("/help"));
                assert!(
                    text.contains("/resume [会话 id]"),
                    "argument hints show:\n{text}"
                );
                assert_eq!(text.lines().count(), c.all().len());
            }
            other => panic!("{other:?}"),
        }
    }

    /// `/cd` 先给「我要去哪儿」的答案,再给「这儿有什么」:标过的目录排最前,
    /// 其次是最近在里面干过活的,最后才是当前目录底下的东西。
    #[tokio::test]
    async fn cd_offers_marked_places_then_recent_ones() {
        struct Marked;
        #[async_trait]
        impl crate::places::Places for Marked {
            async fn bookmarks(&self) -> Vec<String> {
                vec!["/w/marked".to_string()]
            }
            async fn pin(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn unpin(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let here = tempfile::tempdir().unwrap();
        std::fs::create_dir(here.path().join("under-here")).unwrap();
        let host = Arc::new(Recording::default());
        // 先问工作目录,再问会话目录。
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Context {
                window: 1,
                used: 0,
                model: "m".into(),
                working_dir: here.path().display().to_string(),
            }));
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Sessions {
                sessions: vec![
                    atomcode_host_api::StoredSession {
                        id: "one".into(),
                        title: None,
                        working_dir: Some("/w/recent".into()),
                        created_at: 0,
                        updated_at: 2,
                        turns: 1,
                        needs_newer_version: false,
                    },
                    atomcode_host_api::StoredSession {
                        id: "two".into(),
                        title: None,
                        working_dir: Some("/w/marked".into()),
                        created_at: 0,
                        updated_at: 1,
                        turns: 1,
                        needs_newer_version: false,
                    },
                ],
            }));
        let (app, _client, all) = following(&host);
        let _ = app
            .context()
            .provide::<crate::plugin::PlacesSvc>(Arc::new(Marked));
        let Outcome::Open(picker) = all.dispatch("/cd", &app.context()).await else {
            panic!("a picker");
        };
        assert_eq!(picker.id(), "cd");
        let text = picker
            .render(&crate::moment::Viewport::new(
                crate::frame::Rect::sized(80, 20),
                &crate::moment::Moment::default(),
            ))
            .iter()
            .map(|line| line.plain())
            .collect::<Vec<_>>()
            .join("\n");
        let marked = text
            .find("/w/marked")
            .unwrap_or_else(|| panic!("标过的在里面:{text}"));
        let recent = text
            .find("/w/recent")
            .unwrap_or_else(|| panic!("最近去过的在里面:{text}"));
        assert!(marked < recent, "标过的排在最近去过的前面:\n{text}");
        assert_eq!(
            text.matches("/w/marked").count(),
            1,
            "同一个地方只出现一次——它既是标过的又是最近去过的:\n{text}"
        );
        // 而底下浏览的是**宿主说的工作目录**,不是这块屏幕跟着的会话 id:
        // 裸 `/cd` 曾经拿会话 id 当路径去读,于是只会报「读不了」。
        assert!(
            text.contains("under-here"),
            "浏览的是当前工作目录底下的东西:\n{text}"
        );
    }

    /// The classic screen's name for "how do I use this" reaches the listing
    /// this screen already has, rather than a second help written from the same
    /// thirteen lines.
    #[tokio::test]
    async fn the_classic_name_for_help_reaches_it() {
        let c = builtin_for_test();
        let app = bare();
        match c.dispatch("/guide", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("/help"), "{text}"),
            other => panic!("{other:?}"),
        }
        // One row in the menu, annotated — not a second entry competing with it.
        assert_eq!(c.all().iter().filter(|c| c.name == "help").count(), 1);
        assert_eq!(
            c.find("guide").expect("the alias resolves").display_name(),
            "help (guide)"
        );
    }

    #[tokio::test]
    async fn a_command_whose_seam_is_missing_says_so_instead_of_panicking() {
        let c = builtin_for_test();
        let app = bare(); // no session, no control, no tools
        for line in ["/compact", "/context", "/clear", "/resume", "/effort high"] {
            match c.dispatch(line, &app.context()).await {
                Outcome::Refused(m) => assert!(!m.is_empty(), "{line} refused with nothing"),
                other => panic!("{line} should refuse, got {other:?}"),
            }
        }
    }

    /// `/agents` is how a person reaches a member the team strip no longer has a
    /// row for: the roster it reads keeps the stopped ones (`docs/adr/0023` §5),
    /// and picking one asks the screen to look at it rather than switching from
    /// inside the command.
    #[tokio::test]
    async fn agents_lists_stopped_members_and_picking_one_asks_the_screen() {
        let c = builtin_for_test();
        let app = bare();
        let roster = Arc::new(crate::plugin::Roster::default());
        // One member running, one stopped.
        roster.note_for_test("lead/scout", "scout", false);
        roster.note_for_test("lead/lib", "lib", true);
        let _ = app
            .context()
            .provide::<crate::plugin::TeamRosterSvc>(roster.clone());
        let client = Arc::new(crate::plugin::AgentClient::default());
        client.follow("lead");
        let _ = app
            .context()
            .provide::<crate::plugin::AgentClientSvc>(client);

        let picker = match c.dispatch("/agents", &app.context()).await {
            Outcome::Open(picker) => picker,
            other => panic!("{other:?}"),
        };
        assert_eq!(picker.id(), "agents", "the picker says what it is");

        // The lead is a row, and so is every member — the stopped one included,
        // saying that it has stopped so nobody wonders why the strip is bare.
        let moment = crate::moment::Moment::default();
        let vp = crate::moment::Viewport::new(crate::frame::Rect::sized(70, 12), &moment);
        let said = picker
            .render(&vp)
            .iter()
            .map(|line| line.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(said.contains('主'), "the lead is a row:\n{said}");
        assert!(said.contains("scout"), "the running one:\n{said}");
        assert!(
            said.contains("lib") && said.contains("已停"),
            "the stopped one is listed, and says so:\n{said}"
        );

        // And the pick is an action, not a switch done here.
        assert_eq!(
            c.dispatch("/look lead/lib", &app.context()).await,
            Outcome::Do(Action::LookAt("lead/lib".into()))
        );
        // Not listed: it is what a pick dispatches, not something anyone types.
        assert!(
            c.all().iter().all(|x| x.name != "look"),
            "`/look` is hidden"
        );
    }

    #[tokio::test]
    async fn screen_commands_become_actions_so_a_key_and_a_command_share_one_path() {
        let c = builtin_for_test();
        let app = bare();
        assert_eq!(
            c.dispatch("/quit", &app.context()).await,
            Outcome::Do(Action::Quit)
        );
        // `/exit` is an alias of `/quit`, not a second command: it resolves to the
        // same action.
        assert_eq!(
            c.dispatch("/exit", &app.context()).await,
            Outcome::Do(Action::Quit)
        );
        assert_eq!(
            c.dispatch("/reasoning", &app.context()).await,
            Outcome::Do(Action::ToggleFold("reasoning"))
        );
        // `/config` is the settings panel, and it reaches the screen the same
        // way every other screen command does: as an action, so the command and
        // any key bound to it later are one implementation.
        assert_eq!(
            c.dispatch("/config", &app.context()).await,
            Outcome::Do(Action::ToggleSettings)
        );
        // And `/provider` is the other panel, on the same terms. It used to be a
        // picker over `HostCommand::Providers` that could only switch between
        // the legacy `[providers.*]` entries — which is why an account in the
        // new schema was invisible to it (`docs/adr/0022` §3, and the panel's
        // own doc).
        assert_eq!(
            c.dispatch("/provider", &app.context()).await,
            Outcome::Do(Action::ToggleProviders)
        );
    }

    #[tokio::test]
    async fn showinject_names_one_injection_the_group_or_all_of_them() {
        let c = builtin_for_test();
        let app = bare();

        // Bare: the group, which is the gesture a person forms an opinion about.
        assert_eq!(
            c.dispatch("/showinject", &app.context()).await,
            Outcome::Do(Action::ToggleFolds(
                crate::content::ENVIRONMENTAL_INJECTIONS.to_vec()
            ))
        );
        // One, by the name that appears in the menu and by the kind that appears
        // in a fold state. Both spellings, because people name what they see.
        assert_eq!(
            c.dispatch("/showinject reminder", &app.context()).await,
            Outcome::Do(Action::ToggleFold("injected:reminder"))
        );
        assert_eq!(
            c.dispatch("/showinject injected:reminder", &app.context())
                .await,
            Outcome::Do(Action::ToggleFold("injected:reminder"))
        );
        // The injection a person may want most, since it is the one that is off
        // the screen and also the one carrying someone else's words.
        assert_eq!(
            c.dispatch("/showinject peer", &app.context()).await,
            Outcome::Do(Action::ToggleFold("injected:peer"))
        );
        assert_eq!(
            c.dispatch("/showinject all", &app.context()).await,
            Outcome::Do(Action::ToggleFolds(
                crate::content::INJECTIONS.iter().map(|(_, k)| *k).collect()
            ))
        );

        // Case is not the person's problem: the word is lower-cased before it is
        // looked up, so what arrives from a menu or a paste lands the same way.
        assert_eq!(
            c.dispatch("/showinject REMINDER", &app.context()).await,
            Outcome::Do(Action::ToggleFold("injected:reminder"))
        );

        // And a refusal names what would have worked. A silent no-op here looks
        // exactly like the injection not being there.
        match c.dispatch("/showinject nonsense", &app.context()).await {
            Outcome::Refused(why) => {
                assert!(why.contains("reminder"), "{why}");
                assert!(why.contains("all"), "{why}");
            }
            other => panic!("a name nobody has should be refused, not {other:?}"),
        }
    }
}

/// `/plugin`: the panel, and the same jobs from the command line.
///
/// A set of its own rather than a line in [`ScreenCommands`], because this one
/// command is two things: with nothing after it, it pulls the panel up — the
/// same gesture `/config` and `/provider` are — and with something after it, it
/// does the job the panel would have done, without the panel. The classic front
/// end offered both and people type both.
///
/// Everything here goes over [`crate::plugins::Plugins`]. What a marketplace is,
/// where a plugin lands on disk, what `git` has to be run — none of it is here
/// (`docs/adr/0022` §3). What *is* here is reading what a person typed: a bare
/// plugin name resolved against what the marketplaces carry, `--scope` read into
/// a [`Scope`], the four `marketplace` verbs. That is screen work — the same
/// kind `/model <id>` does — and it needs no more than the rows the port already
/// hands over.
/// `/toolbox` — the panel, and the same two switches from the command line.
///
/// Not `/tools`: that one is already how tool *output* is shown.
///
/// The panel is for looking: forty MCP tools is a list, not a name you
/// remember. The typed form is for when you already know the name, and for a
/// pattern (`/tools off mcp__github__*`) that would be a lot of ⏎ in a list.
pub struct ToolCommands;

fn tools_catalogue() -> Vec<Command> {
    vec![Command::said_taking(
        "toolbox",
        t(Msg::CmdTakesToolbox),
        t(Msg::CmdAboutToolbox),
    )]
}

#[async_trait]
impl CommandSet for ToolCommands {
    fn id(&self) -> &'static str {
        "cmd-tools"
    }
    fn commands(&self) -> Vec<Command> {
        tools_catalogue()
    }
    async fn run(&self, _name: &str, args: &str, ctx: &Context) -> Outcome {
        let args = args.trim();
        if args.is_empty() {
            return Outcome::Do(Action::ToggleTools);
        }
        let Some(port) = ctx.service::<crate::plugin::ToolCatalogSvc>() else {
            return Outcome::Refused(t(Msg::NoToolCatalog).into_owned());
        };
        let (verb, pattern) = match args.split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (args, ""),
        };
        let on = match verb {
            "on" => true,
            "off" => false,
            other => {
                return Outcome::Refused(t(Msg::ToolboxUnknownVerb { what: other }).into_owned());
            }
        };
        if pattern.is_empty() {
            return Outcome::Refused(t(Msg::ToolboxNeedsPattern { verb }).into_owned());
        }
        // What changed is read off the catalog the port answers with, not
        // guessed from what was asked: a name the config excluded does not move,
        // and saying it did would be the one lie this command could tell.
        let before = port.list().await.unwrap_or_default();
        match port.switch(pattern, on).await {
            Ok(after) => {
                let moved: Vec<String> = after
                    .tools()
                    .iter()
                    .filter(|t| {
                        before
                            .tools()
                            .iter()
                            .any(|b| b.name == t.name && b.state != t.state)
                    })
                    .map(|t| t.name.clone())
                    .collect();
                if moved.is_empty() {
                    return Outcome::Said(t(Msg::ToolboxNothingMoved { pattern }).into_owned());
                }
                let names = moved.join(&t(Msg::ToolboxNameJoiner));
                Outcome::Said(
                    match on {
                        true => t(Msg::ToolboxPutBack { names: &names }),
                        false => t(Msg::ToolboxTurnedOff { names: &names }),
                    }
                    .into_owned(),
                )
            }
            Err(why) => Outcome::Refused(why),
        }
    }
}

pub struct PluginCommands;

fn plugin_catalogue() -> Vec<Command> {
    vec![Command::said_taking(
        "plugin",
        t(Msg::CmdTakesPlugin),
        t(Msg::CmdAboutPlugin),
    )]
}

/// What `--scope` was set to, and everything that was not that.
///
/// Returns the words with the flag taken out, so the caller reads a plugin name
/// out of what is left rather than having to skip over a flag that may be
/// written three ways.
fn scope_from(args: &str) -> (crate::plugins::Scope, Vec<String>) {
    use crate::plugins::Scope;
    let mut scope = Scope::User;
    let mut rest: Vec<String> = Vec::new();
    let mut expecting = false;
    for word in args.split_whitespace() {
        if expecting {
            expecting = false;
            scope = match word.to_lowercase().as_str() {
                "project" => Scope::Project,
                "local" => Scope::Local,
                _ => Scope::User,
            };
            continue;
        }
        // Three spellings, because all three get typed: `--scope project`,
        // `--scope=project`, and the bare word after `--scope`.
        if let Some(value) = word.strip_prefix("--scope=") {
            scope = match value.to_lowercase().as_str() {
                "project" => Scope::Project,
                "local" => Scope::Local,
                _ => Scope::User,
            };
            continue;
        }
        if word == "--scope" {
            expecting = true;
            continue;
        }
        rest.push(word.to_string());
    }
    (scope, rest)
}

/// A `<名字>` or a `<名字>@<市场>`, matched against what is on offer.
///
/// `Err` is the sentence to show: nothing by that name, or several and here are
/// the commands that say which. Ambiguity is never resolved by picking one —
/// two marketplaces carrying a plugin with one name is exactly the case where
/// guessing installs the wrong thing.
fn pick<'a>(
    rows: &'a [crate::plugins::PluginRow],
    typed: &str,
    verb: &str,
) -> Result<&'a crate::plugins::PluginRow, String> {
    let (name, market) = match typed.split_once('@') {
        Some((name, market)) if !name.is_empty() && !market.is_empty() => (name, Some(market)),
        _ => (typed, None),
    };
    let hits: Vec<&crate::plugins::PluginRow> = rows
        .iter()
        .filter(|row| row.name == name && market.is_none_or(|m| row.marketplace == m))
        .collect();
    match hits.len() {
        0 => Err(t(Msg::PluginNoSuch { typed }).into_owned()),
        1 => Ok(hits[0]),
        _ => {
            let lines: Vec<String> = hits
                .iter()
                .map(|row| format!("  /plugin {verb} {}@{}", row.name, row.marketplace))
                .collect();
            Err(t(Msg::PluginAmbiguous {
                name,
                lines: &lines.join("\n"),
            })
            .into_owned())
        }
    }
}

#[async_trait]
impl CommandSet for PluginCommands {
    fn id(&self) -> &'static str {
        "cmd-plugin"
    }
    fn commands(&self) -> Vec<Command> {
        plugin_catalogue()
    }
    async fn run(&self, _name: &str, args: &str, ctx: &Context) -> Outcome {
        let args = args.trim();
        // Nothing after it is the panel. Everything below needs the port; this
        // does not, because a screen with no port still has to say so with the
        // sentence the action carries rather than one invented here.
        if args.is_empty() {
            return Outcome::Do(Action::TogglePlugins);
        }
        let Some(port) = ctx.service::<crate::plugin::PluginsSvc>() else {
            return Outcome::Refused(t(Msg::NoPluginPort).into_owned());
        };
        // Said as the job goes out, not after: a clone takes seconds, and a
        // command that printed nothing until it was over looks like a command
        // that did nothing.
        let ui = ctx.service::<atomcode_harness::seams::UiSvc>();
        let announce = |line: String| {
            if let Some(ui) = ui.as_ref() {
                ui.say(&line);
            }
        };
        let (verb, rest) = match args.split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (args, ""),
        };
        let view = port.rows();
        match verb {
            "list" => {
                let installed: Vec<String> = view
                    .plugins()
                    .iter()
                    .filter_map(|row| {
                        row.installed
                            .map(|scope| format!("  {} ({})", row.id(), scope.label()))
                    })
                    .collect();
                if installed.is_empty() {
                    return Outcome::Said(t(Msg::PluginNothingInstalled).into_owned());
                }
                Outcome::Said(
                    t(Msg::PluginInstalledList {
                        lines: &installed.join("\n"),
                    })
                    .into_owned(),
                )
            }
            "install" => {
                let (scope, rest) = scope_from(rest);
                let Some(typed) = rest.first() else {
                    return Outcome::Refused(t(Msg::PluginInstallWhich).into_owned());
                };
                let row = match pick(view.plugins(), typed, "install") {
                    Ok(row) => row,
                    Err(why) => return Outcome::Refused(why),
                };
                if row.installed.is_some() {
                    return Outcome::Refused(
                        t(Msg::PluginAlreadyInstalled { id: &row.id() }).into_owned(),
                    );
                }
                let (plugin, market) = (row.name.clone(), row.marketplace.clone());
                announce(
                    t(Msg::PluginInstalling {
                        plugin: &plugin,
                        market: &market,
                    })
                    .into_owned(),
                );
                match port.install(&plugin, &market, scope).await {
                    Ok(said) => reload_then(ctx, said).await,
                    Err(why) => Outcome::Refused(why),
                }
            }
            "uninstall" => {
                let Some(typed) = rest.split_whitespace().next() else {
                    return Outcome::Refused(t(Msg::PluginUninstallWhich).into_owned());
                };
                let installed: Vec<crate::plugins::PluginRow> = view
                    .plugins()
                    .iter()
                    .filter(|row| row.installed.is_some())
                    .cloned()
                    .collect();
                let row = match pick(&installed, typed, "uninstall") {
                    Ok(row) => row.clone(),
                    Err(_) => {
                        return Outcome::Refused(t(Msg::PluginNotInstalled { typed }).into_owned())
                    }
                };
                let scope = row.installed.unwrap_or(crate::plugins::Scope::User);
                announce(t(Msg::PluginUninstalling { id: &row.id() }).into_owned());
                match port.uninstall(&row.name, &row.marketplace, scope).await {
                    Ok(said) => reload_then(ctx, said).await,
                    Err(why) => Outcome::Refused(why),
                }
            }
            "update" => {
                let Some(typed) = rest.split_whitespace().next() else {
                    return Outcome::Refused(t(Msg::PluginUpdateWhich).into_owned());
                };
                let installed: Vec<crate::plugins::PluginRow> = view
                    .plugins()
                    .iter()
                    .filter(|row| row.installed.is_some())
                    .cloned()
                    .collect();
                let row = match pick(&installed, typed, "update") {
                    Ok(row) => row.clone(),
                    Err(_) => {
                        return Outcome::Refused(t(Msg::PluginNotInstalled { typed }).into_owned())
                    }
                };
                let scope = row.installed.unwrap_or(crate::plugins::Scope::User);
                announce(t(Msg::PluginUpdating { id: &row.id() }).into_owned());
                match port.update(&row.name, &row.marketplace, scope).await {
                    Ok(said) => reload_then(ctx, said).await,
                    Err(why) => Outcome::Refused(why),
                }
            }
            "marketplace" | "market" => {
                let (action, rest) = match rest.split_once(char::is_whitespace) {
                    Some((action, rest)) => (action, rest.trim()),
                    None => (rest, ""),
                };
                match action {
                    "list" | "" => {
                        if view.markets().is_empty() {
                            return Outcome::Said(t(Msg::MarketNoneYet).into_owned());
                        }
                        let lines: Vec<String> = view
                            .markets()
                            .iter()
                            .map(|m| {
                                t(Msg::MarketRow {
                                    name: &m.name,
                                    source: &m.source,
                                    plugins: m.plugins,
                                    installed: m.installed,
                                })
                                .into_owned()
                            })
                            .collect();
                        Outcome::Said(
                            t(Msg::MarketList {
                                lines: &lines.join("\n"),
                            })
                            .into_owned(),
                        )
                    }
                    "add" => {
                        if rest.is_empty() {
                            return Outcome::Refused(t(Msg::MarketAddWhich).into_owned());
                        }
                        announce(t(Msg::MarketFetching { what: rest }).into_owned());
                        match port.add_market(rest).await {
                            Ok(said) => reload_then(ctx, said).await,
                            Err(why) => Outcome::Refused(why),
                        }
                    }
                    "remove" | "rm" => {
                        if rest.is_empty() {
                            return Outcome::Refused(t(Msg::MarketRemoveWhich).into_owned());
                        }
                        announce(t(Msg::MarketRemoving { what: rest }).into_owned());
                        match port.remove_market(rest).await {
                            Ok(said) => reload_then(ctx, said).await,
                            Err(why) => Outcome::Refused(why),
                        }
                    }
                    "update" => {
                        if rest.is_empty() {
                            return Outcome::Refused(t(Msg::MarketUpdateWhich).into_owned());
                        }
                        announce(t(Msg::MarketUpdating { what: rest }).into_owned());
                        match port.update_market(rest).await {
                            Ok(said) => reload_then(ctx, said).await,
                            Err(why) => Outcome::Refused(why),
                        }
                    }
                    other => {
                        Outcome::Refused(t(Msg::MarketUnknownAction { what: other }).into_owned())
                    }
                }
            }
            // The same reload `/reload` is, spelled the way the classic front
            // end spelled it: people who learned `/plugin reload` there keep it.
            "reload" => match reload(ctx).await {
                Ok(()) => Outcome::Said(t(Msg::Reloaded).into_owned()),
                Err(why) => Outcome::Refused(why),
            },
            other => Outcome::Refused(t(Msg::PluginUnknownAction { what: other }).into_owned()),
        }
    }
}

/// Say what landed, then build the graph again.
///
/// Writing to disk is not making it so: a plugin brings skills, commands and
/// hooks, and none of them reach the running agent until it is reloaded. A
/// reload that fails is said *with* the success, not instead of it — the files
/// really are on disk, and a person told only about the failure would install
/// the same thing twice.
async fn reload_then(ctx: &Context, said: String) -> Outcome {
    match reload(ctx).await {
        Ok(()) => Outcome::Said(said),
        Err(why) => Outcome::Said(
            t(Msg::ReloadFailedAfter {
                said: &said,
                why: &why,
            })
            .into_owned(),
        ),
    }
}

async fn reload(ctx: &Context) -> Result<(), String> {
    let Some(client) = ctx.service::<crate::plugin::AgentClientSvc>() else {
        return Err(t(Msg::NoAgent).into_owned());
    };
    let Some(control) = client.control() else {
        return Err(t(Msg::NoHost).into_owned());
    };
    control
        .call(HostCommand::Reload {
            session: client.root(),
        })
        .await
        .map(|_| ())
        .map_err(refusal)
}

#[cfg(test)]
mod plugin_tests {
    use super::*;
    use crate::plugins::{PluginRow, Scope};

    fn row(name: &str, market: &str) -> PluginRow {
        PluginRow {
            name: name.into(),
            marketplace: market.into(),
            description: String::new(),
            installed: None,
        }
    }

    /// `--scope` is read out of the words, whichever of the three ways it was
    /// written, and what is left is the name.
    ///
    /// The spaced form is the one that broke in the classic front end: the
    /// parser stripped `--scope=` only, so `--scope project` installed into the
    /// user scope and said nothing.
    #[test]
    fn the_scope_is_read_out_of_the_words_and_never_left_in_the_name() {
        for written in [
            "tidy --scope project",
            "tidy --scope=project",
            "--scope project tidy",
        ] {
            let (scope, rest) = scope_from(written);
            assert_eq!(scope, Scope::Project, "`{written}`");
            assert_eq!(rest, ["tidy"], "`{written}` leaves only the name");
        }
        let (scope, rest) = scope_from("tidy");
        assert_eq!(scope, Scope::User, "nothing said is this machine");
        assert_eq!(rest, ["tidy"]);
        let (scope, _) = scope_from("tidy --scope local");
        assert_eq!(scope, Scope::Local);
    }

    /// A name two marketplaces carry is never guessed at.
    ///
    /// Guessing here installs the wrong thing under the right name, which is
    /// the one outcome nobody can debug afterwards.
    #[test]
    fn an_ambiguous_name_is_refused_with_the_commands_that_settle_it() {
        let rows = vec![
            row("lens", "official"),
            row("lens", "mine"),
            row("tidy", "official"),
        ];
        let Err(why) = pick(&rows, "lens", "install") else {
            panic!("two marketplaces carrying one name is not a pick");
        };
        assert!(why.contains("/plugin install lens@official"), "{why}");
        assert!(why.contains("/plugin install lens@mine"), "{why}");

        // Said in full, it resolves.
        let picked = pick(&rows, "lens@mine", "install").expect("a qualified name is unambiguous");
        assert_eq!(picked.marketplace, "mine");

        // And a name nobody carries is a refusal, not a silent no-op.
        assert!(pick(&rows, "nope", "install").is_err());

        // One carrier needs no qualifying.
        assert_eq!(
            pick(&rows, "tidy", "install")
                .expect("one carrier")
                .marketplace,
            "official"
        );
    }
}

#[cfg(test)]
mod setup_tests {
    use super::*;
    use atomcode_plexus::{App, ConfigTree, PluginRegistry};

    /// A host that keeps what it was asked, and answers `Done`.
    #[derive(Default)]
    struct Recording {
        asked: std::sync::Mutex<Vec<HostCommand>>,
    }

    #[async_trait]
    impl atomcode_host_api::HostControl for Recording {
        async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
            self.asked.lock().unwrap().push(command);
            Ok(HostReply::Done)
        }
        fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_host_api::HostEvent> {
            tokio::sync::mpsc::unbounded_channel().1
        }
    }

    /// A setup port whose answer to "installed?" is `installed`, and which counts
    /// how many times it was asked to install.
    #[derive(Default)]
    struct FakeSetup {
        installed: bool,
        installs: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl crate::setup::Setup for FakeSetup {
        fn installed(&self) -> bool {
            self.installed
        }
        async fn install(&self) -> Result<String, String> {
            *self.installs.lock().unwrap() += 1;
            Ok("✅ Setup 完成 — 1 装好, 0 跳过, 0 失败".into())
        }
    }

    struct Rig {
        app: App,
        host: Arc<Recording>,
        /// What the screen sent the agent — `Invoke` is how `/setup` is
        /// forwarded, and this is the only place that can see it.
        sent: std::sync::Mutex<
            tokio::sync::mpsc::UnboundedReceiver<atomcode_kernel::event::AgentCommand>,
        >,
        port: Arc<FakeSetup>,
        all: Arc<Commands>,
    }

    fn rig(installed: bool) -> Rig {
        let app = App::new(PluginRegistry::new(), ConfigTree::default());
        let host = Arc::new(Recording::default());
        let client = Arc::new(crate::plugin::AgentClient::default());
        let (commands, received) = tokio::sync::mpsc::unbounded_channel();
        client.connect(commands, host.clone());
        client.follow("lead");
        let _ = app
            .context()
            .provide::<crate::plugin::AgentClientSvc>(client);
        let port = Arc::new(FakeSetup {
            installed,
            installs: std::sync::Mutex::new(0),
        });
        let _ = app
            .context()
            .provide::<crate::plugin::SetupSvc>(port.clone());
        let all = Arc::new(Commands::new());
        all.add(Arc::new(SetupCommands)).unwrap();
        Rig {
            app,
            host,
            sent: std::sync::Mutex::new(received),
            port,
            all,
        }
    }

    impl Rig {
        /// Everything the screen has sent the agent so far.
        fn sent(&self) -> Vec<atomcode_kernel::event::AgentCommand> {
            let mut rx = self.sent.lock().unwrap();
            let mut out = Vec::new();
            while let Ok(command) = rx.try_recv() {
                out.push(command);
            }
            out
        }

        /// The name `/setup` handed over, if it handed one over.
        fn invoked(&self) -> Option<String> {
            self.sent().into_iter().find_map(|c| match c {
                atomcode_kernel::event::AgentCommand::Invoke { name, .. } => Some(name),
                _ => None,
            })
        }
    }

    /// On a machine that already has the seeds, `/setup` is one thing: hand the
    /// name to the agent. No unpacking, no file lock, no rebuild.
    #[tokio::test]
    async fn a_machine_that_has_the_seeds_only_forwards() {
        let rig = rig(true);
        let outcome = rig.all.dispatch("/setup", &rig.app.context()).await;
        assert!(
            matches!(outcome, Outcome::Said(_)),
            "forwarding is not a refusal: {outcome:?}"
        );
        assert_eq!(
            rig.invoked().as_deref(),
            Some("setup"),
            "the name went over"
        );
        assert_eq!(*rig.port.installs.lock().unwrap(), 0, "nothing to install");
        assert!(
            rig.host.asked.lock().unwrap().is_empty(),
            "no reload for work that did not happen: {:?}",
            rig.host.asked.lock().unwrap()
        );
    }

    /// On a machine that has never run it, the three steps happen in the order
    /// that makes them work: install, reload, forward. The reload is not
    /// decoration — handing over a name the agent does not have yet is the
    /// failure this whole command exists to avoid.
    #[tokio::test]
    async fn a_machine_without_the_seeds_installs_reloads_then_forwards() {
        let rig = rig(false);
        let outcome = rig.all.dispatch("/setup hooks", &rig.app.context()).await;
        assert!(matches!(outcome, Outcome::Said(_)), "{outcome:?}");
        assert_eq!(*rig.port.installs.lock().unwrap(), 1, "installed once");
        assert_eq!(
            *rig.host.asked.lock().unwrap(),
            vec![HostCommand::Reload {
                session: "lead".into()
            }],
            "the graph is rebuilt before the name is handed over"
        );
        // And the args the person typed travel with it: the seed skill takes a
        // focus area, and dropping it would silently answer a narrower question.
        let sent = rig.sent();
        assert!(
            sent.iter().any(|c| matches!(
                c,
                atomcode_kernel::event::AgentCommand::Invoke { name, args, .. }
                    if name == "setup" && args == "hooks"
            )),
            "the words after `/setup` are the skill's argument: {sent:?}"
        );
    }

    /// The name is claimed whether or not the agent offers it, so a machine with
    /// the seeds and one without end in the same place.
    #[test]
    fn setup_takes_the_name_from_the_agent_catalog() {
        assert_eq!(SetupCommands.overrides(), vec!["setup"]);
        assert!(SetupCommands.commands().iter().any(|c| c.name == "setup"));
    }

    /// A screen with no port says so, rather than claiming to have installed
    /// something.
    #[tokio::test]
    async fn no_port_is_a_refusal_not_a_silent_success() {
        let app = App::new(PluginRegistry::new(), ConfigTree::default());
        let client = Arc::new(crate::plugin::AgentClient::default());
        client.follow("lead");
        let _ = app
            .context()
            .provide::<crate::plugin::AgentClientSvc>(client);
        let all = Arc::new(Commands::new());
        all.add(Arc::new(SetupCommands)).unwrap();
        match all.dispatch("/setup", &app.context()).await {
            Outcome::Refused(why) => assert!(!why.is_empty(), "refused with nothing"),
            other => panic!("{other:?}"),
        }
    }
}
