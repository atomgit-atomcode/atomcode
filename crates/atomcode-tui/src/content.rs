//! The six things a conversation says, as semantic values.
//!
//! Not pre-rendered lines: freezing the width at fold time would make a resize
//! impossible without changing content, which the freeze rule forbids. Keeping
//! them semantic is exactly what lets presentation stay mutable while content
//! does not.

use crate::block::{hash_of, Content, ContentHash, RenderCtx};
use crate::caps::{Caps, Glyph};
use crate::frame::{Color, Line, Span, Style};
use crate::i18n::product::{t as pt, Msg as PMsg};
use crate::i18n::{t, Msg};
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
/// A call that has receded behind a fold.
///
/// The muted grey, so a folded line reads as scaffolding over the answer rather
/// than as one more thing being said: an expanded call is a fact the reader is
/// looking at, a folded one is a fact they have chosen not to, so it recedes
/// like the rest of the chrome. The whole summary takes it — name, subject and
/// mark alike — because a line that stated two colours would be saying two
/// things. (An accent here read as *louder* than the open call, the opposite of
/// receding.)
///
/// It overrides the state a call is in, deliberately. A run that is still going
/// is [`warn`] *while it is open*, where the reader is watching it; folded, it
/// has already told the reader it exists, and the live line below is where "in
/// flight" is stated.
fn fold() -> Style {
    muted()
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

/// The opening block of a new session: the brand, the mascot, where you are, and
/// a few commands worth knowing.
///
/// A **stream producer**'s block, not a view module's: it happens once, it has
/// history, and a reader who scrolls back up should still find it
/// (`docs/adr/0004`). That is also why it refuses to fold.
///
/// The tips are decided when the block is built, never in `lines`: `lines` is
/// called every frame and must be pure, so a block that rolled its tips there
/// would change under the reader and its `content_hash` would move every frame.
/// Tuix needed a persisted `welcome_tip_indices` to work around exactly that;
/// deciding once is the cheaper way to the same property.
#[derive(Debug)]
pub struct WelcomeBlock {
    /// Already a display string — the caller folds the home directory away.
    pub cwd: String,
    pub model: Option<String>,
    pub version: &'static str,
    /// The heading above the tips, as the words in force wrote it.
    ///
    /// Settled here rather than read from a constant in `lines`, for the same
    /// reason the tips are: `lines` runs every frame, and the heading is one of
    /// the things the block *says* — so it belongs to the block, not to the
    /// layout.
    pub heading: String,
    /// The tips to show, as `(command, what it does)`.
    pub tips: Vec<(String, String)>,
    /// What this build calls itself. Handed in by the row that mounts the
    /// producer, so a downstream build changes it in the config tree rather
    /// than in this file.
    pub brand: std::sync::Arc<Brand>,
}

/// What the welcome block says, in the language in force.
///
/// **A seam, not a table.** A build that ships one language writes the words in
/// its own rows (see `modules::welcome`'s shipped set); a product that already
/// has a localisation — this one keeps its in `atomcode-config` — hands an
/// implementation in and the block follows `/language` with the rest of the
/// product rather than growing a second, hand-kept copy of the same sentences.
///
/// That copy is the thing this trait exists to prevent: a second list of
/// descriptions is how the welcome screen comes to describe a command
/// differently from the command's own help, and it is the reason `about` is
/// asked by name instead of the block owning a list.
///
/// A command this build has no words for gets `None`, and the tip falls back to
/// the command's own description — never a blank one, and never a translation
/// invented at the call site.
pub trait WelcomeWords: Send + Sync + 'static {
    /// The heading above the tips.
    fn heading(&self) -> String;

    /// One short line for `command`, without the leading slash.
    fn about(&self, command: &str) -> Option<String>;
}

/// What the launcher has to say about *this launch*, the moment the screen opens.
///
/// Not a fact in the session log and not an answer the host can be asked: these
/// are known before either exists. What reaches a person this way today is a
/// configuration file that did not parse, a `resume` whose session lives in
/// another project and so quietly moved the working directory (and with it that
/// project's hooks and MCP servers), and a session that was forked because the
/// one asked for was busy.
///
/// **stderr is not an answer.** Entering the alternate screen wipes whatever was
/// written before it, so a launcher that prints one of these and then opens a
/// full-screen UI has said nothing at all.
///
/// Said **once per launch**, unlike the welcome block above it, which every
/// session gets: these are not about the session, they are about how this
/// process started.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OpeningNotices(pub Vec<String>);

/// What this build calls itself, as data rather than as constants.
///
/// A fork's whole visible identity is these few fields. The previous shape —
/// `let brand = "◆ AtomCode"` in the middle of the layout code and the art in a
/// `const` beside it — cost the one downstream that tried it a rewrite of the
/// mascot module, three width calculations that had the cell count baked in,
/// and a legend whitelist in a test. None of that is a decision about their
/// product; it is all this file refusing to be told.
///
/// So: a value, with a `Default` that is what this build ships, reached through
/// `BrandSvc` and set from the config tree (`crate::rows::BrandRow`). Nothing
/// below reads a brand constant, and the mascot's width comes from the art.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Brand {
    /// The word in the welcome block's top-left, mark and all.
    pub name: String,
    /// What is shown beside the version, top-right.
    pub licence: String,
    /// The art, when there is any. `None` is a build with no mascot — not a
    /// blank one: the tips then have the whole width.
    pub mascot: Option<Mascot>,
}

impl Default for Brand {
    fn default() -> Self {
        Self {
            name: "◆ AtomCode".into(),
            licence: "MIT".into(),
            mascot: Some(Mascot::default()),
        }
    }
}

/// A mascot: rows of half-pixel cells, and the legend that colours them.
///
/// Each row is two characters per cell — the cell's **upper and lower**
/// half-pixels, drawn as `▀` with the foreground above and the background
/// below. A legend character the palette does not name is transparent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mascot {
    pub rows: Vec<String>,
    /// Legend character -> 256-colour index.
    pub palette: std::collections::BTreeMap<char, u8>,
}

impl Mascot {
    /// How many cells wide the art is — **from the art**, not from a constant
    /// beside it. The old `MASCOT_CELLS` had to be kept in step by hand with
    /// three width calculations, which is one of the things a fork had to
    /// rediscover the hard way.
    pub fn cells(&self) -> usize {
        self.rows
            .iter()
            .map(|row| row.chars().count() / 2)
            .max()
            .unwrap_or(0)
    }

    /// A cell's two pixels, as colours.
    fn cell(&self, row: &str, cell: usize) -> (Option<u8>, Option<u8>) {
        let chars: Vec<char> = row.chars().collect();
        let colour = |c: Option<&char>| c.and_then(|c| self.palette.get(c)).copied();
        (colour(chars.get(cell * 2)), colour(chars.get(cell * 2 + 1)))
    }
}

/// The shipped cat, **verbatim** from `atomcode-tuix` (`render/mascot.rs`).
///
/// **It is coarse, and that is the art.** 18 x 8 pixels is enough for two ears,
/// two eyes and a chin — and no whiskers, no nose, no tail. Rendered large it
/// reads as a rounded blob with a face in it. That was checked before deciding
/// to keep it: the alternative was redrawing a "better" cat, which would be a
/// second mascot to keep in step with tuix's, and the two front ends disagreeing
/// about what the product's cat looks like is a worse outcome than a small one.
///
/// **The palette is literal on purpose.** I first wrote these as roles and it
/// produced a magenta cat that read as a bug — the reason is worth keeping:
/// `Role::Brand` resolves to xterm slot 13 *to mean "the brand"*, and this art
/// is not asking for a meaning, it is a picture of an orange cat. There is no
/// role in the vocabulary that means "orange", so the picture states its own
/// colours and the capability gate in `mascot` is what protects a terminal that
/// cannot show them.
impl Default for Mascot {
    fn default() -> Self {
        Self {
            rows: [
                "oooo.o.o.o.o.ooooo",
                "ooooooewekooewekoo",
                "ooooookokoookokooo",
                "..o.ooooooooooo...",
            ]
            .iter()
            .map(|row| (*row).to_string())
            .collect(),
            palette: [
                ('o', 202u8), // orange        #ff5f00
                ('e', 166),   // eyebrow       #d75f00
                ('w', 231),   // highlight     white
                ('k', 232),   // pupil         near-black
            ]
            .into_iter()
            .collect(),
        }
    }
}

impl Content for WelcomeBlock {
    fn kind(&self) -> &'static str {
        "welcome"
    }

    fn content_hash(&self) -> ContentHash {
        // Shape is not in the hash — same class as width. `Content` promises the
        // hash covers what the block *says* and never the bytes it renders, and
        // "did the cat get drawn" is rendered bytes.
        let mut parts: Vec<&str> = vec!["welcome", self.version, &self.cwd, &self.heading];
        if let Some(model) = &self.model {
            parts.push(model);
        }
        for (command, about) in &self.tips {
            parts.push(command);
            parts.push(about);
        }
        hash_of(&parts)
    }

    /// It refuses to fold.
    ///
    /// Its whole point is "this is how the session started" — a one-line summary
    /// of that is the point folded away. The same reasoning as a loaded skill's.
    fn always_open(&self) -> bool {
        true
    }

    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width as usize;
        // Nothing fits inside the padding, so there is no block — not a block of
        // blank rows. A zero-row block still occupies a slot and `blank_between`
        // would leave a blank row for it, so the screen would gain a stray line.
        if w <= PAD * 2 {
            return Vec::new();
        }
        let content_w = w - PAD * 2;
        let pad = " ".repeat(PAD);

        // ---- The left column: the mascot, then the two bullets ----
        let mut left: Vec<Line> = mascot(ctx, content_w, self.brand.mascot.as_ref());

        // cwd and model are rendered BELOW the whole block, never zipped into the
        // left column beside the tips. Tuix learned this: when the tips are taller
        // than the mascot the spare rows landed on these two, and the screen read
        // `∙ proj` and `set a goal…` on one line.
        let mut below: Vec<Line> = Vec::new();
        let bullet = ctx.caps.g(Glyph::Bullet);
        for text in std::iter::once(Some(self.cwd.as_str()))
            .chain(std::iter::once(self.model.as_deref()))
            .flatten()
        {
            // The cwd and model read at full strength — they are the two facts a
            // person most wants at a glance when a session opens, not chrome.
            below.extend(wrapped(
                text,
                content_w as u16,
                Style::default(),
                &format!("{bullet} "),
            ));
        }

        // ---- The right column: a heading and the tips ----
        let mut right: Vec<Line> = Vec::new();
        if !self.tips.is_empty() {
            right.push(Line::styled(
                width::take_width(&self.heading, content_w),
                muted(),
            ));
            let command_w = self
                .tips
                .iter()
                .map(|(command, _)| width::str_width(command))
                .max()
                .unwrap_or(0);
            for (command, about) in &self.tips {
                // The columns line up on the widest command, but never at the cost
                // of the row: command, gap and description together are clipped to
                // `content_w`. A tip that does not fit is cut, never drawn past the
                // edge. (The two-column test above already refuses to place these
                // beside the mascot when they would not fit there.)
                let command = width::take_width(command, content_w);
                let command_w_here = width::str_width(&command);
                let remaining = content_w.saturating_sub(command_w_here);
                let want_gap = command_w.saturating_sub(command_w_here) + 2;
                let gap = want_gap.min(remaining);
                let room = remaining - gap;

                let mut spans = vec![
                    Span::styled(command, Style::new().fg(Color::role(Role::Accent)).bold()),
                    Span::raw(" ".repeat(gap)),
                ];
                // No span at all when nothing of the description fits: an empty
                // styled span is a style with no text, which the encoder would
                // still have to look at.
                if room > 0 {
                    spans.push(Span::styled(width::take_width(about, room), muted()));
                }
                right.push(Line::from_spans(spans));
            }
        }

        let mut rows: Vec<Line> = Vec::new();

        // ---- The header: brand on the left, version · licence on the right ----
        //
        // Clipped to `content_w`, and that clipping is not decoration: a rect
        // narrower than the two strings would otherwise be drawn past its own
        // edge, which the frame's containment check catches per block but a reader
        // sees as a row running into its neighbour.
        let right_txt = format!("v{}  {}", self.version, self.brand.licence);
        let brand = self.brand.name.as_str();
        let brand_w = width::str_width(brand);
        let right_w = width::str_width(&right_txt);
        let brand_style = Style::new().fg(Color::role(Role::Brand));
        if content_w > brand_w + right_w {
            let fill = content_w - brand_w - right_w;
            rows.push(Line::from_spans(vec![
                Span::raw(pad.clone()),
                Span::styled(brand.to_string(), brand_style),
                Span::raw(" ".repeat(fill)),
                Span::styled(right_txt, muted()),
            ]));
        } else {
            // Too narrow for both on one row: two rows beat a truncated line, and
            // beat the two colliding. Each is still clipped to what there is.
            rows.push(Line::from_spans(vec![
                Span::raw(pad.clone()),
                Span::styled(width::take_width(brand, content_w), brand_style),
            ]));
            rows.push(Line::styled(
                format!("{pad}{}", width::take_width(&right_txt, content_w)),
                muted(),
            ));
        }
        rows.push(Line::empty());

        // ---- Two columns only when the widest tip actually fits ----
        //
        // Tuix's criterion, and its reason: tip rows are not truncated, so two
        // columns that do not fit get hard-wrapped by the terminal and the
        // alignment of the column breaks. It used a fixed underestimate once and
        // that is precisely what happened on a narrow terminal.
        let gap = 4usize;
        let left_w = if left.is_empty() {
            PAD
        } else {
            // From the art, so a fork's taller or wider mascot lines the tips
            // up beside it without touching this calculation.
            PAD + self.brand.mascot.as_ref().map_or(0, Mascot::cells)
        };
        let tips_col = left_w + gap;
        let right_w = right.iter().map(Line::width).max().unwrap_or(0);
        let two_columns = !left.is_empty() && !right.is_empty() && content_w >= tips_col + right_w;

        if two_columns {
            for i in 0..left.len().max(right.len()) {
                let mut line = left.get(i).cloned().unwrap_or_else(Line::empty);
                let have = line.width();
                if have < tips_col {
                    line.push(Span::raw(" ".repeat(tips_col - have)));
                }
                if let Some(row) = right.get(i) {
                    for span in &row.spans {
                        line.push(span.clone());
                    }
                }
                rows.push(line);
            }
        } else {
            rows.append(&mut left);
            for row in right {
                let mut line = Line::from_spans(vec![Span::raw(pad.clone())]);
                for span in &row.spans {
                    line.push(span.clone());
                }
                rows.push(line);
            }
        }
        rows.extend(below);

        // A trailing blank, so whatever arrives next (a connection notice, an
        // upgrade hint) does not butt against the last row. Tuix keeps one too.
        rows.push(Line::empty());

        // Nothing fit: no block at all, rather than a block of blank rows. A
        // zero-row block still occupies a slot and `blank_between` would leave a
        // blank row for it, so the screen would gain a stray empty line.
        if rows.iter().all(|line| line.plain().trim().is_empty()) {
            return Vec::new();
        }
        rows
    }
}

