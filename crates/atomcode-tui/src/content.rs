//! The six things a conversation says, as semantic values.
//!
//! Not pre-rendered lines: freezing the width at fold time would make a resize
//! impossible without changing content, which the freeze rule forbids. Keeping
//! them semantic is exactly what lets presentation stay mutable while content
//! does not.

use crate::block::{hash_of, Content, ContentHash};
use crate::frame::{Color, Line, Span, Style};
use crate::width;

fn dim() -> Style {
    Style::new().dim()
}
fn user() -> Style {
    Style::new().fg(Color::Ansi(39))
}
fn tool() -> Style {
    Style::new().fg(Color::Ansi(37))
}
fn bad() -> Style {
    Style::new().fg(Color::Ansi(203))
}
fn ok() -> Style {
    Style::new().fg(Color::Ansi(78))
}

fn wrapped(text: &str, w: u16, style: Style, prefix: &str) -> Vec<Line> {
    if w == 0 {
        return Vec::new();
    }
    let indent = width::str_width(prefix);
    let body = (w as usize).saturating_sub(indent).max(1);
    let mut out = Vec::new();
    for (i, piece) in width::wrap(text, body).into_iter().enumerate() {
        let lead = if i == 0 {
            prefix.to_string()
        } else {
            " ".repeat(indent)
        };
        // Truncate unconditionally at the end: at a width narrower than the
        // prefix itself, the prefix alone would overflow. Content must never
        // exceed the width it was given, whatever the reason.
        out.push(
            Line::from_spans(vec![Span::styled(lead, dim()), Span::styled(piece, style)])
                .truncate(w as usize),
        );
    }
    out
}

/// What the user said.
#[derive(Debug)]
pub struct UserSaid(pub String);

impl Content for UserSaid {
    fn kind(&self) -> &'static str {
        "user"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&["user", &self.0])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        wrapped(&self.0, w, user(), "❯ ")
    }
}

/// What the model said. Grows while the block is live.
#[derive(Debug, Default)]
pub struct ModelSaid(pub String);

impl Content for ModelSaid {
    fn kind(&self) -> &'static str {
        "assistant"
    }
    fn content_hash(&self) -> ContentHash {
        // Over the source, not the rendering: the same answer at a different
        // width, or with code folded, is the same thing said.
        hash_of(&["assistant", &self.0])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        crate::markdown::render(&self.0, w, Style::new())
    }
    fn summary(&self, w: u16) -> Line {
        let first = self
            .0
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default();
        Line::styled(width::take_width(first, w as usize), dim())
    }
}

/// The model's reasoning channel. Folded by default — a presentation choice,
/// not missing content: expanding is always available and does not change the
/// hash.
#[derive(Debug, Default)]
pub struct ModelThought(pub String);

impl Content for ModelThought {
    fn kind(&self) -> &'static str {
        "reasoning"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&["reasoning", &self.0])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        wrapped(&self.0, w, dim(), "· ")
    }
    fn summary(&self, w: u16) -> Line {
        let n = self.0.lines().count().max(1);
        Line::styled(
            width::take_width(&format!("· thought for {n} line(s)"), w as usize),
            dim(),
        )
    }
}

/// How a tool call ended. `Pending` is what a block carries while it is live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Ok(String),
    Failed(String),
    /// The turn ended before the result arrived. Distinct from a failure: the
    /// tool may well have succeeded, we simply never heard.
    Interrupted,
}

/// A tool call and, once it lands, its result. One block, two facts.
#[derive(Debug)]
pub struct ToolCallBlock {
    pub call_id: String,
    pub name: String,
    pub args: String,
    pub outcome: Outcome,
}

impl ToolCallBlock {
    fn mark(&self) -> (&'static str, Style) {
        match &self.outcome {
            Outcome::Pending => ("⋯", tool()),
            Outcome::Ok(_) => ("✓", ok()),
            Outcome::Failed(_) => ("✗", bad()),
            Outcome::Interrupted => ("—", dim()),
        }
    }

