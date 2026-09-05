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
        wrapped(&self.0, w, user(), "› ")
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
        hash_of(&["assistant", &self.0])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        wrapped(&self.0, w, Style::new(), "")
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
        let (mark, style) = match &self.outcome {
            Outcome::Pending => ("⋯", tool()),
            Outcome::Ok(_) => ("✓", ok()),
            Outcome::Failed(_) => ("✗", bad()),
            Outcome::Interrupted => ("—", dim()),
        };
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
    fn summary(&self, w: u16) -> Line {
        self.lines(w).into_iter().next().unwrap_or_default()
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
