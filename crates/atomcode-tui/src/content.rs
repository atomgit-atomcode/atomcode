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
use atomcode_harness::seams::StopReason;

/// Metadata: the mark on a line, a tool's `· 6 行`, a folded thought.
///
/// A role, not SGR 2. `Style::dim` — "let the terminal decide how much darker"
/// — is what this used to be, and it is why so much of the screen was grey: the
/// contrast was chosen by the terminal, after the palette had done arithmetic
/// to guarantee it, and nothing in the tree could measure the result. The role
/// recedes by a measured amount instead, and `--probe-terminal` reports it.
fn muted() -> Style {
    Style::new().fg(Color::role(Role::Muted))
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
/// Work in flight, and the one thing in a turn that is not a finished fact.
///
/// The same role `modules::live` colours the running turn with, so "still going"
/// is one colour on one screen rather than two that have to be kept in step.
fn warn() -> Style {
    Style::new().fg(Color::role(Role::Warning))
}
/// A call that has receded behind a fold.
///
/// The heading role, so a folded line reads as scaffolding over the answer
/// rather than as one more thing being said: an expanded call is a fact the
/// reader is looking at, a folded one is a fact they have chosen not to. The
/// whole summary takes it — name, subject and mark alike — because a line that
/// stated two colours would be saying two things.
///
/// It overrides the state a call is in, deliberately. A run that is still going
/// is [`warn`] *while it is open*, where the reader is watching it; folded, it
/// has already told the reader it exists, and the live line below is where "in
/// flight" is stated.
fn fold() -> Style {
    Style::new().fg(Color::role(Role::Accent))
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
            Line::from_spans(vec![
                Span::styled(lead, muted()),
                Span::styled(piece, style),
            ])
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
    ///
    /// Spacing around it is not decided here: a block draws its own content and
    /// nothing else, and the blank row under the bar belongs to the seam between
    /// two blocks — see `host::blank_between`, which is the one place that
    /// decides it for both the painter and the scroll.
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
    fn growing_text(&self) -> Option<&str> {
        Some(&self.0)
    }
    fn summary(&self, w: u16) -> Line {
        let first = self
            .0
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default();
        Line::styled(width::take_width(first, w as usize), muted())
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
        wrapped(&self.0, w, muted(), "· ")
    }
    fn summary(&self, w: u16) -> Line {
        let n = self.0.lines().count().max(1);
        Line::styled(
            width::take_width(
                &format!("{} 思考 {n} 行", Caps::default().g(Glyph::Gutter)),
                w as usize,
            ),
            muted(),
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

/// The column a tool call's result hangs in from the left edge of the stream.
///
/// The `●` opens a call at the margin and what came back hangs under it, two
/// cells in — `● read_file(a.rs)` over `  ⎿ 20 行`. An answer is set in by the
/// same amount, and it reads this number rather than naming one of its own, so
/// that "the reply lines up with the work that produced it" is one fact instead
/// of two that happen to agree today. See `host::inset`.
pub(crate) const GUTTER: usize = 2;

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
            Outcome::Pending => ("⋯", warn()),
            Outcome::Ok(_) => ("✓", ok()),
            Outcome::Failed(_) => ("✗", bad()),
            Outcome::Interrupted => ("—", muted()),
        }
    }

    /// The colour the call's name is drawn in.
    ///
    /// Uncoloured once the call is a fact about the past: it sits in a line that
    /// already has a marker, so a colour here would be a second thing saying
    /// what the marker says. While the call is *running* the whole head takes
    /// [`Role::Warning`], the colour the live line uses for work in flight — so
    /// which calls are still going is something the screen says, rather than
    /// something the reader works out by comparing the clock to the last result.
    fn name_style(&self) -> Style {
        match &self.outcome {
            Outcome::Pending => warn(),
            _ => tool(),
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

    /// The opening line: whatever marks it, the tool, and the subject — whole.
    ///
    /// Whole rather than abbreviated, and wrapped rather than cut: expanding a
    /// call is how a reader asks what actually ran, and a command ending in `…`
    /// is not an answer to that question. `lead` is what marks the line, and its
    /// width is the indent its continuations hang under.
    ///
    /// `name_style` is the caller's because the same line is drawn twice at two
    /// different volumes: open, where the call is the subject of the screen, and
    /// folded behind a lid, where it recedes. Which one it is, is a fact about
    /// the screen and not about the call, so it is passed in rather than decided
    /// here — see [`fold`].
    fn head(&self, w: u16, lead: &str, lead_style: Style, name_style: Style) -> Vec<Line> {
        let look = look(&self.name);
        let subject = subject_of(&self.name, &self.args);
        let name = match look.verb {
            Some(verb) => verb.to_string(),
            None => self.name.clone(),
        };
        let mut spans = vec![Span::styled(name, name_style)];
        if !subject.is_empty() {
            spans.push(Span::styled(format!("({subject})"), name_style));
        }
        crate::markdown::wrap_spans(&spans, w, lead, lead_style)
    }

    /// `⎿ what came back` — the line under the head.
    fn note_line(&self, w: u16) -> Line {
        let (note, note_style) = outcome_note(&self.outcome);
        Line::from_spans(vec![
            Span::styled(
                format!(
                    "{}{} ",
                    " ".repeat(GUTTER),
                    Caps::default().g(Glyph::Gutter)
                ),
                muted(),
            ),
            Span::styled(note, note_style),
        ])
        .truncate(w as usize)
    }

    /// A run of calls behind one lid.
    ///
    /// The count is the headline: a run of calls is one piece of work, and what
    /// a reader wants from a folded transcript is how much of it there was —
    /// four calls that all said nothing are four rows of noise. The *last*
    /// command is the one shown, because a run ends with the thing that was
    /// being looked for, and it is that command whose result is on the line
    /// under it.
    ///
    /// Both of the last call's rows are its own, so the lid is the same two rows
    /// a single folded call draws, with the count above them — folding a run
    /// changes how many rows there are, not what the rows are.
    ///
    /// The whole lid is at [`fold`]'s volume, and the count stays [`muted`]:
    /// the count is a figure *about* the work rather than the work, and it was
    /// already the quieter of the two — painting it in the heading role would
    /// have made the folded line louder than the open one it replaces.
    pub fn group_lines(last: &ToolCallBlock, count: usize, w: u16) -> Vec<Line> {
        let caps = Caps::default();
        let mut out = vec![Line::from_spans(vec![
            Span::styled(format!("{} ", caps.g(Glyph::ToolMark)), fold()),
            Span::styled(format!("{count} 个工具"), muted()),
        ])
        .truncate(w as usize)];
        let lead = format!("{}{} ", " ".repeat(GUTTER), caps.g(Glyph::Gutter));
        out.extend(last.head(w, &lead, muted(), fold()));
        out.push(last.note_line(w));
        out
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
/// The thing a call acted on, whole.
///
/// Whole, not abbreviated: this is what the *expanded* form shows, and
/// expanding a call is a request to see what actually ran. The folded summary
/// abbreviates separately, against the width it has — see [`ToolCallBlock`]'s
/// `summary`.
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
                    return text.to_string();
                }
            }
        }
        if obj.is_empty() {
            return String::new();
        }
    }
    let flat = flatten(args);
    flat.trim_matches(|c| c == '{' || c == '}').to_string()
}

