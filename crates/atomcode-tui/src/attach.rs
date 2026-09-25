//! Pictures the composer is holding, and the markers that stand for them.
//!
//! An attachment has to be visible in the thing being typed, or the person
//! cannot tell what they are about to send: the bytes live here, the label
//! lives in the text. The two are joined by a marker — `[Image #1]` — and the
//! join is checked at send time rather than trusted, because typing is what
//! edits the text and backspace is not supposed to be able to leave a picture
//! attached to a sentence that no longer mentions it.
//!
//! The numbers are session-scoped and monotonic. Reusing a number after one
//! marker was deleted would make a stale marker in the text point at a picture
//! the person never put there, which is the one failure mode where an
//! attachment could reach the model without anyone having asked for it.

use atomcode_kernel::message::ImageContent;

/// One image waiting, and the number it is labelled with.
///
/// Private: what the composer holds is nobody's business but this module's.
/// The outside world only ever sees a marker in the text and, at send time,
/// the images that text still refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    marker: usize,
    image: ImageContent,
}

/// The `[Image #N]` marker for one number.
fn marker(n: usize) -> String {
    format!("[Image #{n}]")
}

/// Every `[Image #N]` marker in `text`: its byte span (from `[` through the
/// closing `]`, half-open) and its number, in order. The one scanner the marker
/// helpers below share, rather than three copies of the same walk.
///
/// Every cut is on a boundary by construction: `find` answers with a byte index
/// that is one, `OPEN` is ASCII, and `digits` was taken from the front as ASCII
/// digits. The text is what a person typed, so it is routinely Chinese —
/// arithmetic on it is exactly what killed the TUI four times (see
/// `gates/tui-string-slice.sh`).
fn marker_hits(text: &str) -> Vec<(std::ops::Range<usize>, usize)> {
    const OPEN: &str = "[Image #";
    let mut hits = Vec::new();
    let mut from = 0;
    #[allow(
        clippy::string_slice,
        reason = "`find` returns a boundary, `OPEN` is ASCII, digits are ASCII"
    )]
    while let Some(rel) = text[from..].find(OPEN) {
        let at = from + rel;
        let after = &text[at + OPEN.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let after_digits = &after[digits.len()..];
        if !digits.is_empty() && after_digits.starts_with(']') {
            if let Ok(n) = digits.parse() {
                let end = at + OPEN.len() + digits.len() + 1;
                hits.push((at..end, n));
            }
        }
        from = at + OPEN.len();
    }
    hits
}

/// Every marker number in this text, in the order it appears.
///
/// Used to label what was sent, where the log carries the images but the
/// numbers only survive inside the text the person typed.
pub(crate) fn markers_in(text: &str) -> Vec<usize> {
    marker_hits(text).into_iter().map(|(_, n)| n).collect()
}

/// The image number whose `[Image #N]` marker covers byte offset `off`, if any.
///
/// This is what turns a click into "open image N": a caret offset in the
/// composer, or a byte offset into a logged line, lands somewhere in the text,
/// and a click that lands inside a marker's span is a request to open that
/// picture rather than to move the caret. `None` for a click anywhere else, so
/// ordinary text is untouched.
pub fn marker_at_offset(text: &str, off: usize) -> Option<usize> {
    marker_hits(text)
        .into_iter()
        .find(|(span, _)| span.contains(&off))
        .map(|(_, n)| n)
}

/// The byte span of every `[Image #N]` marker in `text`, in order. Editing
/// treats each as one atomic unit: one backspace deletes the whole marker, the
/// arrows step over it, and the caret never lands inside it — the "an image is a
/// chip, not ten characters" behaviour.
pub fn marker_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    marker_hits(text)
        .into_iter()
        .map(|(span, _)| span)
        .collect()
}

/// Move a caret that landed **strictly inside** an `[Image #N]` out to the nearer
/// edge of that marker, so the chip invariant holds for every caret writer — not
/// just the horizontal arrows. Vertical movement and a click map by visual column
/// and can land between a marker's characters; this is what they run their result
/// through. A caret already on a marker boundary (or outside every marker) is
/// returned unchanged.
pub fn snap_caret_out_of_marker(text: &str, caret: usize) -> usize {
    for span in marker_spans(text) {
        if span.start < caret && caret < span.end {
            // The nearer edge, so the caret barely moves.
            return if caret - span.start <= span.end - caret {
                span.start
            } else {
                span.end
            };
        }
    }
    caret
}

