//! The conversation's own scrollbar. Offsets remain distances from the bottom,
//! shared with wheel scrolling; a drag only maps screen cells to that offset.
use crate::frame::{Line, Rect};

pub(crate) fn content_rect(mut rect: Rect) -> Rect {
    if rect.w > 2 {
        rect.w -= 1;
    }
    rect
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Bar {
    pub track: Rect,
    max: usize,
    thumb: u16,
    start: u16,
    scroll: usize,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Drag {
    bar: Bar,
    offset: u16,
    on_thumb: bool,
}
impl Bar {
    pub fn new(track: Rect, max: usize, scroll: usize, visible: u16) -> Option<Self> {
        if max == 0 || track.h < 2 {
            return None;
        }
        let total = max.saturating_add(visible as usize);
        let thumb =
            ((track.h as u128 * visible as u128 / total as u128) as u16).clamp(1, track.h - 1);
        let travel = track.h - thumb;
        let start = ((max - scroll.min(max)) as u128 * travel as u128 / max as u128) as u16;
        Some(Self {
            track,
            max,
            thumb,
            start,
            scroll: scroll.min(max),
        })
    }
    pub fn grab(self, y: u16) -> Drag {
        let row = y.saturating_sub(self.track.y);
        let on_thumb = (self.start..self.start + self.thumb).contains(&row);
        let offset = if on_thumb {
            row - self.start
        } else {
            self.thumb / 2
        };
        Drag {
            bar: self,
            offset,
            on_thumb,
        }
    }
    pub fn lines(self, caps: &crate::caps::Caps) -> Vec<Line> {
        (0..self.track.h)
            .map(|row| {
                let selected = (self.start..self.start + self.thumb).contains(&row);
                let glyph = if selected {
                    crate::caps::Glyph::Thumb
                } else {
                    crate::caps::Glyph::Track
                };
                Line::styled(
                    caps.g(glyph),
                    crate::theme::fg(if selected {
                        crate::theme::Role::Accent
                    } else {
                        crate::theme::Role::Border
                    }),
                )
            })
            .collect()
    }
}
impl Drag {
    pub fn position(self, y: u16) -> usize {
        let travel = self.bar.track.h - self.bar.thumb;
        let start = y
            .saturating_sub(self.bar.track.y)
            .saturating_sub(self.offset)
            .min(travel);
        // Several offsets can share an endpoint cell. Grabbing that cell
        // without moving must preserve the offset rather than snap to the end.
        if self.on_thumb && start == self.bar.start {
            return self.bar.scroll;
        }
        if start == 0 {
            return self.bar.max;
        }
        if start == travel {
            return 0;
        }
        if self.on_thumb {
            let delta = start as i128 - self.bar.start as i128;
            return (self.bar.scroll as i128 - delta * self.bar.max as i128 / travel as i128)
                .clamp(0, self.bar.max as i128) as usize;
        }
        self.bar.max
            - ((start as u128 * self.bar.max as u128 + travel as u128 / 2) / travel as u128)
                as usize
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clicks_reach_both_ends_and_drag_keeps_the_grabbed_thumb_cell() {
        let bar = Bar::new(Rect::new(79, 3, 1, 20), 1000, 500, 20).unwrap();
        assert_eq!(bar.grab(3).position(3), 1000);
        assert_eq!(bar.grab(22).position(22), 0);
        let y = bar.track.y + bar.start;
        assert_eq!(bar.grab(y).position(y), 500);
        assert_eq!(bar.grab(y).position(0), 1000);
        assert_eq!(bar.grab(y).position(u16::MAX), 0);
    }
    #[test]
    fn grabbing_a_quantized_endpoint_preserves_the_offset_until_movement() {
        let track = Rect::new(79, 3, 1, 20);
        for scroll in [0, 1, 5000, 95000, 99999, 100000] {
            let bar = Bar::new(track, 100000, scroll, 20).unwrap();
            let y = track.y + bar.start;
            assert_eq!(bar.grab(y).position(y), scroll);
        }
        let bar = Bar::new(track, 100000, 95000, 20).unwrap();
        let drag = bar.grab(track.y);
        assert!(drag.position(track.y + 1) < 95000);
        assert_eq!(drag.position(track.bottom()), 0);
    }
    #[test]
    fn short_conversations_and_single_row_tracks_have_no_scrollbar() {
        assert!(Bar::new(Rect::new(10, 0, 1, 20), 0, 0, 20).is_none());
        assert!(Bar::new(Rect::new(10, 0, 1, 1), 100, 0, 1).is_none());
    }
}