/// What came back, in a few words.
fn outcome_note(outcome: &Outcome) -> (String, Style) {
    match outcome {
        Outcome::Pending => ("运行中".into(), muted()),
        Outcome::Interrupted => ("已中断".into(), muted()),
        Outcome::Failed(s) => {
            let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("失败");
            (format!("失败 · {}", clip(first, 60)), bad())
        }
        Outcome::Ok(s) if s.trim().is_empty() => ("完成".into(), muted()),
        Outcome::Ok(s) => {
            let lines = s.lines().filter(|l| !l.trim().is_empty()).count();
            if lines > 1 {
                (format!("{lines} 行"), muted())
            } else {
                (clip(s.trim(), 60), muted())
            }
        }
    }
}

/// `s` as one row.
///
/// A line is not a paragraph: whatever the source used newlines for, a row of
/// the transcript is one row, and text that keeps them is written where the
/// scroll is not counting.
fn flatten(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, cells: usize) -> String {
    let flat = flatten(s);
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
    ///
    /// The head wraps instead of being cut. Expanding is how a reader asks to
    /// see what actually ran, and a command that ends in `…` is not an answer
    /// to that question — so the whole subject goes on the screen, over as many
    /// rows as it takes, hanging under the marker.
    fn lines(&self, w: u16) -> Vec<Line> {
        if w == 0 {
            return Vec::new();
        }
        let caps = Caps::default();
        let lead = format!("{} ", caps.g(Glyph::ToolMark));
        let mut out = self.head(w, &lead, self.mark().1, self.name_style());
        out.push(self.note_line(w));

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
    ///
    /// The subject is abbreviated to fit the room the rest of the line leaves,
    /// rather than to a fixed budget and then cut again by the line's own
    /// truncation: that cut landed on whatever happened to be last, which for a
    /// long command was the result note — the folded line lost its ending while
    /// keeping a command nobody could finish reading.
    ///
    /// One row at [`fold`]'s volume: the mark, the name and the subject are the
    /// summary, and the summary is what the reader has chosen to put away. The
    /// note keeps its own style, because it is not the summary — it is the
    /// answer, and a failed call's red is the one thing on a folded line that
    /// has to survive being folded.
    fn summary(&self, w: u16) -> Line {
        let style = fold();
        let look = look(&self.name);
        let name = match look.verb {
            Some(verb) => verb.to_string(),
            None => self.name.clone(),
        };
        let (note, note_style) = outcome_note(&self.outcome);
        // The note is capped to a share of the line. It is the secondary half —
        // the reader is scanning for *what ran* — and an uncapped one-line
        // result (clipped at sixty cells) could otherwise leave no room for the
        // call it came from.
        let note = if note.is_empty() {
            note
        } else {
            clip(&note, (w as usize / 3).clamp(12, 48))
        };
        // One row by construction, so a command's own newlines have to go: a
        // heredoc's body would otherwise be written as extra *physical* rows
        // under a line the scroll counted as one — the terminal moves down, the
        // accounting does not, and what the next block draws lands on top of it.
        let full = flatten(&subject_of(&self.name, &self.args));
        let has_subject = !full.is_empty();
        // What is already spoken for: the mark and its space, the tool's name,
        // the parentheses, and the ` · ` that introduces the note.
        let fixed = 2
            + width::str_width(&name)
            + if has_subject { 2 } else { 0 }
            + if note.is_empty() {
                0
            } else {
                3 + width::str_width(&note)
            };
        let subject = if has_subject {
            width::elide_middle(&full, (w as usize).saturating_sub(fixed))
        } else {
            String::new()
        };

        let mut spans = vec![Span::styled(
            format!("{} ", Caps::default().g(Glyph::ToolMark)),
            style,
        )];
        // `name(subject)` — the same shape as the expanded form, so folding
        // changes how much you see and not what you are looking at.
        //
        // A verb replaces the tool name when the name is machinery rather than
        // meaning: `$ cargo test` reads; `bash {"command":…}` does not.
        spans.push(Span::styled(name, style));
        if has_subject {
            spans.push(Span::styled(format!("({subject})"), style));
        }
        if !note.is_empty() {
            spans.push(Span::styled(format!(" · {note}"), note_style));
        }
        // Belt and braces at the widths where nothing fits: content must never
        // draw wider than it was given.
        Line::from_spans(spans).truncate(w as usize)
    }

    /// Skills are never folded: loading one changes how the agent behaves for
    /// the rest of the turn, and a summary would hide the most consequential
    /// thing on the screen.
    fn always_open(&self) -> bool {
        look(&self.name).always_open
    }

    /// It is one, which is how the host gets at the call behind the lid.
    fn as_tool_call(&self) -> Option<&ToolCallBlock> {
        Some(self)
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
        wrapped(&self.detail, w, muted(), "⚑ ")
    }
}

/// Model-visible context the harness added on its own initiative.
#[derive(Debug)]
pub struct InjectedBlock {
    /// Which injection this is, as `Presentation` keys it — `injected:reminder`
    /// and so on. Separate from [`origin`](Self::origin) on purpose: that one is
    /// the label a person reads (`[compaction summary]`), this one is the key the
    /// screen folds by. A stray space in a label is a typo; the same space in a
    /// key is a kind nobody can ever hide.
    pub kind: &'static str,
    pub origin: String,
    pub text: String,
}

impl Content for InjectedBlock {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&[self.kind, &self.origin, &self.text])
    }
    fn lines(&self, w: u16) -> Vec<Line> {
        wrapped(&self.text, w, muted(), &format!("[{}] ", self.origin))
    }
    fn summary(&self, w: u16) -> Line {
        Line::styled(
            width::take_width(&format!("[{}]", self.origin), w as usize),
            muted(),
        )
    }
}

