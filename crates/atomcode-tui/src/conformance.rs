//! The property suite every module gets for free.
//!
//! Modules do **not** write their own property tests. Hand-written suites decay
//! predictably: the first module gets a good one, the seventeenth gets whatever
//! its author had patience for. One macro line gives every module the same
//! bar, which is also why adding a module is cheap — the two are the same fact
//! seen from either side.
//!
//! ```ignore
//! tui_conformance!(view Status);
//! tui_conformance!(producer Transcript);
//! ```
//!
//! Each property is paired with a probe in [`probes`] that is *known to violate
//! it*. A judge that only ever runs against sound objects has no discriminating
//! power, so the negative controls are part of the suite rather than an
//! afterthought.

use atomcode_harness::session::{HeaderReason, InjectionOrigin, NoticeKind, SessionEvent};
use atomcode_kernel::stream::TokenUsage;
use atomcode_kernel::tool::ToolCall;

/// A representative conversation, as facts.
///
/// Every variant a module might fold appears at least once, including the awkward
/// ones: a cancelled turn, a failed tool, reasoning, a notice, parallel calls.
pub fn facts() -> Vec<SessionEvent> {
    let call = |id: &str, name: &str, args: &str| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    };
    vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::UserMessage {
            turn: 1,
            text: "fix the build".into(),
            images: Vec::new(),
        },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::RequestHeader {
            turn: 1,
            round: 1,
            model: "replay".into(),
            reason: HeaderReason::Series,
        },
        SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "Look".into(),
            reasoning: false,
        },
        SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "ing.".into(),
            reasoning: false,
        },
        SessionEvent::AssistantChunk {
            turn: 1,
            round: 1,
            delta: "hmm".into(),
            reasoning: true,
        },
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: "Looking.".into(),
            reasoning: "hmm".into(),
            tool_calls: vec![
                call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                call("c2", "read_file", r#"{"file_path":"b.rs"}"#),
            ],
        },
        SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: "c1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            images: Vec::new(),
        },
        SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: "c2".into(),
            content: "no such file".into(),
            is_error: true,
            images: Vec::new(),
        },
        SessionEvent::StepEnd {
            turn: 1,
            step: 1,
            tool_calls: 2,
        },
        SessionEvent::Usage {
            turn: 1,
            round: 1,
            usage: TokenUsage {
                prompt: 1200,
                completion: 80,
                cached: 400,
            },
        },
        SessionEvent::Notice {
            turn: 1,
            notice: NoticeKind::RateLimited,
            detail: "rate limited; waiting 30s (1/5)".into(),
        },
        SessionEvent::Injected {
            turn: 1,
            text: "<system-reminder>keep going</system-reminder>".into(),
            origin: InjectionOrigin::Reminder,
        },
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 2,
            text: "Fixed it. 中文也要能画 🙂".into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
        },
        SessionEvent::TurnEnd {
            turn: 1,
            stop: atomcode_harness::seams::StopReason::Stopped,
            error: None,
        },
        SessionEvent::TurnStart { turn: 2 },
        SessionEvent::UserMessage {
            turn: 2,
            text: "now break it".into(),
            images: Vec::new(),
        },
        SessionEvent::TurnEnd {
            turn: 2,
            stop: atomcode_harness::seams::StopReason::Cancelled,
            error: Some("cancelled by the user".into()),
        },
    ]
}

/// Widths worth trying: the degenerate ones, the ones that split a wide
/// character, and an ordinary one.
pub const WIDTHS: &[u16] = &[0, 1, 2, 3, 7, 20, 80, 200];
/// Heights worth trying, same idea.
pub const HEIGHTS: &[u16] = &[0, 1, 2, 5, 24, 100];

/// Modules that are *known to be wrong*, so the suite can be shown to fail.
pub mod probes {
    use super::*;
    use crate::frame::Line;
    use crate::module::{Height, View};
    use crate::moment::Viewport;

