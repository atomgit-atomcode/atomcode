//! Front ends.
//!
//! The launcher does not run a turn. It mounts a tree, resolves `ui`, and hands
//! over — so "one prompt and exit", "an interactive session" and "print nothing,
//! I am embedding this" are three rows, not three code paths behind two flags.
//!
//! The interactive front end is where the agent's inbox earns its keep: a reader
//! task pushes every line the user types straight into the inbox, so a message
//! typed while the agent is working joins the turn in flight instead of queueing
//! behind it.

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::Agent;
use crate::events::{AgentCreated, AgentInfo};
use crate::seams::{
    AgentLoopSvc, AgentsSvc, ControlSvc, SessionSvc, SessionTitleSvc, StopReason, UiSvc,
    UserInterface, UserQuestions, UserQuestionsSvc,
};

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

/// Create an agent and announce it, so observers attach without being told.
fn spawn_agent(ctx: &Context) -> Result<Arc<Agent>, String> {
    let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
    let agent = agents.create(ctx);
    ctx.emit::<AgentCreated>(&AgentInfo { id: agent.id() });
    Ok(agent)
}

// ---- one-shot -----------------------------------------------------------

struct OneShot;

#[async_trait]
impl UserInterface for OneShot {
    fn describe(&self) -> String {
        "one prompt, one turn, exit".into()
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let Some(prompt) = initial else {
            return Err("this front end needs a prompt".into());
        };
        let driver = ctx.require::<AgentLoopSvc>().map_err(|e| e.to_string())?;
        let agent = spawn_agent(ctx)?;
        agent.send(prompt);
        let outcome = driver.drive(&agent).await;
        match outcome.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

pub struct OneShotUiPlugin;

#[async_trait]
impl Plugin for OneShotUiPlugin {
    fn name(&self) -> &'static str {
        "ui-oneshot"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["agents", "agent-loop"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["ui"]
    }
    fn description(&self) -> &'static str {
        "run the prompt given on the command line, then exit"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UiSvc>(Arc::new(OneShot))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- quiet --------------------------------------------------------------

/// Does nothing at all. For embedding, where the caller drives agents itself
/// and a front end that read stdin would fight it for the terminal.
struct Quiet;

#[async_trait]
impl UserInterface for Quiet {
    fn describe(&self) -> String {
        "no front end; the embedder drives".into()
    }
    async fn run(&self, _ctx: &Context, _initial: Option<String>) -> Result<(), String> {
        Ok(())
    }
}

pub struct QuietUiPlugin;

#[async_trait]
impl Plugin for QuietUiPlugin {
    fn name(&self) -> &'static str {
        "ui-quiet"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["ui"]
    }
    fn description(&self) -> &'static str {
        "no interaction; for embedding"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UiSvc>(Arc::new(Quiet))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- interactive terminal -----------------------------------------------

#[derive(Debug, Deserialize)]
struct ReplRow {
    /// Printed before the first prompt.
    #[serde(default = "default_banner")]
    banner: bool,
    #[serde(default = "default_prompt")]
    prompt: String,
}

impl Default for ReplRow {
    fn default() -> Self {
        Self {
            banner: default_banner(),
            prompt: default_prompt(),
        }
    }
}

fn default_banner() -> bool {
    true
}

fn default_prompt() -> String {
    "› ".into()
}

/// A line-oriented terminal session.
///
/// One agent for the whole session, so the conversation accumulates in one log.
/// Input is read on its own task and delivered to the inbox, which is what makes
/// typing during a turn *steer* it rather than wait for it.
struct Repl {
    prompt: String,
    banner: bool,
}

impl Repl {
    fn print_prompt(&self) {
        print!("{}", self.prompt);
        let _ = std::io::stdout().flush();
    }
}

#[async_trait]
impl UserInterface for Repl {
    fn describe(&self) -> String {
        "interactive terminal session".into()
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let driver = ctx.require::<AgentLoopSvc>().map_err(|e| e.to_string())?;
        let agent = spawn_agent(ctx)?;

        if self.banner {
            eprintln!(
                "\x1b[2matomcode harness — interactive terminal session. /help for commands.\x1b[0m"
            );
        }
        if let Some(text) = initial {
            agent.send(text);
        }

        // The reader owns the agent handle and delivers work **directly to the
        // inbox**, signalling the loop separately. That ordering is what makes
        // typing during a turn steer it: if the line waited in a channel for the
        // loop to come back, it could only ever start the *next* turn.
        let (signal_tx, mut signals) = tokio::sync::mpsc::unbounded_channel::<Input>();
        let reader_agent = agent.clone();
        let reader = tokio::spawn(async move {
            let mut stdin = BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = stdin.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let input = if trimmed.starts_with('/') {
                    Input::Command(trimmed.to_string())
                } else {
                    reader_agent.send(trimmed);
                    Input::Delivered
                };
                if signal_tx.send(input).is_err() {
                    break;
                }
            }
        });

        let mut needs_prompt = true;
        loop {
            // Anything owed gets done before anything is read, so a command
            // typed after a message cannot jump ahead of the work.
            if agent.inbox().has_waking_input() {
                let outcome = driver.drive(&agent).await;
                println!();
                if let Some(error) = &outcome.error {
                    eprintln!("\x1b[31m{error}\x1b[0m");
                }
                if outcome.stop == StopReason::Cancelled {
                    eprintln!("\x1b[2mstopped\x1b[0m");
                }
                needs_prompt = true;
                continue;
            }

            if needs_prompt {
                self.print_prompt();
                needs_prompt = false;
            }
            match signals.recv().await {
                // Already in the inbox; the next pass drives it, and printing a
                // prompt in between would suggest we were waiting for more.
                Some(Input::Delivered) => {}
                Some(Input::Command(command)) => {
                    if !self.handle_line(ctx, &agent, &command).await? {
                        break;
                    }
                    needs_prompt = true;
                }
                None => break,
            }
        }
        reader.abort();
        Ok(())
    }
}

/// What the reader task tells the loop. A message is not carried here — it has
/// already been delivered — only the fact that something happened.
enum Input {
    Command(String),
    Delivered,
}

impl Repl {
    /// Returns `false` when the session should end.
    async fn handle_line(
        &self,
        ctx: &Context,
        agent: &Arc<Agent>,
        line: &str,
    ) -> Result<bool, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(true);
        }
        // Commands are handled here rather than dispatched to the model: they
        // are about the session, not about the work.
        match trimmed {
            "/exit" | "/quit" => return Ok(false),
            "/help" => {
                eprintln!(
                    "\x1b[2m  /agents        live agents\n  \
                     /seams         filled service slots\n  \
                     /rows          config rows, and which are enabled\n  \
                     /patch <FILE>  apply a patch layer to the RUNNING tree\n  \
                     /dump          the running tree in full\n  \
                     /audit         check the running composition\n  \
                     /title         this session's name\n  \
                     /stop          cancel the running turn\n  \
                     /exit          leave\x1b[0m"
                );
            }
            "/rows" => match ctx.service::<ControlSvc>() {
                Some(control) => {
                    for (id, plugin, enabled) in control.rows().await {
                        let mark = if enabled { " " } else { "·" };
                        eprintln!("\x1b[2m  {mark} {id:<28} {plugin}\x1b[0m");
                    }
                }
                None => eprintln!("\x1b[2m  no control service\x1b[0m"),
            },
            "/dump" => match ctx.service::<ControlSvc>() {
                Some(control) => eprintln!("\x1b[2m{}\x1b[0m", control.dump().await),
                None => eprintln!("\x1b[2m  no control service\x1b[0m"),
            },
            "/audit" => match ctx.service::<ControlSvc>() {
                Some(control) => {
                    let findings = control.audit().await;
                    if findings.is_empty() {
                        eprintln!("\x1b[2m  consistent\x1b[0m");
                    }
                    for finding in findings {
                        eprintln!("\x1b[33m  {finding}\x1b[0m");
                    }
                }
                None => eprintln!("\x1b[2m  no control service\x1b[0m"),
            },
            "/stop" => agent.cancel(),
            "/agents" => {
                if let Some(agents) = ctx.service::<AgentsSvc>() {
                    for a in agents.list() {
                        eprintln!("\x1b[2m  #{} {:?}\x1b[0m", a.id(), a.status());
                    }
                }
            }
            "/seams" => {
                for name in ctx.service_names() {
                    eprintln!("\x1b[2m  {name}\x1b[0m");
                }
            }
            "/title" => {
                let title = match (
                    ctx.service::<SessionTitleSvc>(),
                    ctx.service::<SessionSvc>(),
                ) {
                    (Some(titler), Some(log)) => titler.title(&log).await,
                    _ => None,
                };
                eprintln!(
                    "\x1b[2m  {}\x1b[0m",
                    title.unwrap_or_else(|| "(untitled)".into())
                );
            }
            // Reconfigure the running tree. This is the interactive face of
            // `App::patch`: the row changes, its fiber is remounted, and
            // everything that did not change keeps running.
            other if other.starts_with("/patch ") => {
                let path = other.trim_start_matches("/patch ").trim();
                let Some(control) = ctx.service::<ControlSvc>() else {
                    eprintln!("\x1b[2m  no control service\x1b[0m");
                    return Ok(true);
                };
                match std::fs::read_to_string(path) {
                    Ok(toml) => match control.patch(&toml).await {
                        Ok(summary) => eprintln!("\x1b[2m  {summary}\x1b[0m"),
                        Err(e) => eprintln!("\x1b[31m  {e}\x1b[0m"),
                    },
                    Err(e) => eprintln!("\x1b[31m  cannot read {path}: {e}\x1b[0m"),
                }
            }
            other if other.starts_with('/') => {
                eprintln!("\x1b[2m  unknown command; /help\x1b[0m");
            }
            text => agent.send(text),
        }
        Ok(true)
    }
}

