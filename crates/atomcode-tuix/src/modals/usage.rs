//! `/usage` — a tabbed CodingPlan usage modal (Current | Overview | Models).

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use atomcode_codingplan::types::{PlanInfo, RateLimitWindow};
use atomcode_codingplan::usage::{compute_overview, humanize_tokens, OverviewStats, UsageResponse};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::i18n::{t, Msg};
use crate::modals::usage_render::{
    braille_line_plot, braille_series_char, calendar_layout, progress_bar, sparkline,
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
    pub plan: Option<PlanInfo>,
    pub usage: Option<UsageResponse>,
    pub overview: Option<OverviewStats>,
    pub error: Option<String>,
}

/// Tabbed `/usage` modal.
pub struct UsageModal {
    pub(crate) data: UsageData,
    pub(crate) tab: Tab,
    /// Transient "copied" notice — set by Ctrl+S, shown in the footer until next redraw.
    pub(crate) copy_notice: Option<String>,
}

impl UsageModal {
    pub fn new(data: UsageData) -> Self {
        Self {
            data,
            tab: Tab::Current,
            copy_notice: None,
        }
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

    /// Map a tab-navigation key to a tab switch. The single authority for the
    /// nav key set, shared by the interactive modal ([`handle_key`]) and the
    /// streaming footer report so the two surfaces can never drift. Returns
    /// `true` when the key was a nav key (and thus consumed).
    ///
    /// [`handle_key`]: Modal::handle_key
    pub(crate) fn handle_tab_nav(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Tab | KeyCode::Right => self.next_tab(),
            KeyCode::BackTab | KeyCode::Left => self.prev_tab(),
            KeyCode::Char(c @ '1'..='3') => self.select_tab(c),
            _ => return false,
        }
        true
    }