/// How far the block is set in from the rect it was given.
const PAD: usize = 2;

/// The mascot, as nine cells of two vertical pixels each.
///
/// **With `cell_background`** the two pixels are the cell's foreground and
/// background, which is what makes the cat recognisable: `▀` for a cell with an
/// upper pixel only, `▀` (fg upper, bg lower) for both, `▄` for a lower pixel
/// only, blank for neither — tuix's `mascot_cell`, unchanged.
///
/// **Without it** the gate below refuses to draw at all, and that is tuix's
/// behaviour rather than a degradation this code invented. Its condition is
/// `colors && unicode_symbols && (modern_emulator || jediterm)`, and the comment
/// next to it says why: on a terminal that drops backgrounds the art **fragments**.
/// A version of this that "coped" by filling both pixels with the upper colour
/// drew a solid orange rectangle for weeks and looked like a loading placeholder.
///
/// My equivalents: `colors != None` for `colors`, `unicode` for
/// `unicode_symbols`, and the measured `cell_background` for
/// `modern_emulator || jediterm` — the same three facts, one of them measured
/// rather than guessed from an environment variable.
///
/// Cut to `content_w`, like every other row: art wider than the rect it was given
/// is a row running into its neighbour.
fn mascot(ctx: &RenderCtx, content_w: usize, art: Option<&Mascot>) -> Vec<Line> {
    let Some(art) = art else {
        return Vec::new();
    };
    let drawable = ctx.caps.colors != crate::caps::Colors::None
        && ctx.caps.unicode
        && ctx.caps.cell_background;
    if !drawable {
        return Vec::new();
    }
    // The art needs PAD plus its own width. Showing a sliced cat is worse than
    // showing none, and the caller's width is the one thing that decides.
    let cells = art.cells();
    if cells == 0 || content_w < cells {
        return Vec::new();
    }
    art.rows
        .iter()
        .map(|row| {
            let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(PAD))];
            let mut run = String::new();
            let mut run_style = Style::new();
            for cell in 0..cells {
                let (top, bottom) = art.cell(row, cell);
                // tuix's `mascot_cell`, decision for decision. `▄` where the TOP
                // pixel is the transparent one, so the ears' empty half does not
                // paint a default-foreground bar across them — the bug that line
                // exists to prevent.
                let (glyph, style) = match (top, bottom) {
                    (None, None) => (" ", Style::new()),
                    (Some(t), None) => ("\u{2580}", ink(t)),
                    (None, Some(b)) => ("\u{2584}", ink(b)),
                    (Some(t), Some(b)) => ("\u{2580}", ink(t).bg(Color::picture(b))),
                };
                if style == run_style {
                    run.push_str(glyph);
                } else {
                    if !run.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run), run_style));
                    }
                    run.push_str(glyph);
                    run_style = style;
                }
            }
            if !run.is_empty() {
                spans.push(Span::styled(run, run_style));
            }
            Line::from_spans(spans)
        })
        .collect()
}

/// `▀`'s foreground: one of the art's own 256-index colours.
///
/// [`Color::Picture`], not `Color::Ansi`: the index is *the picture's*, and the
/// encoder — which is the only part of this that knows what the terminal can show
/// — resolves it. Same promise [`Color::Role`] keeps for the scheme's colours.
fn ink(index: u8) -> Style {
    Style::new().fg(Color::picture(index))
}

impl Content for UserSaid {
    fn kind(&self) -> &'static str {
        "user"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&["user", &self.0])
    }
    /// The pictures this message carries, so a click on it can reopen one. A
    /// user line is the only block a `[Image #N]` marker ever lands in.
    fn image_markers(&self) -> Vec<usize> {
        crate::attach::markers_in(&self.0)
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
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

/// The VL (vision) caption of a picture a text-only model was asked about.
///
/// A text-only model "sees" a pasted picture only as the block of text a VL
/// helper recognised, which the runtime folds into the user message so the model
/// reads it (`[图片内容（由 X 识别）]\n…`). Shown in full that recognition buries the
/// conversation, so the transcript pulls it out into this block: folded by
/// default to one line (`● VL 识别图片成功，返回 N chars  model`) and opened on a
/// click, the way a finished tool call folds. The words are the VL helper's, not
/// the person's, so they are drawn muted.
#[derive(Debug)]
pub struct VlCaptionBlock {
    pub model: String,
    pub text: String,
}

impl VlCaptionBlock {
    /// The one-line stand-in: the fold dot, the localized "recognised, N chars"
    /// line, and the model named. The dot carries the outcome — green for a
    /// good recognition, the way a finished tool call's `●` does — while the
    /// detail stays muted. The message itself carries no mark of its own, which
    /// is why there is none to take off here.
    fn head_line(&self, ctx: &RenderCtx) -> Line {
        let n = self.text.chars().count();
        let body = pt(PMsg::VisionPreprocessSuccess { char_count: n });
        let mark = ctx.caps.g(Glyph::ToolMark);
        let rest = format!(" {body}  {}", self.model);
        let room = (ctx.width as usize).saturating_sub(width::str_width(mark));
        Line::from_spans(vec![
            Span::styled(mark.to_string(), ok()),
            Span::styled(width::take_width(&rest, room), muted()),
        ])
    }
}

impl Content for VlCaptionBlock {
    fn kind(&self) -> &'static str {
        "vl_caption"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&["vl_caption", &self.model, &self.text])
    }
    /// Open: the head line, then the recognised text, muted and indented.
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let mut out = vec![self.head_line(ctx)];
        out.extend(wrapped(&self.text, ctx.width, muted(), "  "));
        out
    }
    /// Folded: the head line with a dim `点击展开` tail, so a reader who does not
    /// know the row opens finds out it does.
    fn summary(&self, ctx: &RenderCtx) -> Line {
        let mut line = self.head_line(ctx);
        let hint = format!("  {}", pt(PMsg::VlCaptionExpandHint));
        if line.width() + width::str_width(&hint) <= ctx.width as usize {
            line.spans.push(Span::styled(hint, muted()));
        }
        line
    }
}