/// Every injection there is, as *(the word a person types, the kind the screen
/// files it under)*.
///
/// One table, because three places have to agree about it and two of them are
/// bare string lists: `origin_kind` names a block, the presentation decides
/// which kinds open off-screen, and `/showinject` is how a person overrules it.
/// A hand-kept list in three files does not fail loudly when they drift — it
/// fails as a block nobody can hide, or a name nobody can type, and both look
/// exactly like the feature working.
pub const INJECTIONS: &[(&str, &str)] = &[
    ("reminder", "injected:reminder"),
    ("memory", "injected:memory"),
    ("continuation", "injected:continuation"),
    ("compaction", "injected:compaction"),
    ("peer", "injected:peer"),
    ("to-member", "injected:to-member"),
    ("team-note", "injected:team-note"),
];

/// An undo, a rewind or a restore, as a line in the stream (`docs/adr/0024`
/// §17): the stream is not reversible, so what was taken back stays where it is
/// — dimmed — and this says where the conversation went back to.
#[derive(Debug)]
pub struct RewoundBlock {
    /// The turn it went back to before. `None` for a log whose start this
    /// screen never saw.
    pub to_turn: Option<u64>,
    pub scope: atomcode_harness::session::RewindScope,
}

impl Content for RewoundBlock {
    fn kind(&self) -> &'static str {
        "rewound"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&[
            "rewound",
            &self.to_turn.unwrap_or(0).to_string(),
            match self.scope {
                atomcode_harness::session::RewindScope::Conversation => "conversation",
                atomcode_harness::session::RewindScope::Code => "code",
                atomcode_harness::session::RewindScope::Both => "both",
            },
        ])
    }
    fn always_open(&self) -> bool {
        true
    }
    fn lines(&self, width: u16) -> Vec<Line> {
        use atomcode_harness::session::RewindScope;
        let what = match self.scope {
            RewindScope::Conversation => "对话",
            RewindScope::Code => "工作区",
            RewindScope::Both => "对话与工作区",
        };
        let text = match self.to_turn {
            Some(turn) => format!("↶ 已把{what}撤回到第 {turn} 轮之前"),
            None => format!("↶ 已把{what}撤回到更早的一轮之前"),
        };
        vec![Line::styled(
            crate::width::take_width(&text, width as usize),
            crate::theme::fg(Role::Warning),
        )]
    }
}

/// The injections the screen opens without.
///
/// Context the harness added on its own initiative, addressed to the model: a
/// compaction summary, a recalled memory, a `keep going` nudge. The agent needs
/// them in the log and the person reading the transcript is not the audience for
/// them, so they stay in the stream — in the content hashes, in `/transcript`,
/// in what the model was actually sent — and are simply not painted.
///
/// `injected:peer` is deliberately not here. A teammate's report is an answer
/// somebody asked for, and the team panel is showing it for that reason. Nor
/// are what the lead is told about its team — what the person said to a member,
/// a member's report on a turn the person started: the person is the audience.
///
/// A slice of strings rather than a filter over [`INJECTIONS`], because both
/// consumers need it as a `&'static [&'static str]` — the default fold state and
/// the group gesture `/showinject` runs.
/// `the_injection_tables_agree_with_each_other` is what keeps the duplication
/// honest.
pub const ENVIRONMENTAL_INJECTIONS: &[&str] = &[
    "injected:reminder",
    "injected:memory",
    "injected:continuation",
    "injected:compaction",
];

/// Resolve what a person typed after `/showinject` to the kind it names.
///
/// The short word and the full kind both work. The full one is what a fold state
/// and `/transcript` show, and people name what they can see; the short one is
/// what they get from typing `/showinject ` and reading the menu.
pub fn injected_kind(word: &str) -> Option<&'static str> {
    let word = word.trim().to_ascii_lowercase();
    let word = word.strip_prefix("injected:").unwrap_or(&word);
    INJECTIONS
        .iter()
        .find(|(name, _)| *name == word)
        .map(|(_, kind)| *kind)
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
                let mut out = wrapped(&self.question, w, muted(), "? ");
                out.push(
                    Line::from_spans(vec![
                        Span::styled("  → ", muted()),
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
        Line::styled(width::take_width(&head, w as usize), muted())
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
        let style = if self.refused { bad() } else { muted() };
        let mut out = Vec::new();
        for line in self.text.split('\n') {
            out.extend(wrapped(line, w, style, "  "));
        }
        out
    }
    fn summary(&self, w: u16) -> Line {
        Line::styled(
            width::take_width(first_line(&self.text), w as usize),
            if self.refused { bad() } else { muted() },
        )
    }
}

/// How a turn ended.
///
/// The reason is the *typed* one, not a rendering of it. A block that took a
/// string could only hand it back, and the fold would have to decide the words
/// — which is how this line came to say `✓ RunawayFuse`: `format!("{stop:?}")`
/// is a Rust identifier, and `error.is_none()` is not the same question as "did
/// this turn finish". Only [`StopReason::Stopped`] is a clean end; every other
/// variant cut it short, and one of them (a round budget running out) is the
/// outcome a person most needs named, because it looks like a crash and is not.
#[derive(Debug)]
pub struct TurnEndBlock {
    pub stop: StopReason,
    pub error: Option<String>,
    /// What the turn cost. All-zero (the default) means the log recorded
    /// nothing — a turn cut before its first request — and then the rule says
    /// only how it ended, because a zero is noise pretending to be information.
    pub stats: TurnStats,
}

/// What one turn cost, as its own facts recorded it.
///
/// Folded by whoever owns those facts and handed here as a value: the block
/// draws these numbers, it does not know where usage comes from.
///
/// `prompt` is **not** a sum over the turn's rounds, and that is the whole trap
/// in this type. It is the entire context one request sent, so a turn of four
/// rounds sends the same opening prefix four times, growing; summing would
/// count it four times and report a number with no meaning. `cached` is a part
/// of that same request — read off the same reading, which is why the two are
/// kept together instead of the ratio being folded on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnStats {
    /// Model requests the turn ran. `step` and `round` are one counter in the
    /// turn loop, so this is both.
    pub steps: u32,
    /// The context the turn's **last** request sent.
    pub prompt: u32,
    /// Tokens the model generated, summed over the turn's rounds. Unlike
    /// `prompt`, each round's output is new, so this one does add up.
    pub completion: u32,
    /// The cached part of `prompt`, from that same last request.
    pub cached: u32,
}