/// What the composer is holding between submits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachments {
    images: Vec<Pending>,
    /// Every image added this session, by marker number — **kept after send**,
    /// where [`images`](Self::images) is drained. This is what lets a person open
    /// a picture again from the history: the marker `[Image #N]` in a sent line
    /// still resolves to its bytes here. Append-only for the session (numbers are
    /// never reused), and untouched by [`clear`](Self::clear) — a previously sent
    /// image's bytes must survive the composer being emptied.
    gallery: Vec<Pending>,
    next: usize,
}

/// Hand-written rather than derived, because the derived one would start the
/// numbering at 0 while [`Attachments::new`] starts it at 1 — and the live
/// composer is a `Default` (it is a field of `Moment`), so the two would have to
/// agree or the first screenshot on screen would read `[Image #0]` while every
/// test in this file said `[Image #1]`. Delegating makes that impossible.
impl Default for Attachments {
    fn default() -> Self {
        Self::new()
    }
}

impl Attachments {
    pub fn new() -> Self {
        Self {
            images: Vec::new(),
            gallery: Vec::new(),
            next: 1,
        }
    }

    /// Hold another image. The returned marker is what belongs in the text.
    ///
    /// Nothing dedups: the same picture pasted twice is two attachments,
    /// because the person pasted twice and the second one is in there.
    pub fn add(&mut self, image: ImageContent) -> String {
        let n = self.next;
        self.next += 1;
        let pending = Pending { marker: n, image };
        // The send queue is drained at submit; the gallery keeps a copy so the
        // picture can be reopened from the history long after it was sent.
        self.gallery.push(pending.clone());
        self.images.push(pending);
        marker(n)
    }

    /// The bytes of image `n`, for reopening it — from the composer or from a
    /// sent line in the history. `None` if no such number was ever added.
    pub fn image_at(&self, n: usize) -> Option<&ImageContent> {
        self.gallery
            .iter()
            .find(|p| p.marker == n)
            .map(|p| &p.image)
    }

    /// Re-attach the images a recalled line refers to, under FRESH marker
    /// numbers, and rewrite the line to use them — so a recalled image is actually
    /// sent (and, for a text-only model, re-recognised) again rather than reaching
    /// the model as a bare `[Image #N]` placeholder.
    ///
    /// Called **once, at submit** — not on every recall keystroke, which would
    /// re-clone the image on each arrow press (the history line is never mutated,
    /// so it keeps handing back the old markers). A marker whose bytes are already
    /// on the send queue (a freshly-attached image this compose) is left alone;
    /// the same marker twice in one line is handled once.
    ///
    /// Each still-known recalled `[Image #old]` becomes `[Image #new]` (the
    /// renumber the classic front end does, which is why the marker "becomes some
    /// other number"). Returns the numbers whose bytes are gone (a resumed
    /// session's in-memory gallery is empty, or the marker was typed as literal
    /// text) — their markers are left as-is for the caller to note.
    pub fn rehydrate_recalled(&mut self, line: &mut String) -> Vec<usize> {
        use std::collections::HashSet;
        // Markers already going this submit are fresh attachments, not recalls —
        // re-adding them would send (and cache) the same picture twice.
        let queued: HashSet<usize> = self.images.iter().map(|p| p.marker).collect();
        let mut handled: HashSet<usize> = HashSet::new();
        let mut missing = Vec::new();
        for old in markers_in(line) {
            if queued.contains(&old) || !handled.insert(old) {
                continue;
            }
            match self.image_at(old).cloned() {
                Some(image) => {
                    let fresh = self.add(image);
                    *line = line.replace(&marker(old), &fresh);
                }
                None => missing.push(old),
            }
        }
        missing
    }

    /// Hand over what this text still refers to, and forget all of it.
    ///
    /// The filter is the whole point: an attachment whose marker is no longer
    /// in the text was deleted while composing, so sending it would put a
    /// picture in the model's context that no line on screen accounts for.
    /// Everything is forgotten either way — the ones that went are now the
    /// log's business, and the ones that did not are not anybody's.
    pub fn take_shown(&mut self, text: &str) -> Vec<ImageContent> {
        let shown: Vec<usize> = markers_in(text);
        let taken = self
            .images
            .drain(..)
            .filter(|p| shown.contains(&p.marker))
            .map(|p| p.image)
            .collect();
        taken
    }