/// Split a user message into what the person typed and the VL caption the
/// runtime folded in, when there is one. `None` for an ordinary message.
///
/// Locale-robust: the marker's fixed parts are read from the very i18n string
/// that wrote it ([`PMsg::VisionRecognised`]), so a translated marker still
/// splits — there is no hardcoded `[图片内容` prefix to drift.
pub fn split_vl_caption(text: &str) -> Option<(String, String, String)> {
    const MODEL: &str = "\u{1}";
    const BODY: &str = "\u{2}";
    let tmpl = pt(PMsg::VisionRecognised {
        model: MODEL,
        text: BODY,
    })
    .into_owned();
    let (pre, rest) = tmpl.split_once(MODEL)?;
    let (mid, suf) = rest.split_once(BODY)?;
    // A marker with no fixed prefix or separator could match anything.
    if pre.is_empty() || mid.is_empty() {
        return None;
    }
    // `rfind`, not `find`: the runtime APPENDS the marker to the end of the
    // message, so the last occurrence is the real one — a person who typed the
    // prefix themselves earlier does not steal the split.
    let start = text.rfind(pre)?;
    let after_pre = &text[start + pre.len()..];
    let (model, after_model) = after_pre.split_once(mid)?;
    let caption = after_model.strip_suffix(suf).unwrap_or(after_model);
    Some((
        text[..start].trim_end().to_string(),
        model.to_string(),
        caption.to_string(),
    ))
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
        crate::markdown::render(&self.0, w, Style::new())
    }
    fn growing_text(&self) -> Option<&str> {
        Some(&self.0)
    }
    fn summary(&self, ctx: &RenderCtx) -> Line {
        let w = ctx.width;
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
        wrapped(&self.0, w, muted(), "· ")
    }
    fn summary(&self, ctx: &RenderCtx) -> Line {
        let w = ctx.width;
        let n = self.0.lines().count().max(1);
        Line::styled(
            width::take_width(
                &t(Msg::ThoughtLines {
                    gutter: Caps::default().g(Glyph::Gutter),
                    n,
                }),
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
/// cells in — `● ReadFile(a.rs)` over `  ⎿ 20 行`. An answer is set in by the
/// same amount, and it reads this number rather than naming one of its own, so
/// that "the reply lines up with the work that produced it" is one fact instead
/// of two that happen to agree today. See `host::inset`.
pub(crate) const GUTTER: usize = 2;

/// How many rows a folded tool call draws.
///
/// Two, because a call the model explained has two identifying rows — the reason
/// and the call — and folding is supposed to leave out the RESULT rather than
/// half of the call's own name. A call nobody explained has one identifying row
/// and so folds to one; this is a ceiling, not a target.
///
/// Not more: a folded call is a line a reader scans past, and since the subject
/// row wraps, a lid that took every row of it would cost the run it stands for.
const FOLDED_ROWS: usize = 2;

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
            Outcome::Interrupted => ("—", muted()),
        }
    }

    /// The colour the call's name is drawn in.
    ///
    /// The terminal's own foreground throughout — running or finished. It sits in
    /// a line that already has a marker (`⋯`/`✓`/`✗`), so the marker says the
    /// state and the name needs no second colour. Running work is NOT painted the
    /// warning yellow: a call in flight is not a warning, and yellow read as one.
    /// "In flight" is stated by the live line below and the `⋯` marker here.
    fn name_style(&self) -> Style {
        tool()
    }

    /// Whether this call failed. For the run lid, which shows the last call's
    /// result and would otherwise be silent about a failure earlier in it.
    pub fn is_failed(&self) -> bool {
        matches!(self.outcome, Outcome::Failed(_))
    }

    /// Whether this call is still running. The host pulses a running call's `●`
    /// mark (white↔grey) so a live call reads apart from a finished one at a
    /// glance — presentation applied where the tick lives, not here (the mark
    /// this block draws is a still colour; `RenderCtx` carries no phase).
    pub fn is_running(&self) -> bool {
        matches!(self.outcome, Outcome::Pending)
    }

    /// A todo update, batch or incremental. Its "command" is a state marker, not
    /// something to read, so when the model gave it an intent the row is just
    /// that phrase — see [`lines`](Self::lines).
    fn is_todo(&self) -> bool {
        matches!(self.name.as_str(), "todo" | "todowrite")
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

    /// The call's arguments as the subject guess should read them.
    ///
    /// The reason is not something the call ACTS ON, and `subject_of` falls back
    /// to the raw argument text for any tool its table has never heard of — so
    /// an unstripped reason would be flattened into that line as
    /// `"thing":"x.rs","intent":"…"`. Taken out here, and only when it is
    /// actually there, so a call without one keeps the exact bytes it had and
    /// the fallback reads as it always did.
    fn args_without_reason(&self) -> String {
        let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&self.args) else {
            return self.args.clone();
        };
        let Some(map) = parsed.as_object_mut() else {
            return self.args.clone();
        };
        if map.remove("intent").is_none() {
            return self.args.clone();
        }
        serde_json::to_string(&parsed).unwrap_or_else(|_| self.args.clone())
    }

    /// What this call acted on, for display.
    fn subject(&self) -> String {
        subject_of(&self.name, &self.args_without_reason())
    }

    /// Why this call is being made, if the model said.
    ///
    /// `intent` rides in the call's own arguments — that is what makes a reason
    /// per call need no protocol, no log format and no new field anywhere up the
    /// stack (see `atomcode-coding`'s `tool_intent`). It is stripped back off
    /// before the call runs, so this is the only reader.
    ///
    /// `None` for every call that did not carry one, which is most of the calls
    /// in an older session and every call from a model that ignores the guide.
    /// The caller then draws exactly what it drew before this existed.
    fn reason(&self) -> Option<String> {
        let parsed: serde_json::Value = serde_json::from_str(&self.args).ok()?;
        let text = parsed.get("intent")?.as_str()?.trim();
        (!text.is_empty()).then(|| flatten(text))
    }

    /// The word this call reads as: a verb where the tool's name is machinery
    /// (`$ cargo test` reads; `bash {"command":…}` does not), its own name
    /// otherwise.
    fn display_name(&self) -> String {
        match look(&self.name).verb {
            Some(verb) => verb.say(),
            None => display_tool_name(&self.name),
        }
    }

    /// `⎿ ReadFile(what it acted on)` — the call itself, on the gutter line.
    ///
    /// Drawn only when there is a reason above it, which is also why it carries
    /// the tool's name: with the reason on the head line, this row is the only
    /// place left that says WHICH call this is. The name is not decoration there
    /// — `⎿ /path/to/thing.rs` alone would not tell a reader whether the agent
    /// read it, wrote it or searched it.
    ///
    /// Wrapped rather than cut, for the reason the head is: a command ending in
    /// `…` is not an answer to "what actually ran", and expanding a call is
    /// exactly how a reader asks that question. The folded form takes only the
    /// first row of this (`summary_lines`), so the full thing is one keypress
    /// away and nothing here has to guess how much of it to keep.
    fn subject_line(&self, w: u16, name_style: Style) -> Vec<Line> {
        let subject = flatten(&self.subject());
        if subject.is_empty() {
            return Vec::new();
        }
        let caps = Caps::default();
        // Indented under the tool mark, with the gutter glyph on the first row
        // only — the shape a result takes, so the two read as the same kind of
        // thing hanging off the call. The glyph itself recedes; the call's own
        // name keeps the volume the caller asked for, so a folded row is not
        // half-loud.
        let prefix = format!("{}{} ", " ".repeat(GUTTER), caps.g(Glyph::Gutter));
        crate::markdown::wrap_spans(
            &[Span::styled(
                format!("{}({subject})", self.display_name()),
                name_style,
            )],
            w,
            &prefix,
            muted(),
        )
    }

    /// The rows that identify this call — the same ones in both shapes the
    /// screen draws it.
    ///
    /// Solely because the folded form is the open form's identifying rows with
    /// the result left out: two copies of this pairing is how a fold comes to
    /// say something the open call never said. `lines` and `summary_lines` both
    /// come through here.
    fn opening_rows(&self, w: u16, lead: &str, lead_style: Style, name_style: Style) -> Vec<Line> {
        let mut out = self.head(w, lead, lead_style, name_style);
        if self.reason().is_some() {
            out.extend(self.subject_line(w, name_style));
        }
        out
    }

    /// The opening line: whatever marks it, and what the call is about — whole.
    ///
    /// Whole rather than abbreviated, and wrapped rather than cut: expanding a
    /// call is how a reader asks what actually ran, and a command ending in `…`
    /// is not an answer to that question. `lead` is what marks the line, and its
    /// width is the indent its continuations hang under.
    ///
    /// With a reason, this line is the REASON and nothing else — the tool then
    /// names itself on the row below (`subject_line`). Without one the line is
    /// the call as it always was, `Name(subject)`: a call nobody explained must
    /// not need a second row to say what it is.
    ///
    /// `name_style` is the caller's because the same line is drawn twice at two
    /// different volumes: open, where the call is the subject of the screen, and
    /// folded behind a lid, where it recedes. Which one it is, is a fact about
    /// the screen and not about the call, so it is passed in rather than decided
    /// here — see [`fold`].
    fn head(&self, w: u16, lead: &str, lead_style: Style, name_style: Style) -> Vec<Line> {
        let spans = match self.reason() {
            Some(reason) => vec![Span::styled(reason, name_style)],
            None => {
                let subject = self.subject();
                let mut spans = vec![Span::styled(self.display_name(), name_style)];
                if !subject.is_empty() {
                    spans.push(Span::styled(format!("({subject})"), name_style));
                }
                spans
            }
        };
        crate::markdown::wrap_spans(&spans, w, lead, lead_style)
    }

    /// `⎿ what came back` — the line under the head.
    /// A write/edit call's change as a coloured diff — `edit_file`'s own unified
    /// diff, or `write_file`'s content as all-additions — when it succeeded and
    /// there is a change worth colouring. `None` for every other call and state,
    /// which then renders its result as text.
    fn diff_view(&self, w: u16) -> Option<crate::diff::Rendered> {
        const MAX: usize = 400;
        // The indent the plain result body uses too, so folding one for the other
        // does not shift the column.
        let indent = "     ";
        let Outcome::Ok(output) = &self.outcome else {
            return None;
        };
        match self.name.as_str() {
            "edit_file" => crate::diff::render_edit(output, w, indent, MAX),
            "write_file" => {
                let content = serde_json::from_str::<serde_json::Value>(&self.args)
                    .ok()?
                    .get("content")?
                    .as_str()?
                    .to_string();
                if content.trim().is_empty() {
                    return None;
                }
                Some(crate::diff::render_written(&content, w, indent, MAX))
            }
            _ => None,
        }
    }

    /// ` (+N -M)` — the change's shape, appended after the file on the naming
    /// line: `+N` green, `-M` red, the parens dim.
    fn diff_count_spans(&self, added: usize, removed: usize) -> Vec<Span> {
        vec![
            Span::styled(" (".to_string(), muted()),
            Span::styled(
                format!("+{added}"),
                Style::new().fg(Color::role(Role::DiffAdd)),
            ),
            Span::styled(" ".to_string(), muted()),
            Span::styled(
                format!("-{removed}"),
                Style::new().fg(Color::role(Role::DiffRemove)),
            ),
            Span::styled(")".to_string(), muted()),
        ]
    }

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
    /// While the run is still the newest thing on screen — `live`, decided by
    /// the caller: nothing visible has followed it and the turn has not ended
    /// — the lid *is* the call in flight: its command and its note, the same
    /// two rows a single folded call draws. The reader is watching work
    /// happen, and the rows have to keep up with the tools — when the next
    /// call starts, the same two rows are redrawn for it, in place. A call
    /// that merely finished does not collapse the lid: the collapse is for
    /// *history*, and history begins when something visible comes after the
    /// run.
    ///
    /// Once the run has been followed, it is the count alone: `已执行了 N 个
    /// 工具`. The commands were on screen while they ran, and a transcript
    /// read after the fact wants how much work there was, not the last
    /// command a second time — which is also why the count is the headline
    /// and the failures ride it rather than taking a row of their own: *how
    /// much ran* is what the row is for and *what broke* is the qualifier on
    /// it.
    ///
    /// The failure suffix is in the alarm colour. The lid draws the *last*
    /// call's outcome and nothing else of the members, so without this a run
    /// whose third call failed and whose fourth succeeded reads exactly like
    /// a run that never failed — and the red on a failed call is the one
    /// thing a fold has to keep.
    pub fn group_lines(
        last: &ToolCallBlock,
        count: usize,
        failed: usize,
        live: bool,
        w: u16,
    ) -> Vec<Line> {
        if live {
            let caps = Caps::default();
            let lead = format!("{}{} ", " ".repeat(GUTTER), caps.g(Glyph::Gutter));
            // The same identifying rows a folded call draws, through the same
            // accessor: a live lid and a folded call are the same two answers
            // about one call, and a second copy of the pairing is how they come
            // to disagree on screen.
            let mut out = last.opening_rows(w, &lead, muted(), fold());
            out.push(last.note_line(w));
            return out;
        }
        let caps = Caps::default();
        let mut spans = vec![
            Span::styled(format!("{} ", caps.g(Glyph::ToolMark)), fold()),
            Span::styled(t(Msg::ToolsRun { count }).into_owned(), muted()),
        ];
        if failed > 0 {
            spans.push(Span::styled(
                t(Msg::ToolsFailed { failed }).into_owned(),
                bad(),
            ));
        }
        vec![Line::from_spans(spans).truncate(w as usize)]
    }
}

/// How a tool call reads in the transcript.
///
/// The TUI knowing a handful of tool names by heart is presentation-only
/// knowledge: guessing wrong costs a duller line, never a wrong result. The
/// generic fallback below is what actually carries most of the weight — a tool
/// this table has never heard of still gets its subject picked out, which is
/// what stops this from becoming a list that has to be maintained in lockstep
/// with the catalog.
/// The one word a call reads as, for the handful of tools the screen knows.
///
/// A variant rather than the word itself: the table this file's `look` is
/// (`GENERIC` included) is a `const`, and a `const` cannot hold a sentence that
/// depends on the language in force. The word is asked for at render time by
/// [`Verb::say`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Skill,
    /// Not a word in any language: the shell's own prompt character.
    Shell,
    Memory,
    Plan,
}

