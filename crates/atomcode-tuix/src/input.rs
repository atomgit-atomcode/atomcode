use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Print, SetForegroundColor, ResetColor};
use crossterm::cursor;
use crossterm::execute;
use std::io::{stdout, Write};
use std::time::Duration;

const PROMPT: &str = "❯ ";
const PROMPT_DISPLAY_WIDTH: usize = 2; // "❯ " is 1 wide char + 1 space = 2 columns
const CONTINUATION: &str = "  ";
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const PROMPT_COLOR: Color = Color::Rgb { r: 100, g: 160, b: 240 };

/// Read user input line-by-line in raw mode.
/// Returns `Some(text)` on Enter, `None` on Ctrl+C when buffer is empty (exit signal).
pub fn read_input(history: &[String], commands: &[String]) -> Option<String> {
    let mut buf = String::new();
    let mut cursor: usize = 0; // byte offset into buf
    let mut history_idx: Option<usize> = None; // index into history (from end)
    let mut stash = String::new(); // saves current input when navigating history

    redraw(&buf, cursor);

    loop {
        if !poll(POLL_TIMEOUT).ok()? {
            continue;
        }
        let event = read().ok()?;

        match event {
            Event::Key(KeyEvent { kind: KeyEventKind::Release, .. }) => continue,
            Event::Key(KeyEvent { code, modifiers, kind, .. }) if kind == KeyEventKind::Press || kind == KeyEventKind::Repeat => {
                match (code, modifiers) {
                    // Ctrl+C
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        if buf.is_empty() {
                            // Print newline so shell prompt appears cleanly
                            let _ = execute!(stdout(), Print("\n"));
                            return None;
                        }
                        buf.clear();
                        cursor = 0;
                        history_idx = None;
                    }
                    // Ctrl+U — clear entire input
                    (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                        buf.clear();
                        cursor = 0;
                    }
                    // Ctrl+W — delete word backward
                    (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                        if cursor > 0 {
                            let before = &buf[..cursor];
                            // Skip trailing spaces
                            let trimmed = before.trim_end_matches(' ');
                            // Find last space
                            let word_start = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                            buf.drain(word_start..cursor);
                            cursor = word_start;
                        }
                    }
                    // Ctrl+K — delete from cursor to end of line
                    (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => {
                        // Find next newline or end of buf
                        let end = buf[cursor..].find('\n').map(|i| cursor + i).unwrap_or(buf.len());
                        buf.drain(cursor..end);
                    }
                    // Shift+Enter — insert newline
                    (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) => {
                        buf.insert(cursor, '\n');
                        cursor += 1;
                    }
                    // Enter — submit
                    (KeyCode::Enter, _) => {
                        let _ = execute!(stdout(), Print("\n"));
                        let text = buf.trim().to_string();
                        if text.is_empty() {
                            buf.clear();
                            cursor = 0;
                            redraw(&buf, cursor);
                            continue;
                        }
                        return Some(text);
                    }
                    // Backspace
                    (KeyCode::Backspace, _) => {
                        if cursor > 0 {
                            // Find the previous char boundary
                            let prev = prev_char_boundary(&buf, cursor);
                            buf.drain(prev..cursor);
                            cursor = prev;
                        }
                    }
                    // Delete
                    (KeyCode::Delete, _) => {
                        if cursor < buf.len() {
                            let next = next_char_boundary(&buf, cursor);
                            buf.drain(cursor..next);
                        }
                    }
                    // Left
                    (KeyCode::Left, _) => {
                        if cursor > 0 {
                            cursor = prev_char_boundary(&buf, cursor);
                        }
                    }
                    // Right
                    (KeyCode::Right, _) => {
                        if cursor < buf.len() {
                            cursor = next_char_boundary(&buf, cursor);
                        }
                    }
                    // Home
                    (KeyCode::Home, _) => {
                        // Go to start of current line
                        let line_start = buf[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
                        cursor = line_start;
                    }
                    // End
                    (KeyCode::End, _) => {
                        // Go to end of current line
                        let line_end = buf[cursor..].find('\n').map(|i| cursor + i).unwrap_or(buf.len());
                        cursor = line_end;
                    }
                    // Up — history navigation
                    (KeyCode::Up, _) => {
                        if !buf.contains('\n') && !history.is_empty() {
                            if history_idx.is_none() {
                                stash = buf.clone();
                                history_idx = Some(history.len() - 1);
                            } else if let Some(idx) = history_idx {
                                if idx > 0 {
                                    history_idx = Some(idx - 1);
                                } else {
                                    redraw(&buf, cursor);
                                    continue;
                                }
                            }
                            if let Some(idx) = history_idx {
                                buf = history[idx].clone();
                                cursor = buf.len();
                            }
                        }
                    }
                    // Down — history navigation
                    (KeyCode::Down, _) => {
                        if let Some(idx) = history_idx {
                            if idx + 1 < history.len() {
                                history_idx = Some(idx + 1);
                                buf = history[idx + 1].clone();
                                cursor = buf.len();
                            } else {
                                // Restore stash
                                history_idx = None;
                                buf = stash.clone();
                                cursor = buf.len();
                            }
                        }
                    }
                    // Tab — command completion
                    (KeyCode::Tab, _) => {
                        if buf.starts_with('/') {
                            let input = &buf[1..]; // strip leading /
                            let matches: Vec<&String> = commands.iter()
                                .filter(|c| c.starts_with(input))
                                .collect();
                            if matches.len() == 1 {
                                buf = format!("/{} ", matches[0]);
                                cursor = buf.len();
                            } else if matches.len() > 1 {
                                // Longest common prefix
                                let lcp = longest_common_prefix(&matches);
                                if lcp.len() > input.len() {
                                    buf = format!("/{}", lcp);
                                    cursor = buf.len();
                                }
                            }
                        }
                    }
                    // Regular char
                    (KeyCode::Char(c), m) => {
                        // On macOS, Option key sets ALT modifier for special chars,
                        // but we still want to insert them. Only skip if CONTROL is set.
                        if m.contains(KeyModifiers::CONTROL) {
                            // Unhandled ctrl combo — ignore
                            continue;
                        }
                        buf.insert(cursor, c);
                        cursor += c.len_utf8();

                        // Auto-show command list when user types just "/"
                        if buf == "/" {
                            redraw(&buf, cursor);
                            let mut out = stdout();
                            let _ = execute!(out, SetForegroundColor(Color::Rgb { r: 85, g: 88, b: 100 }));
                            for cmd in commands {
                                let _ = write!(out, "\r\n    /{}", cmd);
                            }
                            let _ = execute!(out, ResetColor);
                            let _ = write!(out, "\r\n");
                            let _ = out.flush();
                            // Reprint prompt with current buffer
                            redraw(&buf, cursor);
                            continue;
                        }
                    }
                    _ => continue,
                }
            }
            Event::Paste(text) => {
                buf.insert_str(cursor, &text);
                cursor += text.len();
            }
            _ => continue,
        }

        redraw(&buf, cursor);
    }
}

/// Read a single keypress for tool approval.
/// Returns 'y' for Y/y/Enter, 'n' for N/n/Esc, 'a' for A/a.
pub fn read_approval() -> char {
    loop {
        if poll(POLL_TIMEOUT).unwrap_or(false) {
            if let Ok(event) = read() {
                match event {
                    Event::Key(KeyEvent { kind: KeyEventKind::Release, .. }) => continue,
                    Event::Key(KeyEvent { code, kind, .. }) if kind == KeyEventKind::Press || kind == KeyEventKind::Repeat => {
                        match code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => return 'y',
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return 'n',
                            KeyCode::Char('a') | KeyCode::Char('A') => return 'a',
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

// ─── Internal helpers ───

fn redraw(buf: &str, cursor: usize) {
    let mut out = stdout();
    let lines: Vec<&str> = buf.split('\n').collect();
    let num_lines = lines.len();

    // Figure out which line the cursor is on and the column offset
    let (cursor_line, cursor_col) = cursor_position(buf, cursor);

    // Move up to the first line if multi-line (we may have printed multiple lines before)
    // We track how many lines were printed; for simplicity, move up (num_lines - 1) lines
    // from the last printed position. Since we always end on the last line after printing,
    // we move up from there.
    if num_lines > 1 {
        // Move cursor up to first line
        let up = num_lines - 1;
        let _ = execute!(out, cursor::MoveUp(up as u16));
    }

    // Clear and reprint each line
    for (i, line) in lines.iter().enumerate() {
        // Move to start of line and clear it
        let _ = write!(out, "\r\x1b[K");
        if i == 0 {
            let _ = execute!(
                out,
                SetForegroundColor(PROMPT_COLOR),
                Print(PROMPT),
                ResetColor,
                Print(line),
            );
        } else {
            let _ = execute!(out, Print(CONTINUATION), Print(line));
        }
        // Move to next line if not the last
        if i < num_lines - 1 {
            let _ = write!(out, "\n");
        }
    }

    // Now position the cursor correctly
    // We are currently at the end of the last line.
    // Move up to cursor_line
    let lines_up = (num_lines - 1) - cursor_line;
    if lines_up > 0 {
        let _ = execute!(out, cursor::MoveUp(lines_up as u16));
    }

    // Move to correct column
    let prefix_width = if cursor_line == 0 { PROMPT_DISPLAY_WIDTH } else { 2 };
    let col = prefix_width + display_width(&lines[cursor_line][..cursor_col]);
    let _ = write!(out, "\r\x1b[{}C", col);

    let _ = out.flush();
}

/// Returns (line_index, byte_offset_within_line) for the given cursor byte offset in buf.
fn cursor_position(buf: &str, cursor: usize) -> (usize, usize) {
    let before = &buf[..cursor];
    let line = before.matches('\n').count();
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    (line, cursor - line_start)
}

/// Compute the display width of a string (simple: count chars, treating each as width 1).
/// For a more accurate CJK-aware width, a unicode-width crate would be needed.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos - 1;
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

fn longest_common_prefix(strings: &[&String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = strings[0].as_str();
    let mut len = first.len();
    for s in &strings[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.bytes().zip(s.bytes()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    first[..len].to_string()
}
