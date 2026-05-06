// crates/atomcode-tuix/src/markdown.rs
//
// Line-oriented markdown renderer. Handles:
//   **bold** / *italic* / `code` (inline)
//   # / ## / ### headings
//   - / * bullet lists
//   ```fenced code blocks``` (state-tracked)
//   --- horizontal rules
// Tables are passed through as raw text (pipes show literally).

use crate::terminal::TerminalCaps;

/// Parser state maintained across lines of a streamed response.
#[derive(Default)]
pub struct MdState {
    pub in_code_block: bool,
    /// Accumulates consecutive `|…|` rows; flushed as an aligned block
    /// when a non-table line arrives.
    pub table_buf: Vec<String>,
}

impl MdState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset(&mut self) {
        self.in_code_block = false;
        self.table_buf.clear();
    }
}

/// Render one complete line with block- and inline-level markdown applied.
/// Returns None if the line should be omitted from output (e.g., a fence
/// marker ``` that toggles code-block state but isn't itself visible text).
pub fn render_line(line: &str, state: &mut MdState, caps: TerminalCaps) -> Option<String> {
    render_line_with_width(line, state, caps, 0)
}

/// Width-aware variant of [`render_line`]. When `max_width > 0`, a flushed
/// table's column widths are capped so every line fits the budget — otherwise
/// `wrap_cells_to_width` downstream chops long rows and shatters the table's
/// border structure. `max_width = 0` keeps legacy behaviour.
pub fn render_line_with_width(
    line: &str,
    state: &mut MdState,
    caps: TerminalCaps,
    max_width: usize,
) -> Option<String> {
    let trimmed = line.trim();

    // Table row: buffer and defer emit until block ends.
    if !state.in_code_block && trimmed.starts_with('|') {
        state.table_buf.push(trimmed.to_string());
        return None;
    }

    // Non-table line arriving after buffered rows: flush as aligned block.
    let prefix = if !state.table_buf.is_empty() {
        let t = flush_aligned_table_with_width(&state.table_buf, caps, max_width);
        state.table_buf.clear();
        Some(t)
    } else {
        None
    };
    let prepend = |body: String| -> String {
        match prefix.as_ref() {
            Some(p) => format!("{}\n{}", p, body),
            None => body,
        }
    };
    let prefix_only = || -> Option<String> { prefix.as_ref().map(|p| p.clone()) };

    // Fenced code block fence (``` or ~~~)
    if is_fence(trimmed) {
        state.in_code_block = !state.in_code_block;
        return prefix_only();
    }

    // Inside code block: render in bright white + bold (SGR 97 + 1) with
    // no inline parsing. Bright white reads cleanly on both light and dark
    // backgrounds without the low-contrast pastel hit that SGR 96 (cyan)
    // suffers on iTerm2's default light preset.
    if state.in_code_block {
        let body = if caps.colors {
            format!("\x1b[1;97m{}\x1b[22;39m", line)
        } else {
            line.to_string()
        };
        return Some(prepend(body));
    }

    // Horizontal rule — render as a blank separator line, not a visible
    // rule. A horizontal bar overwhelms the surrounding prose; a blank line
    // communicates the same thematic break far more gracefully.
    if is_hrule(trimmed) {
        return Some(prepend(String::new()));
    }

    // Heading — express hierarchy with SGR weight (bold) and italic
    // rather than coloured greys. SGR 90 (bright-black) renders at near-
    // invisible contrast on several iTerm2 dark presets; italic keeps
    // H4+ visually distinct from H1-3 without relying on a colour that
    // can disappear into the background.
    if let Some((level, rest)) = parse_heading(line) {
        let inner = render_inline(rest, caps);
        let body = if !caps.colors {
            format!("{} {}", "#".repeat(level as usize), inner)
        } else {
            match level {
                1 | 2 | 3 => format!("\x1b[1m{}\x1b[22m", inner),
                _ => format!("\x1b[3m{}\x1b[23m", inner),
            }
        };
        return Some(prepend(body));
    }

    // Unordered list: `- text` / `* text`
    if let Some((indent, rest)) = parse_list_item(line) {
        let inner = render_inline(rest, caps);
        return Some(prepend(format!("{}• {}", " ".repeat(indent), inner)));
    }

    // Default: inline-only
    Some(prepend(render_inline(line, caps)))
}

