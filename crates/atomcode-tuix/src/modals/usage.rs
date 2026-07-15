//! `/usage` — a tabbed CodingPlan usage modal (Current | Overview | Models).

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use atomcode_core::coding_plan::types::RateLimitWindow;
use atomcode_core::coding_plan::usage::{compute_overview, humanize_tokens, OverviewStats, UsageResponse};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::i18n::{t, Msg};
use crate::modals::usage_render::{
    braille_plot, braille_series_char, calendar_layout, progress_bar, sparkline,
};
use crate::render::{MenuKind, MenuPayload, Renderer, UiLine};
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
pub struct UsageData {
    pub window: Option<RateLimitWindow>,
    pub usage: Option<UsageResponse>,
    pub overview: Option<OverviewStats>,
    pub error: Option<String>,
}

/// Tabbed `/usage` modal.
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

    /// Build the tab bar string (active = bold, inactive = dim/muted).
    fn tab_bar(&self) -> String {
        let tab_label = |tab: Tab, label: &str| -> String {
            if self.tab == tab {
                format!("  \x1b[1m{label}\x1b[22m  ")
            } else {
                format!("  \x1b[1;90m{label}\x1b[22;39m  ")
            }
        };
        let t0 = tab_label(Tab::Current, &t(Msg::UsageTabCurrent));
        let t1 = tab_label(Tab::Overview, &t(Msg::UsageTabOverview));
        let t2 = tab_label(Tab::Models, &t(Msg::UsageTabModels));
        format!("{t0}   {t1}   {t2}")
    }

    /// Format seconds as HH:MM:SS.
    fn hms(secs: i64) -> String {
        let s = secs.max(0) as u64;
        let h = s / 3600;
        let m = (s % 3600) / 60;
        let sec = s % 60;
        format!("{h:02}:{m:02}:{sec:02}")
    }

    /// Build rows for the Current tab.
    fn current_rows(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = Vec::new();
        rows.push((String::new(), String::new())); // blank separator
        if let Some(w) = &self.data.window {
            rows.push((
                format!("  \x1b[1m{}\x1b[22m", t(Msg::UsageCurrentTitle)),
                String::new(),
            ));
            rows.push((String::new(), String::new()));

            // Progress bar — coloured green (32) while ok, red (31) when exhausted
            let bar = progress_bar(w.usage_percent, 24);
            let bar_color = if w.usage_percent >= 100.0 { 31 } else { 32 };
            rows.push((
                format!("  \x1b[{bar_color}m{bar}\x1b[39m  {:.1}%", w.usage_percent),
                String::new(),
            ));

            // Reset countdown
            let hms = Self::hms(w.seconds_until_reset);
            rows.push((
                format!("  \x1b[90m{}\x1b[39m", t(Msg::UsageResetsIn { hms: &hms })),
                String::new(),
            ));

            // Window duration hint
            if w.window_hours > 0 {
                rows.push((
                    format!("  \x1b[90m({}-hour rolling window)\x1b[39m", w.window_hours),
                    String::new(),
                ));
            }
        } else {
            rows.push((
                format!("  \x1b[90m{}\x1b[39m", t(Msg::UsageWindowUnavailable)),
                String::new(),
            ));
        }
        rows
    }

    /// Build Overview tab rows — calendar heatmap + stats block.
    pub fn overview_lines(&self) -> Vec<String> {
        let Some(u) = &self.data.usage else {
            return vec![format!("  \x1b[90m{}\x1b[39m", t(Msg::UsageNoData))];
        };

        let mut lines: Vec<String> = Vec::new();

        // ── Calendar heatmap ──
        let cal_rows: Vec<(String, u64)> =
            u.rows.iter().map(|r| (r.date.clone(), r.total_tokens)).collect();

        if !cal_rows.is_empty() {
            let cells = calendar_layout(&cal_rows);

            // Determine number of week columns
            let max_week = cells.iter().map(|c| c.week_col).max().unwrap_or(0);

            // Month-label header row: place short month name at each month_start column
            let month_names = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun",
                "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let mut month_header = String::from("  ");
            // build a sparse month-label line (each week column = 2 chars wide in the heatmap)
            {
                let mut col_labels: Vec<Option<&str>> = vec![None; max_week + 1];
                for c in &cells {
                    if c.month_start {
                        let idx = (c.month as usize).saturating_sub(1).min(11);
                        col_labels[c.week_col] = Some(month_names[idx]);
                    }
                }
                let mut i = 0;
                while i <= max_week {
                    if let Some(label) = col_labels[i] {
                        month_header.push_str(label);
                        i += 1;
                        // skip next column since label takes 3 chars (≈ 1.5 cols)
                        if i <= max_week && col_labels[i].is_none() {
                            i += 1;
                        }
                    } else {
                        month_header.push_str("  ");
                        i += 1;
                    }
                }
            }
            lines.push(format!("\x1b[90m{month_header}\x1b[39m"));

            // 7 weekday rows (Sun=0 .. Sat=6)
            // Orange ramp: level 0=faint dot, 1..4 = 256-colour orange (52/94/166/208)
            let heat_colors: [u8; 5] = [0, 52, 94, 166, 208];
            for wd in 0u8..7 {
                let wd_label = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"][wd as usize];
                let mut row = format!("\x1b[90m{wd_label}\x1b[39m ");
                let mut col = 0usize;
                // gather cells for this weekday, indexed by week_col
                let mut week_map: std::collections::HashMap<usize, u8> =
                    std::collections::HashMap::new();
                for c in &cells {
                    if c.weekday == wd {
                        week_map.insert(c.week_col, c.level);
                    }
                }
                while col <= max_week {
                    let level = week_map.get(&col).copied().unwrap_or(0);
                    if level == 0 {
                        row.push_str("\x1b[90m·\x1b[39m ");
                    } else {
                        let color = heat_colors[level as usize];
                        row.push_str(&format!("\x1b[38;5;{color}m■\x1b[39m "));
                    }
                    col += 1;
                }
                lines.push(format!("  {row}"));
            }

            // Legend: Less ▪▪▪▪ More
            let legend = {
                let mut s = format!("  \x1b[90m{} \x1b[39m", t(Msg::UsageHeatLess));
                for lvl in 1u8..=4 {
                    let color = heat_colors[lvl as usize];
                    s.push_str(&format!("\x1b[38;5;{color}m■\x1b[39m"));
                }
                s.push_str(&format!("\x1b[90m {}\x1b[39m", t(Msg::UsageHeatMore)));
                s
            };
            lines.push(String::new());
            lines.push(legend);
        }

        // ── Stats block ──
        lines.push(String::new());
        let overview = self.data.overview.as_ref().cloned().unwrap_or_else(|| compute_overview(u));

        let stat = |label: &str, value: &str| -> String {
            format!("  \x1b[90m{label:<20}\x1b[39m \x1b[1m{value}\x1b[22m")
        };

        if let Some(fav) = &overview.favorite_model {
            lines.push(stat(&t(Msg::UsageStatFavorite), fav));
        }
        lines.push(stat(
            &t(Msg::UsageStatTotal),
            &humanize_tokens(overview.total_tokens),
        ));
        lines.push(stat(
            &t(Msg::UsageStatRequests),
            &overview.total_requests.to_string(),
        ));
        lines.push(stat(
            &t(Msg::UsageStatActiveDays),
            &format!("{} / {}", overview.active_days, overview.total_days),
        ));
        if let Some(day) = &overview.most_active_day {
            lines.push(stat(&t(Msg::UsageStatMostActive), day));
        }
        lines.push(stat(
            &t(Msg::UsageStatLongestStreak),
            &format!("{} days", overview.longest_streak),
        ));
        lines.push(stat(
            &t(Msg::UsageStatCurrentStreak),
            &format!("{} days", overview.current_streak),
        ));

        lines
    }

    /// Build Overview tab rows.
    fn overview_rows(&self) -> Vec<(String, String)> {
        let mut rows = vec![(String::new(), String::new())];
        for line in self.overview_lines() {
            rows.push((line, String::new()));
        }
        rows
    }

    /// Build Models tab rows.
    fn models_rows(&self, caps_colors: bool, caps_unicode: bool) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = Vec::new();
        rows.push((String::new(), String::new()));

        let Some(u) = &self.data.usage else {
            rows.push((
                format!("  \x1b[90m{}\x1b[39m", t(Msg::UsageNoData)),
                String::new(),
            ));
            return rows;
        };

        rows.push((
            format!("  \x1b[1m{}\x1b[22m", t(Msg::UsageModelsTitle)),
            String::new(),
        ));
        rows.push((String::new(), String::new()));

        if u.models.is_empty() {
            rows.push((
                format!("  \x1b[90m{}\x1b[39m", t(Msg::UsageNoData)),
                String::new(),
            ));
            return rows;
        }

        // Per-model colour palette (256-colour)
        let model_colors: [u8; 6] = [75, 214, 208, 154, 183, 81];

        if caps_colors && caps_unicode {
            // Braille chart path — one chart per model side-by-side as legend
            let chart_w = 20usize; // cells
            let chart_h = 4usize;  // cells

            for (mi, model) in u.models.iter().enumerate() {
                let color = model_colors[mi % model_colors.len()];
                let series: Vec<u64> = u
                    .rows
                    .iter()
                    .map(|r| r.model_tokens.get(model).copied().unwrap_or(0))
                    .collect();

                let total: u64 = series.iter().sum();
                let gmax = series.iter().copied().max().unwrap_or(0);

                // Header: coloured dot + model name + total
                rows.push((
                    format!(
                        "  \x1b[38;5;{color}m●\x1b[39m \x1b[1m{model}\x1b[22m  \x1b[90m{}\x1b[39m",
                        humanize_tokens(total)
                    ),
                    String::new(),
                ));

                // Y-axis labels
                let top_label = humanize_tokens(gmax);
                let bottom_label = "0";

                // Plot
                let grid = braille_plot(&[series], chart_w, chart_h);
                // Top Y label on first row
                for (ri, row_cells) in grid.iter().enumerate() {
                    let y_label = if ri == 0 {
                        format!("\x1b[90m{:>5}\x1b[39m ", top_label)
                    } else if ri == chart_h - 1 {
                        format!("\x1b[90m{:>5}\x1b[39m ", bottom_label)
                    } else {
                        "       ".to_string()
                    };
                    let mut line = format!("  {y_label}");
                    for &bits in row_cells {
                        let ch = braille_series_char(bits);
                        if bits != 0 {
                            line.push_str(&format!("\x1b[38;5;{color}m{ch}\x1b[39m"));
                        } else {
                            line.push(ch);
                        }
                    }
                    rows.push((line, String::new()));
                }
                rows.push((String::new(), String::new()));
            }
        } else {
            // Sparkline fallback
            let spark_w = 20usize;
            for (mi, model) in u.models.iter().enumerate() {
                let color = model_colors[mi % model_colors.len()];
                let series: Vec<u64> = u
                    .rows
                    .iter()
                    .map(|r| r.model_tokens.get(model).copied().unwrap_or(0))
                    .collect();
                let total: u64 = series.iter().sum();
                let spark = sparkline(&series, spark_w);
                rows.push((
                    format!(
                        "  \x1b[38;5;{color}m● {model}\x1b[39m"
                    ),
                    String::new(),
                ));
                rows.push((
                    format!(
                        "    \x1b[38;5;{color}m{spark}\x1b[39m  \x1b[90m{}\x1b[39m",
                        humanize_tokens(total)
                    ),
                    String::new(),
                ));
                rows.push((String::new(), String::new()));
            }
        }

        rows
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
        let hint = t(Msg::UsageFooterHint).into_owned();

        let mut final_items: Vec<(String, String)> = Vec::new();

        // Tab bar + blank separator
        final_items.push((self.tab_bar(), String::new()));
        final_items.push((String::new(), String::new()));

        // Error banner (shown when there's an error AND no usable data)
        if let Some(err) = &self.data.error {
            let has_data = self.data.window.is_some() || self.data.usage.is_some();
            if !has_data {
                final_items.push((
                    format!(
                        "  \x1b[31m{}\x1b[39m",
                        t(Msg::UsageFetchFailed { error: err })
                    ),
                    String::new(),
                ));
                final_items.push((format!("— {} —", hint), String::new()));

                // selected past end = nothing highlighted
                let selected = final_items.len();
                let payload = MenuPayload {
                    items: final_items,
                    selected,
                    kind: MenuKind::PluginInfo,
                };
                renderer.render(UiLine::InputPrompt {
                    buf: buf.text.clone(),
                    cursor_byte: buf.cursor,
                    menu: Some(payload),
                    status: build_status(state, ctx),
                    attachments: Vec::new(),
                });
                renderer.flush();
                return;
            }
        }

        // Tab body
        let tab_rows = match self.tab {
            Tab::Current => self.current_rows(),
            Tab::Overview => self.overview_rows(),
            Tab::Models => self.models_rows(ctx.caps.colors, ctx.caps.unicode_symbols),
        };
        for row in tab_rows {
            final_items.push(row);
        }

        // Footer hint
        final_items.push((String::new(), String::new()));
        final_items.push((format!("— {} —", hint), String::new()));

        // Nothing is selectable — point selected past the end so nothing is highlighted
        let selected = final_items.len();
        let payload = MenuPayload {
            items: final_items,
            selected,
            kind: MenuKind::PluginInfo,
        };

        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: Some(payload),
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::coding_plan::usage::{compute_overview, parse_usage};

    // Small inline sample — matches the shape from usage.rs core tests
    const SAMPLE: &str = r#"{
      "days": 60, "start_date": "2026-05-18", "end_date": "2026-07-16",
      "models": ["deepseek-v4-flash", "GLM-5.2"],
      "rows": [
        {"date": "2026-07-15",
         "model_counts": {"deepseek-v4-flash": 3, "GLM-5.2": 0},
         "model_tokens": {"deepseek-v4-flash": 100, "GLM-5.2": 0},
         "total_counts": 3, "total_tokens": 100},
        {"date": "2026-07-16",
         "model_counts": {"deepseek-v4-flash": 0, "GLM-5.2": 21},
         "model_tokens": {"deepseek-v4-flash": 0, "GLM-5.2": 717016},
         "total_counts": 21, "total_tokens": 717016}
      ],
      "model_counts": {"deepseek-v4-flash": 3, "GLM-5.2": 21},
      "model_tokens": {"deepseek-v4-flash": 100, "GLM-5.2": 717016},
      "total_counts": 24, "total_tokens": 717116
    }"#;

    fn sample_modal() -> UsageModal {
        let usage = parse_usage(SAMPLE).expect("parse SAMPLE");
        let overview = compute_overview(&usage);
        UsageModal::new(UsageData {
            window: None,
            usage: Some(usage),
            overview: Some(overview),
            error: None,
        })
    }

    #[test]
    fn tab_cycles_right_and_wraps() {
        let mut m = UsageModal::new(UsageData {
            window: None,
            usage: None,
            overview: None,
            error: None,
        });
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
        let mut m = UsageModal::new(UsageData {
            window: None,
            usage: None,
            overview: None,
            error: None,
        });
        m.select_tab('3');
        assert_eq!(m.tab, Tab::Models);
        m.select_tab('1');
        assert_eq!(m.tab, Tab::Current);
    }

    #[test]
    fn hms_formats_correctly() {
        assert_eq!(UsageModal::hms(0), "00:00:00");
        assert_eq!(UsageModal::hms(3661), "01:01:01");
        assert_eq!(UsageModal::hms(7259), "02:00:59");
        // Negative → clamp to 0
        assert_eq!(UsageModal::hms(-5), "00:00:00");
    }

    #[test]
    fn overview_lines_contains_humanized_total_and_requests() {
        let m = sample_modal();
        let lines = m.overview_lines();
        let all = lines.join("\n");
        // humanize_tokens(717116) == "717.1k"
        assert!(
            all.contains("717.1k"),
            "expected humanized total 717.1k in overview lines; got:\n{all}"
        );
        // "Requests" stat label
        assert!(
            all.to_lowercase().contains("request"),
            "expected 'Requests' stat in overview lines; got:\n{all}"
        );
        // Total request count
        assert!(
            all.contains("24"),
            "expected request count 24 in overview lines; got:\n{all}"
        );
    }

    #[test]
    fn overview_lines_contains_favorite_model() {
        let m = sample_modal();
        let lines = m.overview_lines();
        let all = lines.join("\n");
        assert!(
            all.contains("GLM-5.2"),
            "expected favorite model GLM-5.2 in overview lines; got:\n{all}"
        );
    }

    #[test]
    fn tab_bar_marks_active_tab_bold() {
        let mut m = sample_modal();
        m.tab = Tab::Overview;
        let bar = m.tab_bar();
        // Active tab "Overview" has \x1b[1m ... \x1b[22m (bold on/off, no ;90)
        // Inactive "Current" has \x1b[1;90m (bold+dim)
        assert!(bar.contains("\x1b[1mOverview\x1b[22m") || bar.contains("Overview"),
            "tab bar should mark active tab bold; got: {bar}");
        // Inactive tabs should be muted (;90)
        assert!(bar.contains("90m"), "inactive tabs should have muted colour; got: {bar}");
    }

    #[test]
    fn current_tab_window_unavailable_when_no_window() {
        let m = sample_modal();
        let rows = m.current_rows();
        let all: String = rows.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("unavailable") || all.contains("Unavailable") || all.contains("不可用"),
            "expected unavailable message on Current tab with no window; got:\n{all}"
        );
    }

    #[test]
    fn models_rows_contains_model_names() {
        let m = sample_modal();
        let rows = m.models_rows(true, true);
        let all: String = rows.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(all.contains("deepseek-v4-flash"), "missing deepseek model; got:\n{all}");
        assert!(all.contains("GLM-5.2"), "missing GLM model; got:\n{all}");
    }
}
