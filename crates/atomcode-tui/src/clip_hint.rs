//! Whether to say "there is a picture on the clipboard", and for how long.
//!
//! The offer is worth making at one moment: a picture has just arrived there,
//! and the person is about to type. So it is **one-shot per picture** — a new
//! picture opens a short window, the same one sitting on the clipboard for an
//! hour does not keep asking, and taking it closes the window at once. These are
//! `atomcode-tuix`'s rules and numbers, kept: a hint that is always up is a hint
//! nobody reads.
//!
//! Pure — times are handed in — so the rules are judged without waiting on a
//! clock. The reading of the clipboard, and when to do it, belong to the screen.

use std::time::{Duration, Instant};

/// How long the offer stays up after a new picture is seen.
pub const SHOWN_FOR: Duration = Duration::from_secs(6);

/// The least time between two looks at the clipboard.
///
/// A look copies the picture's pixels out (tens of MB for a 4K screenshot), so
/// it is not done on every keystroke. A person who pasted a picture and types
/// straight on is seen within this.
pub const LOOK_EVERY: Duration = Duration::from_millis(1500);

/// The clipboard as last seen, and the offer about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipHint {
    /// The picture last seen there — a fingerprint of its pixels — or `None`.
    seen: Option<u64>,
    /// When the offer about [`seen`](Self::seen) comes down; `None` when there
    /// is no offer up.
    until: Option<Instant>,
    /// When the clipboard was last looked at.
    looked: Option<Instant>,
    /// How many times a picture has been taken off the clipboard. A look
    /// carries the count it started at, so one that raced a paste — the key
    /// that pastes starts a look of its own — can tell.
    takes: u64,
}

impl ClipHint {
    /// Whether it has been long enough since the last look to look again.
    pub fn due(&self, now: Instant) -> bool {
        self.looked
            .is_none_or(|at| now.saturating_duration_since(at) >= LOOK_EVERY)
    }

    /// A look is being taken now. Returns the ticket [`found`](Self::found)
    /// is to be handed with what it saw.
    pub fn looking(&mut self, now: Instant) -> u64 {
        self.looked = Some(now);
        self.takes
    }

    /// What the look that was handed `ticket` found. A picture not seen before
    /// opens the offer; the one already seen changes nothing, so it is never
    /// offered twice; nothing there takes the offer down.
    ///
    /// Except when a picture was taken while the look was out: then what it
    /// read is the picture just taken, which is recorded as seen and not
    /// offered — it is already on the line.
    pub fn found(&mut self, picture: Option<u64>, now: Instant, ticket: u64) {
        if picture == self.seen {
            return;
        }
        self.seen = picture;
        let raced = ticket != self.takes;
        self.until = picture.filter(|_| !raced).map(|_| now + SHOWN_FOR);
    }

    /// The picture was taken onto the line: the offer has been answered. It is
    /// still the one seen, so it is not offered again while it stays there.
    pub fn taken(&mut self) {
        self.until = None;
        self.takes += 1;
    }

    /// Whether the offer is up at `now`.
    pub fn showing(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One look, start to finish, with nothing in between.
    fn see(hint: &mut ClipHint, picture: Option<u64>, now: Instant) {
        let ticket = hint.looking(now);
        hint.found(picture, now, ticket);
    }

    #[test]
    fn a_new_picture_is_offered_for_a_while_and_then_not() {
        let t0 = Instant::now();
        let mut hint = ClipHint::default();
        assert!(!hint.showing(t0), "nothing seen, nothing offered");

        see(&mut hint, Some(1), t0);
        assert!(hint.showing(t0));
        assert!(hint.showing(t0 + SHOWN_FOR - Duration::from_millis(1)));
        assert!(
            !hint.showing(t0 + SHOWN_FOR),
            "the offer comes down on its own"
        );
    }

    #[test]
    fn the_same_picture_is_never_offered_twice() {
        let t0 = Instant::now();
        let mut hint = ClipHint::default();
        see(&mut hint, Some(1), t0);

        // Still there after the offer lapsed: not offered again.
        let later = t0 + SHOWN_FOR * 3;
        see(&mut hint, Some(1), later);
        assert!(!hint.showing(later));

        // Taken, and still there: not offered again either.
        let mut taken = ClipHint::default();
        see(&mut taken, Some(1), t0);
        taken.taken();
        assert!(!taken.showing(t0));
        see(&mut taken, Some(1), t0 + Duration::from_secs(1));
        assert!(!taken.showing(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn a_different_picture_is_offered_afresh_and_an_empty_clipboard_takes_it_down() {
        let t0 = Instant::now();
        let mut hint = ClipHint::default();
        see(&mut hint, Some(1), t0);
        hint.taken();

        let t1 = t0 + Duration::from_secs(20);
        see(&mut hint, Some(2), t1);
        assert!(hint.showing(t1), "a new picture is news");

        see(&mut hint, None, t1 + Duration::from_secs(1));
        assert!(
            !hint.showing(t1 + Duration::from_secs(1)),
            "gone from the clipboard"
        );

        // And the one that went, coming back, is new again.
        let t2 = t1 + Duration::from_secs(2);
        see(&mut hint, Some(2), t2);
        assert!(hint.showing(t2));
    }

    /// A look is read off the loop, so it can land after the paste it raced.
    /// The key that pastes starts a look of its own; the picture it reads is
    /// the one that was just taken, and offering it then would offer what is
    /// already on the line.
    #[test]
    fn a_look_that_raced_the_paste_does_not_offer_what_was_taken() {
        let t0 = Instant::now();
        let mut hint = ClipHint::default();
        let ticket = hint.looking(t0);
        hint.taken();
        hint.found(Some(1), t0 + Duration::from_millis(200), ticket);
        assert!(
            !hint.showing(t0 + Duration::from_millis(200)),
            "the picture was taken while it was being looked at"
        );
        // And it is now the one seen: a later look does not offer it either.
        let later = t0 + Duration::from_secs(3);
        let ticket = hint.looking(later);
        hint.found(Some(1), later, ticket);
        assert!(!hint.showing(later));
        // A new picture after that is offered as usual.
        let ticket = hint.looking(later);
        hint.found(Some(2), later, ticket);
        assert!(hint.showing(later));
    }

    #[test]
    fn looks_are_spaced() {
        let t0 = Instant::now();
        let mut hint = ClipHint::default();
        assert!(hint.due(t0), "the first look is always due");
        hint.looking(t0);
        assert!(!hint.due(t0 + LOOK_EVERY - Duration::from_millis(1)));
        assert!(hint.due(t0 + LOOK_EVERY));
    }
}