/// Emit any still-buffered block (e.g., a table that ended without a
/// following non-table line). Call at stream end.
pub fn finalize(state: &mut MdState, caps: TerminalCaps) -> Option<String> {
    finalize_with_width(state, caps, 0)
}

/// Width-aware variant of [`finalize`]. See [`render_line_with_width`].
pub fn finalize_with_width(
    state: &mut MdState,
    caps: TerminalCaps,
    max_width: usize,
) -> Option<String> {
    if state.table_buf.is_empty() {
        return None;
    }
    let t = flush_aligned_table_with_width(&state.table_buf, caps, max_width);
    state.table_buf.clear();
    Some(t)
}

/// Flush a buffered markdown table as a column-aligned block. Computes the
/// max display width per column, pads every cell accordingly, renders with
/// `│`/`┼`/`─` box chars in muted gray. Inline markdown inside cells is
/// honoured.
pub fn flush_aligned_table(rows: &[String], caps: TerminalCaps) -> String {
    flush_aligned_table_with_width(rows, caps, 0)
}

/// Width-aware variant. When `max_width > 0`, column widths are capped so
/// each rendered row fits within the budget (line = `1 + ncols·(w+3)`); cells
/// that exceed the cap are truncated with `…`. `max_width = 0` = no cap.
pub fn flush_aligned_table_with_width(
    rows: &[String],
    caps: TerminalCaps,
    max_width: usize,
) -> String {
    // Parse each row: strip leading/trailing '|', split by '|', trim cells.
    let parsed: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let s = r.trim_start_matches('|').trim_end_matches('|');
            s.split('|').map(|c| c.trim().to_string()).collect()
        })
        .collect();

    // Identify separator row(s) — cells match `[-: ]+` only.
    let is_sep = |row: &[String]| -> bool {
        row.iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
    };

    let ncols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return String::new();
    }

    // Compute col widths from non-separator rows, using display width of
    // the plaintext (markdown markers stripped for width only).
    let mut col_widths = vec![0usize; ncols];
    for row in &parsed {
        if is_sep(row) {
            continue;
        }
        for (j, cell) in row.iter().enumerate() {
            if j >= ncols {
                break;
            }
            let plain = strip_md_for_width(cell);
            let w = crate::width::display_width(&plain);
            col_widths[j] = col_widths[j].max(w);
        }
    }

    // Cap per-column width so the full row fits: line = 1 (left `│`) +
    // ncols · (w + 3). Lower bound 6 keeps cells legible; upper bound 40
    // matches atomcode-tui so short tables don't waste horizontal space.
    if max_width > 0 {
        let overhead = 1 + 3 * ncols;
        let budget = max_width.saturating_sub(overhead);
        let cap = (budget / ncols.max(1)).clamp(6, 40);
        for w in col_widths.iter_mut() {
            *w = (*w).min(cap);
        }
    }

    // Bright-black / DarkGrey (SGR 90) — table borders are chrome,
    // not content. Cyan (SGR 96) made them collide with the input
    // box separator and the inline-code colour, collapsing the
    // visual hierarchy. Gray reads as quiet structure and lets
    // header text + cell content carry the visual weight.
    let border_on = if caps.colors { "\x1b[90m" } else { "" };
    let border_off = if caps.colors { "\x1b[39m" } else { "" };

    // Draw a horizontal rule row with given connector characters.
    let rule = |left: char, mid: char, right: char| -> String {
        let mut s = String::new();
        s.push_str(border_on);
        s.push(left);
        for (j, w) in col_widths.iter().enumerate() {
            for _ in 0..(w + 2) {
                s.push('─');
            }
            if j + 1 < col_widths.len() {
                s.push(mid);
            }
        }
        s.push(right);
        s.push_str(border_off);
        s
    };

    let data_rows: Vec<&Vec<String>> = parsed.iter().filter(|r| !is_sep(r)).collect();

    let mut out = String::new();
    // Top border: ┌─┬─┐
    out.push_str(&rule('┌', '┬', '┐'));
    out.push('\n');

    for (i, row) in data_rows.iter().enumerate() {
        // Data row: │ cell │ cell │
        out.push_str(border_on);
        out.push('│');
        out.push_str(border_off);
        for (j, w) in col_widths.iter().enumerate() {
            let cell = row.get(j).map(|s| s.as_str()).unwrap_or("");
            let plain = strip_md_for_width(cell);
            let plain_w = crate::width::display_width(&plain);
            // Truncate overlong cells to fit the column cap. Inline markdown
            // (`**bold**`, backticks) is dropped on the truncated form — the
            // alternative (truncating the raw string) risks unterminated
            // `**` markers that poison the rest of the line.
            let (body, body_w) = if plain_w > *w {
                let t = crate::width::truncate_with_ellipsis(&plain, *w);
                let tw = crate::width::display_width(&t);
                (t, tw)
            } else {
                (render_inline(cell, caps), plain_w)
            };
            out.push(' ');
            out.push_str(&body);
            let pad = w.saturating_sub(body_w);
            for _ in 0..pad {
                out.push(' ');
            }
            out.push(' ');
            out.push_str(border_on);
            out.push('│');
            out.push_str(border_off);
        }
        out.push('\n');

        // Separator between every pair of rows: ├─┼─┤
        if i + 1 < data_rows.len() {
            out.push_str(&rule('├', '┼', '┤'));
            out.push('\n');
        }
    }

    // Bottom border: └─┴─┘
    out.push_str(&rule('└', '┴', '┘'));
    out
}