    pub fn pending(
        call_id: impl Into<String>,
        name: impl Into<String>,
        args: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            args: args.into(),
            outcome: Outcome::Pending,
        }
    }
    pub fn with(&self, outcome: Outcome) -> Self {
        Self {
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            args: self.args.clone(),
            outcome,
        }
    }
    /// Arguments in the compact form a person scans, not raw JSON.
    fn brief(&self) -> String {
        let trimmed = self.args.trim();
        let inner = trimmed
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(trimmed);
        inner
            .replace('"', "")
            .replace(':', "=")
            .replace(',', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ---- what kind of tool call this is -------------------------------------

/// How a tool call reads in the transcript.
///
/// The TUI knowing a handful of tool names by heart is presentation-only
/// knowledge: guessing wrong costs a duller line, never a wrong result. The
/// generic fallback below is what actually carries most of the weight — a tool
/// this table has never heard of still gets its subject picked out, which is
/// what stops this from becoming a list that has to be maintained in lockstep
/// with the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    /// What this call *is*, in one word, when the tool name alone is not it.
    pub verb: Option<&'static str>,
    /// Argument names that hold the thing acted on, best first.
    pub subject: &'static [&'static str],
    /// Never fold this one.
    ///
    /// Loading a skill is not output, it is a change in how the agent will
    /// behave for the rest of the turn. Collapsing it to a summary hides the
    /// most consequential thing that happened.
    pub always_open: bool,
}

const GENERIC: Look = Look {
    verb: None,
    // Ordered by how specific the key is: a tool with both `pattern` and `path`
    // is a search, and the pattern is what a person remembers it by.
    subject: &[
        "pattern",
        "command",
        "query",
        "name",
        "skill",
        "file_path",
        "path",
        "url",
        "id",
    ],
    always_open: false,
};

pub fn look(tool: &str) -> Look {
    match tool {
        "use_skill" => Look {
            verb: Some("技能"),
            subject: &["name", "skill"],
            always_open: true,
        },
        "list_skills" => Look {
            verb: Some("技能"),
            subject: &[],
            always_open: false,
        },
        "bash" => Look {
            verb: Some("$"),
            subject: &["command"],
            always_open: false,
        },
        "read_file" | "write_file" | "edit_file" | "list_directory" => Look {
            subject: &["file_path", "path"],
            ..GENERIC
        },
        "grep" | "glob" | "ast_grep" => Look {
            subject: &["pattern", "path"],
            ..GENERIC
        },
        "recall" | "web_search" => Look {
            subject: &["query"],
            ..GENERIC
        },
        "describe_self" => Look {
            verb: Some("自省"),
            subject: &["aspect"],
            ..GENERIC
        },
        "memory" => Look {
            verb: Some("记忆"),
            subject: &["action", "content"],
            ..GENERIC
        },
        "todowrite" => Look {
            verb: Some("计划"),
            subject: &[],
            ..GENERIC
        },
        _ => GENERIC,
    }
}

/// The thing this call acted on, pulled out of its arguments.
///
/// Falls back to the raw argument text so an unknown tool still says something
/// — an empty subject reads as "nothing happened", which is worse than noisy.
pub fn subject_of(tool: &str, args: &str) -> String {
    let look = look(tool);
    let parsed: Option<serde_json::Value> = serde_json::from_str(args).ok();
    if let Some(obj) = parsed.as_ref().and_then(|v| v.as_object()) {
        for key in look.subject {
            if let Some(value) = obj.get(*key) {
                let text = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let text = text.trim();
                if !text.is_empty() {
                    // A path is recognisable by its tail; a command or a pattern
                    // by its head. Trimming the wrong end of a long path leaves
                    // the useless half.
                    return if text.len() > 48 && text.contains('/') {
                        format!("…{}", &text[text.len() - 44..])
                    } else {
                        text.to_string()
                    };
                }
            }
        }
        if obj.is_empty() {
            return String::new();
        }
    }
    let flat = args.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.trim_matches(|c| c == '{' || c == '}').to_string()
}

/// What came back, in a few words.
fn outcome_note(outcome: &Outcome) -> (String, Style) {
    match outcome {
        Outcome::Pending => ("运行中".into(), dim()),
        Outcome::Interrupted => ("已中断".into(), dim()),
        Outcome::Failed(s) => {
            let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("失败");
            (format!("失败 · {}", clip(first, 60)), bad())
        }
        Outcome::Ok(s) if s.trim().is_empty() => ("完成".into(), dim()),
        Outcome::Ok(s) => {
            let lines = s.lines().filter(|l| !l.trim().is_empty()).count();
            if lines > 1 {
                (format!("{lines} 行"), dim())
            } else {
                (clip(s.trim(), 60), dim())
            }
        }
    }
}

fn clip(s: &str, cells: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if width::str_width(&flat) <= cells {
        return flat;
    }
    format!("{}…", width::take_width(&flat, cells.saturating_sub(1)))
}