    /// Build the tab bar string (active = bold, inactive = dim/muted).
    fn tab_bar(&self) -> String {
        // Palette-independent active/inactive contrast — see modals::tab_chip
        // (fixed 256-colours, correct on Solarized Dark and every theme).
        let chip = |tab: Tab, label: &str| crate::modals::tab_chip(label, self.tab == tab);
        let t0 = chip(Tab::Current, &t(Msg::UsageTabCurrent));
        let t1 = chip(Tab::Overview, &t(Msg::UsageTabOverview));
        let t2 = chip(Tab::Models, &t(Msg::UsageTabModels));
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
        let m = muted_open();
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
                format!("  {m}{}\x1b[39m", t(Msg::UsageResetsIn { hms: &hms })),
                String::new(),
            ));

            // Window duration hint
            if w.window_hours > 0 {
                rows.push((
                    format!(
                        "  {m}({})\x1b[39m",
                        t(Msg::UsageWindowHours {
                            hours: w.window_hours
                        })
                    ),
                    String::new(),
                ));
            }
        } else {
            rows.push((
                format!("  {m}{}\x1b[39m", t(Msg::UsageWindowUnavailable)),
                String::new(),
            ));
        }

        // ── Plan section (CodingPlan entitlement) ──
        if let Some(plan) = &self.data.plan {
            rows.push((String::new(), String::new()));

            let status_label = if plan.status == 1 {
                format!("\x1b[32m{}\x1b[39m", t(Msg::UsagePlanActive))
            } else {
                format!("\x1b[31m{}\x1b[39m", t(Msg::UsagePlanExpired))
            };
            rows.push((
                format!("  \x1b[1m{}\x1b[22m · {status_label}", plan.plan_name),
                String::new(),
            ));

            if !plan.claimed_at.is_empty() || !plan.expires_at.is_empty() {
                rows.push((
                    format!(
                        "  {m}{}\x1b[39m",
                        t(Msg::UsagePlanClaimedExpires {
                            claimed: &plan.claimed_at,
                            expires: &plan.expires_at,
                        })
                    ),
                    String::new(),
                ));
            }

            if plan.total_days > 0 {
                rows.push((
                    format!(
                        "  {m}{}\x1b[39m",
                        t(Msg::UsagePlanRemaining {
                            remaining: plan.remaining_days,
                            total: plan.total_days,
                        })
                    ),
                    String::new(),
                ));
                // Day-progress bar: elapsed = total - remaining
                let elapsed_pct = ((plan.total_days - plan.remaining_days) as f64
                    / plan.total_days as f64
                    * 100.0)
                    .clamp(0.0, 100.0);
                let bar = progress_bar(elapsed_pct, 24);
                let bar_color = if plan.remaining_days <= 0 { 31 } else { 33 };
                rows.push((
                    format!("  \x1b[{bar_color}m{bar}\x1b[39m  {:.1}%", elapsed_pct),
                    String::new(),
                ));
            }
        }

        rows
    }

    /// Theme-aware, colour-preserving snapshot of the tab bar + the ACTIVE tab.
    ///
    /// Streaming `/usage` can't install the interactive modal (live token
    /// redraws own the footer), so tab switching mid-stream re-renders this
    /// snapshot into the footer instead. It reuses the modal's exact rows so
    /// headings, progress bars, plan status, heatmap, and palette match. Unlike
    /// [`active_tab_text`] (clipboard, ANSI-stripped) it keeps colour so the
    /// footer's bars/heatmap render, and it follows `self.tab` across all three
    /// tabs. `models_rows` needs the terminal caps, hence the parameters.
    ///
    /// [`active_tab_text`]: Self::active_tab_text
    pub(crate) fn active_snapshot_text(&self, caps_colors: bool, caps_unicode: bool) -> String {
        let mut lines = vec![self.tab_bar(), String::new()];
        lines.extend(
            self.active_tab_lines(caps_colors, caps_unicode)
                .into_iter()
                .skip_while(String::is_empty),
        );
        lines.join("\n")
    }

    /// The active tab's body lines with colour intact — the single tab-body
    /// dispatch shared by the colour-keeping [`active_snapshot_text`] and the
    /// ANSI-stripped [`active_tab_text`].
    ///
    /// [`active_snapshot_text`]: Self::active_snapshot_text
    /// [`active_tab_text`]: Self::active_tab_text
    fn active_tab_lines(&self, caps_colors: bool, caps_unicode: bool) -> Vec<String> {
        match self.tab {
            Tab::Current => self.current_rows().into_iter().map(|(l, _)| l).collect(),
            Tab::Overview => self.overview_lines(),
            Tab::Models => self
                .models_rows(caps_colors, caps_unicode)
                .into_iter()
                .map(|(l, _)| l)
                .collect(),
        }
    }

    /// Build Overview tab rows — calendar heatmap + stats block.
    pub fn overview_lines(&self) -> Vec<String> {
        let m = muted_open();
        let Some(u) = &self.data.usage else {
            return vec![format!("  {m}{}\x1b[39m", t(Msg::UsageNoData))];
        };

        let mut lines: Vec<String> = Vec::new();

        // ── Calendar heatmap ──
        let cal_rows: Vec<(String, u64)> = u
            .rows
            .iter()
            .map(|r| (r.date.clone(), r.total_tokens))
            .collect();

        if !cal_rows.is_empty() {
            let cells = calendar_layout(&cal_rows);

            // Determine number of week columns
            let max_week = cells.iter().map(|c| c.week_col).max().unwrap_or(0);

            // Month-label header: a 3-letter month name at the first column of
            // each month present in the data, aligned above that month's cells
            // (indented past the weekday-label column). A label is skipped if it
            // can't fit before the next month, so a sliver month at the edge
            // doesn't show a cramped/clipped name.
            let month_names = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            const CELL_W: usize = 4; // display width per week-column cell (bigger grid)
            const WD_LABEL_W: usize = 4; // "Sun " weekday-label column width
            let header_len = (max_week + 1) * CELL_W;
            let mut header_buf: Vec<char> = vec![' '; header_len];
            let mut month_starts: Vec<(usize, &str)> = cells
                .iter()
                .filter(|c| c.month_start)
                .map(|c| {
                    let idx = (c.month as usize).saturating_sub(1).min(11);
                    (c.week_col, month_names[idx])
                })
                .collect();
            month_starts.sort_by_key(|&(col, _)| col);
            for (i, &(col, label)) in month_starts.iter().enumerate() {
                let start = col * CELL_W;
                // End = next month's column (minus a 1-col gap), or the buffer end.
                let end = if i + 1 < month_starts.len() {
                    (month_starts[i + 1].0 * CELL_W).saturating_sub(1)
                } else {
                    header_len
                };
                // Skip the label if its full name can't fit — avoids a truncated
                // month name at a narrow edge column.
                if end.saturating_sub(start) < label.chars().count() {
                    continue;
                }
                for (j, ch) in label.chars().enumerate() {
                    if start + j < header_buf.len() {
                        header_buf[start + j] = ch;
                    }
                }
            }
            let month_header_str: String = header_buf.into_iter().collect();
            lines.push(format!(
                "{m}  {:pad$}{}\x1b[39m",
                "",
                month_header_str.trim_end(),
                pad = WD_LABEL_W
            ));

            // Claude-Code-style calendar. Index 0 = a day in-range with zero
            // activity (a neutral dark square so the grid stays solid — NOT a
            // dot, which floated as a "hole" between active days); indexes 1..5 =
            // a dark→bright coral ramp ending at the #d97757 brand coral. Only
            // days OUTSIDE the window (leading/trailing padding) render blank.
            // Cells are CELL_W wide so the 60-day grid reads at a comfortable size,
            // and adjacent cells connect.
            // 256-colour (AnsiValue), NOT truecolor: tmux and many terminals
            // drop `38;2;r;g;b`, leaving the block at the default fg (a flat
            // grey). A dark→bright coral/salmon ramp from the xterm-256 cube
            // renders correctly everywhere. Index 0 = in-range-but-zero (neutral
            // dark square so the grid stays solid); 1..5 = least→most.
            // Index 0 = a near-white "empty" square (in-range but zero activity)
            // — reads as blank and, against the light-pink ramp, is far less
            // jarring than a dark hole. Indexes 1..5 = the activity ramp, LIGHT
            // → DEEP RED so more activity reads as a richer/darker red (à la
            // GitHub's "more = darker"). The whole 0..5 sequence darkens
            // monotonically: empty (lightest) → most (deep red, darkest).
            const HEAT_RAMP: [u8; 6] = [
                231, // 0 — no activity (white #ffffff)
                217, // 1 — least (#ffafaf pale pink)
                210, // 2 — (#ff8787 salmon pink)
                174, // 3 — (#d78787 dusty rose)
                131, // 4 — (#af5f5f muted brick)
                88,  // 5 — most (#870000 deep red)
            ];
            let full_block: String = "█".repeat(CELL_W);
            let blank_cell: String = " ".repeat(CELL_W);
            // Index level by (weekday, week_col) in one pass rather than rebuilding
            // a map for each of the 7 weekday rows.
            let mut grid: std::collections::HashMap<(u8, usize), u8> =
                std::collections::HashMap::with_capacity(cells.len());
            for c in &cells {
                grid.insert((c.weekday, c.week_col), c.level);
            }
            let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            for wd in 0u8..7 {
                let mut row = format!("{m}{}\x1b[39m ", weekdays[wd as usize]);
                for col in 0..=max_week {
                    match grid.get(&(wd, col)).copied() {
                        // Outside the data window (padding) → blank, clean edges.
                        None => row.push_str(&blank_cell),
                        // In-range day → square (level 0 neutral … 5 coral); no
                        // trailing gap so adjacent cells connect into a solid grid.
                        Some(level) => {
                            let c = HEAT_RAMP[(level as usize).min(5)];
                            row.push_str(&format!("\x1b[38;5;{c}m{full_block}\x1b[39m"));
                        }
                    }
                }
                lines.push(format!("  {row}"));
            }

            // Legend: Less ██████ More (coral activity swatches, skip the neutral 0)
            let legend = {
                let mut s = format!("  {m}{} \x1b[39m", t(Msg::UsageHeatLess));
                for &c in &HEAT_RAMP[1..] {
                    s.push_str(&format!("\x1b[38;5;{c}m██\x1b[39m"));
                }
                s.push_str(&format!("{m} {}\x1b[39m", t(Msg::UsageHeatMore)));
                s
            };
            lines.push(String::new());
            lines.push(legend);
        }

        // ── Stats block ──
        lines.push(String::new());
        let overview = self
            .data
            .overview
            .as_ref()
            .cloned()
            .unwrap_or_else(|| compute_overview(u));

        // Collect the label/value pairs, then align the value column by DISPLAY
        // width. The old `{label:<20}` padded by CHAR COUNT, but CJK labels have
        // different char-count-vs-cell-width ratios (`请求次数` = 4 chars / 8 cells
        // vs `总 Token 数` = 9 chars / 11 cells), so every value started at a
        // different terminal column — the misalignment reported on `/usage`.
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(fav) = &overview.favorite_model {
            pairs.push((t(Msg::UsageStatFavorite).into_owned(), fav.clone()));
        }
        pairs.push((
            t(Msg::UsageStatTotal).into_owned(),
            humanize_tokens(overview.total_tokens),
        ));
        pairs.push((
            t(Msg::UsageStatRequests).into_owned(),
            overview.total_requests.to_string(),
        ));
        pairs.push((
            t(Msg::UsageStatActiveDays).into_owned(),
            format!("{} / {}", overview.active_days, overview.total_days),
        ));
        if let Some(day) = &overview.most_active_day {
            pairs.push((t(Msg::UsageStatMostActive).into_owned(), day.clone()));
        }
        pairs.push((
            t(Msg::UsageStatLongestStreak).into_owned(),
            format!("{} days", overview.longest_streak),
        ));
        pairs.push((
            t(Msg::UsageStatCurrentStreak).into_owned(),
            format!("{} days", overview.current_streak),
        ));
        lines.extend(align_stat_lines(&pairs, m));

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
        let m = muted_open();
        let mut rows: Vec<(String, String)> = Vec::new();
        rows.push((String::new(), String::new()));

        let Some(u) = &self.data.usage else {
            rows.push((
                format!("  {m}{}\x1b[39m", t(Msg::UsageNoData)),
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
                format!("  {m}{}\x1b[39m", t(Msg::UsageNoData)),
                String::new(),
            ));
            return rows;
        }

        // Per-model colour palette (256-colour)
        let model_colors: [u8; 6] = [75, 214, 208, 154, 183, 81];

        // Gather per-model series + aggregate stats (sorted tokens desc for breakdown).
        // Fall back to summing row-level model_tokens when the top-level map is absent/empty
        // (mirrors how compute_overview falls back).
        let model_series_tokens: std::collections::HashMap<&String, u64> = u
            .models
            .iter()
            .map(|model| {
                let row_sum: u64 = u
                    .rows
                    .iter()
                    .map(|r| r.model_tokens.get(model).copied().unwrap_or(0))
                    .sum();
                (model, row_sum)
            })
            .collect();
        let total_tokens_all: u64 = if u.model_tokens.values().sum::<u64>() > 0 {
            u.model_tokens.values().sum()
        } else {
            model_series_tokens.values().sum()
        };
        let mut model_stats: Vec<(&String, Vec<u64>, u64, u64)> = u
            .models
            .iter()
            .map(|model| {
                let series: Vec<u64> = u
                    .rows
                    .iter()
                    .map(|r| r.model_tokens.get(model).copied().unwrap_or(0))
                    .collect();
                // Prefer top-level map; fall back to row sum when map is empty
                let total_tok: u64 = if u.model_tokens.is_empty() {
                    model_series_tokens.get(model).copied().unwrap_or(0)
                } else {
                    u.model_tokens.get(model).copied().unwrap_or(0)
                };
                let total_req: u64 = u.model_counts.get(model).copied().unwrap_or(0) as u64;
                (model, series, total_tok, total_req)
            })
            .collect();
        model_stats.sort_by(|a, b| b.2.cmp(&a.2)); // sort by tokens desc

        if caps_colors && caps_unicode {
            // ── Unified braille line chart ──
            let chart_w = 52usize; // cells (wider for clearer display)
            let chart_h = 6usize; // cells

            // Global max across all series
            let global_max: u64 = model_stats
                .iter()
                .flat_map(|(_, s, _, _)| s.iter().copied())
                .max()
                .unwrap_or(0);

            // Per-model single-series line grids (for colour attribution).
            // braille_line_plot uses its own internal max, so we need a consistent Y-scale:
            // inject a sentinel max at the end and clear the dummy rightmost column.
            let model_grids: Vec<Vec<Vec<u8>>> = model_stats
                .iter()
                .map(|(_, series, _, _)| {
                    let mut scaled = series.clone();
                    if global_max > 0 && scaled.iter().copied().max().unwrap_or(0) < global_max {
                        scaled.push(global_max);
                        let mut g = braille_line_plot(&[scaled], chart_w, chart_h);
                        // Clear the rightmost column introduced by the dummy point
                        for row in &mut g {
                            if let Some(last) = row.last_mut() {
                                *last = 0;
                            }
                        }
                        g
                    } else {
                        braille_line_plot(&[series.clone()], chart_w, chart_h)
                    }
                })
                .collect();

            // Chart title
            rows.push((format!("  \x1b[1mTokens per Day\x1b[22m"), String::new()));

            // Render rows: merge grids, colour by first model with a dot
            for ri in 0..chart_h {
                // Y-axis gridline labels. Bottom row = 0 baseline; every other row
                // gets its interpolated value (max at the top down to ~0). All arms
                // are 8 visible columns wide (7-wide value + space) so the braille
                // plot never jitters between labelled and blank rows.
                let y_label = if ri == chart_h - 1 {
                    format!("{m}{:>7}\x1b[39m ", "0")
                } else if ri % 2 == 0 {
                    let frac = (chart_h - 1 - ri) as f64 / (chart_h - 1) as f64;
                    let val = (global_max as f64 * frac).round() as u64;
                    format!("{m}{:>7}\x1b[39m ", humanize_tokens(val))
                } else {
                    "        ".to_string()
                };
                let mut line = format!("  {y_label}");
                for ci in 0..chart_w {
                    // Find first model (highest-tokens = first) that has a dot here
                    let mut found_color: Option<u8> = None;
                    let mut merged_bits = 0u8;
                    for (mi, grid) in model_grids.iter().enumerate() {
                        let bits = grid[ri][ci];
                        merged_bits |= bits;
                        if bits != 0 && found_color.is_none() {
                            found_color = Some(model_colors[mi % model_colors.len()]);
                        }
                    }
                    let ch = braille_series_char(merged_bits);
                    if let Some(color) = found_color {
                        line.push_str(&format!("\x1b[38;5;{color}m{ch}\x1b[39m"));
                    } else {
                        line.push(ch);
                    }
                }
                rows.push((line, String::new()));
            }

            // X-axis date labels: up to 5 evenly-spaced ticks placed into a fixed
            // char buffer (like the month header) so they align under the chart and
            // never overlap. Endpoints show the full YYYY-MM-DD (year context);
            // inner ticks show MM-DD to fit the width.
            if !u.rows.is_empty() {
                let n = u.rows.len();
                let indent = "        "; // matches y-label width (8 chars)
                let x_label_line = if n == 1 {
                    format!("  {indent}{m}{}\x1b[39m", u.rows[0].date.as_str())
                } else {
                    let ticks = n.min(5).max(2);
                    let mut buf: Vec<char> = vec![' '; chart_w];
                    let mut used_end = 0usize; // rightmost filled col + gap, prevents overlap
                    for k in 0..ticks {
                        let idx = k * (n - 1) / (ticks - 1);
                        let date = u.rows[idx].date.as_str();
                        let is_endpoint = k == 0 || k == ticks - 1;
                        let label: String = if is_endpoint {
                            date.to_string()
                        } else {
                            date.get(5..).unwrap_or(date).to_string() // MM-DD
                        };
                        let len = label.chars().count();
                        // Endpoints anchor to the axis edges and always render in
                        // full (year context matters most); inner ticks centre on
                        // their column and shove right of the previous label if
                        // they'd collide.
                        let start = if k == 0 {
                            0
                        } else if k == ticks - 1 {
                            chart_w.saturating_sub(len)
                        } else {
                            let center = idx * chart_w.saturating_sub(1) / (n - 1);
                            center
                                .saturating_sub(len / 2)
                                .min(chart_w.saturating_sub(len))
                                .max(used_end)
                        };
                        for (j, ch) in label.chars().enumerate() {
                            if start + j < chart_w {
                                buf[start + j] = ch;
                            }
                        }
                        used_end = (start + len + 1).min(chart_w);
                    }
                    let label_str: String = buf.into_iter().collect();
                    format!("  {indent}{m}{}\x1b[39m", label_str.trim_end())
                };
                rows.push((x_label_line, String::new()));
            }

            rows.push((String::new(), String::new()));

            // Per-model table — the coloured ● also serves as the chart legend.
            // Columns: ● Model | Tokens | Requests | Share (aligned).
            rows.push((
                format!(
                    "  {m}  {:<26}{:>10}{:>9}{:>7}\x1b[39m",
                    "Model", "Tokens", "Requests", "Share"
                ),
                String::new(),
            ));
            for (mi, (model, _, tok, req)) in model_stats.iter().enumerate() {
                let color = model_colors[mi % model_colors.len()];
                let pct = if total_tokens_all > 0 {
                    (*tok as f64 / total_tokens_all as f64 * 100.0).round() as u64
                } else {
                    0
                };
                let share = format!("{pct}%");
                rows.push((
                    format!(
                        "  \x1b[38;5;{color}m●\x1b[39m {model:<26}{m}{:>10}{:>9}{:>7}\x1b[39m",
                        humanize_tokens(*tok),
                        req,
                        share
                    ),
                    String::new(),
                ));
            }
        } else {
            // Sparkline fallback — per-model coloured sparkline + breakdown
            let spark_w = 30usize;
            for (mi, (model, series, tok, req)) in model_stats.iter().enumerate() {
                let color = model_colors[mi % model_colors.len()];
                let spark = sparkline(series, spark_w);
                let pct = if total_tokens_all > 0 {
                    (*tok as f64 / total_tokens_all as f64 * 100.0).round() as u64
                } else {
                    0
                };
                rows.push((
                    format!("  \x1b[38;5;{color}m● {model}\x1b[39m"),
                    String::new(),
                ));
                rows.push((
                    format!("    \x1b[38;5;{color}m{spark}\x1b[39m"),
                    String::new(),
                ));
                rows.push((
                    format!(
                        "    {m}{pct}%  ·  {req} reqs  ·  {}\x1b[39m",
                        humanize_tokens(*tok)
                    ),
                    String::new(),
                ));
                rows.push((String::new(), String::new()));
            }
        }

        rows
    }

    /// Strip ANSI SGR escape sequences from a string for plain-text clipboard copy.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // consume until a letter (the SGR terminator)
                for nc in chars.by_ref() {
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Build a plain-text snapshot of the active tab suitable for clipboard copy.
    fn active_tab_text(&self, caps_colors: bool, caps_unicode: bool) -> String {
        self.active_tab_lines(caps_colors, caps_unicode)
            .iter()
            .map(|l| Self::strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n")
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
        // Ctrl+S — copy active tab as plain text to clipboard
        if code == KeyCode::Char('s') && mods.contains(KeyModifiers::CONTROL) {
            let text = self.active_tab_text(ctx.caps.colors, ctx.caps.unicode_symbols);
            crate::event_loop::commands::copy_text_to_clipboard_osc52(&text);
            self.copy_notice = Some(t(Msg::UsageCopied).into_owned());
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }
        if let KeyCode::Esc | KeyCode::Char('q') = code {
            return Ok(ModalAction::Close);
        }
        // Tab / ←→ / 1-3 switch tabs; other keys are no-ops here.
        self.handle_tab_nav(code);
        let _ = mods;
        // Clear any copy notice on any other keypress
        self.copy_notice = None;
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let hint = if let Some(notice) = &self.copy_notice {
            format!("✓ {notice}")
        } else {
            t(Msg::UsageFooterHint).into_owned()
        };

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

        // Footer hint (or copy confirmation)
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

/// SGR opener for muted / label text that stays visible on both palettes.
///
/// Dark themes use SGR 37 (light gray): SGR 90 (bright-black) is a palette
/// colour many dark terminal themes (e.g. iTerm2) map to ≈ the background, so
/// `\x1b[90m` labels rendered invisible. Light themes keep SGR 90 (dark gray on
/// white). Close with `\x1b[39m` as before. Mirrors `theme::muted_for_current_theme`.
fn muted_open() -> &'static str {
    if crate::highlight::theme::is_light_for_render() {
        "\x1b[90m"
    } else {
        "\x1b[37m"
    }
}

/// Render a `label → value` stats block with the value column aligned by DISPLAY
/// width. Padding is computed from `crate::width::display_width` (CJK-aware), NOT
/// char count, so mixed-width labels (`请求次数` vs `总 Token 数`) still put every
/// value at the same terminal column. `muted` is the label's SGR-open colour;
/// labels are muted, values bold, with a fixed gap between the widest label and
/// the value column.
fn align_stat_lines(pairs: &[(String, String)], muted: &str) -> Vec<String> {
    const GAP: usize = 3;
    let label_w = pairs
        .iter()
        .map(|(l, _)| crate::width::display_width(l))
        .max()
        .unwrap_or(0);
    pairs
        .iter()
        .map(|(label, value)| {
            let pad = label_w.saturating_sub(crate::width::display_width(label)) + GAP;
            let spaces = " ".repeat(pad);
            format!("  {muted}{label}\x1b[39m{spaces}\x1b[1m{value}\x1b[22m")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_codingplan::usage::{compute_overview, parse_usage};

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
            plan: None,
            usage: Some(usage),
            overview: Some(overview),
            error: None,
        })
    }

    fn empty_modal() -> UsageModal {
        UsageModal::new(UsageData {
            window: None,
            plan: None,
            usage: None,
            overview: None,
            error: None,
        })
    }

    #[test]
    fn tab_cycles_right_and_wraps() {
        let mut m = empty_modal();
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
        let mut m = empty_modal();
        m.select_tab('3');
        assert_eq!(m.tab, Tab::Models);
        m.select_tab('1');
        assert_eq!(m.tab, Tab::Current);
    }

    #[test]
    fn handle_tab_nav_switches_on_nav_keys_and_ignores_others() {
        let mut m = empty_modal();
        assert!(m.handle_tab_nav(KeyCode::Tab));
        assert_eq!(m.tab, Tab::Overview);
        assert!(m.handle_tab_nav(KeyCode::Right));
        assert_eq!(m.tab, Tab::Models);
        assert!(m.handle_tab_nav(KeyCode::Left));
        assert_eq!(m.tab, Tab::Overview);
        assert!(m.handle_tab_nav(KeyCode::Char('3')));
        assert_eq!(m.tab, Tab::Models);
        // Non-nav keys are not consumed — the caller keeps them for other uses.
        assert!(!m.handle_tab_nav(KeyCode::Char('x')));
        assert!(!m.handle_tab_nav(KeyCode::Esc));
        assert_eq!(m.tab, Tab::Models);
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
    fn align_stat_lines_aligns_value_column_by_display_width() {
        // CJK labels of different char-count-vs-cell-width ratios must still put
        // every value at the same terminal column (the /usage misalignment bug).
        let pairs = vec![
            ("最常用模型".to_string(), "deepseek-v4-flash".to_string()),
            ("总 Token 数".to_string(), "166.0m".to_string()),
            ("请求次数".to_string(), "4332".to_string()),
            ("最长连续天数".to_string(), "18 days".to_string()),
        ];
        let lines = align_stat_lines(&pairs, "\x1b[37m");

        // Strip SGR, measure the display column where each value (bold) starts.
        let strip = |s: &str| -> String {
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for c2 in chars.by_ref() {
                        if c2 == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        let cols: Vec<usize> = lines
            .iter()
            .map(|l| {
                let prefix = l.split("\x1b[1m").next().unwrap();
                crate::width::display_width(&strip(prefix))
            })
            .collect();
        assert_eq!(cols.len(), 4);
        assert!(
            cols.iter().all(|&c| c == cols[0]),
            "value column must align across CJK labels, got {cols:?}"
        );
        // Values survive intact.
        assert!(lines[0].contains("deepseek-v4-flash"));
        assert!(lines[3].contains("18 days"));
    }

    #[test]
    fn overview_lines_contains_humanized_total_and_requests() {
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
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
    fn overview_lines_are_terminal_compatible() {
        // Compatibility guards (default test theme = dark):
        //  1. Muted labels use SGR 37, NOT SGR 90 — SGR 90 (bright-black) is
        //     invisible on some dark themes (iTerm2). Regression fence for that.
        //  2. The heatmap uses 256-colour (38;5), NOT truecolor (38;2) — tmux
        //     drops truecolor, leaving the grid a flat grey.
        let m = sample_modal();
        let all = m.overview_lines().join("\n");
        assert!(
            all.contains("\x1b[37m"),
            "muted labels must route to SGR 37 on dark; got:\n{all}"
        );
        assert!(
            !all.contains("\x1b[90m"),
            "muted labels must NOT emit SGR 90 (invisible on dark iTerm2); got:\n{all}"
        );
        assert!(
            all.contains("\x1b[38;5;"),
            "heatmap must use 256-colour; got:\n{all}"
        );
        assert!(
            !all.contains("\x1b[38;2;"),
            "heatmap must NOT use truecolor (tmux drops it); got:\n{all}"
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
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut m = sample_modal();
        m.tab = Tab::Overview;
        let bar = m.tab_bar();
        // Active tab "Overview" on the default (dark) test theme: bold + fixed
        // near-white 256-colour (231), the brightest/most prominent.
        assert!(
            bar.contains("\x1b[1;38;5;231mOverview\x1b[22;39m"),
            "active tab should be bold + fixed near-white 231 on dark; got: {bar}"
        );
        // Inactive tabs: fixed mid-grey (245), dimmer than active and
        // palette-independent. Must NOT use SGR 90/37/39 — all broke on
        // Solarized Dark (90≈bg, 37 brighter than default, 39=grey default fg).
        assert!(
            bar.contains("\x1b[38;5;245m"),
            "inactive tabs should use fixed 256-colour grey 245; got: {bar}"
        );
        assert!(
            !bar.contains("\x1b[90m") && !bar.contains("\x1b[37m") && !bar.contains("\x1b[1;39m"),
            "tabs must not rely on palette-dependent SGR 90/37/39; got: {bar}"
        );
    }

    #[test]
    fn streaming_snapshot_keeps_all_three_tab_labels() {
        // Default tab is Current; sample_modal has no window → "unavailable".
        let text = sample_modal().active_snapshot_text(true, true);

        assert!(text.contains(t(Msg::UsageTabCurrent).as_ref()));
        assert!(text.contains(t(Msg::UsageTabOverview).as_ref()));
        assert!(text.contains(t(Msg::UsageTabModels).as_ref()));
        assert!(
            text.contains(t(Msg::UsageWindowUnavailable).as_ref()),
            "snapshot should still render the Current body"
        );
    }

    #[test]
    fn active_snapshot_text_always_keeps_all_three_tab_labels() {
        // The tab bar must render on every tab so the footer snapshot preserves
        // the modal's information hierarchy while streaming.
        for tab in [Tab::Current, Tab::Overview, Tab::Models] {
            let mut m = sample_modal();
            m.tab = tab;
            let text = m.active_snapshot_text(true, true);
            assert!(text.contains(t(Msg::UsageTabCurrent).as_ref()));
            assert!(text.contains(t(Msg::UsageTabOverview).as_ref()));
            assert!(text.contains(t(Msg::UsageTabModels).as_ref()));
        }
    }

    #[test]
    fn active_snapshot_text_renders_models_tab_body() {
        // Switching to the Models tab mid-stream must surface the models body
        // (model names), not the Current body.
        let mut m = sample_modal();
        m.tab = Tab::Models;
        let text = m.active_snapshot_text(true, true);
        assert!(
            text.contains("deepseek-v4-flash"),
            "Models tab snapshot must contain model names; got:\n{text}"
        );
    }

    #[test]
    fn active_snapshot_text_renders_overview_tab_body() {
        // Switching to the Overview tab mid-stream must surface the overview
        // body (favorite model), not the Current body.
        let mut m = sample_modal();
        m.tab = Tab::Overview;
        let text = m.active_snapshot_text(true, true);
        assert!(
            text.contains("GLM-5.2"),
            "Overview tab snapshot must contain overview stats; got:\n{text}"
        );
    }

    #[test]
    fn active_snapshot_text_keeps_ansi_color() {
        // Unlike the clipboard variant, the footer snapshot must keep ANSI so
        // the progress bars / heatmap render in colour below the input box.
        let mut m = sample_modal();
        m.tab = Tab::Overview;
        let text = m.active_snapshot_text(true, true);
        assert!(
            text.contains('\x1b'),
            "footer snapshot must retain ANSI colour codes; got:\n{text}"
        );
    }

    #[test]
    fn current_tab_window_unavailable_when_no_window() {
        let m = sample_modal();
        let rows = m.current_rows();
        let all: String = rows
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("unavailable") || all.contains("Unavailable") || all.contains("不可用"),
            "expected unavailable message on Current tab with no window; got:\n{all}"
        );
    }

    #[test]
    fn models_rows_contains_model_names() {
        let m = sample_modal();
        let rows = m.models_rows(true, true);
        let all: String = rows
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("deepseek-v4-flash"),
            "missing deepseek model; got:\n{all}"
        );
        assert!(all.contains("GLM-5.2"), "missing GLM model; got:\n{all}");
    }

    #[test]
    fn models_rows_unified_chart_contains_breakdown_percent() {
        let m = sample_modal();
        let rows = m.models_rows(true, true);
        let all: String = rows
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // GLM-5.2 has 717016/717116 ≈ 100% of tokens
        assert!(all.contains('%'), "expected percent breakdown; got:\n{all}");
        // The per-model TABLE has a "Requests" column header.
        assert!(
            all.contains("Requests"),
            "expected 'Requests' table column; got:\n{all}"
        );
        // Title should appear
        assert!(
            all.contains("Tokens per Day"),
            "expected chart title; got:\n{all}"
        );
    }

    #[test]
    fn models_rows_fallback_contains_breakdown() {
        let m = sample_modal();
        let rows = m.models_rows(false, false);
        let all: String = rows
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("GLM-5.2"),
            "missing GLM model in fallback; got:\n{all}"
        );
        assert!(
            all.contains('%'),
            "expected percent in fallback breakdown; got:\n{all}"
        );
        assert!(
            all.contains("reqs"),
            "expected 'reqs' in fallback breakdown; got:\n{all}"
        );
    }

    #[test]
    fn current_rows_shows_plan_info_when_present() {
        use atomcode_codingplan::types::PlanInfo;
        let plan = PlanInfo {
            plan_name: "CodingPlan Pro".into(),
            status: 1,
            claimed_at: "2026-06-18".into(),
            expires_at: "2026-07-18".into(),
            remaining_days: 2,
            total_days: 30,
            apply_id: 42,
        };
        let usage = parse_usage(SAMPLE).expect("parse SAMPLE");
        let overview = compute_overview(&usage);
        let m = UsageModal::new(UsageData {
            window: None,
            plan: Some(plan),
            usage: Some(usage),
            overview: Some(overview),
            error: None,
        });
        let rows = m.current_rows();
        let all: String = rows
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // plan_name should appear
        assert!(
            all.contains("CodingPlan Pro"),
            "expected plan_name in current rows; got:\n{all}"
        );
        // remaining/total days should appear
        assert!(
            all.contains("2") && all.contains("30"),
            "expected remaining/total days; got:\n{all}"
        );
        // The standalone "Plan" title heading should NOT be its own line
        // (we omit the UsagePlanTitle push intentionally)
        let stripped: String = rows
            .iter()
            .map(|(l, _)| UsageModal::strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        // The plan section starts directly at "CodingPlan Pro · Active", no bare "Plan" heading line
        assert!(
            !stripped
                .lines()
                .any(|line| line.trim() == t(Msg::UsagePlanTitle).as_ref()),
            "standalone Plan title heading should be absent; got:\n{stripped}"
        );
    }

    #[test]
    fn models_rows_row_sum_fallback_tokens() {
        // When model_tokens top-level map would be absent, row-level sums should be used.
        // In our SAMPLE the top-level map is populated, so we verify consistency:
        // GLM-5.2 has 717016 tokens in rows, which should appear in breakdown.
        let m = sample_modal();
        let rows = m.models_rows(true, true);
        let all: String = rows
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("717.1k") || all.contains("717016") || all.contains("717"),
            "expected GLM token count in breakdown; got:\n{all}"
        );
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let s = "\x1b[1mHello\x1b[22m \x1b[38;5;75mWorld\x1b[39m";
        assert_eq!(UsageModal::strip_ansi(s), "Hello World");
    }

    #[test]
    fn active_tab_text_is_plain() {
        let m = sample_modal();
        let text = m.active_tab_text(false, false);
        assert!(
            !text.contains('\x1b'),
            "active_tab_text should not contain ANSI escapes"
        );
    }
}