    /// Drop everything, markers in the text or not.
    ///
    /// For "clear the composer" (Ctrl+U), which throws away the text
    /// and therefore every marker in it. Keeping the images would leave the
    /// composer holding pictures no line accounts for, and the next submit would
    /// have to re-derive that they are gone; dropping them here keeps the state
    /// and the screen telling the same story at every moment.
    pub fn clear(&mut self) {
        self.images.clear();
    }
}

/// A pasted image file bigger than this is refused rather than attached: the
/// bytes are re-sent on every turn, so a runaway file would blow the per-request
/// body. Matches the classic front end's ceiling.
const MAX_PATH_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Interpret a paste payload as a filesystem path to an image file and load it
/// as an [`ImageContent`] — or `None` when it is not unambiguously an image
/// path, so prose that merely names a file is never grabbed.
///
/// This is the flow a screenshot from WeChat, iTerm2's ⌘V, or a Finder
/// drag-and-drop takes: copying an image *file* (not bitmap bytes) puts a path
/// on the clipboard, and the terminal bracketed-pastes that path as plain text.
/// Without this the composer shows the raw path instead of the picture — the
/// reported "全部展示成路径". The bytes are read **here, at paste time**, so the
/// attachment is self-contained and cannot later disagree with the filesystem.
///
/// All of these must hold: single line; an absolute path after trimming, one
/// layer of matched outer quotes, and `\<space>` unescaping (drag-and-drop emits
/// both); a png/jpg/jpeg/gif/webp extension; an existing regular file no larger
/// than [`MAX_PATH_IMAGE_BYTES`]. A bare relative `snap.png` is deliberately
/// rejected — typed at the prompt it is ambiguous between text and attachment.
pub fn image_from_path(text: &str) -> Option<ImageContent> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    // One layer of matched outer quotes: Finder drag of a path with spaces wraps
    // in `'...'`; some shells produce `"..."`. The quote chars are single-byte
    // ASCII, so the slice stays on a char boundary.
    #[allow(
        clippy::string_slice,
        reason = "the stripped bytes are the ASCII quote chars just matched"
    )]
    let unquoted: &str = if trimmed.len() >= 2
        && ((trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    // iTerm2 / drag-and-drop shell-escape spaces (`/path/with\ space.png`).
    // Backslash before any other char is left alone — no other escape form
    // occurs in real-world drag pastes.
    let unescaped = unquoted.replace("\\ ", " ");
    // A `file://` URL (Finder copy / Cmd+V of a saved file) becomes its plain
    // local path; a bare path is left as-is.
    let from_url = from_file_url(unescaped.trim());
    let candidate = from_url.as_deref().unwrap_or(unescaped.as_str());
    let path = std::path::Path::new(candidate.trim());
    if !path.is_absolute() {
        return None;
    }
    load_image_file(path)
}

/// The picture at `path`, when the path names a picture that is there and small
/// enough. Nothing else is checked — where the path came from, and whether it
/// was unambiguous, is the caller's question.
///
/// Split from [`image_from_path`] because the two callers disagree about
/// exactly one rule and agree about all the rest. A path arriving in a **paste**
/// has to be unambiguously a path (absolute, quoted, `file://`) or prose that
/// merely mentions `notes.png` becomes an attachment. A path arriving as the
/// argument of **`/paste`** carries no such ambiguity: the person typed a
/// command whose argument is a file, so `snap.png` in the working directory is
/// what they meant. Everything after that — the extensions, the ceiling, the
/// re-encode, sniffing the media type from the bytes rather than trusting the
/// suffix — is one answer, and this is where it lives.
pub fn load_image_file(path: &std::path::Path) -> Option<ImageContent> {
    use base64::Engine as _;

    let ext_media_type = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return None,
    };
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PATH_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    // Downscale/re-encode an oversized image exactly as a clipboard paste is
    // (image_normalize), so a big attachment can't blow the per-request body.
    // Falls back to the original bytes on any decode failure — but with the media
    // type sniffed from the bytes, not the file extension: a `.png` that is really
    // JPEG must not be sent as `image/png`, which strict vision providers reject.
    let (media_type, data) =
        match atomcode_capabilities::image_normalize::normalize_image_raw(&bytes) {
            Some((mt, out)) => (mt, base64::engine::general_purpose::STANDARD.encode(out)),
            None => (
                sniff_media_type(&bytes)
                    .unwrap_or(ext_media_type)
                    .to_string(),
                base64::engine::general_purpose::STANDARD.encode(&bytes),
            ),
        };
    Some(ImageContent { media_type, data })
}