impl Content for ToolCallBlock {
    fn kind(&self) -> &'static str {
        "tool_call"
    }
    fn content_hash(&self) -> ContentHash {
        let tag = match &self.outcome {
            Outcome::Pending => "pending".to_string(),
            Outcome::Ok(s) => format!("ok:{s}"),
            Outcome::Failed(s) => format!("failed:{s}"),
            Outcome::Interrupted => "interrupted".into(),
        };
        hash_of(&["tool_call", &self.call_id, &self.name, &self.args, &tag])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        if w == 0 {
            return Vec::new();
        }
        let (mark, style) = self.mark();
        let head = Line::from_spans(vec![
            Span::styled(format!("{mark} "), style),
            Span::styled(self.name.clone(), tool()),
            Span::styled(format!(" {}", self.brief()), dim()),
        ]);
        let mut out = vec![head.truncate(w as usize)];
        let body = match &self.outcome {
            Outcome::Ok(s) | Outcome::Failed(s) => s.as_str(),
            _ => "",
        };
        if !body.is_empty() {
            let detail = if matches!(self.outcome, Outcome::Failed(_)) {
                bad()
            } else {
                dim()
            };
            out.extend(wrapped(body, w, detail, "  "));
        }
        out
    }
    /// One line that is worth reading on its own.
    ///
    /// The old version took the first line of the expanded form, which meant a
    /// folded call said what was *asked* and nothing about what came back —
    /// exactly the half a reader already knows. This one names the tool, the
    /// thing it acted on, and what it returned.
    fn summary(&self, w: u16) -> Line {
        let (mark, style) = self.mark();
        let look = look(&self.name);
        let subject = subject_of(&self.name, &self.args);
        let (note, note_style) = outcome_note(&self.outcome);

        let mut spans = vec![Span::styled(format!("{mark} "), style)];
        match look.verb {
            // A verb replaces the tool name when the name is machinery rather
            // than meaning: `$ cargo test` reads; `bash {"command":…}` does not.
            Some(verb) => spans.push(Span::styled(format!("{verb} "), tool())),
            None => spans.push(Span::styled(format!("{} ", self.name), tool())),
        }
        if !subject.is_empty() {
            spans.push(Span::raw(subject));
        }
        if !note.is_empty() {
            spans.push(Span::styled(format!(" · {note}"), note_style));
        }
        Line::from_spans(spans).truncate(w as usize)
    }

    /// Skills are never folded: loading one changes how the agent behaves for
    /// the rest of the turn, and a summary would hide the most consequential
    /// thing on the screen.
    fn always_open(&self) -> bool {
        look(&self.name).always_open
    }
}

/// Something the harness did that a person should know and the model must not.
#[derive(Debug)]
pub struct NoticeBlock {
    pub detail: String,
}

impl Content for NoticeBlock {
    fn kind(&self) -> &'static str {
        "notice"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&["notice", &self.detail])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        wrapped(&self.detail, w, dim(), "⚑ ")
    }
}

/// Model-visible context the harness added on its own initiative.
#[derive(Debug)]
pub struct InjectedBlock {
    pub origin: String,
    pub text: String,
}

impl Content for InjectedBlock {
    fn kind(&self) -> &'static str {
        "injected"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&["injected", &self.origin, &self.text])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        wrapped(&self.text, w, dim(), &format!("[{}] ", self.origin))
    }
    fn summary(&self, w: u16) -> Line {
        Line::styled(
            width::take_width(&format!("[{}]", self.origin), w as usize),
            dim(),
        )
    }
}

/// A question put to the person, and — once they answer — what they said.
///
/// The only block that *consumes* input. It is produced by whoever fills the
/// `user-questions` seam rather than by the transcript, so the stream itself
/// stays a pure fold: swap that provider for a JSON-RPC client and this block
/// simply stops appearing, with nothing else changing.
#[derive(Debug)]
pub struct ChoiceBlock {
    pub question: String,
    pub options: Vec<String>,
    /// `None` while it is being asked; `Some` once answered, and then frozen.
    pub answer: Option<String>,
}

