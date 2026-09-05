//! The two kinds of module, and the registries the host finds them in.
//!
//! # Why two kinds and not one
//!
//! Irreversibility applies to one of them and not the other. A **stream
//! producer** appends blocks that can never be taken back; a **view module**
//! is redrawn whole every frame and has no history. Conflating them grows two
//! opposite bugs: a status bar that starts accumulating history, and a
//! transcript that gets repainted from scratch. See `docs/adr/0004`.
//!
//! # Why the author writes associated functions
//!
//! [`View::render`] takes `&State`, not `&self`. A module therefore *cannot*
//! capture a `Context`, a service handle, a channel or a clock — the signature
//! makes the "no side effects, no IO, not async" obligation unrepresentable
//! rather than merely discouraged. The host stores the object-safe
//! [`ViewObject`] that [`Mounted`] wraps around it.

use std::sync::RwLock;
use std::time::Duration;

use atomcode_harness::session::SessionEvent;

use crate::block::StreamWriter;
use crate::frame::Line;
use crate::moment::Viewport;

/// How much vertical space a module asks for. The host arbitrates; a module
/// requests, so one module can never blow up the layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Height {
    /// Exactly this many rows.
    Fixed(u16),
    /// At most this many, fewer if it has less to say.
    Hug(u16),
    /// Whatever is left.
    Fill,
}

/// A view module: state folded from facts, redrawn every frame.
pub trait View: Send + Sync + 'static {
    type State: Default + Send + Sync + 'static;

    fn id() -> &'static str;

    /// Fold one committed fact. Pure and order-dependent only on the log, so a
    /// replay reproduces the state exactly.
    fn absorb(state: &mut Self::State, fact: &SessionEvent);

    /// Draw. Pure: the same `(state, viewport)` must give the same lines, on
    /// any machine, at any time. Reading a clock here would leave the whole
    /// test loop green and untrustworthy — see `docs/adr/0008`; time arrives
    /// through `viewport.moment`.
    fn render(state: &Self::State, viewport: &Viewport<'_>) -> Vec<Line>;

    fn height(_state: &Self::State) -> Height {
        Height::Fill
    }

    /// Take something the host computed. Default: ignore it.
    fn set_menu(_state: &mut Self::State, _menu: Vec<(String, String)>) {}

    /// Ask to be redrawn on a timer even when no fact arrives. `None` (the
    /// default) means this module only changes when the conversation does —
    /// which is what stops an idle screen from burning bandwidth.
    fn tick() -> Option<Duration> {
        None
    }
}

/// The object-safe face the host stores.
pub trait ViewObject: Send + Sync {
    fn id(&self) -> &'static str;
    fn absorb(&self, fact: &SessionEvent);
    fn render(&self, viewport: &Viewport<'_>) -> Vec<Line>;
    fn height(&self) -> Height;
    fn tick(&self) -> Option<Duration>;
    /// Hand a module something the host computed for it.
    ///
    /// The slash menu is the host's — it owns the command registry — but the
    /// input module is what draws it. Deliberately narrow: this is not a
    /// general back door into a module's state, and a module that ignores it
    /// (the default) is unaffected.
    fn set_menu(&self, _menu: Vec<(String, String)>) {}
}

/// A [`View`] plus the state it has folded so far.
pub struct Mounted<V: View> {
    state: RwLock<V::State>,
}

impl<V: View> Default for Mounted<V> {
    fn default() -> Self {
        Self {
            state: RwLock::new(V::State::default()),
        }
    }
}

impl<V: View> Mounted<V> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<V: View> ViewObject for Mounted<V> {
    fn id(&self) -> &'static str {
        V::id()
    }
    fn absorb(&self, fact: &SessionEvent) {
        V::absorb(&mut self.state.write().expect("view state poisoned"), fact);
    }
    fn render(&self, viewport: &Viewport<'_>) -> Vec<Line> {
        V::render(&self.state.read().expect("view state poisoned"), viewport)
    }
    fn height(&self) -> Height {
        V::height(&self.state.read().expect("view state poisoned"))
    }
    fn tick(&self) -> Option<Duration> {
        V::tick()
    }
    fn set_menu(&self, menu: Vec<(String, String)>) {
        V::set_menu(&mut self.state.write().expect("view state poisoned"), menu);
    }
}

/// A stream producer: turns facts into blocks that can never be taken back.
pub trait Producer: Send + Sync {
    fn id(&self) -> &'static str;

    /// Fold one fact into the stream. The writer is the only thing that can
    /// change it, and it cannot reach a settled block.
    fn absorb(&self, fact: &SessionEvent, out: &mut StreamWriter<'_>);
}

/// Everything mounted, found by id.
///
/// Read fresh on every frame rather than snapshotted at start-up — the same
/// rule the tool catalog follows, and the reason a module mounted mid-turn
/// appears on the very next frame.
#[derive(Default)]
pub struct Modules {
    views: RwLock<Vec<std::sync::Arc<dyn ViewObject>>>,
    producers: RwLock<Vec<std::sync::Arc<dyn Producer>>>,
}

impl Modules {
    pub fn new() -> Self {
        Self::default()
    }

