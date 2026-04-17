// Rendering logic for atomcode-tuix (CC-style normal-mode streaming output)

use std::io::{self, Write};

use crossterm::{
    execute,
    style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor},
    terminal,
};

// -- Palette constants --

const BRAND: Color = Color::Rgb {
    r: 140,
    g: 175,
    b: 230,
};
const DIM_GRAY: Color = Color::Rgb {
    r: 85,
    g: 88,
    b: 100,
};
const BLUE: Color = Color::Rgb {
    r: 100,
    g: 160,
    b: 240,
};
const ACCENT_DIM: Color = Color::Rgb {
    r: 40,
    g: 55,
    b: 80,
};
const SECONDARY: Color = Color::Rgb {
    r: 140,
    g: 142,
    b: 155,
};
const WARNING: Color = Color::Rgb {
    r: 195,
    g: 155,
    b: 45,
};
const ERROR_RED: Color = Color::Rgb {
    r: 210,
    g: 75,
    b: 75,
};
const GREEN: Color = Color::Rgb {
    r: 80,
    g: 200,
    b: 120,
};
const RED: Color = Color::Rgb {
    r: 210,
    g: 75,
    b: 75,
};

fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

/// Simple word-wrap: splits `text` into lines of at most `width` characters.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        if raw_line.len() <= width {
            lines.push(raw_line.to_string());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split_whitespace() {
            if current.is_empty() {
                current = word.to_string();
            } else if current.len() + 1 + word.len() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Print welcome screen: clear terminal, show logo + project info.
pub fn print_welcome(model: &str, working_dir: &str) {
    let mut out = io::stdout();
    // Clear screen
    let _ = crossterm::execute!(
        out,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    );

    // Logo
    let _ = execute!(out, SetForegroundColor(BRAND), SetAttribute(Attribute::Bold));
    let _ = write!(out, "  \u{25c9}  AtomCode\r\n");
    let _ = execute!(out, SetAttribute(Attribute::Reset), ResetColor);

    // Info line
    let _ = execute!(out, SetForegroundColor(DIM_GRAY));
    let _ = write!(out, "  {}  {}\r\n", model, working_dir);
    let _ = execute!(out, ResetColor);

    // Separator
    let w = term_width();
    let _ = execute!(out, SetForegroundColor(ACCENT_DIM));
    let _ = write!(out, "  {}\r\n", "\u{2500}".repeat(w.saturating_sub(4)));
    let _ = execute!(out, ResetColor);

    // Tips
    let _ = execute!(out, SetForegroundColor(DIM_GRAY));
    let _ = write!(out, "  /help for commands \u{00b7} Ctrl+C to cancel \u{00b7} Shift+Enter for newline\r\n");
    let _ = execute!(out, ResetColor);

    let _ = out.flush();
}

/// Print header: `AtomCode  model_name  ~/working_dir`
pub fn print_header(model: &str, working_dir: &str) {
    let mut out = io::stdout();
    let _ = execute!(
        out,
        SetForegroundColor(BRAND),
        SetAttribute(Attribute::Bold),
    );
    print!("AtomCode");
    let _ = execute!(
        out,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(DIM_GRAY),
    );
    print!("  {}  {}", model, working_dir);
    let _ = execute!(out, ResetColor);
    let _ = write!(io::stdout(), "\r\n"); let _ = io::stdout().flush();
    let _ = out.flush();
}

/// Print the input prompt: `\n❯ ` in blue.
pub fn print_prompt() {
    let mut out = io::stdout();
    let _ = execute!(out, SetForegroundColor(BLUE), SetAttribute(Attribute::Bold));
    print!("\n❯ ");
    let _ = execute!(out, SetAttribute(Attribute::Reset), ResetColor);
    let _ = out.flush();
}

/// Print user message with word-wrapping.
pub fn print_user_message(content: &str) {
    let mut out = io::stdout();
    let wrap_width = term_width().saturating_sub(4).max(20);
    let lines = word_wrap(content, wrap_width);

    let _ = execute!(out, SetForegroundColor(BLUE), SetAttribute(Attribute::Bold));
    print!("\n❯ ");
    let _ = execute!(out, SetAttribute(Attribute::Reset), ResetColor);

    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            print!("  ");
        }
        let _ = write!(io::stdout(), "{}\r\n", line);
    }
    let _ = out.flush();
}

/// Print assistant text, each line prefixed with `  │ `.
pub fn print_assistant_text(content: &str) {
    let mut out = io::stdout();
    for line in content.lines() {
        let _ = execute!(out, SetForegroundColor(ACCENT_DIM));
        print!("  │ ");
        let _ = execute!(out, ResetColor);
        let _ = write!(io::stdout(), "{}\r\n", line);
    }
    let _ = out.flush();
}