impl Content for ChoiceBlock {
    fn kind(&self) -> &'static str {
        "choice"
    }
    fn content_hash(&self) -> ContentHash {
        let opts = self.options.join("\u{1}");
        hash_of(&[
            "choice",
            &self.question,
            &opts,
            self.answer.as_deref().unwrap_or(""),
        ])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        if w == 0 {
            return Vec::new();
        }
        let ask = Style::new().fg(Color::Ansi(214));
        match &self.answer {
            Some(a) => {
                let mut out = wrapped(&self.question, w, dim(), "? ");
                out.push(
                    Line::from_spans(vec![
                        Span::styled("  → ", dim()),
                        Span::styled(a.clone(), ok()),
                    ])
                    .truncate(w as usize),
                );
                out
            }
            None => {
                let mut out = wrapped(&self.question, w, ask, "? ");
                let choices = self
                    .options
                    .iter()
                    .enumerate()
                    .map(|(i, o)| format!("{}) {o}", i + 1))
                    .collect::<Vec<_>>()
                    .join("   ");
                out.push(Line::styled(
                    width::take_width(&format!("  {choices}   esc) 拒绝"), w as usize),
                    ask,
                ));
                out
            }
        }
    }
    fn summary(&self, w: u16) -> Line {
        let head = match &self.answer {
            Some(a) => format!("? {} → {a}", first_line(&self.question)),
            None => format!("? {}", first_line(&self.question)),
        };
        Line::styled(width::take_width(&head, w as usize), dim())
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// What a slash command said back.
///
/// A block like any other, so a command's answer scrolls with the conversation
/// instead of living in a transient bar that the next redraw eats.
#[derive(Debug)]
pub struct CommandSaid {
    pub text: String,
    /// It could not run. Shown differently, because "here is your answer" and
    /// "I could not do that" must never look the same.
    pub refused: bool,
}

impl Content for CommandSaid {
    fn kind(&self) -> &'static str {
        "command"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&[
            "command",
            &self.text,
            if self.refused { "no" } else { "ok" },
        ])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        let style = if self.refused { bad() } else { dim() };
        let mut out = Vec::new();
        for line in self.text.split('\n') {
            out.extend(wrapped(line, w, style, "  "));
        }
        out
    }
    fn summary(&self, w: u16) -> Line {
        Line::styled(
            width::take_width(first_line(&self.text), w as usize),
            if self.refused { bad() } else { dim() },
        )
    }
}

/// How a turn ended.
#[derive(Debug)]
pub struct TurnEndBlock {
    pub stop: String,
    pub error: Option<String>,
}

impl Content for TurnEndBlock {
    fn kind(&self) -> &'static str {
        "turn_end"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&["turn_end", &self.stop, self.error.as_deref().unwrap_or("")])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        let text = match &self.error {
            Some(e) => format!("— {} · {e}", self.stop),
            None => format!("— {}", self.stop),
        };
        let style = if self.error.is_some() { bad() } else { dim() };
        vec![Line::styled(width::take_width(&text, w as usize), style)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_never_draws_wider_than_it_was_given() {
        let items: Vec<Box<dyn Content>> = vec![
            Box::new(UserSaid(
                "a fairly long user message with 中文 and 🙂".into(),
            )),
            Box::new(ModelSaid("answer ".repeat(20))),
            Box::new(ModelThought("thinking hard".into())),
            Box::new(ToolCallBlock {
                call_id: "c".into(),
                name: "read_file".into(),
                args: r#"{"file_path":"very/long/path/to/a/file.rs"}"#.into(),
                outcome: Outcome::Failed("no such file or directory".into()),
            }),
            Box::new(NoticeBlock {
                detail: "rate limited; waiting 30s".into(),
            }),
            Box::new(TurnEndBlock {
                stop: "Cancelled".into(),
                error: Some("by the user".into()),
            }),
        ];
        for item in &items {
            for w in 0..60u16 {
                for line in item.lines(w) {
                    assert!(
                        line.width() <= w as usize,
                        "{} at width {w}: {:?} is {} cells",
                        item.kind(),
                        line.plain(),
                        line.width()
                    );
                }
            }
        }
    }

    #[test]
    fn the_hash_ignores_width_and_folding_but_not_the_words() {
        let a = ModelSaid("hello".into());
        assert_eq!(a.content_hash(), a.content_hash());
        // Rendering at different widths, and asking for the folded form, must
        // not change what the block *says*.
        let _ = a.lines(10);
        let _ = a.lines(200);
        let _ = a.summary(10);
        assert_eq!(a.content_hash(), ModelSaid("hello".into()).content_hash());
        assert_ne!(a.content_hash(), ModelSaid("hellp".into()).content_hash());
    }

    #[test]
    fn a_result_arriving_changes_the_hash_because_it_changes_what_is_said() {
        let pending = ToolCallBlock::pending("c1", "bash", "{}");
        let done = pending.with(Outcome::Ok("done".into()));
        assert_ne!(pending.content_hash(), done.content_hash());
    }

    #[test]
    fn a_folded_thought_says_how_much_it_is_hiding() {
        let t = ModelThought("one\ntwo\nthree".into());
        assert!(t.summary(40).plain().contains("3 line"));
    }

    #[test]
    fn arguments_are_shown_in_the_form_a_person_scans() {
        let c = ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#);
        assert_eq!(c.brief(), "file_path=a.rs");
    }
}
