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

/// Every marker number in this text, in the order it appears.
///
/// Used to label what was sent, where the log carries the images but the
/// numbers only survive inside the text the person typed.
fn markers_in(text: &str) -> Vec<usize> {
    const OPEN: &str = "[Image #";
    let mut out = Vec::new();
    let mut rest = text;
    // Every cut below is on a boundary by construction: `find` answers with a
    // byte index that is one, `OPEN` is ASCII, and `digits` was taken from the
    // front as ASCII digits. The text being scanned is what a person typed, so
    // it is routinely Chinese — arithmetic on it is exactly what killed the
    // TUI four times (see `gates/tui-string-slice.sh`).
    #[allow(
        clippy::string_slice,
        reason = "`find` returns a boundary and `OPEN` is ASCII"
    )]
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        #[allow(
            clippy::string_slice,
            reason = "ASCII digits taken from the front: their byte length is a boundary"
        )]
        let after_digits = &after[digits.len()..];
        if !digits.is_empty() && after_digits.starts_with(']') {
            if let Ok(n) = digits.parse() {
                out.push(n);
            }
        }
        rest = &rest[at + OPEN.len()..];
    }
    out
}

/// What the composer is holding between submits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachments {
    images: Vec<Pending>,
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
        self.images.push(Pending { marker: n, image });
        marker(n)
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
    /// For "clear the composer" (Ctrl+U / `/clear`), which throws away the text
    /// and therefore every marker in it. Keeping the images would leave the
    /// composer holding pictures no line accounts for, and the next submit would
    /// have to re-derive that they are gone; dropping them here keeps the state
    /// and the screen telling the same story at every moment.
    pub fn clear(&mut self) {
        self.images.clear();
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
}