fn strip_md_for_width(s: &str) -> String {
    // Remove markdown markers that add bytes but no display width.
    s.replace("**", "").replace('`', "")
}

/// Legacy single-line inline renderer — kept for direct callers (tests,
/// simple assistant lines). Does not track block state.
pub fn render_inline_line(line: &str, caps: TerminalCaps) -> String {
    render_inline(line, caps)
}

// ─── Helpers ───

fn render_inline(line: &str, caps: TerminalCaps) -> String {
    if !caps.colors {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 16);
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    let mut inner = String::new();
                    let mut closed = false;
                    while let Some(&p) = chars.peek() {
                        if p == '*' {
                            chars.next();
                            if chars.peek() == Some(&'*') {
                                chars.next();
                                closed = true;
                                break;
                            } else {
                                inner.push('*');
                            }
                        } else {
                            chars.next();
                            inner.push(p);
                        }
                    }
                    if closed && !inner.is_empty() {
                        out.push_str("\x1b[1m");
                        out.push_str(&inner);
                        out.push_str("\x1b[22m");
                    } else {
                        out.push_str("**");
                        out.push_str(&inner);
                    }
                } else {
                    let mut inner = String::new();
                    let mut closed = false;
                    while let Some(&p) = chars.peek() {
                        chars.next();
                        if p == '*' {
                            closed = true;
                            break;
                        }
                        inner.push(p);
                    }
                    if closed && !inner.is_empty() {
                        out.push_str("\x1b[3m");
                        out.push_str(&inner);
                        out.push_str("\x1b[23m");
                    } else {
                        out.push('*');
                        out.push_str(&inner);
                    }
                }
            }
            '`' => {
                let mut inner = String::new();
                let mut closed = false;
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p == '`' {
                        closed = true;
                        break;
                    }
                    inner.push(p);
                }
                if closed && !inner.is_empty() {
                    // Bold + bright white — clean, theme-neutral inline
                    // code styling that stays readable on both light and
                    // dark backgrounds.
                    out.push_str("\x1b[1;97m");
                    out.push_str(&inner);
                    out.push_str("\x1b[22;39m");
                } else {
                    out.push('`');
                    out.push_str(&inner);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn is_fence(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    match chars.next() {
        Some('`') => {
            trimmed.len() >= 3 && trimmed.as_bytes()[1] == b'`' && trimmed.as_bytes()[2] == b'`'
        }
        Some('~') => {
            trimmed.len() >= 3 && trimmed.as_bytes()[1] == b'~' && trimmed.as_bytes()[2] == b'~'
        }
        _ => false,
    }
}

fn is_hrule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let first = trimmed.chars().next().unwrap();
    if first != '-' && first != '*' && first != '_' {
        return false;
    }
    let mut n = 0;
    for c in trimmed.chars() {
        if c == first {
            n += 1;
        } else if !c.is_whitespace() {
            return false;
        }
    }
    n >= 3
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let line = line.trim_start();
    let mut level = 0u8;
    for c in line.chars() {
        if c == '#' && level < 6 {
            level += 1;
        } else if level > 0 && c == ' ' {
            let content = &line[(level as usize) + 1..];
            return Some((level, content));
        } else {
            return None;
        }
    }
    None
}

