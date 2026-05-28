---
title: Keyboard Shortcuts
category: Reference
keywords: [keyboard, shortcut, hotkey, keybinding, key, binding, keys, hotkeys, keybindings, how, press]
---

# Keyboard Shortcuts

AtomCode terminal shortcuts, based on readline/emacs style.

## Input

| Shortcut | Function |
|----------|----------|
| `Enter` | Send message |
| `Ctrl+J` | Insert newline (works in all terminals) |
| `\` then `Enter` | Insert newline (atomcode fallback, works in all terminals) |
| `Alt+Enter` | Insert newline * |
| `Shift+Enter` | Insert newline ** |
| `/` | Open slash command menu |
| `Tab` | Auto-complete |
| `Backspace` / `Ctrl+H` | Delete previous character |
| `Delete` / `Ctrl+?` | Delete next character |
| `Ctrl+W` | Delete previous word |
| `Ctrl+U` | Clear current line |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+A` / `Home` | Jump to line start |
| `Ctrl+E` / `End` | Jump to line end |
| `Left` / `Right` | Move cursor left/right |

## History

| Shortcut | Function |
|----------|----------|
| `Up` | Previous input |
| `Down` | Next input |

## Scrolling Output

Use terminal native scrollback: `Cmd+↑/↓`, mouse wheel, tmux copy-mode all work.
Mouse select + `Ctrl+C` to copy (atomcode doesn't intercept mouse).

## Session Control

| Shortcut | Function |
|----------|----------|
| `Ctrl+C` | Cancel current turn / close modal |
| `Ctrl+D` | Exit atomcode |
| `Ctrl+L` | Clear screen |

* `Alt+Enter`: Some terminals (like Windows Terminal) intercept this for fullscreen toggle. Use `Ctrl+J` or `\` + `Enter` instead.
** `Shift+Enter`: Some terminals (like native macOS Terminal) don't support this. Use `Ctrl+J` or `\` + `Enter` instead.
