//! Read-only model cards inside the existing provider panel.
use crate::i18n::{t, Msg};
use crate::providers::{ModelRow, ProvidersView};
use crate::surface::{Key, KeyPress};
use atomcode_capabilities::model_cards::{catalog, resolve_id, score_label};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardPanel {
    configured: String,
    primary: Option<usize>,
    secondary: Option<usize>,
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
impl CardPanel {
    pub fn new(model: &ModelRow) -> Self {
        Self {
            configured: model.id.clone(),
            primary: resolve_id(&model.model).and_then(|id| ids().iter().position(|x| *x == id)),
            secondary: None,
            scroll: 0,
            notes: false,
        }
    }
    pub fn clamp(&mut self, view: &ProvidersView, width: usize) {
        self.scroll = self
            .scroll
            .min(lines(view, self, width).len().saturating_sub(1));
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
                if self.primary == self.secondary {
                    self.secondary = None;
                }
                self.scroll = 0;
            }
            Key::Tab | Key::BackTab if self.primary.is_some() && count > 1 => {
                // One extra position means "no comparison".
                let step = if press.key == Key::BackTab { count } else { 1 };
                let mut next = (self.secondary.map(|n| n + 1).unwrap_or(0) + step) % (count + 1);
                if next > 0 && Some(next - 1) == self.primary {
                    next = (next + step) % (count + 1);
                }
                self.secondary = next.checked_sub(1);
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
fn task_score(task: Option<&Value>) -> String {
    let Some(task) = task else {
        return "—".into();
    };
    format!(
        "{}  ({}/{}, {} {})",
        task["g"]
            .as_f64()
            .map(|n| format!("{n:.1}"))
            .unwrap_or("—".into()),
        task["rank"]
            .as_u64()
            .map(|n| n.to_string())
            .unwrap_or("—".into()),
        task["total"],
        t(Msg::ModelCardEvidence),
        task["n"]
    )
}
fn same_conditions(a: &Value, b: &Value) -> bool {
    [
        "bench",
        "source",
        "source_id",
        "by",
        "date",
        "note",
        "mode",
        "settings",
        "configuration",
        "api_configuration",
        "strong_prompting",
        "evaluator",
        "engine_label",
        "evaluation_run_group",
        "voice",
    ]
    .iter()
    .all(|key| a[*key] == b[*key])
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
    }
    if no_public {
        out.push(t(Msg::ModelCardNoPublic).into_owned());
    }
    let Some(primary) = panel.primary.and_then(|i| keys.get(i)) else {
        if !no_public {
            out.push(t(Msg::ModelCardEmpty).into_owned());
        }
        return wrap(out, width);
    };
    let a = &data["models"][*primary];
    let b = panel
        .secondary
        .and_then(|i| keys.get(i))
        .map(|id| &data["models"][*id]);
    out.push(format!("A: {}", text(&a["name"])));
    if let Some(b) = b {
        out.push(format!("B: {}", text(&b["name"])));
    }
    if panel.notes {
        out.push(t(Msg::ModelCardNotes).into_owned());
        out.push(format!(
            "{} · {}",
            text(&data["upstream"]["updated"]),
            text(&data["upstream"]["commit"])
        ));
        out.push(text(&data["attribution"]["credit"]).into());
        out.push(text(&data["attribution"]["license_url"]).into());
        out.push(text(&data["attribution"]["upstream_notices"]).into());
        for card in [Some(a), b].into_iter().flatten() {
            out.push(text(&card["name"]).into());
            if let Some(note) = card["identity_note"].as_str() {
                out.push(note.into());
            }
            for row in card["evaluations"].as_array().unwrap() {
                out.push(format!(
                    "{} · {} · {}",
                    text(&data["benchmarks"][text(&row["bench"])]["name"]),
                    text(&row["date"]),
                    text(&row["by"])
                ));
                out.push(text(&row["note"]).into());
                out.push(text(&row["source"]).into());
            }
        }
        for source in data["sources"].as_object().unwrap().values() {
            out.push(format!(
                "{} · {}",
                text(&source["name"]),
                text(&source["license"])
            ));
            out.push(text(&source["attribution"]).into());
        }
        return wrap(out, width);
    }
    for card in [Some(a), b].into_iter().flatten() {
        if card["valid_benchmark_count"].as_u64().unwrap_or(0) < 5 {
            out.push(t(Msg::ModelCardSparse).into_owned());
        }
        for (field, label) in [
            ("best_task", Msg::ModelCardBest),
            ("worst_task", Msg::ModelCardWorst),
        ] {
            if let Some(id) = card[field].as_str() {
                out.push(format!(
                    "{}: {} · {}",
                    text(&card["name"]),
                    t(label),
                    task_name(id)
                ));
            }
        }
    }
    out.push(t(Msg::ModelCardTasks).into_owned());
    out.push(t(Msg::ModelCardScope).into_owned());
    for definition in data["task_definitions"].as_array().unwrap() {
        let id = text(&definition["id"]);
        let find = |card: &Value| {
            card["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|task| task["id"] == id)
                .cloned()
        };
        let left = find(a);
        let right = b.and_then(find);
        out.push(format!(
            "{}: A {}{}",
            task_name(id),
            task_score(left.as_ref()),
            if b.is_some() {
                format!(" | B {}", task_score(right.as_ref()))
            } else {
                String::new()
            }
        ));
        if let (Some(l), Some(r)) = (&left, &right) {
            if l["comparison_set"]["id"].is_null()
                || l["comparison_set"]["id"] != r["comparison_set"]["id"]
            {
                out.push(t(Msg::ModelCardDifferent).into_owned());
            }
        }
    }
    out.push(t(Msg::ModelCardBenchmarks).into_owned());
    if b.is_some() {
        out.push(t(Msg::ModelCardDifferent).into_owned());
    }
    let mut right = b
        .map(|card| {
            card["evaluations"]
                .as_array()
                .unwrap()
                .iter()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for row in a["evaluations"].as_array().unwrap() {
        let bench = &data["benchmarks"][text(&row["bench"])];
        let other = right
            .iter()
            .position(|r| same_conditions(row, r))
            .map(|i| right.remove(i));
        let mut line = format!(
            "{}: A {}",
            text(&bench["name"]),
            score_label(row["score"].as_f64().unwrap(), bench)
        );
        if b.is_some() {
            line.push_str(&format!(
                " | B {}",
                other
                    .map(|r| score_label(r["score"].as_f64().unwrap(), bench))
                    .unwrap_or("—".into())
            ));
        }
        out.push(line);
    }
    for row in right {
        let bench = &data["benchmarks"][text(&row["bench"])];
        out.push(format!(
            "{}: A — | B {}",
            text(&bench["name"]),
            score_label(row["score"].as_f64().unwrap(), bench)
        ));
    }
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
    fn shift_tab_reverses_comparison_without_selecting_the_primary_card() {
        let (_, row) = fixture("glm-5.3-flash");
        let mut card = CardPanel::new(&row);
        card.key(KeyPress::plain(Key::Tab));
        assert!(card.secondary.is_some());
        assert_ne!(card.primary, card.secondary);
        card.key(KeyPress::plain(Key::BackTab));
        assert!(card.secondary.is_none());
        card.key(KeyPress::plain(Key::BackTab));
        assert!(card.secondary.is_some());
        assert_ne!(card.primary, card.secondary);
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
        card.clamp(&view, 24);
        let end = card.scroll;
        card.key(KeyPress::plain(Key::Up));
        card.clamp(&view, 24);
        assert_eq!(card.scroll, end - 1);
    }
    #[test]
    fn unknown_alias_requires_manual_browsing_and_comparison_uses_two_cards() {
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
        assert!(card.secondary.is_some());
        assert_ne!(card.primary, card.secondary);
        assert!(lines(&view, &card, 80)
            .iter()
            .any(|line| line.starts_with("B:")));
    }
}
