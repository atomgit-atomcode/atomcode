//! `/usage` — a tabbed CodingPlan usage modal (Current | Overview | Models).

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use atomcode_core::coding_plan::types::RateLimitWindow;
use atomcode_core::coding_plan::usage::{OverviewStats, UsageResponse};

use super::{Modal, ModalAction};
use crate::event_loop::{Buffer, LoopCtx};
use crate::render::Renderer;
use crate::state::UiState;

/// Which tab is currently active in the usage modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Current,
    Overview,
    Models,
}

/// Data fetched for the usage modal.
/// Fields are filled asynchronously; any may be `None` while loading or on error.
#[allow(dead_code)] // filled in Task 8
pub struct UsageData {
    pub window: Option<RateLimitWindow>,
    pub usage: Option<UsageResponse>,
    pub overview: Option<OverviewStats>,
    pub error: Option<String>,
}

/// Tabbed `/usage` modal.
#[allow(dead_code)] // `data` read in Task 8
pub struct UsageModal {
    pub(crate) data: UsageData,
    pub(crate) tab: Tab,
}

impl UsageModal {
    pub fn new(data: UsageData) -> Self {
        Self { data, tab: Tab::Current }
    }

    pub(crate) fn next_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Current => Tab::Overview,
            Tab::Overview => Tab::Models,
            Tab::Models => Tab::Current,
        };
    }

    pub(crate) fn prev_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Current => Tab::Models,
            Tab::Overview => Tab::Current,
            Tab::Models => Tab::Overview,
        };
    }

    pub(crate) fn select_tab(&mut self, c: char) {
        self.tab = match c {
            '1' => Tab::Current,
            '2' => Tab::Overview,
            '3' => Tab::Models,
            _ => self.tab,
        };
    }
}

impl Modal for UsageModal {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(ModalAction::Close),
            KeyCode::Tab | KeyCode::Right => self.next_tab(),
            KeyCode::BackTab | KeyCode::Left => self.prev_tab(),
            KeyCode::Char(c @ '1'..='3') => self.select_tab(c),
            _ => {}
        }
        let _ = mods;
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        // Implemented in Task 8.
        let _ = (buf, state, ctx, renderer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_data() -> UsageData {
        UsageData { window: None, usage: None, overview: None, error: None }
    }

    #[test]
    fn tab_cycles_right_and_wraps() {
        let mut m = UsageModal::new(empty_data());
        assert_eq!(m.tab, Tab::Current);
        m.next_tab();
        assert_eq!(m.tab, Tab::Overview);
        m.next_tab();
        assert_eq!(m.tab, Tab::Models);
        m.next_tab();
        assert_eq!(m.tab, Tab::Current); // wrap
        m.prev_tab();
        assert_eq!(m.tab, Tab::Models); // wrap back
    }

    #[test]
    fn number_keys_jump_tabs() {
        let mut m = UsageModal::new(empty_data());
        m.select_tab('3');
        assert_eq!(m.tab, Tab::Models);
        m.select_tab('1');
        assert_eq!(m.tab, Tab::Current);
    }
}