    /// Two rows claiming one id is an error, not last-write-wins: a layout
    /// naming that id would be ambiguous, and silently picking one is the worst
    /// available answer.
    pub fn add_view(&self, view: std::sync::Arc<dyn ViewObject>) -> Result<(), String> {
        let mut views = self.views.write().expect("modules poisoned");
        if views.iter().any(|v| v.id() == view.id()) {
            return Err(format!(
                "view module `{}` is already mounted; disable the row that owns it first",
                view.id()
            ));
        }
        views.push(view);
        Ok(())
    }

    pub fn add_producer(&self, p: std::sync::Arc<dyn Producer>) -> Result<(), String> {
        let mut ps = self.producers.write().expect("modules poisoned");
        if ps.iter().any(|x| x.id() == p.id()) {
            return Err(format!("stream producer `{}` is already mounted", p.id()));
        }
        ps.push(p);
        Ok(())
    }

    pub fn remove_view(&self, id: &str) {
        self.views
            .write()
            .expect("modules poisoned")
            .retain(|v| v.id() != id);
    }

    pub fn remove_producer(&self, id: &str) {
        self.producers
            .write()
            .expect("modules poisoned")
            .retain(|p| p.id() != id);
    }

    pub fn view(&self, id: &str) -> Option<std::sync::Arc<dyn ViewObject>> {
        self.views
            .read()
            .expect("modules poisoned")
            .iter()
            .find(|v| v.id() == id)
            .cloned()
    }

    pub fn view_ids(&self) -> Vec<&'static str> {
        self.views
            .read()
            .expect("modules poisoned")
            .iter()
            .map(|v| v.id())
            .collect()
    }

    pub fn producers(&self) -> Vec<std::sync::Arc<dyn Producer>> {
        self.producers.read().expect("modules poisoned").clone()
    }

    pub fn has_view(&self, id: &str) -> bool {
        self.view(id).is_some()
    }

    /// The shortest interval any mounted module asked for. `None` when nothing
    /// animates, and then the host redraws only on facts and input.
    pub fn tick(&self) -> Option<Duration> {
        self.views
            .read()
            .expect("modules poisoned")
            .iter()
            .filter_map(|v| v.tick())
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::Moment;
    use std::sync::Arc;

    struct Counter;
    #[derive(Default)]
    struct Count(u32);

    impl View for Counter {
        type State = Count;
        fn id() -> &'static str {
            "counter"
        }
        fn absorb(state: &mut Count, fact: &SessionEvent) {
            if matches!(fact, SessionEvent::TurnStart { .. }) {
                state.0 += 1;
            }
        }
        fn render(state: &Count, _vp: &Viewport<'_>) -> Vec<Line> {
            vec![Line::raw(format!("turns: {}", state.0))]
        }
        fn height(_: &Count) -> Height {
            Height::Fixed(1)
        }
    }

    #[test]
    fn a_view_folds_facts_and_renders_from_what_it_folded() {
        let m: Arc<dyn ViewObject> = Arc::new(Mounted::<Counter>::new());
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(20, 1), &moment);
        assert_eq!(m.render(&vp)[0].plain(), "turns: 0");
        m.absorb(&SessionEvent::TurnStart { turn: 1 });
        m.absorb(&SessionEvent::TurnStart { turn: 2 });
        assert_eq!(m.render(&vp)[0].plain(), "turns: 2");
        assert_eq!(m.height(), Height::Fixed(1));
    }

    #[test]
    fn two_rows_claiming_one_id_is_an_error_not_last_write_wins() {
        let mods = Modules::new();
        assert!(mods.add_view(Arc::new(Mounted::<Counter>::new())).is_ok());
        let err = mods
            .add_view(Arc::new(Mounted::<Counter>::new()))
            .unwrap_err();
        assert!(err.contains("already mounted"), "{err}");
    }

    #[test]
    fn unmounting_removes_it_from_the_next_frame() {
        let mods = Modules::new();
        mods.add_view(Arc::new(Mounted::<Counter>::new())).unwrap();
        assert!(mods.has_view("counter"));
        mods.remove_view("counter");
        assert!(!mods.has_view("counter"), "read fresh, not snapshotted");
    }

    #[test]
    fn nothing_animating_means_no_timer_at_all() {
        let mods = Modules::new();
        mods.add_view(Arc::new(Mounted::<Counter>::new())).unwrap();
        assert_eq!(mods.tick(), None, "an idle screen must not burn bandwidth");
    }
}