/// Asks through the terminal this front end already owns.
struct TerminalQuestions;

#[async_trait]
impl UserQuestions for TerminalQuestions {
    fn describe(&self) -> String {
        "the terminal".into()
    }

    async fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        eprintln!("\n\x1b[33m{question}\x1b[0m");
        eprint!("\x1b[33m[{}]\x1b[0m ", options.join("/"));
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let mut stdin = BufReader::new(tokio::io::stdin());
        if stdin.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let answer = line.trim().to_lowercase();
        options
            .iter()
            .find(|o| o.to_lowercase() == answer || o.to_lowercase().starts_with(&answer))
            .cloned()
    }
}

/// Asking through the terminal, as a row of its own.
///
/// Split from the REPL because being able to ask is a property of *having a
/// terminal*, not of being a particular front end. A one-shot command run from
/// a shell can ask a yes/no question perfectly well, and refusing instead —
/// which is what this harness did before — leaves the agent unable to do the
/// work it was asked to do, with no hint about why.
pub struct TerminalQuestionsPlugin;

#[async_trait]
impl Plugin for TerminalQuestionsPlugin {
    fn name(&self) -> &'static str {
        "user-questions-terminal"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["user-questions"]
    }
    fn description(&self) -> &'static str {
        "ask the person at the terminal"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(TerminalQuestions))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub struct ReplUiPlugin;

#[async_trait]
impl Plugin for ReplUiPlugin {
    fn name(&self) -> &'static str {
        "ui-repl"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["agents", "agent-loop"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["ui", "sessions", "session-title", "control"]
    }
    fn provides(&self) -> &'static [&'static str] {
        // Only the front end. Asking is `user-questions-terminal`'s row — a
        // plain terminal is a plain terminal whether or not this is what is
        // driving it, and one row per capability keeps them independently
        // replaceable.
        &["ui"]
    }
    fn description(&self) -> &'static str {
        "an interactive terminal session"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ReplRow = parse(config)?;
        let _ = ctx
            .provide::<UiSvc>(Arc::new(Repl {
                prompt: row.prompt,
                banner: row.banner,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