/// The image a paste should attach: a file path the text names, else the picture
/// the clipboard is holding.
///
/// Cmd+V is swallowed by the terminal and arrives as a bracketed paste (never the
/// `AttachImage` key Ctrl+V is bound to), so a screenshot pasted with Cmd+V is not
/// a path at all — it has to be recovered from the live clipboard, the same bytes
/// Ctrl+V reads. A Finder-copied file, by contrast, pastes its path, which
/// [`image_from_path`] handles.
///
/// The clipboard is consulted ONLY when the paste carried no text of its own: a
/// pure image arrives as an empty bracketed paste, while a paste WITH text is
/// that text. macOS pasteboards are multi-type — a text selection can sit beside
/// an image — and grabbing that image would silently drop what was typed, so a
/// non-empty paste is always kept as text (Ctrl+V remains the way to force the
/// image).
pub fn image_for_paste(text: &str, surface: &dyn crate::surface::Surface) -> Option<ImageContent> {
    if let Some(image) = image_from_path(text) {
        return Some(image);
    }
    if text.trim().is_empty() {
        return surface.clipboard_image();
    }
    None
}

/// What `/paste` found to put in the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pasted {
    Picture(ImageContent),
    Text(String),
}

/// What `/paste` with no argument gets: the picture the clipboard is holding,
/// or failing that its text.
///
/// **The picture comes first, and that order is the whole point of the
/// command.** Text already has three roads into the composer — typing, the
/// terminal's own paste, a bracketed paste — while a clipboard *picture* has
/// exactly one, `Ctrl+V`, and that is the chord Windows Terminal, PuTTY and a
/// handful of others swallow before this process ever sees it. On those
/// terminals the picture had no road at all. A clipboard holding both (a macOS
/// pasteboard routinely does) is a clipboard something was copied *into*, and
/// what was copied was the picture.
///
/// That is the opposite of [`image_for_paste`]'s rule, deliberately: a
/// *bracketed paste* carrying text is that text, because grabbing the picture
/// there would silently drop what the person actually pasted. Here they asked
/// for the picture by name.
pub fn from_clipboard(surface: &dyn crate::surface::Surface) -> Option<Pasted> {
    if let Some(image) = surface.clipboard_image() {
        return Some(Pasted::Picture(image));
    }
    match surface.clipboard_text() {
        Some(text) if !text.is_empty() => Some(Pasted::Text(text)),
        _ => None,
    }
}

/// 一份粘进编辑区的文本文件,最大到这里。
///
/// **理由和图片那条一样,只是更钝:编辑区里的东西每一轮都要重发。** 把一份
/// 日志粘进来,是把它按轮计费,而且多半一轮都撑不过——一兆文本已经比大多数
/// 模型的上下文窗口还长了。真要让模型看一个大文件,它自己有读文件的工具,
/// 那条路只读它要的那几行。
///
/// 而在此之前,这里是 `read_to_string`,没有上限:一个几百兆的文件会被整个
/// 拉进内存、整个塞进编辑区,然后这块屏幕就没了。
pub const MAX_PASTED_TEXT_BYTES: u64 = 1024 * 1024;

/// `/paste <path>` 拿到了什么。
#[derive(Debug)]
pub enum FromFile {
    /// 拿到了。
    Got(Pasted),
    /// 空文件 —— 一种状态,不是失败。
    Empty,
    /// 太大了,不粘。`bytes` 是它有多大。
    TooBig { bytes: u64 },
}

/// What `/paste <path>` gets: the picture at that path, or its text.
///
/// `Err` is the file not being readable at all, which is the only thing worth
/// an error — empty and too big are both answers (see [`FromFile`]).
///
/// **A relative path counts here**, unlike in a paste ([`load_image_file`] says
/// why): `/paste shot.png` in the working directory is unambiguous, and it is
/// the other half of the Windows fallback — the half for a screenshot that was
/// saved to a file rather than left on the clipboard.
pub fn from_file(path: &std::path::Path) -> Result<FromFile, std::io::Error> {
    if let Some(image) = load_image_file(path) {
        return Ok(FromFile::Got(Pasted::Picture(image)));
    }
    // 先问多大,再读 —— 反过来的话,问的时候文件已经在内存里了。
    let bytes = std::fs::metadata(path)?.len();
    if bytes > MAX_PASTED_TEXT_BYTES {
        return Ok(FromFile::TooBig { bytes });
    }
    let text = std::fs::read_to_string(path)?;
    Ok(match text.is_empty() {
        true => FromFile::Empty,
        false => FromFile::Got(Pasted::Text(text)),
    })
}