impl Verb {
    fn say(self) -> String {
        match self {
            Verb::Skill => t(Msg::VerbSkill).into_owned(),
            Verb::Shell => "$".to_string(),
            Verb::Memory => t(Msg::VerbMemory).into_owned(),
            Verb::Plan => pt(PMsg::TodoPanelTitle).into_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    /// What this call *is*, in one word, when the tool name alone is not it.
    pub verb: Option<Verb>,
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
            verb: Some(Verb::Skill),
            subject: &["name", "skill"],
            always_open: true,
        },
        "list_skills" => Look {
            verb: Some(Verb::Skill),
            subject: &[],
            always_open: false,
        },
        "bash" => Look {
            verb: Some(Verb::Shell),
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
        // No verb: it reads as its own name, `DescribeSelf`, in every language.
        // A localized verb ("自省") hid the one call people go looking for by
        // name, and its English rendering ("about itself") read as prose, not a
        // tool. The name is already English and already the thing to recognise.
        "describe_self" => Look {
            subject: &["aspect"],
            ..GENERIC
        },
        "memory" => Look {
            verb: Some(Verb::Memory),
            subject: &["action", "content"],
            ..GENERIC
        },
        "todowrite" => Look {
            verb: Some(Verb::Plan),
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
/// A tool's snake_case name as a display word: `read_file` → `ReadFile`, the way
/// the reference names a call. Used when the tool has no hand-written verb, so
/// `● ReadFile(a.rs)` reads as a name rather than a raw wire identifier.
pub fn display_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for word in name.split('_').filter(|w| !w.is_empty()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() {
        name.to_string()
    } else {
        out
    }
}

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
        Outcome::Pending => (pt(PMsg::BgStateRunning).into_owned(), muted()),
        Outcome::Interrupted => (t(Msg::OutcomeInterrupted).into_owned(), muted()),
        Outcome::Failed(s) => {
            let fallback = pt(PMsg::SubagentStatusFailed).into_owned();
            let first = s
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or(&fallback);
            (
                t(Msg::OutcomeFailedWith {
                    first: &clip(first, 60),
                })
                .into_owned(),
                bad(),
            )
        }
        Outcome::Ok(s) if s.trim().is_empty() => {
            (pt(PMsg::SubagentStatusDone).into_owned(), muted())
        }
        Outcome::Ok(s) => {
            let lines = s.lines().filter(|l| !l.trim().is_empty()).count();
            if lines > 1 {
                (t(Msg::OutcomeLines { lines }).into_owned(), muted())
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
    /// ● ReadFile(README.md)
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
        if w == 0 {
            return Vec::new();
        }
        let caps = Caps::default();
        let lead = format!("{} ", caps.g(Glyph::ToolMark));
        // A todo update is a state marker, not a command worth reading: when the
        // model gave it an intent, that phrase is the whole row — the
        // `● 待办("action":…)` line and its 运行中 note are dropped, so a run of
        // todos reads as its own summaries instead of a wall of JSON. An update
        // with no intent keeps the ordinary shape: the args are then all it has to
        // say, and the caller was fine either way.
        //
        // Only while running or done cleanly: a FAILED or INTERRUPTED update
        // keeps the full shape so its `失败 · <error>` / `已中断` note is not
        // swallowed — a todo that did not apply is exactly what the reader (and
        // the model, re-reading) must be told, and the red dot alone does not say
        // what broke.
        if self.is_todo()
            && self.reason().is_some()
            && matches!(self.outcome, Outcome::Pending | Outcome::Ok(_))
        {
            return self.head(w, &lead, self.mark().1, self.name_style());
        }
        let mut out = self.opening_rows(w, &lead, self.mark().1, self.name_style());

        let body = match &self.outcome {
            Outcome::Ok(s) | Outcome::Failed(s) => s.as_str(),
            _ => "",
        };
        // A write/edit shows its change as a coloured, line-numbered diff rather
        // than the raw result text — the same shape the other front end draws.
        // The `(+N -M)` count rides the line that NAMES the call (the last opening
        // row: the subject line when the model gave a reason, the head otherwise),
        // right after the file, and the line is re-clipped so the suffix cannot
        // push it past the width.
        if let Some(diff) = self.diff_view(w) {
            if let Some(mut last) = out.pop() {
                last.spans
                    .extend(self.diff_count_spans(diff.added, diff.removed));
                out.push(last.truncate(w as usize));
            }
            out.extend(diff.lines);
            return out;
        }
        let non_empty = body.lines().filter(|l| !l.trim().is_empty()).count();
        let gutter = format!("{}{} ", " ".repeat(GUTTER), caps.g(Glyph::Gutter));
        match non_empty {
            // Pending / interrupted / a call that returned nothing: the short
            // status note (`运行中` / `已中断` / `完成`) is all there is to say.
            0 => out.push(self.note_line(w)),
            // A single-line result rides one gutter line, shown WHOLE and wrapped
            // rather than clipped — an image-attachment note or a one-line message
            // is never cut off at the edge. A failure keeps its `失败 ·` word and
            // the alarm colour; a clean result shows its own text.
            1 => {
                let line = body
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                let (text, style) = if matches!(self.outcome, Outcome::Failed(_)) {
                    (
                        t(Msg::OutcomeFailedWith { first: line }).into_owned(),
                        bad(),
                    )
                } else {
                    (line.to_string(), muted())
                };
                out.extend(wrapped(&text, w, style, &gutter));
            }
            // A multi-line result: the `⎿ N 行` count, then the whole body under it.
            _ => {
                out.push(self.note_line(w));
                let detail = if matches!(self.outcome, Outcome::Failed(_)) {
                    bad()
                } else {
                    Style::new()
                };
                out.extend(wrapped(body, w, detail, "     "));
            }
        }
        out
    }

    /// The folded call, in one of two shapes.
    ///
    /// **Explained** (the model said why): the first two rows of the expanded
    /// form, and no result — the same rows, produced by the same
    /// [`opening_rows`], so folding changes how much you see rather than what you
    /// are looking at. The result is what expanding is FOR, so a successful one is
    /// not repeated; a call in flight, an interrupted one and a failed one still
    /// say so, appended to the second row rather than taking a third (the mark is
    /// the same muted `●` in every state, so that note is the only thing on a
    /// folded row that would say anything happened at all).
    ///
    /// **Unexplained**: exactly the row it always was — [`summary`](Self::summary),
    /// one elided line carrying the call and its result. A session nobody
    /// annotated, an older log replayed and a tool whose schema never took the
    /// argument all keep the density they had before any of this existed.
    ///
    /// Two, not ten, is the point in the explained case: a folded call is a line
    /// you scan past, and a lid that grew a row per wrapped command would cost the
    /// whole run it hides.
    fn summary_lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        if self.reason().is_none() {
            return vec![self.summary(ctx)];
        }
        let w = ctx.width;
        if w == 0 {
            return Vec::new();
        }
        let caps = Caps::default();
        let lead = format!("{} ", caps.g(Glyph::ToolMark));
        // A todo update folds to the intent row it opens as — there is no
        // `待办(args)` line or result to leave out (see `lines`), recessed to the
        // fold grey and capped like any other fold so a paragraph-length intent
        // cannot grow the wall it was meant to shrink. The outcome guard matches
        // `lines`: a failed/interrupted update keeps the full shape below so its
        // note survives folding too.
        if self.is_todo() && matches!(self.outcome, Outcome::Pending | Outcome::Ok(_)) {
            return self
                .head(w, &lead, self.mark().1, fold())
                .into_iter()
                .take(FOLDED_ROWS)
                .collect();
        }
        // The `●` head carries the call's outcome even folded (green done / red
        // failed / muted running), the same as the unexplained `summary`; the
        // reason text stays muted so the dot is the only lit thing on the row.
        let mut rows: Vec<Line> = self
            .opening_rows(w, &lead, self.mark().1, fold())
            .into_iter()
            .take(FOLDED_ROWS)
            .collect();
        if matches!(self.outcome, Outcome::Ok(_)) {
            return rows;
        }
        let (note, note_style) = outcome_note(&self.outcome);
        if note.is_empty() {
            return rows;
        }
        // Capped to a share of the row, then dropped if the row has no room left
        // for it: the call below is what the reader is scanning for, and at a
        // width where both cannot fit the one that identifies the call wins.
        let note = clip(&note, (w as usize / 3).clamp(12, 48));
        if let Some(last) = rows.last_mut() {
            if last.width() + 3 + width::str_width(&note) <= w as usize {
                last.spans
                    .push(Span::styled(format!(" · {note}"), note_style));
            }
        }
        rows
    }

    /// One line worth reading on its own: the call, its subject, and its result.
    ///
    /// Used for a call the model never explained — the folded form of every call
    /// that existed before reasons did, and of every call from a model that
    /// ignores the guide. Kept intact for exactly that reason: it is the only
    /// line those calls have, so it carries everything they can carry.
    ///
    /// The subject is abbreviated to the room the rest of the line leaves, rather
    /// than to a fixed budget and then cut again by the line's own truncation:
    /// that cut landed on whatever happened to be last, which for a long command
    /// was the result note — the folded line lost its ending while keeping a
    /// command nobody could finish reading.
    ///
    /// The note keeps its own style, because it is not the summary — it is the
    /// answer, and a failed call's red is the one thing on a folded line that has
    /// to survive being folded.
    fn summary(&self, ctx: &RenderCtx) -> Line {
        let w = ctx.width;
        let style = fold();
        let name = self.display_name();
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
        let full = flatten(&self.subject());
        let has_subject = !full.is_empty();
        // What is already spoken for: the two-cell indent (where the mark used to
        // be), the tool's name, the parentheses, and the ` · ` before the note.
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

        // The `●` carries the call's OUTCOME even folded: green when it landed,
        // red when it failed, muted while it runs — so a screenful of folded
        // calls says which ones are done and which broke without expanding any of
        // them. The mark keeps the outcome colour; the name and subject stay
        // muted (`fold()`), so the row reads as one folded line with a status
        // dot, not a lit label.
        //
        // `name(subject)` — the same shape and column as the expanded head:
        // folding changes how much you see, not what you are looking at. A verb
        // replaces the tool name when the name is machinery rather than meaning:
        // `$ cargo test` reads; `bash {"command":…}` does not.
        let mut spans = vec![Span::styled(
            format!("{} ", Caps::default().g(Glyph::ToolMark)),
            self.mark().1,
        )];
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
        wrapped(&self.text, w, muted(), &format!("[{}] ", self.origin))
    }
    fn summary(&self, ctx: &RenderCtx) -> Line {
        let w = ctx.width;
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
    fn lines(&self, ctx: &crate::block::RenderCtx) -> Vec<Line> {
        use atomcode_harness::session::RewindScope;
        let width = ctx.width;
        let what = match self.scope {
            RewindScope::Conversation => t(Msg::RewindScopeConversation),
            RewindScope::Code => t(Msg::RewindScopeCode),
            RewindScope::Both => t(Msg::RewindScopeBoth),
        };
        let text = match self.to_turn {
            Some(turn) => t(Msg::RewoundToTurn { what: &what, turn }),
            None => t(Msg::RewoundEarlier { what: &what }),
        }
        .into_owned();
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
        if w == 0 {
            return Vec::new();
        }
        let ask = Style::new().fg(Color::role(Role::Secondary));
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
                    width::take_width(
                        &format!("  {choices}   esc) {}", t(Msg::AskRefuseChoice)),
                        w as usize,
                    ),
                    ask,
                ));
                out
            }
        }
    }
    fn summary(&self, ctx: &RenderCtx) -> Line {
        let w = ctx.width;
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

/// Cut a line into runs, with every `http://` / `https://` address in it a
/// terminal hyperlink (OSC 8).
///
/// A command says an address when a person has to go there — an authorization
/// page a browser did not open, most of all. Such an address is long, and wrapped
/// to the width it becomes several rows that a selection copies with the breaks
/// in them; as a link it opens with a click whatever the wrapping, because every
/// piece of it carries the whole URL. An address runs to the next whitespace.
fn with_links(line: &str, style: Style) -> Vec<Span> {
    let mut runs = Vec::new();
    let mut rest = line;
    while let Some(start) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| rest.find(scheme))
        .min()
    {
        #[allow(
            clippy::string_slice,
            reason = "`start` is where an ASCII scheme begins, so it is a boundary"
        )]
        let (before, from) = (&rest[..start], &rest[start..]);
        let end = from.find(char::is_whitespace).unwrap_or(from.len());
        #[allow(
            clippy::string_slice,
            reason = "`end` is where a whitespace char begins, or the end"
        )]
        let (url, after) = (&from[..end], &from[end..]);
        if !before.is_empty() {
            runs.push(Span::styled(before.to_string(), style));
        }
        runs.push(Span::linked(
            url.to_string(),
            style.underline(),
            url.to_string(),
        ));
        rest = after;
    }
    if !rest.is_empty() {
        runs.push(Span::styled(rest.to_string(), style));
    }
    runs
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
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
        let style = if self.refused { bad() } else { muted() };
        let mut out = Vec::new();
        for line in self.text.split('\n') {
            let runs = with_links(line, style);
            if runs.iter().any(|run| run.link.is_some()) && w > 0 {
                out.extend(crate::markdown::wrap_spans(&runs, w, "  ", muted()));
            } else {
                out.extend(wrapped(line, w, style, "  "));
            }
        }
        out
    }
    fn summary(&self, ctx: &RenderCtx) -> Line {
        let w = ctx.width;
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
    /// Which `DONE_LABELS` verb a clean stop uses, advanced once per completed
    /// turn by the producer so consecutive turns vary. Ignored for every stop
    /// but [`StopReason::Stopped`].
    pub done_index: usize,
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
    /// Tool calls the turn ran, summed over its steps.
    pub tools: u32,
    /// Wall-clock the turn took, in milliseconds. `0` when the log carried no
    /// timing — a turn cut before it opened, or a replay of a log old enough not
    /// to have stamped one.
    pub elapsed_ms: u64,
}

impl TurnStats {
    /// The figures worth printing, in `atomcode-tuix`'s turn-summary shape
    /// (`2 轮 · 2 工具 · 32.7s · 2.60K tokens · 97% cached`), or `None` when the
    /// turn was cut before it ran and the rule should say only how it ended.
    ///
    /// `with_cached` is off for an interrupted turn: tuix drops the cache ratio
    /// from anything but a clean stop, and a hit rate beside a failure reads as a
    /// figure about the failure.
    fn caption(&self, with_cached: bool) -> Option<String> {
        // A turn that never reached its first request has no rounds and no
        // tokens; the rule then carries only the outcome.
        if self.steps == 0 && self.prompt == 0 && self.completion == 0 {
            return None;
        }
        // What the turn actually cost: its output plus the part of the context
        // that was NOT served from cache. Re-reading the cached prefix each round
        // is near-free, so this is the figure tuix reports rather than the gross
        // prompt+completion.
        let billable =
            self.completion as usize + (self.prompt as usize).saturating_sub(self.cached as usize);
        let mut parts = vec![
            t(Msg::TurnRounds { steps: self.steps }).into_owned(),
            t(Msg::TurnTools { tools: self.tools }).into_owned(),
            fmt_dur(self.elapsed_ms),
            format!("{} tokens", fmt_tokens(billable)),
        ];
        if with_cached {
            if let Some(pct) = self.cache_pct() {
                parts.push(format!("{pct}% cached"));
            }
        }
        Some(parts.join(" · "))
    }

    /// The cached share of the last request's context as a whole percent, or
    /// `None` when the provider reported no caching (so a misleading `0% cached`
    /// never appears — the same rule as [`cache_hit_rate`], to the integer tuix's
    /// summary shows).
    fn cache_pct(&self) -> Option<u8> {
        (self.cached > 0 && self.prompt > 0)
            .then(|| ((self.cached as u64 * 100 / self.prompt as u64).min(100)) as u8)
    }
}

