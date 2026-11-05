//! Read-only model cards inside the existing provider panel.
use crate::i18n::{t, Msg};
use crate::providers::{ModelRow, ProvidersView};
use crate::surface::{Key, KeyPress};
use atomcode_capabilities::model_cards::{catalog, resolve_id};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardPanel {
    configured: String,
    primary: Option<usize>,
    pub scroll: usize,
    notes: bool,
}
fn ids() -> Vec<&'static str> {
    catalog()["models"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}
pub(crate) fn viewport_rows(total: usize, height: usize) -> usize {
    total.min(height.saturating_sub(5).min(16))
}

impl CardPanel {
    pub fn new(model: &ModelRow) -> Self {
        Self {
            configured: model.id.clone(),
            primary: resolve_id(&model.model).and_then(|id| ids().iter().position(|x| *x == id)),
            scroll: 0,
            notes: false,
        }
    }
    pub fn clamp(&mut self, view: &ProvidersView, width: usize, height: usize) {
        let total = lines(view, self, width).len();
        self.scroll = self
            .scroll
            .min(total.saturating_sub(viewport_rows(total, height).max(1)));
    }
    /// True closes only this read-only view; never changes a runtime model.
    pub fn key(&mut self, press: KeyPress) -> bool {
        if press == KeyPress::ctrl('c') {
            return true;
        }
        let count = ids().len();
        match press.key {
            Key::Esc => return true,
            Key::Up => self.scroll = self.scroll.saturating_sub(1),
            Key::Down => self.scroll = self.scroll.saturating_add(1),
            Key::PageUp => self.scroll = self.scroll.saturating_sub(8),
            Key::PageDown => self.scroll = self.scroll.saturating_add(8),
            Key::Home => self.scroll = 0,
            Key::End => self.scroll = usize::MAX,
            Key::Left | Key::Right if count > 0 => {
                let forward = press.key == Key::Right;
                self.primary = Some(match self.primary {
                    None => 0,
                    Some(at) if forward => (at + 1) % count,
                    Some(at) => (at + count - 1) % count,
                });
                self.scroll = 0;
            }
            Key::Char('d') => {
                self.notes = !self.notes;
                self.scroll = 0;
            }
            _ => {}
        }
        false
    }
}
fn task_name(id: &str) -> String {
    let msg = match id {
        "agents" => Msg::ModelCardAgents,
        "coding" => Msg::ModelCardCoding,
        "chat" => Msg::ModelCardChat,
        "math" => Msg::ModelCardMath,
        "vision" => Msg::ModelCardVision,
        "search" => Msg::ModelCardSearch,
        _ => return id.into(),
    };
    t(msg).into_owned()
}
fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("—")
}
fn padded(value: &str, columns: usize) -> String {
    format!(
        "{}{}",
        value,
        " ".repeat(columns.saturating_sub(crate::width::str_width(value)))
    )
}
pub fn lines(view: &ProvidersView, panel: &CardPanel, width: usize) -> Vec<String> {
    let data = catalog();
    let keys = ids();
    let mut out = vec![t(Msg::ModelCardTitle).into_owned()];
    let no_public = view.model(&panel.configured).is_some_and(|model| {
        atomcode_capabilities::model_cards::status(&model.model) == "no_public_data"
    });
    if let Some(model) = view.model(&panel.configured) {
        out.push(format!("{}: {}", t(Msg::ModelCardConfigured), model.model));
        out.push(format!(
            "{}: {}  {}: {}",
            t(Msg::FieldWindow),
            model.window,
            t(Msg::FieldVision),
            model
                .vision
                .map(|v| t(if v { Msg::VisionYes } else { Msg::VisionNo }).into_owned())
                .unwrap_or("—".into())
        ));
        out.push(t(Msg::ContextBudgetSource).into_owned());
        if matches!(model.model.rsplit('/').next(), Some("auto" | "auto-beta")) {
            out.push(t(Msg::AutoContextBudgetNote).into_owned());
        }
    }
    if no_public {
        out.push(t(Msg::ModelCardNoPublic).into_owned());
    }
    let Some(primary) = panel.primary.and_then(|i| keys.get(i)) else {
        if !no_public {
            out.push(t(Msg::ModelCardEmpty).into_owned());
        }
        out.push(t(Msg::ModelCardWebHint).into_owned());
        return wrap(out, width);
    };
    let card = &data["models"][*primary];
    out.push(format!(
        "{}: {}",
        t(Msg::ModelCardViewing),
        text(&card["name"])
    ));
    out.push(t(Msg::ModelCardReadOnly).into_owned());
    if panel.notes {
        out.push(t(Msg::ModelCardNotes).into_owned());
        out.push(text(&data["upstream"]["updated"]).into());
        out.push(text(&data["attribution"]["credit"]).into());
        out.push(text(&data["attribution"]["license_url"]).into());
        out.push(t(Msg::ModelCardWebHint).into_owned());
        return wrap(out, width);
    }
    for (field, label) in [
        ("best_task", Msg::ModelCardBest),
        ("worst_task", Msg::ModelCardWorst),
    ] {
        if let Some(id) = card[field].as_str() {
            out.push(format!("{}: {}", t(label), task_name(id)));
        }
    }
    if card["valid_benchmark_count"].as_u64().unwrap_or(0) < 5 {
        out.push(t(Msg::ModelCardSparse).into_owned());
    }
    let definitions = data["task_definitions"].as_array().unwrap();
    let task_width = definitions
        .iter()
        .map(|d| crate::width::str_width(&task_name(text(&d["id"]))))
        .chain([crate::width::str_width(&t(Msg::ModelCardTasks))])
        .max()
        .unwrap_or(0);
    let score_width = crate::width::str_width(&t(Msg::ModelCardScore)).max(5);
    out.push(format!(
        "{} | {} | {}",
        padded(&t(Msg::ModelCardTasks), task_width),
        padded(&t(Msg::ModelCardScore), score_width),
        t(Msg::ModelCardRank)
    ));
    for definition in definitions {
        let id = text(&definition["id"]);
        let task = card["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"] == id);
        let score = task
            .and_then(|v| v["g"].as_f64())
            .map(|v| format!("{v:.1}"))
            .unwrap_or("—".into());
        let rank = task
            .and_then(|v| v["rank"].as_u64().zip(v["total"].as_u64()))
            .map(|(rank, total)| format!("{rank}/{total}"))
            .unwrap_or("—".into());
        out.push(format!(
            "{} | {} | {}",
            padded(&task_name(id), task_width),
            padded(&score, score_width),
            rank
        ));
    }
    out.push(t(Msg::ModelCardScope).into_owned());
    out.push(t(Msg::ModelCardWebHint).into_owned());
    wrap(out, width)
}