/// A `file://` URL as a local path: scheme (and optional `localhost` host)
/// stripped and percent-escapes decoded, or `None` when `s` is not one. macOS
/// puts a `file://` URL on the pasteboard for a Finder-copied file, and one whose
/// path has spaces arrives percent-encoded (`%20`).
fn from_file_url(s: &str) -> Option<String> {
    let rest = s.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// The media type named by an image's magic bytes, or `None` when the bytes do
/// not begin with one this UI carries. Trusted over the file extension so a
/// mislabeled file — a `.png` holding JPEG bytes — is sent with the type its
/// bytes actually are. Only the four the composer accepts are recognised; the
/// caller falls back to the extension for anything else.
fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(tag: &str) -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: tag.into(),
        }
    }

    #[test]
    fn a_marker_and_its_image_travel_together() {
        let mut a = Attachments::new();
        let m = a.add(img("one"));
        assert_eq!(m, "[Image #1]");
        let sent = a.take_shown(&format!("look at this {m}"));
        assert_eq!(sent, vec![img("one")]);
        // Handed over and forgotten: a second send finds nothing left, so the
        // same picture cannot go twice.
        assert!(
            a.take_shown(&format!("look at this {m}")).is_empty(),
            "what was sent is not still pending"
        );
    }

    #[test]
    fn deleting_the_marker_deletes_the_attachment() {
        // Backspace over `[Image #1]` is the person saying "not that one", and
        // the only honest reading of it is that the picture is gone too.
        let mut a = Attachments::new();
        let one = a.add(img("one"));
        let two = a.add(img("two"));
        let sent = a.take_shown(&format!("keep {two} only"));
        assert_eq!(sent, vec![img("two")], "the deleted one is not sent");
        assert_eq!(one, "[Image #1]");
    }

    #[test]
    fn a_number_is_never_reused_so_a_stale_marker_cannot_resurrect_an_image() {
        let mut a = Attachments::new();
        let first = a.add(img("one"));
        let second = a.add(img("two"));
        assert_ne!(first, second);
        // The first is deleted; the second is labelled #2 forever.
        assert_eq!(a.take_shown(&format!("only {second}")), vec![img("two")]);
        let third = a.add(img("three"));
        assert_eq!(third, "[Image #3]", "past attachments are not renumbered");
        assert!(a.take_shown(&first).is_empty(), "no image answers #1 again");
    }

    #[test]
    fn clearing_drops_what_the_text_still_mentions() {
        let mut a = Attachments::new();
        let m = a.add(img("one"));
        a.clear();
        assert!(a.take_shown(&format!("still says {m}")).is_empty());
    }

    #[test]
    fn markers_are_found_in_the_text_in_order_and_only_when_well_formed() {
        assert_eq!(
            markers_in("a [Image #2] b [Image #10]"),
            vec![2, 10],
            "two digits are a number, not one"
        );
        assert_eq!(markers_in("[Image #]"), Vec::<usize>::new());
        assert_eq!(markers_in("[Image #x]"), Vec::<usize>::new());
        assert_eq!(markers_in("[Image #1x]"), Vec::<usize>::new());
        assert_eq!(markers_in("no markers here"), Vec::<usize>::new());
        // A Chinese sentence around one, because that is what this UI is typed in.
        assert_eq!(markers_in("看这个 [Image #3] 对吗"), vec![3]);
    }

    // The gallery outlives the send: after `take_shown` has drained the queue,
    // the bytes are still reachable by number so the picture can be reopened
    // from the history. `clear` (Ctrl+U) must not drop them either.
    #[test]
    fn the_gallery_keeps_bytes_reachable_after_send_and_clear() {
        let mut a = Attachments::new();
        let m = a.add(img("one"));
        assert_eq!(
            a.image_at(1),
            Some(&img("one")),
            "reachable while composing"
        );
        let _ = a.take_shown(&format!("sent {m}"));
        assert_eq!(
            a.image_at(1),
            Some(&img("one")),
            "still reachable after send"
        );
        a.clear();
        assert_eq!(
            a.image_at(1),
            Some(&img("one")),
            "clearing the composer keeps it"
        );
        assert_eq!(a.image_at(9), None, "a number never added has no image");
    }

    // A click position (a byte offset) inside an `[Image #N]` span opens that
    // image; anywhere else is ordinary text and opens nothing.
    #[test]
    fn a_click_inside_a_marker_span_names_its_image() {
        let text = "看 [Image #2] 和 [Image #10] 对吗";
        let open2 = text.find("[Image #2]").unwrap();
        let open10 = text.find("[Image #10]").unwrap();
        assert_eq!(marker_at_offset(text, open2), Some(2), "on the `[`");
        assert_eq!(
            marker_at_offset(text, open2 + 5),
            Some(2),
            "inside the span"
        );
        assert_eq!(
            marker_at_offset(text, open2 + "[Image #2]".len() - 1),
            Some(2),
            "on the closing `]`"
        );
        assert_eq!(
            marker_at_offset(text, open2 + "[Image #2]".len()),
            None,
            "just past the span is text again"
        );
        assert_eq!(
            marker_at_offset(text, open10 + 6),
            Some(10),
            "two-digit number"
        );
        assert_eq!(marker_at_offset(text, 0), None, "on the leading 看");
        assert_eq!(marker_at_offset("no markers", 3), None);
    }

    // Arrow-up recall re-attaches a still-known image under a fresh number and
    // renumbers the line; a gone image is reported and left as a bare marker.
    #[test]
    fn recall_reattaches_known_images_and_renumbers() {
        let mut a = Attachments::new();
        let m1 = a.add(img("one")); // [Image #1]
        let m2 = a.add(img("two")); // [Image #2]

        // Both were "sent"; the send queue drains but the gallery keeps them.
        let _ = a.take_shown(&format!("{m1} {m2}"));

        // Recall a line that referenced #1 — it re-attaches under a fresh number.
        let mut line = format!("look {m1} ok");
        let missing = a.rehydrate_recalled(&mut line);
        assert!(missing.is_empty(), "the image is still known");
        assert_eq!(line, "look [Image #3] ok", "renumbered to a fresh marker");
        // The fresh marker resolves to the same bytes and is queued to send.
        assert_eq!(a.image_at(3), Some(&img("one")));
        assert_eq!(a.take_shown(&line), vec![img("one")], "it goes on submit");

        // A number never added (or a resumed empty gallery) is reported, not faked.
        let mut gone = "stale [Image #99] here".to_string();
        assert_eq!(a.rehydrate_recalled(&mut gone), vec![99]);
        assert_eq!(gone, "stale [Image #99] here", "left as-is for the caller");
    }

    // A freshly-attached image is already on the send queue, so submit-time
    // rehydration must leave it alone — re-adding would send/cache it twice — and
    // the same marker twice in one line is handled once, not duplicated.
    #[test]
    fn rehydrate_leaves_queued_markers_and_deduplicates() {
        let mut a = Attachments::new();
        let fresh = a.add(img("fresh")); // [Image #1], still on the send queue
        let mut line = format!("{fresh} and {fresh} again");
        let missing = a.rehydrate_recalled(&mut line);
        assert!(missing.is_empty());
        assert_eq!(
            line, "[Image #1] and [Image #1] again",
            "queued marker untouched"
        );
        assert_eq!(
            a.take_shown(&line),
            vec![img("fresh")],
            "the one queued image is sent once, not duplicated"
        );
    }

    // Spans cover each marker whole, so editing can treat it as one chip.
    #[test]
    #[allow(
        clippy::string_slice,
        reason = "test: spans come from `marker_spans`, whose edges are ASCII `[` / `]`"
    )]
    fn marker_spans_cover_each_marker_whole() {
        let text = "a [Image #2] b [Image #10] c";
        let spans = marker_spans(text);
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[0].clone()], "[Image #2]");
        assert_eq!(&text[spans[1].clone()], "[Image #10]");
        assert!(marker_spans("no markers").is_empty());
        // A malformed `[Image #]` is not a span (no digits).
        assert!(marker_spans("[Image #]").is_empty());
    }

    // The media type follows the bytes, not the extension: a mislabeled file is
    // not sent with a Content-Type that contradicts its magic bytes.
    #[test]
    fn media_type_is_sniffed_from_the_bytes() {
        assert_eq!(sniff_media_type(b"\x89PNG\r\n\x1a\n..."), Some("image/png"));
        assert_eq!(
            sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_media_type(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            sniff_media_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(
            sniff_media_type(b"not an image"),
            None,
            "unknown → fall back to ext"
        );
        assert_eq!(sniff_media_type(b""), None, "empty never panics");
    }

    // A pasted absolute path to a real image file becomes an attachment — the
    // WeChat/iTerm2/Finder flow. The bytes need not decode: an unrecognised
    // payload with an image extension still attaches (raw fallback), matching
    // the clipboard-paste path, so the test does not depend on a real PNG.
    #[test]
    fn an_absolute_image_path_loads_as_an_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shot.png");
        std::fs::write(&file, b"\x89PNG not-really-but-has-the-extension").unwrap();

        let img = image_from_path(file.to_str().unwrap()).expect("an image path attaches");
        assert!(!img.data.is_empty(), "the bytes were read and encoded");

        // Quoted and shell-escaped forms (drag-and-drop) resolve to the same file.
        let spaced = dir.path().join("my shot.png");
        std::fs::write(&spaced, b"bytes").unwrap();
        assert!(
            image_from_path(&format!("'{}'", spaced.display())).is_some(),
            "matched outer quotes are stripped"
        );
        assert!(
            image_from_path(&spaced.display().to_string().replace(' ', "\\ ")).is_some(),
            "shell-escaped spaces are unescaped"
        );
    }

    // macOS Finder-copy (and Cmd+V of a saved file) pastes a `file://` URL, and
    // one with spaces arrives percent-encoded. Both name a real image file and
    // must attach the same as a bare path.
    #[test]
    fn a_file_url_attaches_and_percent_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a b.png");
        std::fs::write(&file, b"bytes").unwrap();
        let encoded = file.display().to_string().replace(' ', "%20");
        assert!(
            image_from_path(&format!("file://{encoded}")).is_some(),
            "percent-encoded file:// URL attaches"
        );
        let plain = dir.path().join("c.png");
        std::fs::write(&plain, b"bytes").unwrap();
        assert!(
            image_from_path(&format!("file://{}", plain.display())).is_some(),
            "unencoded file:// URL attaches"
        );
    }

    // Cmd+V is swallowed by the terminal and arrives as a bracketed paste, not
    // the `AttachImage` key, so a screenshot pasted with Cmd+V has to be
    // recovered from the live clipboard — the same bytes Ctrl+V reads.
    #[test]
    fn a_paste_recovers_the_clipboard_image_when_the_text_is_not_a_path() {
        let surface = crate::surface::Headless::new(10, 2);
        surface.set_clipboard_image(ImageContent {
            media_type: "image/png".into(),
            data: "QUJD".into(),
        });
        let got = image_for_paste("", surface.as_ref()).expect("clipboard image recovered");
        assert_eq!(got.data, "QUJD", "the paste took the clipboard image");
        // A paste that carried its OWN text is that text: a clipboard image beside
        // it on a multi-type pasteboard must not silently replace what was typed.
        assert!(
            image_for_paste("here is a paragraph", surface.as_ref()).is_none(),
            "non-empty paste text is kept, not swapped for the clipboard image"
        );
        // With nothing in the clipboard, prose stays prose.
        let empty = crate::surface::Headless::new(10, 2);
        assert!(
            image_for_paste("just some prose", empty.as_ref()).is_none(),
            "no image anywhere ⇒ text"
        );
    }

    /// `/paste` prefers the picture, and that order is the reason the command
    /// is worth having: text already has three roads into the composer, a
    /// clipboard picture has one — `Ctrl+V` — and that is the chord Windows
    /// Terminal and PuTTY swallow.
    #[test]
    fn paste_takes_the_picture_first_even_when_there_is_text_beside_it() {
        let surface = crate::surface::Headless::new(10, 2);
        surface.set_clipboard_image(ImageContent {
            media_type: "image/png".into(),
            data: "QUJD".into(),
        });
        surface.set_clipboard_text("a caption someone copied earlier");
        assert_eq!(
            from_clipboard(surface.as_ref()),
            Some(Pasted::Picture(ImageContent {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            })),
            "a multi-type pasteboard is one something was copied INTO, and what \
             was copied was the picture"
        );
    }

    #[test]
    fn paste_falls_back_to_text_and_then_to_nothing() {
        let surface = crate::surface::Headless::new(10, 2);
        surface.set_clipboard_text("some prose");
        assert_eq!(
            from_clipboard(surface.as_ref()),
            Some(Pasted::Text("some prose".into()))
        );
        let empty = crate::surface::Headless::new(10, 2);
        assert_eq!(
            from_clipboard(empty.as_ref()),
            None,
            "nothing in it is an answer, not a failure"
        );
    }

    /// The other half of the Windows fallback: a screenshot that was saved to a
    /// file rather than left on the clipboard. **A relative path counts here**,
    /// where in a paste it would be ambiguous prose.
    #[test]
    fn paste_of_a_path_attaches_a_picture_and_reads_anything_else() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG not-really-but-has-the-extension").unwrap();
        assert!(
            matches!(from_file(&png), Ok(FromFile::Got(Pasted::Picture(_)))),
            "a named picture file attaches"
        );
        // The same path as a *paste payload* is still not an attachment — the
        // rule that separates the two callers of `load_image_file`.
        assert!(
            image_from_path("shot.png").is_none(),
            "a bare relative path in a paste stays prose"
        );

        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "two lines\nof prose").unwrap();
        assert!(
            matches!(
                from_file(&notes),
                Ok(FromFile::Got(Pasted::Text(ref text))) if text == "two lines\nof prose"
            ),
            "anything else is read as its text"
        );

        let blank = dir.path().join("blank.txt");
        std::fs::write(&blank, "").unwrap();
        assert!(
            matches!(from_file(&blank), Ok(FromFile::Empty)),
            "empty is a state"
        );
        assert!(
            from_file(&dir.path().join("nope.txt")).is_err(),
            "unreadable is the only error"
        );
    }

    /// 一份太大的文件粘不进编辑区,而且**不是**说成「读不了」。
    ///
    /// 在此之前这里是 `read_to_string`,没有上限:`/paste` 一个几百兆的日志
    /// 会把它整个拉进内存、整个塞进编辑区,而编辑区里的东西**每一轮都要重发**
    /// —— 是把那份日志按轮计费,而且多半一轮都撑不过。
    ///
    /// 「不是读不了」这一半要单独钉:把它并进那个 `Err` 分支,人看到的是
    /// 「这个文件读不了」,于是去查权限、查编码,而文件好好的。
    #[test]
    fn a_file_too_big_for_the_composer_is_refused_and_not_called_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("huge.log");
        std::fs::write(&big, vec![b'x'; MAX_PASTED_TEXT_BYTES as usize + 1]).unwrap();
        match from_file(&big) {
            Ok(FromFile::TooBig { bytes }) => {
                assert_eq!(bytes, MAX_PASTED_TEXT_BYTES + 1, "说得出它有多大")
            }
            other => panic!("{other:?}"),
        }
        // 而刚好到上限的那一份进得来 —— 否则这条判据只是在说「大的别要」,
        // 把上限设成 0 也照样绿。
        let fits = dir.path().join("fits.log");
        std::fs::write(&fits, vec![b'x'; MAX_PASTED_TEXT_BYTES as usize]).unwrap();
        assert!(matches!(
            from_file(&fits),
            Ok(FromFile::Got(Pasted::Text(_)))
        ));
    }

    // Everything that is NOT an unambiguous image-attachment intent is left as
    // text, so ordinary prose and paths are never silently eaten.
    #[test]
    fn non_image_paste_payloads_are_left_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("real.png");
        std::fs::write(&png, b"bytes").unwrap();

        assert!(image_from_path("just some prose").is_none(), "prose");
        assert!(
            image_from_path("snap.png").is_none(),
            "relative path is ambiguous"
        );
        assert!(
            image_from_path(dir.path().join("notes.txt").to_str().unwrap()).is_none(),
            "non-image extension"
        );
        assert!(
            image_from_path(dir.path().join("gone.png").to_str().unwrap()).is_none(),
            "missing file"
        );
        assert!(
            image_from_path(&format!("look at {}", png.display())).is_none(),
            "a path embedded in a sentence is prose, not an attachment"
        );
        assert!(
            image_from_path(&format!("{}\n{}", png.display(), png.display())).is_none(),
            "multi-line is a text paste, never a single attachment"
        );
    }
}
