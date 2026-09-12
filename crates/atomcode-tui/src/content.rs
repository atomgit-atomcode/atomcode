//! The six things a conversation says, as semantic values.
//!
//! Not pre-rendered lines: freezing the width at fold time would make a resize
//! impossible without changing content, which the freeze rule forbids. Keeping
//! them semantic is exactly what lets presentation stay mutable while content
//! does not.

use crate::block::{hash_of, Content, ContentHash};
use crate::caps::{Caps, Glyph};
use crate::frame::{Color, Line, Span, Style};
use crate::theme::Role;
use crate::width;

fn dim() -> Style {
    Style::new().dim()
}
fn user() -> Style {
    Style::new().fg(Color::role(Role::Accent))
}
fn tool() -> Style {
    Style::new().fg(Color::role(Role::ToolName))
}
fn bad() -> Style {
    Style::new().fg(Color::role(Role::Error))
}
fn ok() -> Style {
    Style::new().fg(Color::role(Role::Success))
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
    /// A full-width bar, the way `atomcode-tuix` echoes what you typed.
    ///
    /// The background is the point: in a screen of assistant prose and tool
    /// output, the bar is where you scan to find "what did I ask". A chevron
    /// alone gets lost among the `●` and `⎿` markers around it.
    fn lines(&self, w: u16) -> Vec<Line> {
        // Roles, not colours. This used to name `Theme::Dark` outright, which
        // is how the whole transcript stayed dark on a light screen: a module
        // that can resolve is a module that can resolve wrongly.
        let bar = crate::theme::bg(crate::theme::Role::PanelBg)
            .under(crate::theme::fg(crate::theme::Role::PanelFg));
        wrapped(
            &self.0,
            w,
            user(),
            &format!("{} ", Caps::default().g(Glyph::Prompt)),
        )
        .into_iter()
        .map(|line| {
            let pad = (w as usize).saturating_sub(line.width());
            let mut spans: Vec<Span> = line
                .spans
                .into_iter()
                .map(|sp| Span::styled(sp.text, sp.style.under(bar)))
                .collect();
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), bar));
            }
            Line::from_spans(spans)
        })
        .collect()
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
            width::take_width(
                &format!("{} 思考 {n} 行", Caps::default().g(Glyph::Gutter)),
                w as usize,
            ),
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
                    // the useless half. Measured in cells, not bytes: the same
                    // number of bytes is fewer characters in Chinese, and a byte
                    // offset that landed mid-character used to panic the whole
                    // TUI.
                    return if width::str_width(text) > 48 && text.contains('/') {
                        format!("…{}", width::take_width_from_end(text, 43))
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
    /// The shape `atomcode-tuix` ships, because two front ends with two looks
    /// are two products:
    ///
    /// ```text
    /// ● read_file(README.md)
    ///   ⎿ 20 行
    ///      1  <div align="center">
    /// ```
    ///
    /// The marker opens the call, the gutter hangs the result off it, and the
    /// first result line is metadata in muted grey — subordinate to both the
    /// assistant text above and the call header, which is what makes a screenful
    /// of tool calls skimmable.
    fn lines(&self, w: u16) -> Vec<Line> {
        if w == 0 {
            return Vec::new();
        }
        let caps = Caps::default();
        let look = look(&self.name);
        let subject = subject_of(&self.name, &self.args);
        let name = match look.verb {
            Some(verb) => verb.to_string(),
            None => self.name.clone(),
        };
        let head = Line::from_spans(vec![
            Span::styled(format!("{} ", caps.g(Glyph::ToolMark)), self.mark().1),
            Span::styled(name, tool()),
            Span::raw(if subject.is_empty() {
                String::new()
            } else {
                format!("({subject})")
            }),
        ]);
        let mut out = vec![head.truncate(w as usize)];

        let (note, note_style) = outcome_note(&self.outcome);
        out.push(
            Line::from_spans(vec![
                Span::styled(format!("  {} ", caps.g(Glyph::Gutter)), dim()),
                Span::styled(note, note_style),
            ])
            .truncate(w as usize),
        );

        let body = match &self.outcome {
            Outcome::Ok(s) | Outcome::Failed(s) => s.as_str(),
            _ => "",
        };
        if !body.is_empty() && body.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
            let detail = if matches!(self.outcome, Outcome::Failed(_)) {
                bad()
            } else {
                Style::new()
            };
            out.extend(wrapped(body, w, detail, "     "));
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
        let (_mark, style) = self.mark();
        let look = look(&self.name);
        let subject = subject_of(&self.name, &self.args);
        let (note, note_style) = outcome_note(&self.outcome);

        let mut spans = vec![Span::styled(
            format!("{} ", Caps::default().g(Glyph::ToolMark)),
            style,
        )];
        // `name(subject)` — the same shape as the expanded form, so folding
        // changes how much you see and not what you are looking at.
        match look.verb {
            // A verb replaces the tool name when the name is machinery rather
            // than meaning: `$ cargo test` reads; `bash {"command":…}` does not.
            Some(verb) => spans.push(Span::styled(verb.to_string(), tool())),
            None => spans.push(Span::styled(self.name.clone(), tool())),
        }
        if !subject.is_empty() {
            spans.push(Span::raw(format!("({subject})")));
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
        let ask = Style::new().fg(Color::role(Role::Warning));
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
    /// A divider with the turn's outcome set into it, the way tuix closes a
    /// turn: `───── ✓ 完成 ─────`. A bare line of text at the left margin reads
    /// as something that was said; a captioned rule reads as a boundary.
    fn lines(&self, w: u16) -> Vec<Line> {
        let caps = Caps::default();
        let (mark, style) = match &self.error {
            Some(_) => (caps.g(Glyph::Fail), bad()),
            None => (caps.g(Glyph::Ok), dim()),
        };
        let short = format!("{mark} {}", self.stop);
        let Some(error) = &self.error else {
            return vec![crate::el::captioned_rule(&short, w as usize, dim(), style)];
        };
        // A short cause sits in the rule the way a clean ending does. A long
        // one is not dropped: `captioned_rule` drops a caption it cannot place,
        // and the cause of a failed turn is the one caption a person must see —
        // without it a dead network looks like a turn that ended in silence.
        // So it goes under the rule, wrapped, however long the provider made it.
        let full = format!("{short} · {error}");
        if crate::el::caption_fits(&full, w as usize) {
            return vec![crate::el::captioned_rule(&full, w as usize, dim(), style)];
        }
        let mut out = vec![crate::el::captioned_rule(&short, w as usize, dim(), style)];
        out.extend(wrapped(error, w, style, "  "));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_turn_keeps_its_cause_on_screen_however_long_it_is() {
        // What a dead network produces: a provider sentence far wider than the
        // screen. Set into the rule it would be dropped whole, and the turn
        // would look like it ended in silence.
        let error = "open failed: error sending request for url \
                     (https://openrouter.ai/api/v1/chat/completions): client error (Connect): \
                     dns error: failed to lookup address information: nodename nor servname provided";
        let block = TurnEndBlock {
            stop: "ProviderError".into(),
            error: Some(error.into()),
        };
        let lines = block.lines(100);
        let text: String = lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ProviderError"), "{text}");
        assert!(
            text.contains("nodename nor servname"),
            "the cause must reach the screen:\n{text}"
        );
        assert!(
            lines.len() > 1,
            "wider than the rule means wrapped under it"
        );
        for line in &lines {
            assert!(line.width() <= 100, "{:?}", line.plain());
        }

        // A short cause still sits in the rule, on one line.
        let short = TurnEndBlock {
            stop: "Cancelled".into(),
            error: Some("by the user".into()),
        };
        let lines = short.lines(100);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].plain().contains("Cancelled · by the user"));
    }

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
        assert!(t.summary(40).plain().contains("思考 3 行"));
    }

    #[test]
    fn arguments_are_shown_in_the_form_a_person_scans() {
        // `brief` used to render `file_path=a.rs`, which reads like a debug
        // dump. `subject_of` picks the argument that names the thing acted on,
        // so the line reads `read_file(a.rs)` — and it has a fallback, so a
        // tool nobody wrote a rule for still says something.
        let c = ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#);
        assert_eq!(subject_of(&c.name, &c.args), "a.rs");
        assert!(c.summary(40).plain().contains("read_file(a.rs)"));

        let unknown = ToolCallBlock::pending("d", "some_new_tool", r#"{"thing":"x.rs"}"#);
        assert!(
            !subject_of(&unknown.name, &unknown.args).is_empty(),
            "an unknown tool still gets a subject, or the table would have to \
             track the catalog"
        );
    }

    #[test]
    fn a_long_subject_is_trimmed_on_a_character_boundary_not_a_byte_one() {
        // The command that killed the TUI four times: over 48 bytes, contains
        // '/', and `text.len() - 44` landed inside a Chinese character. The old
        // `&text[text.len() - 44..]` panicked here with "byte index 50 is not a
        // char boundary" — `subject_of` is called while rendering, so the panic
        // took the whole process down mid-turn.
        let command = "中文".repeat(15) + "/尾";
        assert!(
            !command.is_char_boundary(command.len() - 44),
            "the sample must reproduce the old panic, or it guards nothing"
        );
        let subject = subject_of("bash", &format!(r#"{{"command":"{command}"}}"#));
        let tail = subject.trim_start_matches('…');
        assert!(
            subject.starts_with('…'),
            "{subject:?} should be abbreviated"
        );
        assert!(
            command.ends_with(tail),
            "{tail:?} is not a suffix of the command"
        );
        // The budget is cells, so a Chinese path keeps as much as an ASCII one.
        assert_eq!(width::str_width(&subject), 44, "{subject:?}");
    }

    #[test]
    fn an_abbreviation_says_as_much_about_a_chinese_path_as_an_ascii_one() {
        // 44 *bytes* is 44 ASCII characters but only fourteen CJK ones, so the
        // byte offset truncated Chinese paths harder for no reason: the same
        // path on screen, described less. Both now spend the same 44-cell
        // budget, to within the one cell a two-wide character cannot fill —
        // half a character is not a thing you can print.
        let ascii = subject_of("bash", &format!(r#"{{"command":"/x/{}"}}"#, "a".repeat(60)));
        let cjk = subject_of(
            "bash",
            &format!(r#"{{"command":"/x/{}"}}"#, "中".repeat(30)),
        );
        for subject in [&ascii, &cjk] {
            let cells = width::str_width(subject);
            assert!((43..=44).contains(&cells), "{subject:?} is {cells} cells");
        }
    }
}