fn wrap(lines: Vec<String>, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in lines {
        let line = crate::text::for_screen(&line);
        let mut chunk = String::new();
        let mut used = 0;
        for c in line.chars() {
            let size = crate::width::char_width(c);
            if used + size > width.max(1) && !chunk.is_empty() {
                result.push(std::mem::take(&mut chunk));
                used = 0;
            }
            chunk.push(c);
            used += size;
        }
        result.push(chunk);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{key, Panel, Step, Tab};
    fn fixture(wire: &str) -> (ProvidersView, ModelRow) {
        let model = ModelRow {
            id: "configured".into(),
            account: "account".into(),
            model: wire.into(),
            window: 12345,
            vision: Some(true),
            effort: None,
            levels: vec![],
            current: true,
            managed: false,
        };
        (
            ProvidersView::new(vec![], vec![model.clone()], vec![], vec![]),
            model,
        )
    }
    #[test]
    fn card_keys_are_read_only_and_escape_returns_to_model_list() {
        let (view, _) = fixture("glm-5.3-flash");
        let mut panel = Panel {
            tab: Tab::Models,
            ..Default::default()
        };
        view.settle_cursor(&mut panel);
        let mut secret = String::new();
        assert_eq!(
            key(&view, &mut panel, &mut secret, KeyPress::plain(Key::F(2))),
            Step::Stay
        );
        assert!(panel.card.is_some());
        for press in [Key::Tab, Key::Down, Key::Right, Key::Char('d')] {
            assert_eq!(
                key(&view, &mut panel, &mut secret, KeyPress::plain(press)),
                Step::Stay
            );
        }
        assert_eq!(
            key(&view, &mut panel, &mut secret, KeyPress::plain(Key::Esc)),
            Step::Stay
        );
        assert!(panel.card.is_none());
        assert_eq!(view.models()[0].model, "glm-5.3-flash");
    }
    #[test]
    fn unlisted_model_has_no_score_and_browsing_does_not_replace_its_identity() {
        let (view, row) = fixture("lumen-alpha");
        let mut card = CardPanel::new(&row);
        assert!(card.primary.is_none());
        let initial = lines(&view, &card, 200);
        assert!(initial
            .iter()
            .any(|line| line.contains(t(Msg::ModelCardNoPublic).as_ref())));
        assert!(!initial.iter().any(|line| line.starts_with("A:")));
        card.key(KeyPress::plain(Key::Right));
        let browsing = lines(&view, &card, 200);
        assert!(browsing.iter().any(|line| line.contains("lumen-alpha")));
        assert!(browsing
            .iter()
            .any(|line| line.contains(t(Msg::ModelCardNoPublic).as_ref())));
        assert_eq!(view.models()[0].model, "lumen-alpha");
    }
    #[test]
    fn tab_does_not_enable_comparison_and_summary_names_the_browsed_model() {
        let (view, row) = fixture("glm-5.3-flash");
        let mut card = CardPanel::new(&row);
        let before = card.clone();
        card.key(KeyPress::plain(Key::Tab));
        card.key(KeyPress::plain(Key::BackTab));
        assert_eq!(card, before);
        let rendered = lines(&view, &card, 140);
        assert!(rendered
            .iter()
            .any(|line| line.contains(t(Msg::ModelCardViewing).as_ref())));
        assert!(rendered
            .iter()
            .any(|line| line.contains(t(Msg::ModelCardRank).as_ref())));
        assert!(rendered.iter().any(|line| line.contains("/webui")));
        assert!(!rendered
            .iter()
            .any(|line| line.starts_with("A:") || line.starts_with("B:")));
    }
    #[test]
    fn card_scroll_stops_at_the_last_full_page_and_short_cards_do_not_scroll() {
        let (view, row) = fixture("unmapped-model");
        let mut card = CardPanel::new(&row);
        let total = lines(&view, &card, 140).len();
        assert!(total <= 16);
        card.scroll = usize::MAX;
        card.clamp(&view, 140, 21);
        assert_eq!(card.scroll, 0);
        card.scroll = usize::MAX;
        card.clamp(&view, 140, 7);
        assert_eq!(card.scroll, total.saturating_sub(2));
        card.key(KeyPress::plain(Key::Up));
        card.clamp(&view, 140, 7);
        assert_eq!(card.scroll, total.saturating_sub(3));
    }

    #[test]
    fn narrow_cards_keep_configuration_and_scroll_back_from_end() {
        let (view, row) = fixture("glm-5.3-flash");
        let mut card = CardPanel::new(&row);
        let rendered = lines(&view, &card, 24);
        assert!(rendered.iter().any(|line| line.contains("12345")));
        assert!(rendered
            .iter()
            .all(|line| crate::width::str_width(line) <= 24));
        card.key(KeyPress::plain(Key::End));
        card.clamp(&view, 24, 21);
        let end = card.scroll;
        card.key(KeyPress::plain(Key::Up));
        card.clamp(&view, 24, 21);
        assert_eq!(card.scroll, end - 1);
    }
    #[test]
    fn unknown_alias_requires_manual_browsing_without_changing_configuration() {
        let (view, row) = fixture("deepseek-flash");
        let mut card = CardPanel::new(&row);
        assert!(card.primary.is_none());
        assert!(
            lines(&view, &card, 80)
                .iter()
                .any(|line| line
                    .contains(&t(Msg::ModelCardEmpty).chars().take(8).collect::<String>()))
        );
        card.key(KeyPress::plain(Key::Right));
        card.key(KeyPress::plain(Key::Tab));
        assert!(card.primary.is_some());
        assert!(lines(&view, &card, 140)
            .iter()
            .any(|line| line.contains("deepseek-flash")));
        assert!(!lines(&view, &card, 140)
            .iter()
            .any(|line| line.starts_with("B:")));
    }
}