/// A token count in `atomcode-tuix`'s two-decimal thousands — the shape a turn's
/// summary reports it in (`2.60K`, `152.00K`, `1.05M`). Distinct from
/// [`token_count`], which the live line uses at one decimal: the two lines
/// answer different questions, and tuix draws them differently on purpose.
fn fmt_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.2}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// A turn's duration as `atomcode-tuix` writes it: `340ms` under a second,
/// `32.7s` under a minute, then `2m3s` / `1h2m3s`.
fn fmt_dur(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let total = ms / 1000;
    if total < 60 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h == 0 {
        format!("{m}m{s}s")
    } else {
        format!("{h}h{m}m{s}s")
    }
}

/// The rotating turn-completion verbs, `atomcode-tuix`'s pool ported verbatim.
/// Kept English in every locale, exactly as tuix keeps them: a translated cute
/// verb reads awkward, while the structural words around it (`轮`/`工具`) localise.
pub const DONE_LABELS: &[&str] = &[
    "Done",
    "Nailed it",
    "Wrapped",
    "Shipped",
    "Baked",
    "Plated",
    "Served",
    "Bagged",
    "Handled",
    "Dialed in",
    "Locked in",
    "Sealed",
    "Stuck the landing",
    "Buttoned up",
    "Squared away",
    "Cooked",
    "Dusted",
    "Called it",
    "Delivered",
    "Tied off",
];

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
    // `k` from a thousand, with one decimal kept (`8.0k`, not `8k`): the live line
    // shows `入` and `出` side by side, and a bare `8003` next to `78.2k` reads as
    // two different units. One decimal, always, keeps the column consistent.
    if n < 1_000 {
        return n.to_string();
    }
    format!("{:.1}k", n as f64 / 1000.0)
}

/// The same, for a figure an account service reports.
///
/// A separate entry point rather than a widened [`token_count`]: a session's
/// tokens fit in a `u32` and an account's months do not, and the account's
/// figures run to millions, where thousands stop being readable. Same rule
/// though — exact while it can be read, rounded once it cannot.
pub fn token_count_u64(n: u64) -> String {
    const K: f64 = 1_000.0;
    const M: f64 = 1_000_000.0;
    const B: f64 = 1_000_000_000.0;
    let f = n as f64;
    let (scaled, suffix) = if f < 10_000.0 {
        return n.to_string();
    } else if f < M {
        (f / K, "k")
    } else if f < B {
        (f / M, "m")
    } else {
        (f / B, "b")
    };
    let text = format!("{scaled:.1}");
    let trimmed = text.strip_suffix(".0").unwrap_or(&text);
    format!("{trimmed}{suffix}")
}

