//! 一个多步流程(引导、配对的向导)画在底下:和 `/resume`、`/provider`、底部单子
//! 一样从输入框的位置升起来,而不是盖在对话中间的一个框。
//!
//! 流程本身(走到哪一步、答了什么、在等什么)是 overlay 的事(`crate::wizard`),
//! 宿主每一帧把它画进 [`crate::moment::Moment::flow`];这里只加上面板的骨架 ——
//! 一条规则线、一行表头 —— 然后把画好的那些行放进来。

use crate::frame::Line;
use crate::module::{Height, View};
use crate::modules::chrome::{self, panel_edge};
use crate::moment::{Moment, Viewport};

pub const ID: &str = "flow";

#[derive(Default)]
pub struct State;

pub struct FlowView;

impl View for FlowView {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(shown) = vp.moment.flow.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let mut out = vec![
            panel_edge(w, vp.moment.caps),
            Line::from_spans(chrome::header_parts(&shown.title, &[], usize::MAX).0).truncate(w),
        ];
        let room = vp.rect.h.saturating_sub(3) as usize;
        let body_len = shown.lines.len().saturating_sub(1);
        let offset = shown.scroll.min(body_len.saturating_sub(room));
        out.extend(
            shown
                .lines
                .iter()
                .take(body_len)
                .skip(offset)
                .take(room)
                .map(|line| line.clone().truncate(w)),
        );
        if vp.rect.h > 2 {
            if let Some(hint) = shown.lines.last() {
                out.push(hint.clone().truncate(w));
            }
        }
        out.truncate(vp.rect.h as usize);
        out
    }

    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        match moment.flow.as_ref() {
            Some(shown) if width > 0 => {
                Height::Hug((2 + shown.lines.len()).min(u16::MAX as usize) as u16)
            }
            _ => Height::Hug(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;

    #[test]
    fn overflowing_flow_scrolls_its_body_and_keeps_the_hint_visible() {
        let mut moment = Moment {
            flow: Some(crate::overlay::Shown {
                id: "app-pair".into(),
                title: "Pair".into(),
                scroll: 0,
                lines: (0..40)
                    .map(|n| Line::raw(format!("qr row {n}")))
                    .chain([Line::raw("enter continue · esc cancel")])
                    .collect(),
            }),
            ..Default::default()
        };
        let first = FlowView::render(&State, &Viewport::new(Rect::sized(80, 12), &moment));
        assert_eq!(first.len(), 12);
        assert_eq!(first.last().unwrap().plain(), "enter continue · esc cancel");
        assert_eq!(first[2].plain(), "qr row 0");
        moment.flow.as_mut().unwrap().scroll = 40;
        let last = FlowView::render(&State, &Viewport::new(Rect::sized(80, 12), &moment));
        assert_eq!(last[10].plain(), "qr row 39");
        assert_eq!(last.last().unwrap().plain(), "enter continue · esc cancel");
    }

    /// 表头是流程的名字,底下是它自己画的那些行,整块和它要的一样高。
    #[test]
    fn a_flow_is_a_rule_its_title_and_what_it_drew() {
        let moment = Moment {
            flow: Some(crate::overlay::Shown {
                id: "onboarding".into(),
                scroll: 0,
                title: "首次设置".into(),
                lines: vec![Line::raw("  选一个语言"), Line::raw("  enter 继续")],
            }),
            ..Default::default()
        };
        assert_eq!(FlowView::height(&State, &moment, 40), Height::Hug(4));
        let vp = Viewport::new(Rect::sized(40, 4), &moment);
        let lines: Vec<String> = FlowView::render(&State, &vp)
            .iter()
            .map(|l| l.plain())
            .collect();
        assert!(lines[1].contains("首次设置"), "{lines:?}");
        assert_eq!(lines[2], "  选一个语言");
        assert_eq!(
            FlowView::height(&State, &Moment::default(), 40),
            Height::Hug(0)
        );
    }
}