impl TurnStats {
    /// The figures worth printing, or `None` when there is nothing to say.
    fn caption(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.steps > 0 {
            parts.push(format!("{} 步", self.steps));
        }
        if self.prompt > 0 {
            parts.push(format!("入 {}", token_count(self.prompt)));
        }
        if self.completion > 0 {
            parts.push(format!("出 {}", token_count(self.completion)));
        }
        if let Some(hit) = self.cache_hit() {
            parts.push(format!("缓存 {hit}"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// The cached share of the context, or `None` if the provider said nothing.
    fn cache_hit(&self) -> Option<String> {
        cache_hit_rate(self.cached, self.prompt)
    }
}

/// The cached share of one request's context, as a percentage to two decimals.
///
/// `None` when the provider said nothing about caching: one that does not report
/// it reports zero, and printing `0.00%` would state a fact we do not have — the
/// same rule as the status line's dropped zero counter.
///
/// **Two decimals, and one function.** The live line, the end of a turn and the
/// figure a person compares them against all describe the same request, on the
/// same screen; `98%` beside `98.15%` is a disagreement someone has to stop and
/// resolve, and at a context of tens of thousands of tokens the whole integer
/// part is 99 for a long stretch — the decimals are the only part of this number
/// that moves.
pub fn cache_hit_rate(cached: u32, prompt: u32) -> Option<String> {
    (cached > 0 && prompt > 0).then(|| format!("{:.2}%", cached as f64 * 100.0 / prompt as f64))
}

/// A token count as a person says it: exact while it is small enough to read,
/// rounded in thousands once it is not.
///
/// One function because the status line and the end of a turn report the same
/// quantity on the same screen, and two renderings of one number is a
/// disagreement a person has to stop and resolve.
pub fn token_count(n: u32) -> String {
    if n < 10_000 {
        return n.to_string();
    }
    let thousands = format!("{:.1}", n as f64 / 1000.0);
    let trimmed = thousands.strip_suffix(".0").unwrap_or(&thousands);
    format!("{trimmed}k")
}

/// The mark and the words for one stop reason.
///
/// `完成` / `已中断` are `atomcode-tuix`'s two words for these two outcomes, and
/// this crate's own tool-result note (`outcome_note`) already uses them, so the
/// vocabulary is the product's rather than new. What is added is the cause,
/// where a person can do something about it: a turn that ran out of rounds says
/// so, in words, instead of showing them a variant name or nothing at all.
fn turn_end_note(stop: StopReason) -> (Glyph, String, Style) {
    use StopReason::*;
    let warn = Style::new().fg(Color::role(Role::Warning));
    match stop {
        // The only clean end: the model answered and asked for nothing.
        Stopped => (Glyph::Ok, "完成".to_string(), muted()),
        // The person's own doing, so it is stated without alarm.
        Cancelled => (Glyph::Interrupted, "已中断".to_string(), muted()),
        // Three ways to be cut short, and they were one sentence here until a
        // person went looking for a round budget that was not the thing that
        // stopped them. `StopReason`'s own docs are the authority:
        //
        // * `MaxRounds` — the round budget ran out. That one really is rounds.
        // * `StoppedByPolicy` — *a* `turn-stopping` listener ended it: "a round
        //   budget, a deadline, a cost ceiling". The shipped `round-cap` row
        //   returns this for its `max_seconds`, so in the default tree it means
        //   the clock, and "轮数上限" was wrong for the only case that ships.
        // * `RunawayFuse` — "not a policy: the fuse exists so a tree with no
        //   stopping policy at all still terminates". Calling it a limit hides
        //   the one actionable fact, which is that nothing was watching.
        MaxRounds => (Glyph::Interrupted, "已中断 · 轮数用完了".to_string(), warn),
        StoppedByPolicy => (
            Glyph::Interrupted,
            "已中断 · 一条停止策略叫停(时限或预算)".to_string(),
            warn,
        ),
        RunawayFuse => (
            Glyph::Interrupted,
            "已中断 · 兜底熔断,这棵树没挂停止策略".to_string(),
            warn,
        ),
        ToolLoopDetected => (
            Glyph::Interrupted,
            "已中断 · 检测到重复循环".to_string(),
            warn,
        ),
        PromptRejected => (Glyph::Interrupted, "已中断 · 输入被拒绝".to_string(), warn),
        // A hard boundary refused a call; the refusal itself is the tool's result.
        PolicyDenied => (
            Glyph::Interrupted,
            "已中断 · 安全策略拦下了这一步".to_string(),
            warn,
        ),
        // A pause, not a failure: the reset time is on the notice above it.
        RateLimited => (Glyph::Interrupted, "已暂停 · 触发限流".to_string(), warn),
        // A failure: the stream went silent and retrying did not bring it back.
        Timeout => (
            Glyph::Fail,
            "已中断 · 模型长时间没有回应".to_string(),
            bad(),
        ),
        // A failure, with the provider's own sentence folded in below.
        ProviderError => (Glyph::Fail, "已中断".to_string(), bad()),
        // Not a failed request: the log cannot explain what reached the model,
        // and from here resume, fork and compaction are unsound. A person is
        // owed that in words rather than sharing a sentence with a dead
        // network — what they do next is start a new session, not retry.
        InvariantViolated => (
            Glyph::Fail,
            "已中断 · 内部不变量被破坏,这条会话不宜再续".to_string(),
            bad(),
        ),
        // The kernel's two fuses. One `StopReason` now serves the log and the
        // handle (`docs/adr/0021` §6), so these can reach a screen too.
        MaxContinuations => (
            Glyph::Interrupted,
            "已中断 · 自动续跑次数用完了".to_string(),
            warn,
        ),
        RepeatLoop => (
            Glyph::Interrupted,
            "已中断 · 检测到重复循环".to_string(),
            warn,
        ),
        // `StopReason` is `non_exhaustive`: a cause added later still ends the
        // turn visibly rather than failing to compile a screen.
        _ => (Glyph::Interrupted, "已中断".to_string(), warn),
    }
}

impl Content for TurnEndBlock {
    fn kind(&self) -> &'static str {
        "turn_end"
    }
    fn content_hash(&self) -> ContentHash {
        // The variant name is identity here, not presentation: it is never
        // drawn, and two turns that stopped for different reasons must not hash
        // alike even when neither has a cause attached.
        //
        // The cost is in here because the block now says it: two turns that
        // stopped the same way cost different amounts, and the freeze
        // instrument is about what a block says.
        hash_of(&[
            "turn_end",
            &format!("{:?}", self.stop),
            self.error.as_deref().unwrap_or(""),
            &format!(
                "{}:{}:{}:{}",
                self.stats.steps, self.stats.prompt, self.stats.completion, self.stats.cached
            ),
        ])
    }
    /// A divider with the turn's outcome set into it, the way tuix closes a
    /// turn: `───── ✓ 完成 · 4 步 · 入 90.7k · 出 4200 · 缓存 99.82% ─────`. A bare
    /// line of text at the left margin reads as something that was said; a
    /// captioned rule reads as a boundary.
    ///
    /// What the turn cost is set into that same rule, because the boundary is
    /// exactly where a person asks "what did that take". It is the *last* thing
    /// to be added and the first to be dropped: `captioned_rule` throws away a
    /// caption it cannot place, and how the turn ended is the one thing this
    /// line is for. So the caption is built widest-first and falls back to the
    /// outcome alone, with whatever was dropped going under the rule, wrapped —
    /// the same ladder the cause of a failed turn already climbed.
    fn lines(&self, w: u16) -> Vec<Line> {
        let caps = Caps::default();
        let (mark, said, style) = turn_end_note(self.stop);
        let short = format!("{} {said}", caps.g(mark));

        // Under the rule, in the order they are worth reading: the cost first
        // (it is about this turn), then the cause of a failure.
        let mut under: Vec<String> = Vec::new();
        let mut caption = short;
        if let Some(stats) = self.stats.caption() {
            let wider = format!("{caption} · {stats}");
            if crate::el::caption_fits(&wider, w as usize) {
                caption = wider;
            } else {
                under.push(stats);
            }
        }
        if let Some(error) = &self.error {
            // A cause can be long: a provider sentence for a dead network is
            // wider than the screen. Set into the rule it would be dropped
            // whole, and the turn would look like it ended in silence — so a
            // long one goes under the rule, wrapped, however long it is.
            let wider = format!("{caption} · {error}");
            if crate::el::caption_fits(&wider, w as usize) {
                caption = wider;
            } else {
                under.push(error.clone());
            }
        }

        let mut out = vec![crate::el::captioned_rule(
            &caption,
            w as usize,
            muted(),
            style,
        )];
        for line in under {
            out.extend(wrapped(&line, w, style, "  "));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block as it reaches the screen, as one string.
    fn drawn(block: &dyn Content, w: u16) -> String {
        block
            .lines(w)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_failed_turn_keeps_its_cause_on_screen_however_long_it_is() {
        // What a dead network produces: a provider sentence far wider than the
        // screen. Set into the rule it would be dropped whole, and the turn
        // would look like it ended in silence.
        let error = "open failed: error sending request for url \
                     (https://openrouter.ai/api/v1/chat/completions): client error (Connect): \
                     dns error: failed to lookup address information: nodename nor servname provided";
        let block = TurnEndBlock {
            stop: StopReason::ProviderError,
            error: Some(error.into()),
            stats: TurnStats::default(),
        };
        let lines = block.lines(100);
        let text: String = lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("已中断"), "{text}");
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
            stop: StopReason::Cancelled,
            error: Some("by the user".into()),
            stats: TurnStats::default(),
        };
        let lines = short.lines(100);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].plain().contains("已中断 · by the user"));
    }

    /// The reason a turn stopped is a value, and a person reads words. This is
    /// the regression: a turn the loop's own fuse ended was drawn as
    /// `✓ RunawayFuse` — a success mark on a turn that was cut short, and a Rust
    /// identifier where the words belong.
    #[test]
    fn a_stop_reason_is_spoken_not_printed() {
        let drawn = |stop: StopReason| {
            TurnEndBlock {
                stop,
                error: None,
                stats: TurnStats::default(),
            }
            .lines(80)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
        };

        // Three different ways to be cut short, and three different things a
        // person would do about them — so three different sentences. They were
        // one sentence about a round budget until someone went looking for a
        // round budget that was not what stopped them.
        let cut_short = [
            StopReason::MaxRounds,
            StopReason::StoppedByPolicy,
            StopReason::RunawayFuse,
        ];
        for stop in cut_short {
            let text = drawn(stop);
            assert!(text.contains("已中断"), "{text}");
            assert!(
                !text.contains(&format!("{stop:?}")),
                "no variant name on the screen: {text}"
            );
        }
        let said: Vec<String> = cut_short.into_iter().map(drawn).collect();
        assert!(
            said[0].contains("轮数"),
            "the round budget is the one that is about rounds: {}",
            said[0]
        );
        assert!(
            !said[1].contains("轮数") && !said[2].contains("轮数"),
            "and the other two are not, whatever the loop calls them: {said:?}"
        );
        assert!(
            said[2].contains("没挂停止策略"),
            "the fuse says the actionable thing: nothing was watching: {}",
            said[2]
        );
        assert_eq!(
            said.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3,
            "three reasons, three sentences: {said:?}"
        );

        let unsound = drawn(StopReason::InvariantViolated);
        assert!(
            unsound.contains("不宜再续"),
            "a broken invariant is not a failed request: {unsound}"
        );

        let clean = drawn(StopReason::Stopped);
        assert!(
            clean.contains("完成") && !clean.contains("Stopped"),
            "{clean}"
        );

        let cancelled = drawn(StopReason::Cancelled);
        assert!(
            cancelled.contains("已中断") && !cancelled.contains("Cancelled"),
            "{cancelled}"
        );

        let failed = drawn(StopReason::ProviderError);
        assert!(failed.contains("已中断"), "{failed}");

        // Every reason is one of two outcomes, and the mark says which.
        let mark = |stop| turn_end_note(stop).0;
        assert_eq!(mark(StopReason::Stopped), Glyph::Ok);
        for cut in [
            StopReason::Cancelled,
            StopReason::MaxRounds,
            StopReason::StoppedByPolicy,
            StopReason::RunawayFuse,
            StopReason::ToolLoopDetected,
            StopReason::PromptRejected,
            StopReason::ProviderError,
            StopReason::InvariantViolated,
        ] {
            assert_ne!(mark(cut), Glyph::Ok, "{cut:?} was not a clean end");
        }
    }

    /// What a turn cost, on the line that closes it.
    ///
    /// The figures are a real reading, not invented: a four-round turn whose
    /// last request carried 90659 tokens of context, 90496 of them served from
    /// cache, and which produced 4200 tokens of output.
    #[test]
    fn the_end_of_a_turn_says_what_it_cost() {
        let block = TurnEndBlock {
            stop: StopReason::Stopped,
            error: None,
            stats: TurnStats {
                steps: 4,
                prompt: 90_659,
                completion: 4_200,
                cached: 90_496,
            },
        };
        let text = drawn(&block, 100);
        for want in ["完成", "4 步", "入 90.7k", "出 4200", "缓存 99.82%"] {
            assert!(text.contains(want), "{want} missing from {text:?}");
        }
        assert_eq!(block.lines(100).len(), 1, "one rule, not a paragraph");
    }

    /// A provider that says nothing about caching reports zero, and zero is not
    /// a hit rate of nothing — it is a fact we do not have. Same rule as the
    /// status line's dropped zero counter.
    #[test]
    fn no_reported_caching_is_not_reported_as_zero_percent() {
        let block = TurnEndBlock {
            stop: StopReason::Stopped,
            error: None,
            stats: TurnStats {
                steps: 1,
                prompt: 6_223,
                completion: 28,
                cached: 0,
            },
        };
        let text = drawn(&block, 80);
        assert!(!text.contains("缓存"), "{text:?}");
        assert!(text.contains("入 6223"), "the rest is still said: {text:?}");
    }

    /// A turn the log recorded nothing about — cut before its first request —
    /// is drawn exactly as it was before there were figures to draw.
    #[test]
    fn a_turn_with_nothing_recorded_says_only_how_it_ended() {
        let block = TurnEndBlock {
            stop: StopReason::Cancelled,
            error: None,
            stats: TurnStats::default(),
        };
        let lines = block.lines(80);
        assert_eq!(lines.len(), 1, "nothing to say means no extra row");
        let text = drawn(&block, 80);
        assert!(text.contains("已中断"), "{text:?}");
        for absent in ["步", "入", "出", "缓存", "%"] {
            assert!(!text.contains(absent), "{absent} in {text:?}");
        }
    }

    /// The outcome is what this line is for, so it is the one thing a narrow
    /// screen may not take away. The figures are the first thing dropped, and
    /// they go under the rule rather than nowhere.
    #[test]
    fn a_narrow_screen_drops_the_figures_before_it_drops_the_outcome() {
        let block = TurnEndBlock {
            stop: StopReason::Stopped,
            error: None,
            stats: TurnStats {
                steps: 4,
                prompt: 90_659,
                completion: 4_200,
                cached: 90_496,
            },
        };
        let outcome = format!("{} 完成", Caps::default().g(Glyph::Ok));
        let first = (0..200u16)
            .find(|w| crate::el::caption_fits(&outcome, *w as usize))
            .expect("the outcome fits on some screen");
        for w in first..=120 {
            let text = drawn(&block, w);
            assert!(text.contains("完成"), "w={w}: {text:?}");
            // Read with the whitespace taken out: a narrow rule moves the
            // figures under itself, where they are wrapped mid-phrase — they
            // are all still said, which is the property.
            let flat: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            for want in ["4步", "入90.7k", "出4200", "缓存99.82%"] {
                assert!(flat.contains(want), "w={w}: {want} lost from {text:?}");
            }
            for line in block.lines(w) {
                assert!(line.width() <= w as usize, "w={w}: {:?}", line.plain());
            }
        }
    }

    /// The counted cells and the drawn cells are the same number, at every
    /// width, for the widest caption this block can produce.
    #[test]
    fn a_turn_that_knows_a_lot_still_fits_the_line_it_is_given() {
        let block = TurnEndBlock {
            stop: StopReason::MaxRounds,
            error: Some("context window exhausted mid-round".into()),
            stats: TurnStats {
                steps: 40,
                prompt: 1_048_576,
                completion: 123_456,
                cached: 1_000_000,
            },
        };
        for w in 0..160u16 {
            for line in block.lines(w) {
                assert!(line.width() <= w as usize, "w={w}: {:?}", line.plain());
            }
        }
    }

    #[test]
    fn token_counts_are_exact_while_that_is_readable_and_rounded_after() {
        assert_eq!(token_count(0), "0");
        assert_eq!(token_count(28), "28");
        assert_eq!(token_count(9_999), "9999");
        assert_eq!(token_count(10_000), "10k");
        assert_eq!(token_count(90_659), "90.7k");
        assert_eq!(token_count(1_048_576), "1048.6k");
    }

    /// Two decimals, because the integer part stops moving: at a context of tens
    /// of thousands of tokens served almost entirely from cache, `99` is the
    /// whole integer part for as long as the number is worth reading — the
    /// decimals are the part that changes between one request and the next.
    #[test]
    fn a_cache_share_keeps_two_decimals_and_nothing_is_claimed_from_a_zero() {
        assert_eq!(cache_hit_rate(400, 1_200).as_deref(), Some("33.33%"));
        assert_eq!(cache_hit_rate(90_496, 90_659).as_deref(), Some("99.82%"));
        assert_eq!(cache_hit_rate(1, 3).as_deref(), Some("33.33%"));
        assert_eq!(
            cache_hit_rate(2, 3).as_deref(),
            Some("66.67%"),
            "rounded, not truncated"
        );
        // A provider that reports no caching reports zero, and a provider that
        // answered nothing reports nothing: neither is a hit rate of zero.
        assert_eq!(cache_hit_rate(0, 1_200), None);
        assert_eq!(cache_hit_rate(400, 0), None);
        // Not clamped: a reading of more cached tokens than the request carried
        // is a provider saying something it cannot mean, and hiding that behind
        // a tidy `100.00%` is the one thing this function must not do.
        assert_eq!(cache_hit_rate(1_300, 1_200).as_deref(), Some("108.33%"));
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
                stop: StopReason::RunawayFuse,
                error: None,
                stats: TurnStats::default(),
            }),
            Box::new(TurnEndBlock {
                stop: StopReason::Cancelled,
                error: Some("by the user".into()),
                stats: TurnStats::default(),
            }),
            // With figures, and with figures plus a cause: the caption is
            // longest here, so this is the case that would run off the edge.
            Box::new(TurnEndBlock {
                stop: StopReason::Stopped,
                error: None,
                stats: TurnStats {
                    steps: 12,
                    prompt: 128_456,
                    completion: 9_876,
                    cached: 120_000,
                },
            }),
            Box::new(TurnEndBlock {
                stop: StopReason::ProviderError,
                error: Some("connection reset by peer while reading the response body".into()),
                stats: TurnStats {
                    steps: 3,
                    prompt: 62_120,
                    completion: 812,
                    cached: 0,
                },
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

    /// Work in flight is the one thing in a turn that is not a finished fact,
    /// and the whole head says so: the mark *and* the tool's name take the
    /// warning role, because a screen where only a dot changed colour is a
    /// screen you have to squint at to answer "is anything still running".
    ///
    /// The assertion is on the role, never on a colour: a test that named
    /// `#ffcc00` would pass on a palette where yellow reads as red, and would
    /// have to be edited the first time the theme moved.
    #[test]
    fn a_running_call_is_the_warning_colour_and_a_finished_one_is_not() {
        let warn = Some(crate::frame::Color::role(Role::Warning));

        let running = ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#);
        assert_eq!(running.mark().1.fg, warn, "{:?}", running.mark());
        let head = running.lines(60).remove(0);
        let named = head
            .spans
            .iter()
            .find(|s| s.text.contains("read_file"))
            .expect("the tool's name");
        assert_eq!(named.style.fg, warn, "the name is not in flight: {head:?}");

        // The other half: a call that has an answer is a fact about the past,
        // and the successful and failed marks are their own colours rather than
        // the warning one — otherwise everything is "in flight" and the colour
        // stops meaning anything.
        for done in [
            Outcome::Ok("20 行".into()),
            Outcome::Failed("no such file".into()),
            Outcome::Interrupted,
        ] {
            let block =
                ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#).with(done);
            assert_ne!(block.mark().1.fg, warn, "{:?}", block.mark());
            let head = block.lines(60).remove(0);
            let named = head
                .spans
                .iter()
                .find(|s| s.text.contains("read_file"))
                .expect("the tool's name");
            assert_ne!(
                named.style.fg, warn,
                "a settled call is still in flight: {head:?}"
            );
        }
    }

    /// A folded call recedes: it is scaffolding over the answer rather than one
    /// more thing being said, so its summary takes the heading role — and takes
    /// it *instead of* the state it is in. A run still going is yellow while it
    /// is open; folded it is heading-coloured like everything else, because the
    /// reader has already been told it exists and the live line is where "still
    /// running" is stated.
    ///
    /// The note is the exception, and deliberately: `失败 · …` is the answer
    /// rather than the summary, and a fold must not swallow that.
    #[test]
    fn a_folded_call_recedes_to_the_heading_colour_and_keeps_its_failure_note() {
        let heading = Some(crate::frame::Color::role(Role::Accent));
        let pending = ToolCallBlock::pending(
            "c",
            "read_file",
            r#"{"file_path":"/Users/x/crates/atomcode-tui/src/content.rs"}"#,
        );

        let folded = pending.summary(80);
        let named = folded
            .spans
            .iter()
            .find(|s| s.text.contains("read_file"))
            .expect("the tool's name");
        assert_eq!(named.style.fg, heading, "the folded name is not receding");
        let subject = folded
            .spans
            .iter()
            .find(|s| s.text.contains("content.rs"))
            .expect("the subject");
        assert_eq!(
            subject.style.fg, heading,
            "the folded subject is not receding"
        );
        // The whole line, so a folded line cannot be half-loud.
        assert_ne!(
            folded.spans.first().expect("the mark").style.fg,
            Some(crate::frame::Color::role(Role::Warning)),
            "a folded call is still painted as in flight: {folded:?}"
        );

        // And the note survives the fold in its own colour.
        let failed = pending.with(Outcome::Failed("no such file".into()));
        let line = failed.summary(80);
        let note = line
            .spans
            .iter()
            .find(|s| s.text.contains("失败"))
            .expect("the failure note");
        assert_eq!(
            note.style.fg,
            Some(crate::frame::Color::role(Role::Error)),
            "a fold swallowed the one thing that had to survive it: {line:?}"
        );

        // A run behind one lid is the same drawing, so it recedes too.
        let lid = ToolCallBlock::group_lines(&failed, 3, 80);
        let head = lid.iter().find(|l| l.plain().contains("read_file"));
        let head = head.expect("the last call under the lid");
        let named = head
            .spans
            .iter()
            .find(|s| s.text.contains("read_file"))
            .expect("the tool's name");
        assert_eq!(
            named.style.fg, heading,
            "a merged run is not folded like a single one: {head:?}"
        );
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
        // char boundary" — the abbreviation runs while rendering, so the panic
        // took the whole process down mid-turn.
        //
        // Asserted through `summary`, because that is where the cut now lives:
        // `subject_of` returns the command whole and the folded line abbreviates
        // it to the room it has.
        let command = "中文".repeat(15) + "/尾";
        assert!(
            !command.is_char_boundary(command.len() - 44),
            "the sample must reproduce the old panic, or it guards nothing"
        );
        let c = ToolCallBlock::pending("c", "bash", format!(r#"{{"command":"{command}"}}"#));
        let line = c.summary(60).plain();
        assert!(line.contains('…'), "{line:?} should be abbreviated");
        // Reading the line back is what panicked before: a cut at a byte offset
        // produced a string that could not be sliced again at all.
        let _ = line.chars().count();
        assert!(
            width::str_width(&line) <= 60,
            "{line:?} is {} cells",
            width::str_width(&line)
        );
    }

    #[test]
    fn no_row_keeps_a_newline_from_the_command_it_shows() {
        // A heredoc is one command written over several rows, and both forms of
        // the call have to be made of rows. A `Line` that keeps a newline is
        // written by the terminal as extra rows the scroll is not counting, so
        // whatever the next block draws lands on top of it: the lid put
        // `PY) · 34 行` on a row of its own, and the expanded head did the same
        // with the whole body.
        let command = "cd /tmp && python3 - <<'PY'\nimport json\nprint('hi')\nPY";
        // The args as they really arrive: the newlines are in the *value*, which
        // is what `subject_of` parses back out — a hand-written literal would be
        // invalid JSON and take the fallback path instead.
        let args = serde_json::json!({ "command": command }).to_string();
        let c = ToolCallBlock::pending("c", "bash", &args);

        let lid = c.summary(200).plain();
        assert!(
            !lid.contains('\n') && !lid.contains('\r'),
            "the lid is more than one row: {lid:?}"
        );
        // Still the command, still readable at both ends.
        assert!(lid.starts_with("● $(cd /tmp"), "{lid:?}");
        assert!(lid.ends_with("PY) · 运行中"), "{lid:?}");

        // Expanded: over as many rows as it takes, and every one of them one row.
        let rows = c.lines(200);
        assert!(rows.len() > 1, "the command came out as one row: {rows:#?}");
        for (i, line) in rows.iter().enumerate() {
            let text = line.plain();
            assert!(
                !text.contains('\n') && !text.contains('\r'),
                "row {i} of the expanded head carries a newline: {text:?}"
            );
        }
        // The body is on those rows rather than lost with the newlines.
        assert!(
            rows.iter().any(|l| l.plain().contains("import json")),
            "{rows:#?}"
        );
    }

    #[test]
    fn an_abbreviation_says_as_much_about_a_chinese_path_as_an_ascii_one() {
        // The budget is cells, not bytes: 44 *bytes* is 44 ASCII characters but
        // only fourteen CJK ones, so a byte budget truncated Chinese commands
        // harder for no reason — the same command on screen, described less.
        // Both spend the same budget to within the one cell a two-wide
        // character cannot fill — half a character is not a thing you can print.
        let ascii = ToolCallBlock::pending(
            "c",
            "bash",
            format!(r#"{{"command":"/x/{}"}}"#, "a".repeat(120)),
        );
        let cjk = ToolCallBlock::pending(
            "c",
            "bash",
            format!(r#"{{"command":"/x/{}"}}"#, "中".repeat(60)),
        );
        let a = width::str_width(&ascii.summary(60).plain());
        let c = width::str_width(&cjk.summary(60).plain());
        assert!(
            (a as i64 - c as i64).abs() <= 1,
            "ascii {a} vs cjk {c} cells"
        );
        assert_eq!(a, 60, "the line does not use the width it was given");
    }

    #[test]
    fn the_folded_line_keeps_both_ends_of_the_command_and_its_result() {
        // 「摘要太短，看不清楚」. The folded line used to cut the subject to 44
        // cells from the *end* — dropping `$ git log` and keeping a tail nobody
        // can place — and the line's own truncation then ate the result note off
        // the far end. A reader scanning for what ran got neither end.
        let command = "git log --oneline --all --decorate --stat --author=lichao";
        let mut c = ToolCallBlock::pending("c", "bash", format!(r#"{{"command":"{command}"}}"#));
        c = c.with(Outcome::Ok("a\nb\nc".into()));
        // Narrower than the command, so the line is forced to abbreviate.
        let line = c.summary(48).plain();
        assert!(line.contains('…'), "nothing was abbreviated: {line:?}");
        assert!(
            line.starts_with("● $(git log"),
            "the head of the command is gone: {line:?}"
        );
        assert!(
            line.contains("lichao"),
            "the tail of the command is gone: {line:?}"
        );
        assert!(
            line.ends_with("· 3 行"),
            "the result was cut off the end: {line:?}"
        );
        assert!(
            width::str_width(&line) <= 48,
            "{line:?} is {} cells",
            width::str_width(&line)
        );
    }

    #[test]
    fn expanding_a_call_shows_the_command_whole_rather_than_abbreviated() {
        // 「点击展开时，命令同样展开全部」. The expanded head used to be the same
        // 44-cell abbreviation as the folded line and was then truncated at the
        // width, so clicking a call could reveal *less* of the command than the
        // summary it replaced.
        let command = "git log --oneline --all --decorate --stat --author=lichao";
        let c = ToolCallBlock::pending("c", "bash", format!(r#"{{"command":"{command}"}}"#));
        for w in [40u16, 72, 120] {
            let head: String = c
                .lines(w)
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
                .join("\n");
            let flat: String = head.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                flat.contains(command),
                "at {w} the command is not whole:\n{head}"
            );
            assert!(
                !head.contains('…'),
                "at {w} the expanded head is still abbreviated:\n{head}"
            );
        }
    }

    #[test]
    fn a_wrapped_head_stays_inside_the_width_it_was_given() {
        // The expanded head now runs over several rows. A hanging indent added
        // after the wrap is exactly how a row ends up one cell too wide, which
        // the containment check would then reject.
        let c = ToolCallBlock::pending(
            "c",
            "bash",
            format!(r#"{{"command":"{}"}}"#, "中".repeat(80)),
        );
        for w in 1u16..=40 {
            for line in c.lines(w) {
                assert!(
                    width::str_width(&line.plain()) <= w as usize,
                    "at {w}: {:?} is {} cells",
                    line.plain(),
                    width::str_width(&line.plain())
                );
            }
        }
    }

    #[test]
    fn the_injection_tables_agree_with_each_other() {
        // Three lists have to say the same thing — `origin_kind` names the block,
        // `INJECTIONS` is what `/showinject` accepts, and `ENVIRONMENTAL_INJECTIONS`
        // is what opens hidden — and nothing but this test is watching them. The
        // failure they drift into is silent both ways: a kind with no name is a
        // block nobody can type their way back to, and a name with no kind is a
        // refusal for something that is on the screen.
        use crate::modules::transcript::origin_kind;
        use atomcode_harness::session::InjectionOrigin;

        let every = [
            InjectionOrigin::Reminder,
            InjectionOrigin::Memory,
            InjectionOrigin::Continuation,
            InjectionOrigin::CompactionSummary,
            InjectionOrigin::Peer {
                from: "lead-1/scout".into(),
            },
        ];
        for origin in &every {
            let kind = origin_kind(origin);
            assert!(
                INJECTIONS.iter().any(|(_, k)| *k == kind),
                "{origin:?} is filed under `{kind}`, which no name in INJECTIONS reaches"
            );
        }

        for (name, kind) in INJECTIONS {
            assert_eq!(
                injected_kind(name),
                Some(*kind),
                "`/showinject {name}` does not resolve to the kind it names"
            );
            assert_eq!(
                injected_kind(kind),
                Some(*kind),
                "`/showinject {kind}` does not resolve to itself"
            );
        }

        // The group is a subset, and it is the group minus the peer: a teammate's
        // report is the one injection that is an answer rather than a nudge.
        for kind in ENVIRONMENTAL_INJECTIONS {
            assert!(
                INJECTIONS.iter().any(|(_, k)| k == kind),
                "`{kind}` opens hidden but has no name to type"
            );
            assert_ne!(*kind, origin_kind(&every[4]), "a peer report opens hidden");
        }
        assert!(
            ENVIRONMENTAL_INJECTIONS.len() < INJECTIONS.len(),
            "the group gesture and `all` are the same gesture, so nothing is hidden by default"
        );
    }

    #[test]
    fn an_injection_is_labelled_by_its_origin_not_by_its_kind() {
        // The two are deliberately different strings: one is read, one is keyed.
        // A label that drifted into a kind would put `injected:` in front of every
        // reminder on screen, and a kind that drifted into a label would be a fold
        // state keyed by prose — the second is invisible, the first is not.
        let b = InjectedBlock {
            kind: "injected:reminder",
            origin: "reminder".into(),
            text: "keep going".into(),
        };
        assert_eq!(b.kind(), "injected:reminder");
        assert_eq!(b.lines(40)[0].plain(), "[reminder] keep going");
        assert_eq!(b.summary(40).plain(), "[reminder]");
    }
}