/// Print tool call: `  │   ▸ ToolName(detail)`
pub fn print_tool_call(name: &str, detail: &str) {
    let mut out = io::stdout();
    let _ = execute!(out, SetForegroundColor(ACCENT_DIM));
    print!("  │   ");
    let _ = execute!(out, SetForegroundColor(SECONDARY));
    print!("▸ {}", name);
    let _ = execute!(out, SetForegroundColor(DIM_GRAY));
    if !detail.is_empty() {
        print!("({})", detail);
    }
    let _ = execute!(out, ResetColor);
    let _ = write!(io::stdout(), "\r\n"); let _ = io::stdout().flush();
    let _ = out.flush();
}

/// Print tool result: `  │   ✓ summary` or `  │   ✗ summary`
pub fn print_tool_result(success: bool, summary: &str) {
    let mut out = io::stdout();
    let _ = execute!(out, SetForegroundColor(ACCENT_DIM));
    print!("  │   ");

    if success {
        let _ = execute!(out, SetForegroundColor(GREEN));
        print!("✓ ");
    } else {
        let _ = execute!(out, SetForegroundColor(RED));
        print!("✗ ");
    }

    let _ = execute!(out, SetForegroundColor(DIM_GRAY));
    print!("{}", summary);
    let _ = execute!(out, ResetColor);
    let _ = write!(io::stdout(), "\r\n"); let _ = io::stdout().flush();
    let _ = out.flush();
}

/// Overwrite current line with spinner frame and label.
pub fn print_spinner(frame: &str, label: &str) {
    let mut out = io::stdout();
    print!("\r\x1b[K");
    let _ = execute!(out, SetForegroundColor(BRAND));
    print!("  {} ", frame);
    let _ = execute!(out, SetForegroundColor(DIM_GRAY));
    print!("{}", label);
    let _ = execute!(out, ResetColor);
    let _ = out.flush();
}

/// Clear the spinner line.
pub fn clear_spinner() {
    let mut out = io::stdout();
    print!("\r\x1b[K");
    let _ = out.flush();
}

/// Print approval prompt for tool use.
pub fn print_approval_prompt(tool_name: &str, detail: &str) {
    let mut out = io::stdout();
    let _ = execute!(out, SetForegroundColor(WARNING));
    print!("\n  Allow {}({})?", tool_name, detail);
    print!(" [Y]es / [N]o / [A]lways");
    let _ = execute!(out, ResetColor);
    let _ = write!(io::stdout(), "\r\n"); let _ = io::stdout().flush();
    let _ = out.flush();
}

/// Print error message.
pub fn print_error(msg: &str) {
    let mut out = io::stdout();
    let _ = execute!(out, SetForegroundColor(ERROR_RED));
    let _ = write!(io::stdout(), "\r\n  [Error: {}]\r\n", msg);
    let _ = execute!(out, ResetColor);
    let _ = out.flush();
}

/// Print a diff line with appropriate coloring.
pub fn print_diff_line(line: &str) {
    let mut out = io::stdout();
    let _ = execute!(out, SetForegroundColor(ACCENT_DIM));
    print!("  │     ");

    if line.starts_with("- ") {
        let _ = execute!(out, SetForegroundColor(RED));
    } else if line.starts_with("+ ") {
        let _ = execute!(out, SetForegroundColor(GREEN));
    }
    print!("{}", line);
    let _ = execute!(out, ResetColor);
    let _ = write!(io::stdout(), "\r\n"); let _ = io::stdout().flush();
    let _ = out.flush();
}

/// Extract a concise detail string from tool args JSON.
pub fn format_tool_detail(name: &str, args_json: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };

    match name {
        "read_file" | "edit_file" | "write_file" | "create_file" => {
            if let Some(path) = val.get("file_path").and_then(|v| v.as_str()) {
                // Extract basename
                path.rsplit('/').next().unwrap_or(path).to_string()
            } else {
                String::new()
            }
        }
        "grep" => {
            if let Some(pattern) = val.get("pattern").and_then(|v| v.as_str()) {
                let truncated: String = pattern.chars().take(30).collect();
                if pattern.len() > 30 {
                    format!("{}…", truncated)
                } else {
                    truncated
                }
            } else {
                String::new()
            }
        }
        "bash" => {
            if let Some(cmd) = val.get("command").and_then(|v| v.as_str()) {
                let truncated: String = cmd.chars().take(40).collect();
                if cmd.len() > 40 {
                    format!("{}…", truncated)
                } else {
                    truncated
                }
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}