/// The mark and the words for one stop reason.
///
/// `完成` / `已中断` are `atomcode-tuix`'s two words for these two outcomes, and
/// this crate's own tool-result note (`outcome_note`) already uses them, so the
/// vocabulary is the product's rather than new. What is added is the cause,
/// where a person can do something about it: a turn that ran out of rounds says
/// so, in words, instead of showing them a variant name or nothing at all.
fn turn_end_note(stop: StopReason, done_index: usize) -> (Glyph, String, Style) {
    use StopReason::*;
    let warn = Style::new().fg(Color::role(Role::Warning));
    match stop {
        // The only clean end: the model answered and asked for nothing. The word
        // rotates through `DONE_LABELS` the way tuix's does, so consecutive turns
        // read a little differently instead of the same `完成` every time.
        Stopped => (
            Glyph::Sparkle,
            DONE_LABELS[done_index % DONE_LABELS.len()].to_string(),
            muted(),
        ),
        // The person's own doing, so it is stated without alarm.
        Cancelled => (
            Glyph::Interrupted,
            t(Msg::StopCancelled).into_owned(),
            muted(),
        ),
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
        MaxRounds => (Glyph::Interrupted, t(Msg::StopMaxRounds).into_owned(), warn),
        StoppedByPolicy => (Glyph::Interrupted, t(Msg::StopByPolicy).into_owned(), warn),
        RunawayFuse => (
            Glyph::Interrupted,
            t(Msg::StopRunawayFuse).into_owned(),
            warn,
        ),
        ToolLoopDetected => (Glyph::Interrupted, t(Msg::StopToolLoop).into_owned(), warn),
        PromptRejected => (
            Glyph::Interrupted,
            t(Msg::StopPromptRejected).into_owned(),
            warn,
        ),
        // A hard boundary refused a call; the refusal itself is the tool's result.
        PolicyDenied => (
            Glyph::Interrupted,
            t(Msg::StopPolicyDenied).into_owned(),
            warn,
        ),
        // A pause, not a failure: the reset time is on the notice above it.
        RateLimited => (
            Glyph::Interrupted,
            t(Msg::StopRateLimited).into_owned(),
            warn,
        ),
        // A failure: the stream went silent and retrying did not bring it back.
        Timeout => (Glyph::Fail, t(Msg::StopTimeout).into_owned(), bad()),
        // A failure, with the provider's own sentence folded in below.
        ProviderError => (Glyph::Fail, t(Msg::StopCancelled).into_owned(), bad()),
        // Not a failed request: the log cannot explain what reached the model,
        // and from here resume, fork and compaction are unsound. A person is
        // owed that in words rather than sharing a sentence with a dead
        // network — what they do next is start a new session, not retry.
        InvariantViolated => (
            Glyph::Fail,
            t(Msg::StopInvariantViolated).into_owned(),
            bad(),
        ),
        // The kernel's two fuses. One `StopReason` now serves the log and the
        // handle (`docs/adr/0021` §6), so these can reach a screen too.
        MaxContinuations => (
            Glyph::Interrupted,
            t(Msg::StopMaxContinuations).into_owned(),
            warn,
        ),
        RepeatLoop => (Glyph::Interrupted, t(Msg::StopToolLoop).into_owned(), warn),
        // `StopReason` is `non_exhaustive`: a cause added later still ends the
        // turn visibly rather than failing to compile a screen.
        _ => (Glyph::Interrupted, t(Msg::StopCancelled).into_owned(), warn),
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
                "{}:{}:{}:{}:{}:{}:{}",
                self.stats.steps,
                self.stats.prompt,
                self.stats.completion,
                self.stats.cached,
                self.stats.tools,
                self.stats.elapsed_ms,
                self.done_index,
            ),
        ])
    }
    /// The turn's outcome and its cost on one light line at the left margin:
    /// `✻ Nailed it · 2 轮 · 2 工具 · 32.7s · 2.60K tokens · 97% cached`. It used to
    /// be a full-width captioned rule (`───── … ─────`), but a transcript of turns
    /// drew a screen full of ─ boundaries; the glyph and the dim carry the boundary
    /// now, closer to `✻ Crunched for …`.
    ///
    /// The cost rides that same line because its end is exactly where a person asks
    /// "what did that take". It is the *last* thing added and the first dropped: the
    /// line is built widest-first and falls back to the outcome alone, with whatever
    /// did not fit going under it, wrapped — the same ladder the cause of a failed
    /// turn already climbed.
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        // A turn you stopped yourself closes on the composer, not here: the dim
        // `已中断 · …` line under the field carries it (driven by
        // `moment.interrupted`), so the transcript drops the centered separator
        // for a cancel rather than draw a boundary the composer already draws.
        if matches!(self.stop, StopReason::Cancelled) {
            return Vec::new();
        }
        let w = ctx.width;
        let caps = Caps::default();
        let (mark, said, style) = turn_end_note(self.stop, self.done_index);
        let short = format!("{} {said}", caps.g(mark));

        // Under the rule, in the order they are worth reading: the cost first
        // (it is about this turn), then the cause of a failure. The cache ratio
        // rides only a clean stop, the way tuix drops it from a failed turn.
        let mut under: Vec<String> = Vec::new();
        let mut caption = short;
        let with_cached = matches!(self.stop, StopReason::Stopped);
        if let Some(stats) = self.stats.caption(with_cached) {
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

        // A light, left-aligned line rather than a full-width captioned rule: a
        // screen of ─ boundaries reads as noise, and a turn's end is already set
        // apart by its outcome glyph and the dim of its cost. The mark and the
        // figures ride at the margin the way the rule's caption did, minus the
        // rule — nearer `✻ Crunched for …` than `───── ✓ Done ─────`.
        let mut out = vec![Line::styled(width::take_width(&caption, w as usize), style)];
        for line in under {
            out.extend(wrapped(&line, w, style, "  "));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_vl_caption_pulls_the_recognition_out_of_a_user_message() {
        // Built exactly as the runtime does: the person's words, a blank line,
        // then the VL marker (locale-round-tripped so the test is not a hardcode).
        let said = pt(PMsg::VisionRecognised {
            model: "qwen-vl",
            text: "a long recognised description",
        })
        .into_owned();
        let msg = format!("[Image #1] 这个是啥？\n\n{said}");
        let (before, model, caption) = split_vl_caption(&msg).expect("the caption is split out");
        assert_eq!(before, "[Image #1] 这个是啥？");
        assert_eq!(model, "qwen-vl");
        assert_eq!(caption, "a long recognised description");
        // An ordinary message has none.
        assert!(split_vl_caption("just a question").is_none());
        // A caption with no user words folds to an empty `before`.
        let (before, ..) = split_vl_caption(&said).expect("marker-only splits too");
        assert!(before.is_empty(), "no words before the marker: {before:?}");
        // A person who typed the marker prefix themselves does not steal the
        // split: the runtime's marker is the one appended at the end.
        let decoy = pt(PMsg::VisionRecognised {
            model: "decoy",
            text: "typed by the user",
        })
        .into_owned();
        let real = pt(PMsg::VisionRecognised {
            model: "real-vl",
            text: "the actual recognition",
        })
        .into_owned();
        let msg = format!("quoting {decoy}\n\n{real}");
        let (_, model, caption) = split_vl_caption(&msg).expect("splits at the last marker");
        assert_eq!(
            model, "real-vl",
            "the appended marker wins, not the typed one"
        );
        assert_eq!(caption, "the actual recognition");
    }

    #[test]
    fn a_vl_caption_block_folds_to_one_row_and_opens_to_the_recognition() {
        let block = VlCaptionBlock {
            model: "qwen-vl".into(),
            text: "line one\nline two\nline three".into(),
        };
        let ctx = crate::block::RenderCtx::bare(80);
        // Folded: one row, the `●` head naming the model and the char count.
        let folded = block.summary_lines(&ctx);
        assert_eq!(folded.len(), 1, "folds to a single row: {folded:?}");
        let head = folded[0].plain();
        assert!(head.contains("qwen-vl"), "names the model: {head}");
        assert!(
            !head.contains("line two"),
            "the body is hidden when folded: {head}"
        );
        // The dot carries the outcome: green for a good recognition.
        assert_eq!(
            folded[0].spans[0].style.fg,
            Some(Color::role(crate::theme::Role::Success)),
            "the ● is green: {:?}",
            folded[0].spans[0]
        );
        // Open: the head plus the recognised text.
        let open = block.lines(&ctx);
        assert!(open.len() > 1, "opens to more than the head: {open:?}");
        assert!(
            open.iter().any(|l| l.plain().contains("line two")),
            "the recognition is there when opened"
        );
    }

    /// An address a command says is a link on every row it wraps onto.
    ///
    /// Found at a real terminal: an MCP sign-in's authorization URL, wider than
    /// the screen, was cut into rows a selection copies with the breaks in them
    /// — a link that no longer opens. As a hyperlink each piece carries the
    /// whole address, so a click opens it however it wrapped.
    #[test]
    fn an_address_a_command_says_is_a_link_on_every_row_it_wraps_onto() {
        let url = "https://mcp.linear.app/authorize?response_type=code&client_id=2aRtnaX2z9oqKLsZ&state=fdd43d4a-a686-4a8e-a010-2daa9a3cce54";
        let said = CommandSaid {
            text: format!("浏览器没有打开的话,复制这个链接去打开:{url}"),
            refused: false,
        };
        let lines = said.lines(&wctx(40, false));
        let linked: Vec<&Span> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.link.is_some())
            .collect();
        assert!(lines.len() > 2, "the address wrapped: {lines:?}");
        assert!(
            linked.iter().all(|span| span.link.as_deref() == Some(url)),
            "every piece links to the whole address"
        );
        let pieces: String = linked.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(
            pieces, url,
            "the linked pieces are the address, and all of it"
        );
        assert!(
            lines
                .iter()
                .all(|line| width::str_width(&line.plain()) <= 40),
            "no row runs past the width"
        );
        // And a line with no address is left as it was.
        let plain = CommandSaid {
            text: "没有地址的一句话".into(),
            refused: false,
        };
        assert!(plain
            .lines(&wctx(40, false))
            .iter()
            .all(|line| line.spans.iter().all(|span| span.link.is_none())));
    }

    fn welcome() -> WelcomeBlock {
        WelcomeBlock {
            cwd: "~/proj".into(),
            model: Some("a-model".into()),
            version: "9.9.9",
            heading: "上手提示".into(),
            tips: vec![
                ("/resume".into(), "接着上次".into()),
                ("/help".into(), "列出所有命令".into()),
            ],
            brand: std::sync::Arc::new(Brand::default()),
        }
    }

    /// A render context with the two shape bits a caller cares about.
    fn wctx(width: u16, cell_background: bool) -> RenderCtx {
        RenderCtx {
            width,
            caps: crate::block::ShapeCaps {
                unicode: true,
                colors: crate::caps::Colors::Ansi256,
                cell_background,
            },
        }
    }

    fn lines_of(block: &WelcomeBlock, width: u16, cell_background: bool) -> Vec<String> {
        block
            .lines(&wctx(width, cell_background))
            .iter()
            .map(Line::plain)
            .collect()
    }

    #[test]
    fn the_welcome_says_the_four_things_it_has() {
        let all = lines_of(&welcome(), 80, true).join("\n");
        for want in ["AtomCode", "9.9.9", "~/proj", "a-model", "/resume", "/help"] {
            assert!(all.contains(want), "{want} missing from:\n{all}");
        }
    }

    #[test]
    fn the_mascot_is_the_same_cat_tuix_draws() {
        // The art and its palette are tuix's, unchanged — a "close enough" redraw
        // would be a second cat to keep in step. Checked against the shipped
        // value rather than against a transcription of it.
        let art = Mascot::default();
        let cells = art.cells();
        for (i, row) in art.rows.iter().enumerate() {
            assert_eq!(
                row.chars().count(),
                cells * 2,
                "row {i} is not {cells} cells of two pixels"
            );
            assert!(
                row.chars()
                    .all(|c| c == '.' || art.palette.contains_key(&c)),
                "row {i} has a legend character the palette does not colour"
            );
        }
        assert_eq!(
            art.palette.get(&'o'),
            Some(&202),
            "orange, as tuix bakes it"
        );
        assert_eq!(art.palette.get(&'e'), Some(&166), "dark-orange eyebrow");
        assert_eq!(art.palette.get(&'w'), Some(&231), "white highlight");
        assert_eq!(art.palette.get(&'k'), Some(&232), "black pupil");
        assert_eq!(art.palette.get(&'.'), None, "transparent, not a colour");

        // tuix's own judgement on the art: one white highlight per eye, eyebrows
        // above them. If the bytes are ever edited, this says what they must keep.
        let eyes = &art.rows[1];
        assert_eq!(eyes.matches('w').count(), 2, "one highlight per eye");
        assert_eq!(
            eyes.matches('e').count(),
            4,
            "an eyebrow per eye, 2 cells wide"
        );
    }

    /// The identity is data: a build that calls itself something else says so,
    /// draws its own art, and the tips still line up beside art of another size.
    ///
    /// This is the one a fork needs. Before it, changing the name meant editing
    /// the layout code and the cell count in three places.
    #[test]
    fn another_build_can_call_itself_something_else() {
        let mine = Brand {
            name: "◆ 龙仔".into(),
            licence: "内部使用".into(),
            mascot: Some(Mascot {
                // Three cells wide rather than nine, to catch a width that was
                // taken from a constant instead of from the art.
                rows: vec!["oo..oo".into(), "..oo..".into()],
                palette: [('o', 40u8)].into_iter().collect(),
            }),
        };
        let block = WelcomeBlock {
            brand: std::sync::Arc::new(mine.clone()),
            ..welcome()
        };
        let all = lines_of(&block, 80, true).join("\n");
        assert!(all.contains("龙仔"), "its own name:\n{all}");
        assert!(all.contains("内部使用"), "its own licence:\n{all}");
        assert!(!all.contains("AtomCode"), "and not ours:\n{all}");
        assert!(!all.contains("MIT"), "nor our licence:\n{all}");

        // Its own colour, and none of the shipped cat's.
        let lines = block.lines(&wctx(80, true));
        let mut colours: Vec<u8> = Vec::new();
        let index = |c: Option<Color>| match c {
            Some(Color::Picture(n)) => Some(n),
            _ => None,
        };
        for span in lines.iter().flat_map(|line| &line.spans) {
            colours.extend(index(span.style.fg));
            colours.extend(index(span.style.bg));
        }
        assert!(colours.contains(&40), "the new art's colour: {colours:?}");
        assert!(!colours.contains(&202), "not the cat's orange: {colours:?}");

        // The tips sit beside art of the new width, not of the old one.
        let tip = lines_of(&block, 80, true)
            .into_iter()
            .find(|line| line.contains("/resume"))
            .expect("a tip");
        let indent = tip[..tip.find("/resume").expect("found")].chars().count();
        assert_eq!(indent, PAD + 3 + 4, "PAD + three cells + the gap: {tip:?}");

        // And a build with no mascot at all draws none.
        let bare = WelcomeBlock {
            brand: std::sync::Arc::new(Brand {
                mascot: None,
                ..mine
            }),
            ..welcome()
        };
        let drawn = lines_of(&bare, 80, true).join("\n");
        assert!(
            !drawn.contains('\u{2580}') && !drawn.contains('\u{2584}'),
            "{drawn}"
        );
    }

    #[test]
    fn the_mascot_states_its_own_colours_rather_than_asking_for_a_role() {
        // It is a picture of an orange cat, not a request for "the brand colour".
        // Written with roles it came out magenta (Brand → xterm 13) and read as a
        // bug: there is no role in the vocabulary that means "orange". So the art
        // carries literal indices, and the gate below is what protects a terminal
        // that cannot show them.
        let lines = welcome().lines(&wctx(80, true));
        // Both pixels, because which one a colour lands on is the art's business:
        // the highlight sits *below* the eyebrow in the same cell (`ew`), so it
        // arrives as a background. Collecting foregrounds only would report the
        // white as missing when it is right there.
        let mut colours: Vec<u8> = Vec::new();
        let mut backgrounds = 0usize;
        for span in lines.iter().flat_map(|line| &line.spans) {
            let index = |c: Option<Color>| match c {
                Some(Color::Picture(n)) => Some(n),
                _ => None,
            };
            if let Some(n) = index(span.style.fg) {
                colours.push(n);
            }
            if let Some(n) = index(span.style.bg) {
                colours.push(n);
                backgrounds += 1;
            }
        }
        for want in [202u8, 166, 231, 232] {
            assert!(
                colours.contains(&want),
                "the palette index {want} never reached a cell: {colours:?}"
            );
        }
        assert!(
            backgrounds > 0,
            "no cell carried a background pixel — two colours per cell is what makes \
             this art possible"
        );
        // Per **span**, not per line: the mascot shares its rows with the tips
        // (two columns), and the tips are roles on purpose. So the claim is about
        // the spans that actually carry block glyphs.
        let glyph_spans: Vec<&Span> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.text.contains('\u{2580}') || span.text.contains('\u{2584}'))
            .collect();
        assert!(!glyph_spans.is_empty(), "no mascot spans found");
        assert!(
            !glyph_spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Role(_)))),
            "the cat must not be drawn from roles"
        );
        // And they do carry the art's own indices.
        assert!(
            glyph_spans
                .iter()
                .all(|s| matches!(s.style.fg, Some(Color::Picture(_)))),
            "every block glyph should carry the picture's index: {glyph_spans:?}"
        );
    }

    #[test]
    fn the_mascot_needs_a_background_and_says_so_by_not_drawing() {
        // tuix's gate, kept exactly: `colors && unicode_symbols && (modern ||
        // jediterm)`. Everything else **draws nothing**, because on a terminal that
        // drops backgrounds the half-block art fragments — and a version that
        // "coped" by filling both pixels with the upper colour drew a solid orange
        // rectangle that looked like a loading placeholder.
        let with = lines_of(&welcome(), 80, true).join("\n");
        assert!(with.contains('▀'), "no mascot at all:\n{with}");

        for (why, ctx) in [
            (
                "no cell background",
                RenderCtx {
                    width: 80,
                    caps: crate::block::ShapeCaps {
                        cell_background: false,
                        ..wctx(80, true).caps
                    },
                },
            ),
            (
                "no colour at all",
                RenderCtx {
                    width: 80,
                    caps: crate::block::ShapeCaps {
                        colors: crate::caps::Colors::None,
                        ..wctx(80, true).caps
                    },
                },
            ),
        ] {
            let all = welcome()
                .lines(&ctx)
                .iter()
                .map(Line::plain)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !all.contains('▀') && !all.contains('▄'),
                "half blocks were drawn with {why} — the art would fragment:\n{all}"
            );
            // And no solid-block stand-in either: that was the bug.
            assert!(!all.contains('█'), "a rectangle is not a cat:\n{all}");
            // The words are still there: only the art is withheld.
            assert!(all.contains("AtomCode") && all.contains("~/proj"), "{all}");
        }
    }

    #[test]
    fn a_terminal_with_no_unicode_gets_no_mascot() {
        let none = RenderCtx {
            width: 80,
            caps: crate::block::ShapeCaps {
                unicode: false,
                ..wctx(80, true).caps
            },
        };
        let all = welcome()
            .lines(&none)
            .iter()
            .map(Line::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !all.contains('█') && !all.contains('▀'),
            "a grid of tofu is not a picture:\n{all}"
        );
        // The words are still there: only the art needs Unicode.
        assert!(all.contains("AtomCode") && all.contains("~/proj"));
    }

    #[test]
    fn cwd_and_model_land_below_the_tips_and_never_on_top_of_them() {
        // Tuix's bug, kept as a judgement: when the tips are taller than the cat,
        // the spare rows used to land on these two, and one line read `∙ proj`
        // and `set a goal…` at once.
        let lines = lines_of(&welcome(), 80, true);
        let cwd_row = lines
            .iter()
            .position(|l| l.contains("~/proj"))
            .expect("cwd");
        let tip_row = lines
            .iter()
            .position(|l| l.contains("/resume"))
            .expect("tips");
        assert!(
            cwd_row > tip_row,
            "the bullets belong under the block, not beside the tips:\n{lines:#?}"
        );
    }

    #[test]
    fn nothing_that_fits_means_no_block_rather_than_a_blank_one() {
        // A zero-row block still occupies a slot, and `blank_between` would leave a
        // blank row for it — the screen would gain a stray empty line.
        for width in [0u16, 1, 2, 3] {
            assert!(
                welcome().lines(&wctx(width, true)).is_empty(),
                "width {width} fits nothing, so there is no block"
            );
        }
    }

    #[test]
    fn the_welcome_cannot_be_folded_away() {
        assert!(
            welcome().always_open(),
            "its whole point is that it happened"
        );
    }

    #[test]
    fn the_hash_covers_what_it_says_and_not_how_it_draws() {
        assert_eq!(welcome().content_hash(), welcome().content_hash());
        assert_ne!(
            welcome().content_hash(),
            WelcomeBlock {
                model: None,
                ..welcome()
            }
            .content_hash(),
            "a different model is a different block"
        );
    }

    #[test]
    fn no_row_is_wider_than_the_width_it_was_given() {
        // The invariant every block owes the frame, across widths and both shapes
        // of terminal.
        for width in [4u16, 9, 20, 40, 61, 80, 120] {
            for cell_background in [true, false] {
                for line in welcome().lines(&wctx(width, cell_background)) {
                    assert!(
                        line.width() <= width as usize,
                        "at {width} (background={cell_background}): {line:?} is {} cells",
                        line.width()
                    );
                }
            }
        }
    }

    #[test]
    fn an_empty_tip_list_drops_the_whole_column_and_not_just_the_heading() {
        // A heading with nothing under it announces nothing.
        let bare = WelcomeBlock {
            tips: Vec::new(),
            ..welcome()
        };
        let all = lines_of(&bare, 80, true).join("\n");
        assert!(!all.contains("上手提示"), "{all}");
        assert!(all.contains("~/proj"), "the rest is still there: {all}");
    }

    #[test]
    fn the_art_source_is_well_formed() {
        // The constant is borrowed from tuix; if it were ever edited, this says
        // what the reader below assumes.
        let art = Mascot::default();
        let cells = art.cells();
        for (i, row) in art.rows.iter().enumerate() {
            assert_eq!(
                row.chars().count(),
                cells * 2,
                "row {i} is not {cells} cells of two pixels"
            );
            assert!(
                row.chars()
                    .all(|c| matches!(c, '.' | 'o' | 'e' | 'w' | 'k')),
                "row {i} has a legend character nobody draws"
            );
        }
    }

    /// A block as it reaches the screen, as one string.
    fn drawn(block: &dyn Content, w: u16) -> String {
        block
            .lines(&crate::block::RenderCtx::bare(w))
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
            done_index: 0,
        };
        let lines = block.lines(&crate::block::RenderCtx::bare(100));
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
            stop: StopReason::ProviderError,
            error: Some("quota reached".into()),
            stats: TurnStats::default(),
            done_index: 0,
        };
        let lines = short.lines(&crate::block::RenderCtx::bare(100));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].plain().contains("已中断 · quota reached"));
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
                done_index: 0,
            }
            .lines(&crate::block::RenderCtx::bare(80))
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
            // The clean end rotates through `DONE_LABELS`; index 0 is `Done`.
            clean.contains("Done") && !clean.contains("Stopped"),
            "{clean}"
        );

        // A turn you stopped yourself draws no separator at all: it closes on the
        // dim `已中断` line under the composer instead (see `modules::input`), so
        // the transcript block is empty. Its label still exists for that line —
        // `turn_end_note` below proves the mark — it just is not drawn here.
        let cancelled = drawn(StopReason::Cancelled);
        assert!(
            cancelled.trim().is_empty(),
            "a cancel closes on the composer, not a transcript separator: {cancelled:?}"
        );

        let failed = drawn(StopReason::ProviderError);
        assert!(failed.contains("已中断"), "{failed}");

        // Every reason is one of two outcomes, and the mark says which. The clean
        // end wears the `✻` sparkle; nothing cut short does.
        let mark = |stop| turn_end_note(stop, 0).0;
        assert_eq!(mark(StopReason::Stopped), Glyph::Sparkle);
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
            assert_ne!(mark(cut), Glyph::Sparkle, "{cut:?} was not a clean end");
        }
    }

    /// What a turn cost, on the line that closes it, in tuix's shape:
    /// `✻ Done · 4 轮 · 2 工具 · 32.7s · 4.36K tokens · 99% cached`.
    ///
    /// The figures are a real reading, not invented: a four-round turn whose
    /// last request carried 90659 tokens of context, 90496 of them served from
    /// cache, and which produced 4200 tokens of output — so the turn's billable
    /// cost is `4200 + (90659 - 90496) = 4363` tokens.
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
                tools: 2,
                elapsed_ms: 32_700,
            },
            done_index: 0,
        };
        let text = drawn(&block, 100);
        for want in [
            "Done",
            "4 轮",
            "2 工具",
            "32.7s",
            "4.36K tokens",
            "99% cached",
        ] {
            assert!(text.contains(want), "{want} missing from {text:?}");
        }
        assert_eq!(
            block.lines(&crate::block::RenderCtx::bare(100)).len(),
            1,
            "one rule, not a paragraph"
        );
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
                tools: 0,
                elapsed_ms: 1_200,
            },
            done_index: 0,
        };
        let text = drawn(&block, 80);
        assert!(!text.contains("cached"), "{text:?}");
        // Billable with nothing cached is the whole request plus its output:
        // `28 + 6223 = 6251` → `6.25K`.
        assert!(
            text.contains("6.25K tokens"),
            "the rest is still said: {text:?}"
        );
    }

    /// A turn the log recorded nothing about — cut before its first request —
    /// is drawn exactly as it was before there were figures to draw. (Not a
    /// self-cancel, which draws no separator at all now — a provider that died
    /// before the first reply is the same "nothing recorded" shape and still
    /// closes the turn on a rule.)
    #[test]
    fn a_turn_with_nothing_recorded_says_only_how_it_ended() {
        let block = TurnEndBlock {
            stop: StopReason::ProviderError,
            error: None,
            stats: TurnStats::default(),
            done_index: 0,
        };
        let lines = block.lines(&crate::block::RenderCtx::bare(80));
        assert_eq!(lines.len(), 1, "nothing to say means no extra row");
        let text = drawn(&block, 80);
        assert!(text.contains("已中断"), "{text:?}");
        for absent in ["轮", "工具", "tokens", "cached"] {
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
                tools: 2,
                elapsed_ms: 32_700,
            },
            done_index: 0,
        };
        let outcome = format!("{} Done", Caps::default().g(Glyph::Ok));
        let first = (0..200u16)
            .find(|w| crate::el::caption_fits(&outcome, *w as usize))
            .expect("the outcome fits on some screen");
        for w in first..=120 {
            let text = drawn(&block, w);
            assert!(text.contains("Done"), "w={w}: {text:?}");
            // Read with the whitespace taken out: a narrow rule moves the
            // figures under itself, where they are wrapped mid-phrase — they
            // are all still said, which is the property.
            let flat: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            for want in ["4轮", "2工具", "32.7s", "4.36Ktokens", "99%cached"] {
                assert!(flat.contains(want), "w={w}: {want} lost from {text:?}");
            }
            for line in block.lines(&crate::block::RenderCtx::bare(w)) {
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
                tools: 87,
                elapsed_ms: 3_725_000,
            },
            done_index: 0,
        };
        for w in 0..160u16 {
            for line in block.lines(&crate::block::RenderCtx::bare(w)) {
                assert!(line.width() <= w as usize, "w={w}: {:?}", line.plain());
            }
        }
    }

    #[test]
    fn token_counts_are_exact_while_that_is_readable_and_rounded_after() {
        assert_eq!(token_count(0), "0");
        assert_eq!(token_count(28), "28");
        assert_eq!(token_count(999), "999");
        // From a thousand it is `k` with one decimal kept, so `入`/`出` share a unit.
        assert_eq!(token_count(1_000), "1.0k");
        assert_eq!(token_count(8_003), "8.0k");
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
                done_index: 0,
            }),
            Box::new(TurnEndBlock {
                stop: StopReason::Cancelled,
                error: Some("by the user".into()),
                stats: TurnStats::default(),
                done_index: 0,
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
                    tools: 15,
                    elapsed_ms: 92_400,
                },
                done_index: 1,
            }),
            Box::new(TurnEndBlock {
                stop: StopReason::ProviderError,
                error: Some("connection reset by peer while reading the response body".into()),
                stats: TurnStats {
                    steps: 3,
                    prompt: 62_120,
                    completion: 812,
                    cached: 0,
                    tools: 4,
                    elapsed_ms: 15_000,
                },
                done_index: 0,
            }),
        ];
        for item in &items {
            for w in 0..60u16 {
                for line in item.lines(&crate::block::RenderCtx::bare(w)) {
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
    fn an_edit_file_call_renders_a_coloured_diff() {
        let block = ToolCallBlock {
            call_id: "c".into(),
            name: "edit_file".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
            outcome: Outcome::Ok(
                "Edited a.rs (1 replacement)\n@@ -1,2 +1,2 @@\n keep\n-old\n+new".into(),
            ),
        };
        let lines = block.lines(&crate::block::RenderCtx::bare(80));
        // The `(+N -M)` count rides the naming line, right after the file — not a
        // separate summary row.
        let naming = lines
            .iter()
            .find(|l| l.plain().contains("EditFile"))
            .expect("the naming line");
        assert!(
            naming.plain().contains("(+1 -1)"),
            "count on the name line: {}",
            naming.plain()
        );
        let text: String = lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("- old") && text.contains("+ new"),
            "diff rows: {text}"
        );
        let red = Some(crate::frame::Color::role(Role::DiffRemove));
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.fg == red),
            "a removed row is red"
        );
    }

    #[test]
    fn a_write_file_call_renders_its_content_as_green_additions() {
        let block = ToolCallBlock {
            call_id: "c".into(),
            name: "write_file".into(),
            args: r#"{"file_path":"a.html","content":"<h1>hi</h1>\nbye"}"#.into(),
            outcome: Outcome::Ok("Wrote a.html".into()),
        };
        let lines = block.lines(&crate::block::RenderCtx::bare(80));
        let naming = lines
            .iter()
            .find(|l| l.plain().contains("WriteFile"))
            .expect("the naming line");
        assert!(
            naming.plain().contains("(+2 -0)"),
            "count on the name line: {}",
            naming.plain()
        );
        let text: String = lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("+ <h1>hi</h1>") && text.contains("+ bye"),
            "the written content as additions: {text}"
        );
        let green = Some(crate::frame::Color::role(Role::DiffAdd));
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.fg == green),
            "an added row is green"
        );
    }

    #[test]
    fn the_hash_ignores_width_and_folding_but_not_the_words() {
        let a = ModelSaid("hello".into());
        assert_eq!(a.content_hash(), a.content_hash());
        // Rendering at different widths, and asking for the folded form, must
        // not change what the block *says*.
        let _ = a.lines(&crate::block::RenderCtx::bare(10));
        let _ = a.lines(&crate::block::RenderCtx::bare(200));
        let _ = a.summary(&crate::block::RenderCtx::bare(10));
        assert_eq!(a.content_hash(), ModelSaid("hello".into()).content_hash());
        assert_ne!(a.content_hash(), ModelSaid("hellp".into()).content_hash());
    }

    #[test]
    fn a_result_arriving_changes_the_hash_because_it_changes_what_is_said() {
        let pending = ToolCallBlock::pending("c1", "bash", "{}");
        let done = pending.with(Outcome::Ok("done".into()));
        assert_ne!(pending.content_hash(), done.content_hash());
    }

    /// A running call is NOT painted the warning yellow: a call in flight is not
    /// a warning, and the yellow read as one. Its `⋯` mark and its name take the
    /// terminal's own foreground; the `⋯` glyph and the live line below say "still
    /// going". A finished call keeps its own mark colour (✓ green, ✗ red), and none
    /// of the states borrow the warning role.
    ///
    /// The assertion is on the role, never on a colour: a test that named
    /// `#ffcc00` would pass on a palette where yellow reads as red.
    #[test]
    fn a_running_call_is_not_painted_the_warning_colour() {
        let warn = Some(crate::frame::Color::role(Role::Warning));

        let running = ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#);
        assert_ne!(
            running.mark().1.fg,
            warn,
            "a running call is not a warning: {:?}",
            running.mark()
        );
        assert_eq!(
            running.mark().0,
            "⋯",
            "the `⋯` glyph is what marks it in flight"
        );
        let head = running.lines(&crate::block::RenderCtx::bare(60)).remove(0);
        let named = head
            .spans
            .iter()
            .find(|s| s.text.contains("ReadFile"))
            .expect("the tool's name");
        assert_ne!(
            named.style.fg, warn,
            "the running name is not the warning yellow: {head:?}"
        );

        // A finished call's mark is its own colour, and never the warning one.
        for done in [
            Outcome::Ok("20 行".into()),
            Outcome::Failed("no such file".into()),
            Outcome::Interrupted,
        ] {
            let block =
                ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#).with(done);
            assert_ne!(block.mark().1.fg, warn, "{:?}", block.mark());
        }
    }

    /// `is_running` is what the host asks to decide whether to pulse a call's
    /// mark: true only while the call is in flight, false the moment any outcome
    /// lands.
    #[test]
    fn is_running_is_true_only_while_the_call_is_in_flight() {
        let live = ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#);
        assert!(live.is_running(), "a pending call is running");
        for done in [
            Outcome::Ok("20 行".into()),
            Outcome::Failed("no such file".into()),
            Outcome::Interrupted,
        ] {
            assert!(
                !live.with(done).is_running(),
                "a finished call is not running"
            );
        }
    }

    /// A todo update the model explained is its intent phrase and nothing else:
    /// one `● …` row, no `待办("action":…)` line and no result. A run of them
    /// then reads as its own summaries rather than a wall of state JSON.
    #[test]
    fn a_todo_update_is_just_its_intent_line_not_the_raw_args() {
        let call = ToolCallBlock::pending(
            "c",
            "todo",
            r#"{"intent":"转向 sha pin 兼容","action":"update","id":5,"status":"in_progress"}"#,
        )
        .with(Outcome::Ok("ok".into()));
        let lines = call.lines(&crate::block::RenderCtx::bare(80));
        assert_eq!(lines.len(), 1, "a todo update is one row: {lines:?}");
        let row = lines[0].plain();
        assert!(
            row.contains("转向 sha pin 兼容"),
            "the intent is the row: {row:?}"
        );
        assert!(!row.contains("action"), "the raw args are gone: {row:?}");
        assert!(
            !row.contains("in_progress"),
            "the status json is gone: {row:?}"
        );
        // Folded is that same single row — nothing to leave out.
        let folded = call.summary_lines(&crate::block::RenderCtx::bare(80));
        assert_eq!(folded.len(), 1, "folds to the one row: {folded:?}");
        assert!(folded[0].plain().contains("转向 sha pin 兼容"));
    }

    /// A todo update with no intent keeps the ordinary call shape: its args are
    /// then the only thing it has to say, so they are shown like any other
    /// unexplained call.
    #[test]
    fn a_todo_update_without_an_intent_keeps_its_args() {
        let call = ToolCallBlock::pending(
            "c",
            "todo",
            r#"{"action":"update","id":5,"status":"completed"}"#,
        );
        let text: String = call
            .lines(&crate::block::RenderCtx::bare(80))
            .iter()
            .map(|l| l.plain())
            .collect();
        // The ordinary call shape — the tool names itself and shows its
        // argument subject (`Todo(5)`), rather than my intent-only single row.
        assert!(
            text.contains("Todo"),
            "an unexplained todo keeps the ordinary call shape: {text:?}"
        );
    }

    /// A todo update that FAILED keeps the full shape so its error is not
    /// swallowed by the intent-only collapse: the collapse is for running/clean
    /// updates, and a todo that did not apply is exactly what the reader — and
    /// the model re-reading — must be told.
    #[test]
    fn a_failed_todo_update_still_shows_its_error() {
        let call = ToolCallBlock::pending(
            "c",
            "todo",
            r#"{"intent":"标记 #5 完成","action":"update","id":5,"status":"done"}"#,
        )
        .with(Outcome::Failed("未知的任务 id 5".into()));
        let text: String = call
            .lines(&crate::block::RenderCtx::bare(80))
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            text.contains("未知的任务 id 5"),
            "the failure text must survive the todo collapse: {text:?}"
        );
    }

    /// A folded call recedes: it is scaffolding over the answer rather than one
    /// more thing being said, so its rows take the muted grey — and take it
    /// *instead of* the state it is in. A run still going is yellow while it is
    /// open; folded it is muted like the rest of the chrome, because the reader
    /// has already been told it exists and the live line is where "still running"
    /// is stated. (An accent here read as louder than the open call.)
    ///
    /// The note is the exception, and deliberately: `失败 · …` is the answer
    /// rather than the summary, and a fold must not swallow that.
    #[test]
    fn a_folded_call_recedes_to_the_muted_grey_and_keeps_its_failure_note() {
        let receded = Some(crate::frame::Color::role(Role::Muted));
        let pending = ToolCallBlock::pending(
            "c",
            "read_file",
            r#"{"file_path":"/Users/x/crates/atomcode-tui/src/content.rs"}"#,
        );

        let folded = pending.summary_lines(&crate::block::RenderCtx::bare(80));
        let first = folded.first().expect("the folded call has a first row");
        let named = first
            .spans
            .iter()
            .find(|s| s.text.contains("ReadFile"))
            .expect("the tool's name");
        assert_eq!(named.style.fg, receded, "the folded name is not receding");
        let subject = first
            .spans
            .iter()
            .find(|s| s.text.contains("content.rs"))
            .expect("the subject");
        assert_eq!(
            subject.style.fg, receded,
            "the folded subject is not receding"
        );
        // Every row, so a folded line cannot be half-loud.
        for row in &folded {
            assert_ne!(
                row.spans.first().expect("the mark").style.fg,
                Some(crate::frame::Color::role(Role::Warning)),
                "a folded call is still painted as in flight: {row:?}"
            );
        }

        // And the note survives the fold in its own colour.
        let failed = pending.with(Outcome::Failed("no such file".into()));
        let rows = failed.summary_lines(&crate::block::RenderCtx::bare(80));
        let note = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .find(|s| s.text.contains("失败"))
            .expect("the failure note");
        assert_eq!(
            note.style.fg,
            Some(crate::frame::Color::role(Role::Error)),
            "a fold swallowed the one thing that had to survive it: {rows:?}"
        );

        // A run still at the visible end of the stream draws the call in
        // flight — the same two rows a single folded call draws, finished or
        // not; once something visible has followed it, the lid is the count
        // alone. The receding volume is the folded call's question
        // (`summary_lines`), so here it is only asserted that the two forms stay
        // the two forms.
        let running = ToolCallBlock::group_lines(&pending, 3, 0, true, 80);
        assert!(
            running.iter().any(|l| l.plain().contains("ReadFile")),
            "a live run does not show the call in it: {running:?}"
        );
        assert!(running.len() >= 2, "the call in flight lost its note");
        let done = ToolCallBlock::group_lines(&failed, 3, 0, false, 80);
        assert_eq!(
            done.len(),
            1,
            "a followed run is the count and nothing else: {done:?}"
        );
        assert!(
            done[0].plain().contains("已执行了 3 个工具"),
            "the settled lid does not say how much ran: {done:?}"
        );
    }

    #[test]
    fn a_folded_thought_says_how_much_it_is_hiding() {
        let t = ModelThought("one\ntwo\nthree".into());
        assert!(t
            .summary(&crate::block::RenderCtx::bare(40))
            .plain()
            .contains("思考 3 行"));
    }

    #[test]
    fn arguments_are_shown_in_the_form_a_person_scans() {
        // `brief` used to render `file_path=a.rs`, which reads like a debug
        // dump. `subject_of` picks the argument that names the thing acted on,
        // so the line reads `ReadFile(a.rs)` — and it has a fallback, so a
        // tool nobody wrote a rule for still says something.
        let c = ToolCallBlock::pending("c", "read_file", r#"{"file_path":"a.rs"}"#);
        assert_eq!(subject_of(&c.name, &c.args), "a.rs");
        assert!(c
            .summary(&crate::block::RenderCtx::bare(40))
            .plain()
            .contains("ReadFile(a.rs)"));

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
        // Asserted through `summary`, because that is where the cut lives for a
        // call nobody explained: `subject_of` returns the command whole and the
        // folded line abbreviates it to the room it has.
        let command = "中文".repeat(15) + "/尾";
        assert!(
            !command.is_char_boundary(command.len() - 44),
            "the sample must reproduce the old panic, or it guards nothing"
        );
        let c = ToolCallBlock::pending("c", "bash", format!(r#"{{"command":"{command}"}}"#));
        let line = c.summary(&crate::block::RenderCtx::bare(60)).plain();
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

        let lid = c.summary(&crate::block::RenderCtx::bare(200)).plain();
        assert!(
            !lid.contains('\n') && !lid.contains('\r'),
            "the lid is more than one row: {lid:?}"
        );
        // Still the command, still readable at both ends, opened by the status
        // dot (muted while it runs).
        assert!(lid.starts_with("● $(cd /tmp"), "{lid:?}");
        assert!(lid.ends_with("PY) · 运行中"), "{lid:?}");

        // Expanded: over as many rows as it takes, and every one of them one row.
        let rows = c.lines(&crate::block::RenderCtx::bare(200));
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
        let a = width::str_width(&ascii.summary(&crate::block::RenderCtx::bare(60)).plain());
        let c = width::str_width(&cjk.summary(&crate::block::RenderCtx::bare(60)).plain());
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
        let line = c.summary(&crate::block::RenderCtx::bare(48)).plain();
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
                .lines(&crate::block::RenderCtx::bare(w))
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
            for line in c.lines(&crate::block::RenderCtx::bare(w)) {
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
        assert_eq!(
            b.lines(&crate::block::RenderCtx::bare(40))[0].plain(),
            "[reminder] keep going"
        );
        assert_eq!(
            b.summary(&crate::block::RenderCtx::bare(40)).plain(),
            "[reminder]"
        );
    }

    /// A call the model explained: the head row is the reason and nothing else,
    /// and the call itself moves to the gutter row under it — tool name and all.
    ///
    /// Both halves matter and they are asserted together on purpose. A build that
    /// dropped the tool's name from the row below would leave a bare path a
    /// reader cannot tell was read, written or searched; one that kept the name
    /// on the head would answer "why" by putting the mechanics back in front of
    /// it.
    #[test]
    fn a_reason_takes_the_head_and_the_call_moves_to_the_gutter() {
        let c = ToolCallBlock::pending(
            "c",
            "read_file",
            r#"{"file_path":"src/auth.rs","intent":"finding where credentials are loaded"}"#,
        );
        let rows: Vec<String> = c
            .lines(&crate::block::RenderCtx::bare(80))
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows[0].contains("finding where credentials are loaded"),
            "the head line carries the reason: {rows:?}"
        );
        assert!(
            !rows[0].contains("ReadFile") && !rows[0].contains("src/auth.rs"),
            "and nothing else — the call names itself on the row below: {rows:?}"
        );
        assert!(
            rows[1].contains("ReadFile(src/auth.rs)"),
            "the call keeps its tool name where the reason took the head: {rows:?}"
        );
    }

    /// The negative control: a call nobody annotated draws exactly what it drew
    /// before this existed — one head line, no extra row.
    ///
    /// Without this the whole feature could be a rewrite of every call in every
    /// existing session, and the criteria above would not notice.
    #[test]
    fn a_call_without_a_reason_draws_exactly_as_before() {
        let c = ToolCallBlock::pending("c", "read_file", r#"{"file_path":"src/auth.rs"}"#);
        let rows: Vec<String> = c
            .lines(&crate::block::RenderCtx::bare(80))
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(
            rows[0].contains("ReadFile(src/auth.rs)"),
            "the head is unchanged: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("src/auth.rs")).count(),
            1,
            "the subject is named once, on the head — no gutter line is added: {rows:?}"
        );
    }

    /// For a tool the table has never heard of, `subject_of` falls back to the
    /// raw argument text — and the reason is an argument. It must not end up
    /// flattened into that line, in either shape.
    #[test]
    fn an_unknown_tools_subject_never_quotes_the_reason() {
        let c = ToolCallBlock::pending(
            "c",
            "some_new_tool",
            r#"{"thing":"x.rs","intent":"checking the thing"}"#,
        );
        let ctx = crate::block::RenderCtx::bare(120);
        let rows: Vec<String> = c.lines(&ctx).iter().map(|l| l.plain()).collect();
        assert!(
            rows.iter().any(|r| r.contains("x.rs")),
            "the fallback still names the argument: {rows:?}"
        );
        assert!(
            !rows[1].contains("intent") && !rows[1].contains("checking the thing"),
            "the reason is not part of what it acted on: {rows:?}"
        );
        // The folded form draws those same rows, so it has to agree.
        let folded: Vec<String> = c.summary_lines(&ctx).iter().map(|l| l.plain()).collect();
        assert!(
            !folded.iter().any(|r| r.contains("intent")),
            "a folded row scanned for what ran must not quote the reason: {folded:?}"
        );
        assert_eq!(
            c.lines(&ctx).len().min(2),
            folded.len(),
            "folding takes the first two rows and no rewriting: {folded:?}"
        );
    }

    /// The folded call is the OPEN call's first two rows, minus the result.
    ///
    /// This is the whole contract of the shape: the two must be the same rows,
    /// so that folding changes how much you see and not what you are looking at
    /// — and a second, hand-written summary is exactly how they drift apart.
    #[test]
    fn a_folded_call_is_the_open_calls_first_two_rows_without_the_result() {
        let c = ToolCallBlock::pending(
            "c",
            "read_file",
            r#"{"file_path":"src/auth.rs","intent":"finding where credentials are loaded"}"#,
        )
        .with(Outcome::Ok("20 行".into()));
        let ctx = crate::block::RenderCtx::bare(80);
        let open: Vec<String> = c.lines(&ctx).iter().map(|l| l.plain()).collect();
        let folded: Vec<String> = c.summary_lines(&ctx).iter().map(|l| l.plain()).collect();

        assert_eq!(
            folded.len(),
            2,
            "explained calls fold to two rows: {folded:?}"
        );
        assert_eq!(
            folded,
            open[..2].to_vec(),
            "the folded rows must BE the open ones: {folded:?} vs {open:?}"
        );
        assert!(
            !folded.iter().any(|r| r.contains("20 行")),
            "and the result is what folding leaves out: {folded:?}"
        );
    }

    /// A successful call folds without its result; a call that is still running,
    /// was interrupted, or failed still says so — on the second row, not a third.
    ///
    /// The mark is the same muted `●` in every state, so that note is the only
    /// thing left on a folded row that would say anything happened at all.
    #[test]
    fn a_folded_call_keeps_its_outcome_note_except_when_it_succeeded() {
        let ctx = crate::block::RenderCtx::bare(80);
        let call = || {
            ToolCallBlock::pending(
                "c",
                "read_file",
                r#"{"file_path":"src/auth.rs","intent":"finding the credentials loader"}"#,
            )
        };
        let folded = |b: ToolCallBlock| -> Vec<String> {
            b.summary_lines(&ctx).iter().map(|l| l.plain()).collect()
        };

        assert!(
            folded(call()).last().expect("a row").contains("运行中"),
            "a call still in flight must not fold to silence: {:?}",
            folded(call())
        );

        let rows = folded(call().with(Outcome::Failed("no such file".into())));
        assert!(
            rows.last().expect("a row").contains("no such file"),
            "a fold swallowed the one thing that had to survive it: {rows:?}"
        );
        assert_eq!(
            rows.len(),
            2,
            "the note rides the second row rather than taking a third: {rows:?}"
        );

        let done = folded(call().with(Outcome::Ok("20 行".into())));
        assert!(
            !done.iter().any(|r| r.contains("20 行")),
            "a successful result is what expanding the call is for: {done:?}"
        );
    }
}