    /// Draws wider than the rect it was given.
    pub struct Overflowing;
    impl View for Overflowing {
        type State = ();
        fn id() -> &'static str {
            "probe-overflowing"
        }
        fn absorb(_: &mut (), _: &SessionEvent) {}
        fn render(_: &(), vp: &Viewport<'_>) -> Vec<Line> {
            vec![Line::raw("x".repeat(vp.rect.w as usize + 5))]
        }
        fn height(_: &(), _: &crate::moment::Moment, _: u16) -> Height {
            Height::Fixed(1)
        }
    }

    /// Renders differently depending on where it sits — the shape a module
    /// takes when it reads something it does not own.
    pub struct PositionDependent;
    impl View for PositionDependent {
        type State = ();
        fn id() -> &'static str {
            "probe-position-dependent"
        }
        fn absorb(_: &mut (), _: &SessionEvent) {}
        fn render(_: &(), vp: &Viewport<'_>) -> Vec<Line> {
            vec![Line::raw(format!("at {},{}", vp.rect.x, vp.rect.y))]
        }
    }

    /// Panics at a width it did not expect.
    pub struct Fragile;
    impl View for Fragile {
        type State = ();
        fn id() -> &'static str {
            "probe-fragile"
        }
        fn absorb(_: &mut (), _: &SessionEvent) {}
        fn render(_: &(), vp: &Viewport<'_>) -> Vec<Line> {
            let text = "hello";
            // Slices without checking — the classic.
            vec![Line::raw(&text[..(vp.rect.w as usize).min(99)])]
        }
    }
}

/// Run the view suite over one module. Returns the failures, empty when sound.
///
/// A function rather than only a macro so the negative controls can call it on
/// a probe and assert that it *does* complain.
pub fn check_view<V: crate::module::View>() -> Vec<String> {
    use crate::frame::Rect;
    use crate::moment::Moment;

    let mut bad = Vec::new();
    let mounted = crate::module::Mounted::<V>::new();
    let obj: &dyn crate::module::ViewObject = &mounted;

    // 5. Renders at its default state, before any fact.
    let moment = Moment::default();
    for &w in WIDTHS {
        for &h in HEIGHTS {
            let vp = crate::moment::Viewport::new(Rect::sized(w, h), &moment);
            let a = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| obj.render(&vp)));
            let Ok(lines) = a else {
                bad.push(format!("{} panicked at {w}×{h}", V::id()));
                continue;
            };
            // 2. Never draws wider than the rect it was given.
            for (i, line) in lines.iter().enumerate() {
                if line.width() > w as usize {
                    bad.push(format!(
                        "{} line {i} is {} cells at width {w}",
                        V::id(),
                        line.width()
                    ));
                }
            }
            // 4. Deterministic.
            if obj.render(&vp) != lines {
                bad.push(format!("{} rendered differently twice at {w}×{h}", V::id()));
            }
        }
    }

    // Now with a conversation folded in, so the properties are checked against
    // real state rather than only the empty one.
    for fact in facts() {
        obj.absorb(&fact);
    }
    for &w in WIDTHS {
        let vp = crate::moment::Viewport::new(Rect::sized(w, 24), &moment);
        let Ok(lines) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| obj.render(&vp)))
        else {
            bad.push(format!("{} panicked at width {w} after folding", V::id()));
            continue;
        };
        for (i, line) in lines.iter().enumerate() {
            if line.width() > w as usize {
                bad.push(format!(
                    "{} line {i} is {} cells at width {w} after folding",
                    V::id(),
                    line.width()
                ));
            }
        }
        // 1. Position independent: same size elsewhere gives the same lines.
        let elsewhere = crate::moment::Viewport::new(Rect::new(7, 3, w, 24), &moment);
        if obj.render(&elsewhere) != lines {
            bad.push(format!(
                "{} renders differently at a different position (width {w})",
                V::id()
            ));
        }
    }
    bad
}