fn parse_list_item(line: &str) -> Option<(usize, &str)> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    let rest = &line[indent..];
    if let Some(r) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        Some((indent, r))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{EnvView, TerminalCaps};

    fn caps() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        })
    }
    fn plain_caps() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: true,
            term: Some("xterm".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn inline_bold() {
        assert_eq!(
            render_inline_line("**bold**", caps()),
            "\x1b[1mbold\x1b[22m"
        );
    }

    #[test]
    fn inline_italic() {
        assert_eq!(render_inline_line("*em*", caps()), "\x1b[3mem\x1b[23m");
    }

    #[test]
    fn inline_code() {
        // Inline code uses SGR 1+97 (bold + bright white) — clean, theme-
        // neutral, readable on both light and dark backgrounds.
        assert!(render_inline_line("`x`", caps()).contains("\x1b[1;97mx"));
    }

    #[test]
    fn plain_pass_through() {
        assert_eq!(render_inline_line("**b**", plain_caps()), "**b**");
    }

    #[test]
    fn heading_styled() {
        let mut st = MdState::new();
        let out = render_line("## Hello", &mut st, caps()).unwrap();
        assert!(out.contains("Hello"));
        // Headings now use SGR bold (\x1b[1m) with default foreground —
        // readable on both light and dark terminal themes.
        assert!(out.contains("\x1b[1m"));
    }

    #[test]
    fn heading_plain_keeps_hashes() {
        let mut st = MdState::new();
        let out = render_line("### Sub", &mut st, plain_caps()).unwrap();
        assert_eq!(out, "### Sub");
    }

    #[test]
    fn fence_toggles_state_and_hides() {
        let mut st = MdState::new();
        assert!(render_line("```rust", &mut st, caps()).is_none());
        assert!(st.in_code_block);
        let inside = render_line("let x = 1;", &mut st, caps()).unwrap();
        assert!(inside.contains("let x = 1;"));
        // Inside code block, inline markdown is NOT parsed
        let inside2 = render_line("**not bold**", &mut st, caps()).unwrap();
        assert!(inside2.contains("**not bold**"));
        assert!(render_line("```", &mut st, caps()).is_none());
        assert!(!st.in_code_block);
    }

    #[test]
    fn hrule_becomes_blank_line() {
        // Horizontal rules now render as blank lines (thematic break), not
        // visible rules — a line of "─" chars is visually noisier than the
        // blank separator it's supposed to stand in for.
        let mut st = MdState::new();
        let out = render_line("---", &mut st, caps()).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn list_bullets() {
        let mut st = MdState::new();
        let out = render_line("- item", &mut st, caps()).unwrap();
        assert!(out.starts_with("• "));
    }

    #[test]
    fn list_nested_indent() {
        let mut st = MdState::new();
        let out = render_line("  - nested", &mut st, caps()).unwrap();
        assert!(out.starts_with("  • "));
    }

    #[test]
    fn cjk_bold() {
        assert_eq!(
            render_inline_line("**你好**", caps()),
            "\x1b[1m你好\x1b[22m"
        );
    }

    /// Regression: `flush_aligned_table` computed col widths from raw cell
    /// text with no upper bound. Long CJK rows produced lines far wider than
    /// the terminal, which `wrap_cells_to_width` downstream chopped mid-border
    /// — same structural-corruption class as the atomcode-tui table bug.
    ///
    /// Also prints a visual demo of a very long table so the developer can
    /// eyeball the box-drawing alignment in a narrow panel.
    #[test]
    fn table_fits_within_narrow_panel_width() {
        // ---- visual demo ----
        let demo_rows = vec![
            "| 功能模块 | 核心描述 | 技术栈 | 状态 | 优先级 | 负责人 |".to_string(),
            "|------|------|------|------|------|------|".to_string(),
            "| 用户认证系统 | 支持手机号验证码登录、邮箱密码登录、第三方 OAuth2 集成（微信、钉钉、Google）、JWT Token 自动续期、多端设备管理与会话锁定、密码策略配置与强制改密功能 | Rust + Actix-web + Redis + PostgreSQL | 开发中 | P0 | 张三 |".to_string(),
            "| 权限管理系统 | RBAC 模型实现、细粒度资源级权限控制、数据行级访问控制、动态角色分配与审批流程、权限变更实时审计与日志追踪、组织架构同步与部门级权限继承 | Rust + PostgreSQL + Redis | 已上线 | P0 | 李四 |".to_string(),
            "| 消息推送中心 | WebSocket 实时消息通道、FCM/APNs 推送、邮件模板引擎、短信网关集成、消息重试与死信队列、阅读状态回执与未读计数、批量推送任务调度与进度监控 | Rust + RabbitMQ + Redis + FCM | 规划中 | P1 | 王五 |".to_string(),
            "| 文件存储服务 | 分片上传与断点续传、图片自动缩放与水印、PDF 在线预览、病毒扫描集成、CDN 加速与缓存策略、存储配额与用量统计、版本控制与回溯、大文件秒传（MD5 校验） | Rust + MinIO + Cloudflare R2 | 开发中 | P1 | 赵六 |".to_string(),
            "| 数据分析面板 | 多维度数据聚合查询、自定义仪表盘与 Widget 布局、时间序列数据可视化、数据导出为 Excel/PDF、定时报表生成与邮件分发、实时数据大屏模式 | Rust + ClickHouse + Chart.js | 规划中 | P2 | 孙七 |".to_string(),
            "| 工作流引擎 | 可视化流程编排与拖拽设计器、条件分支与并行网关、人工任务审批流、定时触发与延迟节点、流程版本管理与灰度发布、执行日志追踪与失败重试 | Rust + PostgreSQL + Redis | 规划中 | P2 | 周八 |".to_string(),
            "| API 网关 | 限流与熔断、请求路由与负载均衡、身份认证与签名验证、请求响应转换、CORS 预检处理、API 文档自动生成、灰度发布与 A/B 测试、流量镜像与影子测试 | Rust + OpenResty + Lua | 已上线 | P0 | 吴九 |".to_string(),
            "| 国际化支持 | 多语言资源文件管理与翻译工作流、RTL 右向左语言适配、时区与日期格式本地化、多货币与税务规则、区域性功能开关与特性路由、A/B 测试地域定向 | Rust + i18n-next | 开发中 | P2 | 郑十 |".to_string(),
        ];
        let max_w = 80;
        println!("\n=== Rendered table demo (max_width={}) ===", max_w);
        let rendered = flush_aligned_table_with_width(&demo_rows, plain_caps(), max_w);
        print!("{}", rendered);
        println!("=== End ===\n");

        // ---- regression check ----
        let rows = vec![
            "| 功能 | 描述 | 状态 |".to_string(),
            "|------|------|------|".to_string(),
            "| 用户认证系统 | 支持手机号验证码登录、邮箱密码登录、第三方 OAuth2 集成（微信、钉钉、Google）、JWT Token 自动续期 | 开发中 |".to_string(),
            "| 权限管理系统 | RBAC 模型实现、细粒度资源级权限控制、数据行级访问控制、动态角色分配与审批流程 | 已上线 |".to_string(),
        ];
        let out = flush_aligned_table_with_width(&rows, plain_caps(), max_w);
        for (i, line) in out.lines().enumerate() {
            let w = crate::width::display_width(line);
            assert!(
                w <= max_w,
                "line {} rendered at {} cols — exceeds max_width {}",
                i,
                w,
                max_w
            );
        }
    }
}