/// Run the stream-producer suite. Returns the failures.
pub fn check_producer(
    make: &dyn Fn() -> std::sync::Arc<dyn crate::module::Producer>,
) -> Vec<String> {
    use crate::block::Stream;

    let mut bad = Vec::new();
    let all = facts();

    // Fold one at a time.
    let mut incremental = Stream::new();
    let p = make();
    for fact in &all {
        let mut w = incremental.writer(p.id());
        p.absorb(fact, &mut w);
    }
    let inc_hashes = incremental.settled_hashes();

    // Fold the same facts into a fresh stream, and check the settled prefix
    // after every step only ever grows and never changes.
    let mut grown = Stream::new();
    let q = make();
    let mut prev: Vec<_> = Vec::new();
    for (i, fact) in all.iter().enumerate() {
        let mut w = grown.writer(q.id());
        q.absorb(fact, &mut w);
        let now = grown.settled_hashes();
        if now.len() < prev.len() {
            bad.push(format!("{} lost settled blocks at fact {i}", p.id()));
        } else if now[..prev.len()] != prev[..] {
            bad.push(format!(
                "{} rewrote a settled block at fact {i}: {:?} became {:?}",
                p.id(),
                prev,
                &now[..prev.len()]
            ));
        }
        prev = now;
    }

    // Replay equivalence: the two paths above must agree.
    if inc_hashes != prev {
        bad.push(format!(
            "{} folds differently incrementally than in one pass",
            p.id()
        ));
    }

    // Total: every fact, in isolation, must be survivable.
    for (i, fact) in all.iter().enumerate() {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut s = Stream::new();
            let p = make();
            let mut w = s.writer(p.id());
            p.absorb(fact, &mut w);
        }));
        if r.is_err() {
            bad.push(format!("{} panicked on fact {i}: {fact:?}", p.id()));
        }
    }
    bad
}

/// Give a module the whole suite, as one line in its test module.
///
/// A macro over a plain call so the generated test carries the module's name,
/// and so a module can never be *silently* uncovered — `every_module_is_covered`
/// walks the registry and fails on anything this was not applied to.
#[macro_export]
macro_rules! tui_conformance {
    (view $ty:ty as $name:ident) => {
        #[test]
        fn $name() {
            let bad = $crate::conformance::check_view::<$ty>();
            assert!(
                bad.is_empty(),
                "{} failed conformance: {bad:#?}",
                <$ty as $crate::module::View>::id()
            );
        }
    };
    (producer $make:expr, as $name:ident) => {
        #[test]
        fn $name() {
            let bad = $crate::conformance::check_producer(&$make);
            assert!(bad.is_empty(), "producer failed conformance: {bad:#?}");
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suite_passes_a_sound_module() {
        struct Fine;
        impl crate::module::View for Fine {
            type State = ();
            fn id() -> &'static str {
                "fine"
            }
            fn absorb(_: &mut (), _: &SessionEvent) {}
            fn render(_: &(), vp: &crate::moment::Viewport<'_>) -> Vec<crate::frame::Line> {
                vec![crate::frame::Line::raw(crate::width::take_width(
                    "hello",
                    vp.rect.w as usize,
                ))]
            }
        }
        assert!(check_view::<Fine>().is_empty());
    }

    // ---- negative controls: the suite must fail these -------------------

    #[test]
    fn the_suite_catches_a_module_drawing_too_wide() {
        let bad = check_view::<probes::Overflowing>();
        assert!(!bad.is_empty(), "an overflowing module must be caught");
        assert!(bad.iter().any(|m| m.contains("cells at width")), "{bad:?}");
    }

    #[test]
    fn the_suite_catches_a_module_that_reads_its_own_position() {
        let bad = check_view::<probes::PositionDependent>();
        assert!(
            bad.iter().any(|m| m.contains("different position")),
            "{bad:?}"
        );
    }

    #[test]
    fn the_suite_catches_a_module_that_panics() {
        let bad = check_view::<probes::Fragile>();
        assert!(bad.iter().any(|m| m.contains("panicked")), "{bad:?}");
    }

    #[test]
    fn the_corpus_covers_every_awkward_shape() {
        let f = facts();
        let has = |p: &dyn Fn(&SessionEvent) -> bool| f.iter().any(p);
        assert!(has(&|e| matches!(e, SessionEvent::Notice { .. })));
        assert!(has(&|e| matches!(e, SessionEvent::Injected { .. })));
        assert!(has(&|e| matches!(
            e,
            SessionEvent::ToolResultLogged { is_error: true, .. }
        )));
        assert!(has(&|e| matches!(
            e,
            SessionEvent::AssistantChunk {
                reasoning: true,
                ..
            }
        )));
        assert!(has(&|e| matches!(
            e,
            SessionEvent::TurnEnd {
                stop: atomcode_harness::seams::StopReason::Cancelled,
                ..
            }
        )));
        assert!(
            f.iter().any(|e| matches!(e, SessionEvent::AssistantMessage { tool_calls, .. } if tool_calls.len() >= 2)),
            "parallel calls must appear, they are what breaks a single-live-block design"
        );
    }
}
