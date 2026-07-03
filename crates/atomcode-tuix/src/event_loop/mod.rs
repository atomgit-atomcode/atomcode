// crates/atomcode-tuix/src/event_loop/mod.rs
//
// Main event-loop crate root. `run_loop` is the entry point from
// `atomcode-tuix::run`; everything else in this module tree supports it.
//
// Layout:
//   mod.rs       — App struct + LoopCtx + run_loop + input plumbing
//                  (handle_input / handle_idle_key / handle_streaming_key /
//                  handle_approval_key / redraw helpers), plus Buffer +
//                  BufferResult + agent-event handler + spinner draw.
//   commands.rs  — slash-command dispatcher + /login (OAuth child handoff)
//
// Over time more subfiles should split out (agent_events, redraw helpers,
// Buffer); modal overlays already live in `crate::modals`.

pub(crate) mod bg_runtime;
pub(crate) mod commands;
pub(crate) mod file_index;
pub(crate) mod live_sync;
pub(crate) mod monitor;
pub(crate) mod oauth_poll;
pub(crate) mod usage_monitor;
use commands::{attach_live_session, dispatch_undo, execute_slash_command, format_rate_limited_line};
pub use commands::{perform_session_rename, validate_session_name, MAX_SESSION_NAME_LEN};

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use atomcode_core::agent::{
    AgentClient, AgentCommand, AgentEvent, AgentPhase, AgentRuntimeFactory,
};
use atomcode_core::config::Config;
use atomcode_core::session::{SessionId, SessionManager};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use tokio::sync::mpsc;

use atomcode_core::conversation::message::ImagePart;
use base64::Engine;

use crate::commands::{parse_bash_command, parse_slash_line, CommandRegistry};
use crate::input::history::History;
use crate::input::key_action::{classify, Action};
use crate::input::InputEvent;
use crate::render::{Renderer, UiLine};
use crate::state::{UiPhase, UiState};
use crate::think::ThinkStripper;

/// Encode raw RGBA pixel data as a PNG image in memory.
fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut encoder = png::Encoder::new(&mut buf, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(rgba).ok()?;
    drop(writer);
    Some(buf)
}

/// Try to grab an image from the system clipboard via `arboard`.
/// Returns `Some((ImagePart, fingerprint))` if the clipboard holds an
/// image, `None` otherwise. The fingerprint is hashed off the raw RGBA
/// Try to get an image from the clipboard. First attempts to read image
/// bytes directly (screenshots, Preview Copy). If that fails, falls back
/// to reading a file:// URL from the clipboard text (Finder Cmd+C case)
/// and loading the image from that path.
/// Returns the image data and a fingerprint hash.
/// bytes (not the PNG-encoded base64) — same hash function the status
/// poll uses, so paste-side and poll-side fingerprints line up for the
/// "is this the same image we already attached?" check.
fn try_paste_clipboard_image() -> Option<(ImagePart, u64)> {
    // Three-tier fallback chain for Ctrl+V → image attach. Each tier
    // covers a real-world clipboard shape Cmd+V already handled via
    // bracketed paste; Ctrl+V is intercepted at the key layer before
    // the terminal's paste pipeline runs, so we have to reproduce
    // those shapes from the clipboard ourselves.
    let mut clipboard = arboard::Clipboard::new().ok()?;

    // Tier 1: raw bytes (NSPasteboardTypePNG / TIFF / NSImage).
    //   Sources: Cmd+Shift+Ctrl+4 screenshot, Preview "Copy", browser
    //   "Copy image", any app's Edit-menu Copy on a bitmap. arboard's
    //   get_image decodes these into RGBA.
    if let Ok(img) = clipboard.get_image() {
        let hash = rgba_fingerprint(img.width, img.height, img.bytes.as_ref());
        let png_data = encode_rgba_to_png(img.width as u32, img.height as u32, img.bytes.as_ref())?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
        return Some((
            ImagePart {
                media_type: "image/png".into(),
                data: b64,
            },
            hash,
        ));
    }

    // Tier 2: file URL / path arriving via the text type (`public.utf8-
    // plain-text` on macOS, `text/uri-list` on X11/Wayland through
    // arboard's text bridge). Some apps mirror their file-URL into
    // text as a courtesy ("Copy as path" tools, browser drag-source,
    // certain file managers). Trim, strip `file://`, percent-decode,
    // hand off to the existing path attachment helper.
    if let Ok(text) = clipboard.get_text() {
        let trimmed = text.trim();
        let stripped = trimmed.strip_prefix("file://").unwrap_or(trimmed);
        if let Ok(decoded) = urlencoding::decode(stripped) {
            if let Some(result) = try_attach_image_from_path(&decoded) {
                return Some(result);
            }
        }
    }

    // Tier 3 (macOS only): read NSPasteboard's `public.file-url` type
    // directly. This is the case Finder `Cmd+C` on an image file
    // produces — there are NO image bytes and the text type is
    // typically NOT auto-populated. iTerm2's Cmd+V handles this by
    // querying the file-URL type and writing the temp path to the
    // PTY; we read the same type via AppKit so Ctrl+V matches.
    #[cfg(target_os = "macos")]
    if let Some(path) = read_macos_clipboard_file_url() {
        if let Some(result) = try_attach_image_from_path(&path) {
            return Some(result);
        }
    }

    None
}

/// Pull plain text off the system clipboard for the Ctrl+V → text-paste
/// fallback. Returns `None` when arboard fails to open the clipboard
/// or the clipboard holds no text — the caller is expected to swallow
/// the keystroke in that case rather than insert a literal `v`.
///
/// Why a dedicated helper instead of inlining `arboard::Clipboard::new`:
/// the Ctrl+V branch already shells out to `try_paste_clipboard_image`,
/// which itself opens a fresh `Clipboard` handle; symmetry keeps the
/// two call sites readable, and the helper drops the handle promptly
/// (some Windows clipboards lock briefly after a read).
fn try_paste_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok().filter(|s| !s.is_empty())
}

/// Read NSPasteboard's `public.file-url` type and return the decoded
/// filesystem path. Returns `None` when the type isn't on the
/// pasteboard, the value isn't a `file://` URL, or percent-decoding
/// fails — caller should fall through, not abort.
///
/// Why AppKit instead of arboard: arboard 3.x doesn't expose any
/// pasteboard type beyond `image` and `text`. Finder `Cmd+C` writes
/// to `public.file-url` exclusively, so we have to query that type
/// directly. The `objc2-app-kit` / `objc2-foundation` deps are
/// already in the tree transitively (arboard pulls them on macOS),
/// so this is cheap to add — just wires up a binding we own.
#[cfg(target_os = "macos")]
fn read_macos_clipboard_file_url() -> Option<String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
    let pb = NSPasteboard::generalPasteboard();
    let raw = unsafe { pb.stringForType(NSPasteboardTypeFileURL) }?.to_string();
    let stripped = raw.strip_prefix("file://").unwrap_or(&raw);
    let decoded = urlencoding::decode(stripped).ok()?;
    Some(decoded.into_owned())
}

/// Map an `ImagePart::media_type` to a cache filename extension.
/// Unknown MIMEs degrade to `bin` — they still round-trip via the
/// stored `media_type` field on `HistoryImageRef`, so the extension is
/// purely informational for humans poking at `~/.atomcode/image-cache/`.
fn ext_for_mt(mt: &str) -> &'static str {
    match mt {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

/// Best-effort cache write. Decodes `img.data` (base64) and persists
/// the raw bytes to `<cache_dir>/<hex_hash>.<ext>`. Skips if the file
/// already exists (cache is content-addressable). Failures are
/// trace-logged and swallowed — the in-memory pending_images path is
/// the source of truth for the current submit.
fn cache_write_image(
    cache_dir: &std::path::Path,
    img: &atomcode_core::conversation::message::ImagePart,
    hash: u64,
) {
    let path = cache_dir.join(format!("{:016x}.{}", hash, ext_for_mt(&img.media_type)));
    if path.exists() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        crate::tuix_trace!("IMG", "cache mkdir failed: {}", e);
        return;
    }
    let raw = match base64::engine::general_purpose::STANDARD.decode(&img.data) {
        Ok(b) => b,
        Err(e) => {
            crate::tuix_trace!("IMG", "cache base64 decode failed: {}", e);
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, &raw) {
        crate::tuix_trace!("IMG", "cache write failed: {}", e);
    }
}

/// Compute the set of `[Image #N]` markers in `buf_text` whose `N`
/// actually corresponds to image bytes that will be sent on submit.
///
/// Two sources count as "real attachment":
///   1. Freshly-attached this session — the marker `N` lives in
///      `state.pending_image_markers`, with bytes in
///      `state.pending_images` at the same index.
///   2. Cache-recalled via arrow-up — the marker `N` lives in
///      `state.pending_recalled_attachments[*].n` (still using the
///      saved-history numbering; will be renumbered on submit by
///      `hydrate_recalled_attachments`).
///
/// Markers in `buf_text` that match neither (e.g. user typed
/// `[Image #99]` literally as text) are excluded. Result preserves
/// the order markers appear in `buf_text` and de-duplicates so the
/// same marker referenced twice surfaces a single preview row.
///
/// Used by `redraw_idle_plain` / `draw_spinner_now` / similar to
/// populate `UiLine::InputPrompt { attachments }`, which the
/// renderer then turns into `└ [Image #N]` preview rows under the
/// input box. Mirror of the post-submit echo (`UiLine::ImageAttachment`)
/// — same visual treatment so users see the attachment status pre-
/// AND post-submit identically.
/// Extract every `[Image #N]` marker number from `text`, in first-occurrence
/// order, de-duped. Unlike `compute_input_attachments` this does NOT filter
/// against pending state — it re-derives the markers purely from the text, used
/// to render the `└ [Image #N]` echoes for a user message that arrives via
/// `UserEcho` (sync mode), where the local submit path intentionally skipped
/// them (the user row itself is also re-rendered from the echo, so emitting the
/// echoes locally at submit time would orphan them ABOVE the later user row).
fn image_markers_in_order(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let needle = b"[Image #";
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            let mut n: usize = 0;
            let mut had_digit = false;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n.saturating_mul(10).saturating_add((bytes[j] - b'0') as usize);
                j += 1;
                had_digit = true;
            }
            if had_digit && j < bytes.len() && bytes[j] == b']' {
                if seen.insert(n) {
                    out.push(n);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

pub(crate) fn compute_input_attachments(
    state: &crate::state::UiState,
    buf_text: &str,
) -> Vec<usize> {
    let mut available: std::collections::HashSet<usize> =
        state.pending_image_markers.iter().copied().collect();
    for refed in &state.pending_recalled_attachments {
        available.insert(refed.n);
    }
    if available.is_empty() {
        return Vec::new();
    }
    // Walk `buf_text` once collecting `[Image #N]` markers in order.
    // De-dupe while preserving first-occurrence order.
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut out = Vec::new();
    let bytes = buf_text.as_bytes();
    let needle = b"[Image #";
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            let mut n: usize = 0;
            let mut had_digit = false;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n
                    .saturating_mul(10)
                    .saturating_add((bytes[j] - b'0') as usize);
                j += 1;
                had_digit = true;
            }
            if had_digit && j < bytes.len() && bytes[j] == b']' {
                if available.contains(&n) && seen.insert(n) {
                    out.push(n);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Drain `state.pending_recalled_attachments`. For each entry: read the
/// cache file, allocate a fresh marker via `session_image_count`, rewrite
/// `[Image #old]` → `[Image #new]` in `line`, and push into the live
/// pending_* vecs. On cache miss, strip the marker and accumulate a
/// notice string for the caller to render.
///
/// Returns the list of notice strings (empty when every attachment hit).
pub(crate) fn hydrate_recalled_attachments(
    state: &mut UiState,
    line: &mut String,
    cache_dir: &std::path::Path,
) -> Vec<String> {
    use base64::Engine;
    let mut notices = Vec::new();
    if state.pending_recalled_attachments.is_empty() {
        return notices;
    }
    for refed in std::mem::take(&mut state.pending_recalled_attachments) {
        let cache_path = cache_dir.join(format!("{}.{}", refed.hash, ext_for_mt(&refed.mt)));
        match std::fs::read(&cache_path) {
            Ok(raw) => {
                state.session_image_count += 1;
                let new_marker = state.session_image_count;
                *line = line.replace(
                    &format!("[Image #{}]", refed.n),
                    &format!("[Image #{}]", new_marker),
                );
                let hash_u64 = u64::from_str_radix(&refed.hash, 16).unwrap_or(0);
                state
                    .pending_images
                    .push(atomcode_core::conversation::message::ImagePart {
                        media_type: refed.mt.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(&raw),
                    });
                state.pending_image_hashes.push(hash_u64);
                state.pending_image_markers.push(new_marker);
            }
            Err(_) => {
                *line = line.replace(&format!("[Image #{}]", refed.n), "");
                notices.push(format!("[Image #{}] 缓存已丢失，已从消息中移除", refed.n));
            }
        }
    }
    notices
}

/// Upper bound on a single attached image (20 MB raw bytes). OpenAI's
/// chat/completions cap is 20 MB per image; Anthropic's is 5 MB. We pick
/// the looser of the two as the tool-side gate so the attempt at least
/// reaches the API — the server's 413 with a clearer reason is a better
/// signal than a silent local rejection.
const MAX_PATH_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Try to interpret a paste payload as a filesystem path to an image
/// file and load it as an [`ImagePart`]. Returns `Some` only when the
/// payload looks unambiguously like an image-attachment intent, never
/// for plain prose that happens to mention a file name.
///
/// The two real-world flows this covers:
///
/// 1. **iTerm2 Cmd+V on image clipboard** — iTerm2 saves the clipboard
///    image to a temp file under `/var/folders/.../T/com.googlecode.iterm2/`
///    and pastes the **file path** as plaintext through the PTY. The
///    image bytes never travel through `InputEvent::Paste`'s text payload
///    or through the system clipboard's "text" slot, so the existing
///    `try_paste_clipboard_image()` empty-text fallback wouldn't fire.
///    Recognising the path is the only way to attach the image. This is
///    the workflow Claude Code / Aider / cursor-cli all support.
/// 2. **Finder drag-and-drop into the terminal** — terminal types the
///    file's absolute path as plaintext, optionally quoted (paths with
///    spaces wrap in `'...'`) or shell-escaped (`\ ` for spaces).
///
/// Acceptance criteria — all must hold:
/// * Single-line content (no `\n`).
/// * After trimming + stripping balanced outer quotes + unescaping
///   `\<space>`, the remainder is an absolute path.
/// * Extension is one of png/jpg/jpeg/gif/webp (case-insensitive).
/// * The path resolves to an existing regular file.
/// * File size ≤ `MAX_PATH_IMAGE_BYTES`.
///
/// Returns `None` for anything that fails any of these — including
/// legitimate text pastes, relative paths (a literal `notes.png` typed
/// at the prompt is ambiguous: text or attachment?), missing files, and
/// oversized files.
///
/// The fingerprint is hashed off the raw file bytes via the same
/// [`rgba_fingerprint`] helper. Identical paste of the same path
/// produces the same hash so the dedup check in `pending_image_hashes`
/// works; collisions with a clipboard-paste of the same image (which
/// hashes RGBA, not file bytes) are out of scope — the hash is a
/// per-source dedup signal, not a global content identity.
fn try_attach_image_from_path(text: &str) -> Option<(ImagePart, u64)> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    // Strip a single layer of matched outer quotes. Finder drag of paths
    // containing spaces wraps in `'...'`; some shells produce `"..."`.
    let unquoted: &str = if trimmed.len() >= 2
        && ((trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    // Unescape shell-escaped spaces (iTerm2 / drag-and-drop emit
    // `/path/with\ space.png`). Backslash before any other char is left
    // alone — no other shell-escape forms occur in real-world drag
    // pastes.
    let unescaped = unquoted.replace("\\ ", " ");
    let candidate = unescaped.trim();
    let path = std::path::Path::new(candidate);
    if !path.is_absolute() {
        return None;
    }
    let media_type = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return None,
    };
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PATH_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let hash = rgba_fingerprint(0, 0, &bytes);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some((
        ImagePart {
            media_type: media_type.into(),
            data: b64,
        },
        hash,
    ))
}

#[cfg(test)]
mod image_path_tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::tempdir;

    /// Materialise a small file at `<dir>/<name>` whose contents are
    /// `bytes`. Returned absolute path is what the user-facing paste
    /// detector sees on iTerm2 Cmd+V or Finder drag.
    fn write_tmp_file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).expect("create tmp file");
        f.write_all(bytes).expect("write tmp file");
        p
    }

    /// PNG path → ImagePart with `image/png` media type. The single
    /// happy-path covering iTerm2's Cmd+V-of-image temp-file shape.
    #[test]
    fn png_path_attaches_as_image_png() {
        let dir = tempdir().unwrap();
        let p = write_tmp_file(&dir, "snap.png", b"\x89PNG\r\n\x1a\nstub-bytes");
        let res = try_attach_image_from_path(p.to_str().unwrap());
        let (img, _) = res.expect("PNG path must be recognised");
        assert_eq!(img.media_type, "image/png");
        assert!(!img.data.is_empty(), "base64 data must be populated");
    }

    /// JPG and JPEG both map to `image/jpeg` (case-insensitive ext).
    #[test]
    fn jpg_and_jpeg_map_to_image_jpeg() {
        let dir = tempdir().unwrap();
        for name in ["a.jpg", "b.JPEG", "c.Jpg"] {
            let p = write_tmp_file(&dir, name, b"\xff\xd8\xff\xe0\x00\x10JFIF stub");
            let (img, _) = try_attach_image_from_path(p.to_str().unwrap())
                .unwrap_or_else(|| panic!("expected attachment for {name}"));
            assert_eq!(
                img.media_type, "image/jpeg",
                "{name} must map to image/jpeg"
            );
        }
    }

    /// Quoted absolute path (Finder drag of paths-with-spaces) is
    /// recognised after a single layer of outer quotes is stripped.
    /// Both ASCII single and double quotes are accepted.
    #[test]
    fn quoted_absolute_path_is_recognised() {
        let dir = tempdir().unwrap();
        let p = write_tmp_file(&dir, "shot with space.png", b"stub");
        let path_str = p.to_str().unwrap();
        let single_quoted = format!("'{}'", path_str);
        let double_quoted = format!("\"{}\"", path_str);
        assert!(try_attach_image_from_path(&single_quoted).is_some());
        assert!(try_attach_image_from_path(&double_quoted).is_some());
    }

    /// Shell-escaped spaces (`\ `) are unescaped before fs lookup —
    /// matches the form some terminals emit on drag-and-drop.
    #[test]
    fn shell_escaped_space_is_unescaped() {
        let dir = tempdir().unwrap();
        let p = write_tmp_file(&dir, "shot with space.png", b"stub");
        let abs = p.to_str().unwrap();
        // Replace each space in the absolute path with `\<space>` to
        // simulate the drag-paste form.
        let escaped = abs.replace(' ', "\\ ");
        assert!(
            try_attach_image_from_path(&escaped).is_some(),
            "shell-escaped path must be unescaped before fs lookup"
        );
    }

    /// Trailing whitespace (iTerm2 often appends a space after the
    /// path) must not defeat detection.
    #[test]
    fn trailing_whitespace_is_trimmed() {
        let dir = tempdir().unwrap();
        let p = write_tmp_file(&dir, "snap.png", b"stub");
        let with_trailing_ws = format!("{}   \t  ", p.to_str().unwrap());
        assert!(try_attach_image_from_path(&with_trailing_ws).is_some());
    }

    /// Same path pasted twice → same fingerprint, so the dedup check in
    /// `pending_image_hashes` works.
    #[test]
    fn same_path_yields_same_fingerprint() {
        let dir = tempdir().unwrap();
        let p = write_tmp_file(&dir, "snap.png", b"stub-bytes");
        let path_str = p.to_str().unwrap();
        let (_, h1) = try_attach_image_from_path(path_str).unwrap();
        let (_, h2) = try_attach_image_from_path(path_str).unwrap();
        assert_eq!(h1, h2, "deterministic hash for the same file");
    }

    /// Plain prose containing words must NOT be treated as a path.
    #[test]
    fn prose_paste_is_not_an_image() {
        assert!(try_attach_image_from_path("hello world").is_none());
        assert!(try_attach_image_from_path("see /tmp/notes for context").is_none());
        assert!(try_attach_image_from_path("").is_none());
        assert!(try_attach_image_from_path("   ").is_none());
    }

    /// Multi-line paste (real text content) is rejected — the path
    /// detector is a single-line gate.
    #[test]
    fn multi_line_paste_is_not_an_image() {
        let two_lines = "/tmp/snap.png\nsecond line";
        assert!(try_attach_image_from_path(two_lines).is_none());
    }

    /// Relative paths are ambiguous (could be intentional text). Must
    /// not be auto-attached — only absolute paths flip the switch.
    #[test]
    fn relative_path_is_not_attached() {
        assert!(try_attach_image_from_path("snap.png").is_none());
        assert!(try_attach_image_from_path("./snap.png").is_none());
        assert!(try_attach_image_from_path("../snap.png").is_none());
    }

    /// Non-image extensions are rejected even when the file exists.
    /// Defends against the user pasting an absolute path to a `.txt` /
    /// `.json` / etc. — that's clearly text-attachment intent, not
    /// image-attachment intent.
    #[test]
    fn non_image_extension_is_rejected() {
        let dir = tempdir().unwrap();
        let p = write_tmp_file(&dir, "notes.txt", b"hello");
        assert!(try_attach_image_from_path(p.to_str().unwrap()).is_none());
        let p2 = write_tmp_file(&dir, "data.json", b"{}");
        assert!(try_attach_image_from_path(p2.to_str().unwrap()).is_none());
    }

    /// Absolute path with image extension but no file on disk — the
    /// paste was just a literal-looking path string that happens to
    /// match the shape. Reject so we don't silently swallow the text.
    #[test]
    fn missing_file_is_rejected() {
        // Nonexistent path under a real tempdir prefix — guaranteed
        // unique and unwriteable in normal test layout.
        assert!(
            try_attach_image_from_path("/this/path/definitely/does/not/exist/snap.png").is_none()
        );
    }

    /// Files larger than `MAX_PATH_IMAGE_BYTES` are rejected. The cap
    /// is the looser of OpenAI / Anthropic's per-image limits — beyond
    /// it, server-side rejection is certain and round-tripping the
    /// payload wastes bandwidth.
    #[test]
    fn oversized_file_is_rejected() {
        let dir = tempdir().unwrap();
        let huge = vec![0u8; (MAX_PATH_IMAGE_BYTES + 1) as usize];
        let p = write_tmp_file(&dir, "huge.png", &huge);
        assert!(
            try_attach_image_from_path(p.to_str().unwrap()).is_none(),
            "files over MAX_PATH_IMAGE_BYTES must be rejected before read"
        );
    }
}

#[cfg(test)]
mod compute_input_attachments_tests {
    use super::compute_input_attachments;
    use crate::input::history::HistoryImageRef;
    use crate::state::UiState;

    fn recalled(n: usize) -> HistoryImageRef {
        HistoryImageRef {
            hash: "0".repeat(16),
            mt: "image/png".into(),
            n,
        }
    }

    #[test]
    fn fresh_paste_marker_emits_preview() {
        let mut s = UiState::default();
        s.pending_image_markers.push(3);
        let attachments = compute_input_attachments(&s, "look [Image #3] here");
        assert_eq!(attachments, vec![3]);
    }

    #[test]
    fn cache_recalled_marker_emits_preview() {
        let mut s = UiState::default();
        s.pending_recalled_attachments.push(recalled(7));
        let attachments = compute_input_attachments(&s, "[Image #7] from history");
        assert_eq!(attachments, vec![7]);
    }

    #[test]
    fn typed_marker_with_no_pending_emits_no_preview() {
        let s = UiState::default();
        let attachments = compute_input_attachments(&s, "I typed [Image #99] literally");
        assert!(
            attachments.is_empty(),
            "literal text must not surface a preview row"
        );
    }

    #[test]
    fn marker_deleted_from_buffer_disappears_from_preview() {
        let mut s = UiState::default();
        s.pending_image_markers.push(1);
        let with_marker = compute_input_attachments(&s, "see [Image #1]");
        assert_eq!(with_marker, vec![1]);
        let without_marker = compute_input_attachments(&s, "no marker now");
        assert!(
            without_marker.is_empty(),
            "removing marker text must drop preview row"
        );
    }

    #[test]
    fn duplicate_markers_dedup_to_first_occurrence() {
        let mut s = UiState::default();
        s.pending_image_markers.push(2);
        let attachments = compute_input_attachments(&s, "[Image #2] then [Image #2] again");
        assert_eq!(
            attachments,
            vec![2],
            "same marker referenced twice must surface a single preview row"
        );
    }

    #[test]
    fn preserves_first_occurrence_order_across_sources() {
        let mut s = UiState::default();
        s.pending_image_markers.push(5);
        s.pending_recalled_attachments.push(recalled(3));
        let attachments = compute_input_attachments(&s, "first [Image #5] then [Image #3]");
        assert_eq!(
            attachments,
            vec![5, 3],
            "preview rows follow buffer text order, not source order"
        );
    }
}

#[derive(Debug, Clone)]
pub struct McpReloadProgress {
    pub total: usize,
    pub done: usize,
    pub connected: usize,
    pub failed: usize,
    pub started_at: std::time::Instant,
}

/// Optional override for spawning agent runtimes. `None` ⇒ use
/// `runtime_factory.spawn_runtime` (the v1 engine). When set, in-TUI session
/// switches (`/session`, `/bg`, disk `/resume`) spawn through it instead — the
/// cli injects the engine-v2 bridge here. It receives the CURRENT config +
/// working dir (so it tracks `/model` / `/provider` / `/cd`) and returns the same
/// `(client, event_rx)` pair the factory does.
pub type RuntimeSpawnOverride = std::sync::Arc<
    dyn Fn(&Config, &std::path::Path) -> (AgentClient, mpsc::UnboundedReceiver<AgentEvent>)
        + Send
        + Sync,
>;

/// Bag of handles passed into the loop.
pub struct LoopCtx {
    pub config: Config,
    pub model_name: String,
    pub agent: AgentClient,
    /// Force-exit watchdog deadline. Armed by [`arm_shutdown_watchdog`] when the
    /// user genuinely asks to leave (`/quit`, `/exit`, a confirmed Ctrl+C). The
    /// normal exit is the graceful break (Idle + `cmd_tx` closed); this is the
    /// safety net for when a wedged teardown await never closes the channel, so
    /// the user is never trapped. `None` = no quit requested. The `/upgrade`
    /// restart path deliberately does NOT arm this — it needs the normal
    /// `ExitReason::UpgradeRestart` return to re-exec the new binary.
    pub shutdown_deadline: Option<std::time::Instant>,
    pub runtime_factory: AgentRuntimeFactory,
    /// Optional engine-v2 spawner; `None` ⇒ the v1 `runtime_factory`. See
    /// [`RuntimeSpawnOverride`].
    pub runtime_spawn_override: Option<RuntimeSpawnOverride>,
    pub bg_manager: bg_runtime::BgRuntimeManager,
    pub foreground_runtime_id: bg_runtime::RuntimeId,
    pub runtime_event_tx: mpsc::UnboundedSender<bg_runtime::RuntimeEvent>,
    pub runtime_event_rx: mpsc::UnboundedReceiver<bg_runtime::RuntimeEvent>,
    pub working_dir: PathBuf,
    pub previous_dir: Option<PathBuf>,
    /// Recently visited project directories, most recent first (max 5).
    /// Persisted to `~/.atomcode/recent_dirs.txt`. Drives the `/cd`
    /// picker when invoked with no argument and is updated whenever
    /// the working directory changes (via slash command or agent tool).
    pub recent_dirs: Vec<PathBuf>,
    pub history: History,
    pub input_rx: mpsc::UnboundedReceiver<InputEvent>,
    pub commands: CommandRegistry,
    pub session_manager: SessionManager,
    /// Session actively being accumulated. Updated on TurnComplete /
    /// TurnCancelled (both carry the latest `messages` slice), saved to
    /// disk via `session_manager` on the same events so `/resume` after
    /// a quit sees the conversation. Replaced wholesale when the user
    /// resumes another session via `/resume` + SessionPicker.
    pub current_session: atomcode_core::session::Session,
    /// Shared "new version available" hint. Populated by the detached
    /// version-check task spawned from `run()`; read by `build_status`
    /// on each redraw. `None` = no hint (either check still pending,
    /// network failed silently, or already up to date).
    pub update_hint: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Shared CodingPlan drift-monitor warning slot. Written by the
    /// detached check task (see `monitor::spawn_check`); read by
    /// `build_status` on each redraw. Takes precedence over `update_hint`
    /// so a drift warning isn't buried by an upgrade banner. Cleared
    /// when `/codingplan` persists a fresh config (re-sync resets the
    /// hint state).
    pub monitor_warning: std::sync::Arc<std::sync::Mutex<Option<monitor::CodingPlanWarning>>>,
    /// Hook execution failure hint for the status bar. Written by the
    /// `AgentEvent::HookWarningHint` handler; read by `build_status` on
    /// each redraw. Takes precedence over `usage_hint` so a broken hook
    /// is immediately visible. Cleared at the start of each new turn.
    pub hook_warning_hint: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Last time a monitor check was fired this session. Pre-turn
    /// triggers respect `monitor::CHECK_COOLDOWN` (15 min) against this
    /// timestamp; startup + `/model` switch bypass the cooldown.
    /// `None` = no check has run yet this session.
    pub monitor_last_check_at: Option<std::time::Instant>,
    /// CodingPlan token-usage snapshot. Populated by
    /// `usage_monitor::spawn_check` at startup and after each
    /// TurnComplete (30s cooldown). Read on every redraw to construct
    /// the right-aligned usage hint when usage_percent ≥ 80% and the
    /// current model is on a CodingPlan provider.
    pub usage_slot:
        std::sync::Arc<std::sync::Mutex<Option<atomcode_core::coding_plan::types::UsageInfo>>>,
    /// Last time `usage_monitor::spawn_check` was invoked. Used to
    /// enforce `usage_monitor::USAGE_COOLDOWN` on TurnComplete-triggered
    /// refreshes. `None` = no check has run yet this session.
    pub usage_last_check_at: Option<std::time::Instant>,
    /// Last-observed timestamp from the shared CodingPlan sync marker
    /// (`~/.atomcode/codingplan_sync.json`). On every user input we
    /// re-read it; a change means ANOTHER atomcode process (e.g. a
    /// second terminal) just ran `/codingplan` and the server is now
    /// in sync with the on-disk config. We then hot-reload config
    /// from disk + clear the stale drift warning. Without this,
    /// Terminal A's "CodingPlan 模型列表更新" hint would stick forever
    /// after Terminal B ran the fix.
    pub monitor_last_sync_seen: Option<std::time::SystemTime>,
    /// Wake signal from background tasks (version check + CodingPlan
    /// drift monitor). One `()` sent when any task needs the event loop
    /// to repaint so a freshly-computed hint/warning appears without
    /// waiting for the user's next keystroke. Bounded at 1 — overlapping
    /// wakes coalesce since the redraw is idempotent.
    pub wake_rx: mpsc::Receiver<()>,
    /// Sender side of `wake_rx`. Cloned into every spawned check task
    /// so `/model` switches, pre-turn triggers, and the like can wake
    /// the event loop after updating `monitor_warning`.
    pub wake_tx: mpsc::Sender<()>,
    /// Receiver for `OauthEvent`s emitted by the QR-fast-path onboarding
    /// poll thread (see `event_loop::oauth_poll`). One event arrives
    /// per spawned poll task (Authorized or Failed). The `tokio::select!`
    /// arm that reads this channel closes the wizard modal + flips
    /// `pending_run_login_setup` on Authorized, or surfaces the failure
    /// reason in scrollback on Failed.
    pub oauth_event_rx: mpsc::UnboundedReceiver<oauth_poll::OauthEvent>,
    /// Sender cloned into each spawned poll task.
    pub oauth_event_tx: mpsc::UnboundedSender<oauth_poll::OauthEvent>,
    /// Control handle for the crossterm reader thread — `Some` in raw-mode
    /// TTY sessions, `None` in pipe mode. Used by child-process handoffs
    /// (OAuth login, future `/shell`) to pause+resume event consumption
    /// so our reader doesn't race the child for stdin bytes.
    pub reader: Option<crate::input::reader::ReaderHandle>,
    /// Sender used by `/upgrade` to report streaming progress/failure
    /// events from the detached upgrade task. Cloned into the task at
    /// spawn time; kept here so the receiver in the loop outlives any
    /// number of upgrades (no reconstructing on each invocation).
    pub upgrade_tx: mpsc::UnboundedSender<atomcode_core::self_update::UpgradeEvent>,
    /// Consumed in the main `select!` so upgrade progress is rendered
    /// alongside agent events.
    pub upgrade_rx: mpsc::UnboundedReceiver<atomcode_core::self_update::UpgradeEvent>,
    /// Long-lived channel for /plugin marketplace add|update and /plugin
    /// install. Each invocation spawns a blocking task that does the git
    /// clone/pull and pushes a `PluginJobEvent` here when done. Mirrors the
    /// `upgrade_tx`/`rx` layout so the event loop only has to add a single
    /// `select!` arm. Unbounded — events are tiny terminal results.
    pub plugin_job_tx: mpsc::UnboundedSender<atomcode_core::plugin::PluginJobEvent>,
    pub plugin_job_rx: mpsc::UnboundedReceiver<atomcode_core::plugin::PluginJobEvent>,
    /// Signal channel from the `/issue` wizard modal back to the event
    /// loop. The wizard's Enter handler can't touch `App` directly
    /// (modals only see `LoopCtx`), so it stores the collected title +
    /// body here, returns `Close`, and the event loop's post-close
    /// branch POSTs the issue to AtomGit and echoes the URL of the
    /// newly-created issue back into the conversation.
    pub pending_new_issue: Option<NewIssueDraft>,
    /// Set by `OnboardingWizard` (step 3, Setup) when the user picks
    /// option 0 (Set up CodingPlan). The event loop drains this on
    /// modal close and runs the full `/login` flow (OAuth if needed →
    /// claim → fetch models → register providers). Needs raw-mode
    /// suspend/resume, something modals can't drive themselves. Same
    /// pattern as `pending_new_issue`.
    pub pending_run_login_setup: bool,
    /// Set by `OnboardingWizard` (step 3, Setup) when the user picks
    /// option 1 (Configure manually). The event loop drains this on
    /// modal close and swaps in `ProviderWizard::MainMenu` — a
    /// Modal-to-Modal transition that needs mutable `active_modal`
    /// access only the event loop has.
    pub pending_open_provider_wizard: bool,
    /// MCP server registry for `/mcp` status display. `None` when no MCP
    /// servers are configured or all failed to connect.
    pub mcp_registry: Option<std::sync::Arc<atomcode_core::mcp::McpRegistry>>,
    /// Channel for receiving MCP connection status events (Connected/Failed).
    /// Events are rendered into scrollback as they arrive during startup.
    pub mcp_connect_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<atomcode_core::mcp::McpConnectEvent>>,
    /// When `/mcp reload` is invoked, we track progress until every configured
    /// server reports Connected/Failed, then emit a one-line summary.
    pub mcp_reload: Option<McpReloadProgress>,
    /// Channel for receiving LSP connection status events (Started / Failed
    /// / Warning). Same plumbing as `mcp_connect_rx` — wired in TUI mode
    /// so the manager's start failures land in scrollback as `✗ LSP server
    /// 'rust-analyzer' for .rs failed: ...` instead of leaking to stderr
    /// and printing inside the input box.
    pub lsp_connect_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<atomcode_core::lsp::LspConnectEvent>>,
    /// Telemetry handle — used to emit `UseCommand` at each slash dispatch.
    pub telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
    /// Original working dir before `/worktree create`, for `/worktree done`.
    pub worktree_original_dir: Option<PathBuf>,
    /// User-defined custom commands loaded from `~/.atomcode/commands/` and
    /// `<project>/.atomcode/commands/`. Queried by the slash-command
    /// dispatcher as a fallback when the entered name doesn't match a
    /// built-in command.
    pub custom_commands: crate::custom_commands::CustomCommandRegistry,
    /// Loaded skills (`.claude/skills/*/SKILL.md`, etc.). Same `Arc`
    /// the agent loop holds, so `reload(...)` there is visible here
    /// without extra plumbing. Used by the slash-command palette to
    /// surface user-invocable skills, and by the dispatcher to expand
    /// `/skill_name [args]` into a SendMessage.
    pub skill_registry: std::sync::Arc<std::sync::RwLock<atomcode_core::skill::SkillRegistry>>,
    /// Snapshot of the terminal's rendering capabilities. Probed once at
    /// startup in `lib.rs`; threaded into `App::new` so `UiState` knows
    /// whether to use Unicode or ASCII fallbacks for the spinner glyph
    /// and ellipsis. Same value as `RetainedRenderer` was constructed
    /// with — single source of truth.
    pub caps: crate::terminal::TerminalCaps,
    /// Session loaded by the CLI auto-continue path (`atomcode -c` /
    /// `--continue`). Replayed into scrollback AND restored into the
    /// agent's model context via `AgentCommand::SetConversation` on first
    /// `run_loop` entry, then dropped — matching `/resume` behaviour.
    pub replay_on_start: Option<atomcode_core::session::Session>,
    /// Lazy file/dir index for `@`-mention popup. Built on first `@`
    /// keystroke via `FileIndex::filter`; session-life cache.
    pub file_index: file_index::FileIndex,
    /// Active session id once `/resume` has loaded one. Required by the
    /// `/rename` slash command to know which session file to update.
    pub current_session_id: Option<SessionId>,
    /// Cached "clipboard currently holds an image" flag, with a short TTL
    /// so the right-aligned `Image in clipboard · ctrl+v to paste` hint
    /// stays current without thrashing the system clipboard on every
    /// redraw. Refreshed lazily inside `build_status`.
    pub clipboard_check: std::sync::Arc<std::sync::Mutex<ClipboardCheckState>>,
    /// 同步模式：Some 时输入投 LiveSession、渲染来自 live_sync 转发任务。None=独立（默认）。
    pub sync_session: Option<std::sync::Arc<atomcode_core::live::LiveSession>>,
    /// live 转发任务句柄（分离同步时 abort）。
    pub sync_forwarder: Option<tokio::task::JoinHandle<()>>,
    /// `/app` 拉起的 relay-client 子进程。`kill_on_drop(true)`，所以 TUI 退出或
    /// `/app stop` 时随之清理，不留僵尸进程。None=未开启 App 远程访问。
    pub app_relay_child: Option<tokio::process::Child>,
    /// `true` when the TUI was launched with `PlainRenderer` (CI / pipe
    /// / non-TTY). The onboarding wizard checks this — plain mode can't
    /// run interactive multi-step flows, so first-run falls through to
    /// the existing "no provider configured" status hint.
    pub is_plain_renderer: bool,
    /// When true, the --dangerously-skip-permissions flag was passed.
    /// Shown as a red "⚠ BYPASS" badge in the status line so the
    /// user is always aware that all tool calls are auto-approved.
    pub dangerously_skip_permissions: bool,
    /// When true, AtomCode is running with administrator/root privileges.
    /// A warning banner is shown in scrollback on startup.
    pub is_admin: bool,
    /// When `/guide <topic>` triggers auto-install of the "ask" skill,
    /// the topic is stashed here so `handle_plugin_job_event` can
    /// auto-invoke the skill once installation completes.
    pub pending_guide_topic: Option<String>,
    /// Current reasoning_effort for the active provider's model.
    /// None = not set (API uses its own default). Cycled via Ctrl+T.
    pub reasoning_effort: Option<String>,
    /// Transient status-line hint with auto-dismiss.
    pub transient_hint: std::sync::Arc<std::sync::Mutex<Option<TransientHint>>>,
    /// Receiver for password-prompt requests forwarded by the askpass server.
    /// `Some` when the TUI is running an askpass-capable session (Task 10 wires
    /// the real value); `None` means the arm is inert. Unix-only — sudo/SSH
    /// askpass is not supported on Windows.
    #[cfg(unix)]
    pub askpass_rx: Option<tokio::sync::mpsc::Receiver<atomcode_askpass::server::AskpassPrompt>>,
}

/// A transient hint shown on the status line, with auto-dismiss deadline.
#[derive(Debug, Clone)]
pub struct TransientHint {
    pub text: String,
    pub deadline: std::time::Instant,
}

/// Memoised result of the most recent clipboard probe. The hash is a
/// content fingerprint of the clipboard image's raw RGBA bytes (or
/// `None` when the clipboard holds no image). Letting `build_status`
/// compare this against `UiState::pending_image_hashes` is what powers
/// the "hide hint after I already pasted THIS image, but show it again
/// if the user copies a different one" UX.
#[derive(Debug, Default)]
pub struct ClipboardCheckState {
    pub image_hash: Option<u64>,
    pub last_checked: Option<std::time::Instant>,
}

/// Cheap content fingerprint for clipboard images. Hashes width, height,
/// total byte length, plus the first and last 1KB of RGBA bytes — enough
/// to distinguish typical screenshots while keeping the per-poll cost
/// O(2KB) regardless of image dimensions (a 4K screenshot's 32MB raw
/// buffer would be too slow to hash in full at 1.5s polling cadence).
fn rgba_fingerprint(width: usize, height: usize, bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    bytes.len().hash(&mut hasher);
    let head_end = bytes.len().min(1024);
    bytes[..head_end].hash(&mut hasher);
    let tail_start = bytes.len().saturating_sub(1024);
    bytes[tail_start..].hash(&mut hasher);
    hasher.finish()
}

/// What the `/issue` wizard hands back to the event loop after the user
/// finishes step 2. The event loop turns this into a `POST /repos/.../issues`
/// API call and echoes the resulting issue URL into scrollback.
#[derive(Debug, Clone)]
pub struct NewIssueDraft {
    pub owner: String,
    pub repo: String,
    pub title: String,
    pub body: String,
}

/// Line-edit buffer for input composition. Byte-indexed cursor.
///
/// Large pasted blocks are folded into `[Pasted #N +M lines]` placeholders
/// stored in `text`; the original contents live in `pastes` and are
/// spliced back in when the line is submitted. This keeps the visible
/// input short (matching CC's paste UX) without truncating what the
/// agent actually sees.
pub struct Buffer {
    pub text: String,
    pub cursor: usize,
    history_idx: Option<usize>,
    /// One-shot: suppress the slash-command menu for text placed into the
    /// buffer programmatically (a cancelled message restored on Esc). Without
    /// this, restoring `/skills foo` would immediately re-pop the command
    /// list. Cleared on the next key in `apply`, so editing reopens the menu.
    menu_suppressed: bool,
    stash: String,
    /// Placeholder index → original pasted text. Index 0 = paste #1.
    pastes: Vec<String>,
    /// The draft's `pastes` registry, parked alongside `stash` when the
    /// user scrolls into history (Up). Restored together with `stash`
    /// when HistoryNext walks back past the newest entry, so a draft
    /// that contained a folded paste survives a round-trip through
    /// history navigation. Mirrors `stash` for the paste registry.
    stash_pastes: Vec<String>,
}

/// Minimum line count or char count for a paste to fold into a
/// placeholder. Smaller pastes are inserted inline — no point hiding
/// 3 lines behind a `[Pasted ...]` token.
const PASTE_FOLD_LINES: usize = 5;
const PASTE_FOLD_CHARS: usize = 400;

/// Fold `\r\n` and lone `\r` line endings to `\n`. Bracketed-paste
/// payloads from macOS Terminal / iTerm2 / Windows clipboard frequently
/// carry CR separators; leaving them in place makes `str::lines()` miss
/// line breaks and can confuse downstream JSON/prompt serialisation.
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

impl Buffer {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history_idx: None,
            menu_suppressed: false,
            stash: String::new(),
            pastes: Vec::new(),
            stash_pastes: Vec::new(),
        }
    }

    /// Whether the slash-command menu should stay closed for the current
    /// buffer contents (set by `set_restored_text`, cleared on the next key).
    pub fn menu_suppressed(&self) -> bool {
        self.menu_suppressed
    }

    /// Place `text` into the buffer programmatically (cancelled message
    /// restored on Esc): cursor at the end, and suppress the slash menu for
    /// one frame so a restored `/command` doesn't immediately re-pop the list.
    pub fn set_restored_text(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
        self.history_idx = None;
        self.menu_suppressed = true;
    }

    /// Restore a cancelled prompt for edit-and-resend, PRESERVING any draft the
    /// user typed while the turn was running. When the buffer already holds a
    /// non-blank draft, the cancelled prompt is prepended on its own line
    /// instead of clobbering it; an empty/whitespace buffer just gets the
    /// prompt (same as `set_restored_text`). Cursor lands at the end.
    pub fn restore_cancelled_text(&mut self, prompt: String) {
        let merged = if self.text.trim().is_empty() {
            prompt
        } else {
            format!("{}\n{}", prompt, self.text)
        };
        self.set_restored_text(merged);
    }

    /// True while the user is scrolling input history (Up/Down on an
    /// empty / non-empty buffer). The slash-command menu suppresses
    /// itself in this state so that recalling a previous `/session foo`
    /// from history doesn't immediately re-pop the menu and trap Up
    /// inside it. Cleared automatically by `Insert` / `Cancel` (typing
    /// or Esc) and by `HistoryNext` returning past the newest entry
    /// to the user's stashed draft.
    pub fn is_in_history(&self) -> bool {
        self.history_idx.is_some()
    }

    /// The index into history of the entry currently being displayed,
    /// or `None` if the buffer is showing the user's own draft. Used
    /// by `event_loop` to look up `HistoryEntry::images` after every
    /// `apply()` so `pending_recalled_attachments` mirrors what the
    /// buffer is showing.
    pub fn history_idx(&self) -> Option<usize> {
        self.history_idx
    }

    /// Insert a pasted block. Folds into a `[Pasted …]` placeholder if
    /// the block exceeds the fold threshold, keeping the visible input
    /// terse. Returns the placeholder that was inserted (or the raw
    /// text for small pastes) so callers can advance the cursor.
    ///
    /// Single-line long pastes (e.g. a 600-char URL) use a `{N} chars`
    /// summary — `+1 lines` would be misleading. Multi-line pastes use
    /// `+{M} lines` which is what people expect for code blocks / diffs.
    ///
    /// **Line-ending normalisation:** most terminals in bracketed paste
    /// mode emit `\r` (or `\r\n`) between lines rather than `\n`. Without
    /// normalising, a 20-line paste looks like one gigantic line to
    /// `str::lines()` (returning count 1), and downstream agents may
    /// mis-handle payloads that mix CR-only separators. We fold `\r\n`
    /// and lone `\r` to `\n` at ingress so both the placeholder summary
    /// and the expanded agent payload are in canonical form.
    pub fn insert_paste(&mut self, text: String) -> String {
        let text = normalize_newlines(&text);
        let line_count = text.lines().count().max(1);
        let char_count = text.chars().count();
        if line_count >= PASTE_FOLD_LINES || char_count >= PASTE_FOLD_CHARS {
            let id = self.pastes.len() + 1;
            let placeholder = if line_count <= 1 {
                format!("[Pasted #{} {} chars]", id, char_count)
            } else {
                format!("[Pasted #{} +{} lines]", id, line_count)
            };
            self.pastes.push(text);
            self.text.insert_str(self.cursor, &placeholder);
            self.cursor += placeholder.len();
            placeholder
        } else {
            let n = text.len();
            self.text.insert_str(self.cursor, &text);
            self.cursor += n;
            text
        }
    }

    /// Expand every `[Pasted #N +M lines]` token in `line` back to the
    /// original paste contents. Called at submit time — the agent gets
    /// the full pasted payload, while history/display keeps the compact
    /// form.
    fn expand_pastes(&self, line: &str) -> String {
        if self.pastes.is_empty() {
            return line.to_string();
        }
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(start) = rest.find("[Pasted #") {
            out.push_str(&rest[..start]);
            let tail = &rest[start..];
            if let Some(end) = tail.find(']') {
                // Parse id from "[Pasted #N +M lines]"
                let header = &tail[..=end];
                let id_part = header
                    .strip_prefix("[Pasted #")
                    .and_then(|s| s.split_whitespace().next());
                if let Some(id_str) = id_part {
                    if let Ok(id) = id_str.parse::<usize>() {
                        if id >= 1 && id <= self.pastes.len() {
                            out.push_str(&self.pastes[id - 1]);
                            rest = &tail[end + 1..];
                            continue;
                        }
                    }
                }
                // Malformed or out-of-range token — leave as-is.
                out.push_str(header);
                rest = &tail[end + 1..];
            } else {
                out.push_str(tail);
                rest = "";
                break;
            }
        }
        out.push_str(rest);
        out
    }

    fn clear_pastes(&mut self) {
        self.pastes.clear();
    }

    /// Current buffer text with every `[Pasted #N …]` placeholder expanded
    /// back to its original contents. Modals that consume `text` directly
    /// (instead of going through the Submit/Commit path, which expands at
    /// the event loop) use this so a folded paste is seen in full.
    pub fn expanded_text(&self) -> String {
        self.expand_pastes(&self.text)
    }

    pub(crate) fn apply(
        &mut self,
        action: Action,
        history: &[crate::input::history::HistoryEntry],
        commands: &CommandRegistry,
    ) -> BufferResult {
        // Any interaction lifts the one-shot menu suppression set by a
        // restore, so editing / navigating a restored `/command` reopens the
        // command list as usual.
        self.menu_suppressed = false;
        match action {
            Action::Insert(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.history_idx = None;
                BufferResult::Redraw
            }
            Action::Submit => {
                // Line continuation: a `\` immediately before the cursor
                // is consumed and replaced with `\n`. Lets users insert
                // newlines on terminals that swallow Shift/Ctrl/Alt+Enter
                // (notably WSL + Windows Terminal). Mirrors Claude Code's
                // behavior and matches the shell line-continuation
                // convention Linux users already know.
                if self.cursor > 0 && self.text.as_bytes()[self.cursor - 1] == b'\\' {
                    let bs = self.cursor - 1;
                    self.text.replace_range(bs..self.cursor, "\n");
                    self.cursor = bs + 1;
                    self.history_idx = None;
                    return BufferResult::Redraw;
                }
                let mut line = self.text.trim();
                // Strip leading shell prompt characters (❯, $, >, #, %, λ)
                // that users accidentally paste from terminal output.
                while let Some(rest) =
                    line.strip_prefix(|c: char| matches!(c, '❯' | '$' | '>' | '#' | '%' | 'λ'))
                {
                    line = rest.trim_start();
                }
                let line = line.to_string();
                if line.is_empty() {
                    return BufferResult::Redraw;
                }
                BufferResult::Commit(line)
            }
            Action::InsertNewline => {
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
                BufferResult::Redraw
            }
            Action::Cancel => {
                if self.text.is_empty() {
                    BufferResult::Exit
                } else {
                    self.text.clear();
                    self.cursor = 0;
                    self.history_idx = None;
                    self.pastes.clear();
                    BufferResult::Redraw
                }
            }
            Action::ClearLine => {
                self.text.clear();
                self.cursor = 0;
                self.pastes.clear();
                BufferResult::Redraw
            }
            Action::DeleteWordBackward => {
                let before = &self.text[..self.cursor];
                let trimmed = before.trim_end_matches(' ');
                let word_start = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                self.text.drain(word_start..self.cursor);
                self.cursor = word_start;
                BufferResult::Redraw
            }
            Action::DeleteToEnd => {
                let end = self.text[self.cursor..]
                    .find('\n')
                    .map(|i| self.cursor + i)
                    .unwrap_or(self.text.len());
                self.text.drain(self.cursor..end);
                BufferResult::Redraw
            }
            Action::Backspace => {
                if self.cursor > 0 {
                    let p = prev_boundary(&self.text, self.cursor);
                    self.text.drain(p..self.cursor);
                    self.cursor = p;
                }
                BufferResult::Redraw
            }
            Action::DeleteForward => {
                if self.cursor < self.text.len() {
                    let n = next_boundary(&self.text, self.cursor);
                    self.text.drain(self.cursor..n);
                }
                BufferResult::Redraw
            }
            Action::CursorLeft => {
                if self.cursor > 0 {
                    self.cursor = prev_boundary(&self.text, self.cursor);
                }
                BufferResult::Redraw
            }
            Action::CursorRight => {
                if self.cursor < self.text.len() {
                    self.cursor = next_boundary(&self.text, self.cursor);
                }
                BufferResult::Redraw
            }
            Action::LineStart => {
                let start = self.text[..self.cursor]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                self.cursor = start;
                BufferResult::Redraw
            }
            Action::LineEnd => {
                let end = self.text[self.cursor..]
                    .find('\n')
                    .map(|i| self.cursor + i)
                    .unwrap_or(self.text.len());
                self.cursor = end;
                BufferResult::Redraw
            }
            Action::HistoryPrev => {
                if history.is_empty() {
                    return BufferResult::Redraw;
                }
                // The current buffer (including any newlines) is stashed
                // before we replace it with a history entry, so users
                // who pressed Up mid-multi-line-compose can recover it
                // via HistoryNext (Down). No need to block the action.
                let new_idx = match self.history_idx {
                    None => {
                        self.stash = self.text.clone();
                        // Park the draft's paste registry too, so a draft
                        // containing a folded paste survives Up→Down.
                        self.stash_pastes = self.pastes.clone();
                        Some(history.len() - 1)
                    }
                    Some(i) if i > 0 => Some(i - 1),
                    Some(i) => Some(i),
                };
                self.history_idx = new_idx;
                if let Some(i) = new_idx {
                    self.text = history[i].text.clone();
                    // Rehydrate the paste registry from the recalled entry
                    // so its `[Pasted #N …]` placeholders expand back to the
                    // original bodies on submit (issue #843). The live
                    // registry was cleared after the prior submit, so without
                    // this the agent would receive the literal placeholder.
                    self.pastes = history[i].pastes.clone();
                    // Park cursor at column 0 — recalled history is for
                    // re-running, not editing in place. A `/session foo`
                    // pulled from history would otherwise leave the
                    // cursor at end and re-trigger the slash menu via
                    // `is_in_history()`-gated logic; keeping it at 0
                    // mirrors Claude Code's behaviour and feels
                    // consistent with "this is recalled text, scroll
                    // again to keep going".
                    self.cursor = 0;
                }
                BufferResult::Redraw
            }
            Action::HistoryNext => {
                if let Some(i) = self.history_idx {
                    if i + 1 < history.len() {
                        // Still inside history — same cursor-at-0 rule
                        // as HistoryPrev.
                        self.history_idx = Some(i + 1);
                        self.text = history[i + 1].text.clone();
                        // Rehydrate pastes for the newly shown entry, same
                        // as HistoryPrev (issue #843).
                        self.pastes = history[i + 1].pastes.clone();
                        self.cursor = 0;
                    } else {
                        // Past the newest entry — restore the user's
                        // stashed draft. Cursor goes to end so they
                        // can keep typing where they left off before
                        // they started scrolling.
                        self.history_idx = None;
                        self.text = self.stash.clone();
                        self.pastes = self.stash_pastes.clone();
                        self.cursor = self.text.len();
                    }
                }
                BufferResult::Redraw
            }
            Action::Complete => {
                if self.text.starts_with('/') {
                    let prefix = &self.text[1..];
                    let matches = commands.matching_prefix(prefix);
                    if matches.len() == 1 {
                        self.text = format!("/{} ", matches[0].name);
                        self.cursor = self.text.len();
                    }
                    // Could also show a list for multiple matches; omit for v1.
                }
                BufferResult::Redraw
            }
            Action::NoOp => BufferResult::NoOp,
            Action::ToggleToolOutput => BufferResult::NoOp,
        }
    }

    /// Try to move the cursor up one logical line, preserving the
    /// column (measured in display cells so CJK lines up). Returns
    /// `false` only when the cursor is already at byte 0 — caller
    /// can then fall through to history navigation. Designed for the
    /// `Up` keystroke in multi-line composition: pressing Up walks
    /// the cursor through the buffer's lines first, then snaps to
    /// the start of the first line, and only the next Up after that
    /// surfaces history. Costs one extra keystroke before history
    /// kicks in but rescues anyone who paged Up to fix a typo on
    /// line 1 from losing their draft.
    pub fn cursor_line_up(&mut self) -> bool {
        let cur_line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if cur_line_start == 0 {
            // Already on the first line. Snap to byte 0 first; only
            // the next Up after that falls through to HistoryPrev.
            if self.cursor > 0 {
                self.cursor = 0;
                return true;
            }
            return false;
        }
        let target_col = crate::width::display_width(&self.text[cur_line_start..self.cursor]);
        let prev_line_end = cur_line_start - 1;
        let prev_line_start = self.text[..prev_line_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = prev_line_start
            + byte_offset_at_col(&self.text[prev_line_start..prev_line_end], target_col);
        true
    }

    /// Mirror of [`cursor_line_up`] for `Down`. Returns `false` only
    /// when the cursor is already at `text.len()` — caller then
    /// falls through to HistoryNext. On the last logical line, Down
    /// first snaps to end-of-buffer; the keystroke after that hands
    /// off to history.
    pub fn cursor_line_down(&mut self) -> bool {
        let Some(rel_end) = self.text[self.cursor..].find('\n') else {
            if self.cursor < self.text.len() {
                self.cursor = self.text.len();
                return true;
            }
            return false;
        };
        let cur_line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let target_col = crate::width::display_width(&self.text[cur_line_start..self.cursor]);
        let next_line_start = self.cursor + rel_end + 1;
        let next_line_end = self.text[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(self.text.len());
        self.cursor = next_line_start
            + byte_offset_at_col(&self.text[next_line_start..next_line_end], target_col);
        true
    }
}

/// Find the byte offset within `line` at the first character whose
/// cumulative display width meets or exceeds `target_col`. If the line
/// is shorter than `target_col` cells, returns `line.len()` — the
/// caller clamps the cursor to the end of that shorter line.
fn byte_offset_at_col(line: &str, target_col: usize) -> usize {
    let mut acc = 0usize;
    for (i, ch) in line.char_indices() {
        if acc >= target_col {
            return i;
        }
        acc += crate::width::cell_char_width(ch).unwrap_or(0);
    }
    line.len()
}

#[cfg(test)]
mod buffer_tests {
    use super::*;

    #[test]
    fn second_esc_within_window_triggers_undo() {
        let first = std::time::Instant::now();
        let second = first + DOUBLE_ESC_UNDO_WINDOW;
        assert!(second_esc_triggers_undo(Some(first), second));
    }

    #[test]
    fn second_esc_after_window_does_not_trigger_undo() {
        let first = std::time::Instant::now();
        let second = first + DOUBLE_ESC_UNDO_WINDOW + Duration::from_millis(1);
        assert!(!second_esc_triggers_undo(Some(first), second));
        assert!(!second_esc_triggers_undo(None, second));
    }

    #[test]
    fn first_empty_bare_esc_is_consumed_and_arms_undo() {
        let now = std::time::Instant::now();
        let mut pending = None;

        assert_eq!(
            intercept_empty_bare_esc(&mut pending, now),
            EmptyEscIntercept::Consumed
        );
        assert_eq!(pending, Some(now));
    }

    #[test]
    fn second_empty_bare_esc_triggers_undo_and_clears_pending() {
        let first = std::time::Instant::now();
        let second = first + DOUBLE_ESC_UNDO_WINDOW;
        let mut pending = Some(first);

        assert_eq!(
            intercept_empty_bare_esc(&mut pending, second),
            EmptyEscIntercept::TriggerUndo
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn spinner_label_never_shows_stall_hint() {
        // The "· esc to cancel" stall hint was removed by request: even a model
        // stream silent well past the threshold must NOT advertise it in the
        // footer (esc still cancels — it's just not shown).
        let mut s = UiState::new();
        s.on_submit(); // phase=Streaming, spinner = a thinking label
        s.last_stream_activity =
            Some(std::time::Instant::now() - crate::state::STREAM_STALL_HINT * 2);
        let hint = crate::i18n::t(crate::i18n::Msg::StreamStalled);
        let stalled = format_spinner_label(&s, 0, None);
        assert!(
            !stalled.contains(&*hint),
            "stall hint must never appear in the spinner, got {stalled:?}"
        );
    }

    #[test]
    fn spinner_label_maps_tool_names_to_thinking_word() {
        // Tool-execution labels must not flash tool names in the footer spinner;
        // they map back to the turn's thinking word. The body ▸ rows carry the
        // tool detail instead.
        let mut s = UiState::new();
        s.on_submit();
        let thinking = format_spinner_label(&s, 0, None);

        s.on_tool_call_streaming("ReadFile");
        let preparing = format_spinner_label(&s, 0, None);
        assert!(
            !preparing.contains("ReadFile") && !preparing.contains("Preparing"),
            "streaming spinner leaked a tool name: {preparing:?}"
        );

        s.on_tool_call_started("Bash");
        let running = format_spinner_label(&s, 0, None);
        assert!(
            !running.contains("Bash") && !running.contains("Running"),
            "running spinner leaked a tool name: {running:?}"
        );
        // …and it's the same thinking word the turn started with (minus the
        // ticking elapsed suffix).
        let word = |s: &str| s.split('…').next().unwrap_or("").to_string();
        assert_eq!(word(&running), word(&thinking));
    }

    #[test]
    fn small_paste_inserts_inline() {
        let mut b = Buffer::new();
        b.insert_paste("hi\n".to_string());
        assert_eq!(b.text, "hi\n");
        assert!(b.pastes.is_empty(), "small paste should not fold");
    }

    #[test]
    fn large_paste_folds_into_placeholder() {
        let mut b = Buffer::new();
        let big = "line\n".repeat(10);
        b.insert_paste(big.clone());
        assert!(b.text.contains("[Pasted #1 +10 lines]"));
        assert_eq!(b.pastes, vec![big]);
    }

    #[test]
    fn expand_pastes_restores_original() {
        let mut b = Buffer::new();
        let big = "line\n".repeat(10);
        b.insert_paste(big.clone());
        let committed = b.text.clone();
        let expanded = b.expand_pastes(&committed);
        assert_eq!(expanded, big);
    }

    #[test]
    fn expand_pastes_is_noop_without_placeholders() {
        let b = Buffer::new();
        assert_eq!(b.expand_pastes("plain text"), "plain text");
    }

    /// The conversation scrollback echoes the SAME expanded text the agent
    /// receives (`expand_pastes` output) — the full pasted body, never the
    /// `[Pasted #N …]` placeholder — while the input box and Up-arrow history
    /// keep the folded form. Regression guard for "对话框里还显示
    /// [Pasted #1 +N lines]"; documents the asymmetry both submit-echo sites
    /// rely on (they render `expanded.clone()`, history stores folded `line`).
    #[test]
    fn conversation_echo_uses_expanded_payload_not_placeholder() {
        let mut b = Buffer::new();
        let body = "line of pasted code\n".repeat(11);
        b.insert_paste(body.clone());

        // Input box + Up-arrow history: folded placeholder.
        let folded = b.text.clone();
        assert!(
            folded.contains("[Pasted #1 +11 lines]"),
            "input/history must keep the compact placeholder: {folded:?}"
        );

        // Agent payload AND conversation echo: fully expanded (no placeholder).
        let expanded = b.expand_pastes(&folded);
        assert!(expanded.contains(&body), "echo/payload must carry the full body");
        assert!(
            !expanded.contains("[Pasted #"),
            "no placeholder may survive in the echo/payload: {expanded:?}"
        );
        assert_ne!(
            expanded, folded,
            "echo (expanded) and input/history (folded) must differ for a folded paste"
        );
    }

    #[test]
    fn slash_command_arg_expands_folded_paste() {
        // Regression: `/goal <pasted body>` must hand the command the
        // real pasted text, not the literal `[Pasted #N …]` placeholder.
        // The submit path now expands the slash arg before dispatch; this
        // mirrors that expansion on the `arg` slice of the committed line.
        let mut b = Buffer::new();
        let body = "do the thing\n".repeat(69);
        b.insert_paste(body.clone());
        // Buffer now looks like the user typed `/goal ` then pasted.
        let line = format!("/goal {}", b.text);
        let (cmd, arg) = parse_slash_line(&line).expect("recognised as slash line");
        assert_eq!(cmd, "goal");
        assert!(arg.contains("[Pasted #1"), "arg still folded pre-expansion: {arg:?}");
        let expanded = b.expand_pastes(arg);
        assert_eq!(expanded, body, "slash arg must expand to the pasted body");
        assert!(!expanded.contains("[Pasted #"), "no placeholder should survive");
    }

    #[test]
    fn expanded_text_recovers_folded_paste() {
        // Modals (e.g. the provider Template step) read `expanded_text()`
        // instead of `text`, so a folded multi-line paste is seen in full
        // rather than as the literal `[Pasted #N +M lines]` placeholder.
        let mut b = Buffer::new();
        let body = "line\n".repeat(30);
        b.insert_paste(body.clone());
        assert!(b.text.contains("[Pasted #"), "should fold to placeholder");
        assert!(!b.expanded_text().contains("[Pasted #"));
        assert_eq!(b.expanded_text(), body);
    }

    #[test]
    fn paste_with_cr_separators_folds_correctly() {
        // Bracketed-paste often uses \r between lines (esp. macOS
        // Terminal.app). Without normalising, str::lines() sees one
        // gigantic line and the placeholder misreports "+1 lines".
        let mut b = Buffer::new();
        let cr_paste: String = (1..=20).map(|i| format!("line{}\r", i)).collect();
        b.insert_paste(cr_paste.clone());
        assert!(
            b.text.contains("+20 lines"),
            "expected 20-line placeholder, got: {}",
            b.text
        );
        // Original stored in pastes[0] is normalised (no \r).
        assert!(!b.pastes[0].contains('\r'));
        // Expanded body round-trips with \n separators.
        let expanded = b.expand_pastes(&b.text);
        assert_eq!(expanded.lines().count(), 20);
    }

    #[test]
    fn expand_handles_multiple_pastes_interleaved() {
        let mut b = Buffer::new();
        b.insert_paste("A\n".repeat(6));
        b.text.insert_str(b.cursor, " then ");
        b.cursor += 6;
        b.insert_paste("B\n".repeat(6));
        let line = b.text.clone();
        let out = b.expand_pastes(&line);
        assert!(out.contains("A\n"));
        assert!(out.contains(" then "));
        assert!(out.contains("B\n"));
        assert!(!out.contains("[Pasted"));
    }

    /// Regression: `clear_pastes` then `expand_pastes` is the broken
    /// ordering that shipped before — the agent received the bare
    /// `[Pasted #N +M lines]` placeholder instead of the pasted body
    /// and (correctly) responded "I don't see any pasted content".
    /// Callers MUST expand FIRST, clear SECOND. This test pins that
    /// contract: if someone reintroduces an early clear, the
    /// substitution silently turns into a no-op and the
    /// `contains("important data")` assertion below catches it.
    #[test]
    fn clear_before_expand_loses_paste_body() {
        let mut b = Buffer::new();
        let body = "important data\n".repeat(200);
        b.insert_paste(body.clone());
        let line = b.text.clone();
        // Mis-ordered: clear first.
        b.clear_pastes();
        let expanded = b.expand_pastes(&line);
        assert!(
            expanded.contains("[Pasted #1"),
            "early-clear must leave the placeholder unsubstituted: {}",
            expanded
        );
        assert!(
            !expanded.contains("important data"),
            "early-clear must NOT magically still have the body: {}",
            expanded
        );

        // Sanity check the correct order: expand first, then clear.
        let mut b = Buffer::new();
        b.insert_paste(body.clone());
        let line = b.text.clone();
        let expanded = b.expand_pastes(&line);
        b.clear_pastes();
        assert!(
            expanded.contains("important data"),
            "expand-before-clear must surface the body: {}",
            &expanded[..expanded.len().min(120)]
        );
        assert!(
            b.pastes.is_empty(),
            "clear after expand must still empty the registry"
        );
    }

    /// Regression (issue #843): a message containing a folded paste is
    /// submitted, then recalled via Up-arrow and re-sent. Before the fix
    /// the recalled buffer carried only the `[Pasted #N …]` placeholder
    /// — the live `pastes` registry had been cleared after the first
    /// submit — so `expand_pastes` was a no-op and the agent received the
    /// literal placeholder. The history entry now persists the paste
    /// bodies and `HistoryPrev` rehydrates `Buffer.pastes`, so expansion
    /// works on recall.
    #[test]
    fn recalled_paste_expands_after_history_prev() {
        use crate::input::history::HistoryEntry;
        let reg = CommandRegistry::builtin();

        // Simulate the first submit: paste folds, then the submit path
        // snapshots the live registry into the pushed history entry.
        let mut b = Buffer::new();
        let body = "important data\n".repeat(8);
        b.insert_paste(body.clone());
        let submitted_text = b.text.clone();
        assert!(submitted_text.contains("[Pasted #1"), "should fold to placeholder");
        let entry = HistoryEntry {
            text: submitted_text.clone(),
            images: vec![],
            pastes: b.pastes.clone(),
        };
        // Submit consumes the buffer + clears the live registry.
        b.text.clear();
        b.cursor = 0;
        b.clear_pastes();

        // A fresh turn: empty buffer, empty registry. User presses Up.
        let history = vec![entry];
        let _ = b.apply(Action::HistoryPrev, &history, &reg);
        assert_eq!(b.text, submitted_text, "Up recalls the folded text");
        assert!(!b.pastes.is_empty(), "registry must be rehydrated on recall");

        // Submitting now expands back to the real body, not the placeholder.
        let expanded = b.expand_pastes(&b.text);
        assert!(
            expanded.contains("important data"),
            "recalled paste must expand to its body: {}",
            &expanded[..expanded.len().min(120)]
        );
        assert!(!expanded.contains("[Pasted #"), "no placeholder may survive");
    }

    /// A folded paste in the draft must survive an Up→Down round-trip
    /// through history: pressing Down past the newest entry restores the
    /// stashed draft AND its paste registry, so expansion still works.
    #[test]
    fn draft_paste_survives_history_round_trip() {
        use crate::input::history::HistoryEntry;
        let reg = CommandRegistry::builtin();
        let history = vec![HistoryEntry {
            text: "earlier".into(),
            images: vec![],
            pastes: vec![],
        }];

        let mut b = Buffer::new();
        let body = "draft body\n".repeat(8);
        b.insert_paste(body.clone());
        let draft_text = b.text.clone();

        // Up into history (stashes draft + its pastes), then Down back out.
        let _ = b.apply(Action::HistoryPrev, &history, &reg);
        assert_eq!(b.text, "earlier");
        let _ = b.apply(Action::HistoryNext, &history, &reg);
        assert_eq!(b.text, draft_text, "draft text restored");

        let expanded = b.expand_pastes(&b.text);
        assert!(
            expanded.contains("draft body"),
            "draft paste must still expand after round-trip: {}",
            &expanded[..expanded.len().min(120)]
        );
    }

    #[test]
    fn submit_with_trailing_backslash_inserts_newline() {
        // WSL + Windows Terminal swallows Shift/Ctrl/Alt+Enter, so we
        // give users a `\<Enter>` continuation escape. The `\` itself
        // must not survive into the buffer.
        let reg = CommandRegistry::builtin();
        let history: Vec<crate::input::history::HistoryEntry> = Vec::new();
        let mut b = Buffer::new();
        b.text = "hello\\".to_string();
        b.cursor = b.text.len();
        let r = b.apply(Action::Submit, &history, &reg);
        assert!(matches!(r, BufferResult::Redraw));
        assert_eq!(b.text, "hello\n");
        assert_eq!(b.cursor, b.text.len());
    }

    #[test]
    fn set_restored_text_cursor_at_end_and_suppresses_menu() {
        let mut b = Buffer::new();
        b.set_restored_text("/provider".to_string());
        assert_eq!(b.text, "/provider");
        assert_eq!(
            b.cursor,
            "/provider".len(),
            "cursor at end for edit-and-resend"
        );
        assert!(
            b.menu_suppressed(),
            "restored /command must not pop the menu"
        );
    }

    #[test]
    fn restore_cancelled_text_prepends_before_existing_draft() {
        let mut b = Buffer::new();
        // User started typing a new message while the previous turn ran.
        b.text = "my new draft".to_string();
        b.cursor = b.text.len();
        // Then cancelled the previous request → its prompt comes back, but
        // the draft must be preserved (prompt prepended on its own line).
        b.restore_cancelled_text("original prompt".to_string());
        assert_eq!(b.text, "original prompt\nmy new draft");
        assert_eq!(b.cursor, b.text.len(), "cursor at end for edit-and-resend");
        assert!(b.menu_suppressed(), "restored /command must not pop the menu");
    }

    #[test]
    fn restore_cancelled_text_replaces_when_draft_empty() {
        let mut b = Buffer::new();
        // No draft typed → behaves exactly like the old restore (just the prompt).
        b.restore_cancelled_text("original prompt".to_string());
        assert_eq!(b.text, "original prompt");
        assert_eq!(b.cursor, "original prompt".len());
    }

    #[test]
    fn restore_cancelled_text_ignores_whitespace_only_draft() {
        let mut b = Buffer::new();
        b.text = "   \n".to_string();
        b.cursor = b.text.len();
        // A draft that's only whitespace isn't worth preserving — treat as empty.
        b.restore_cancelled_text("original prompt".to_string());
        assert_eq!(b.text, "original prompt");
    }

    #[test]
    fn restored_menu_suppression_lifts_on_next_key() {
        let reg = CommandRegistry::builtin();
        let history: Vec<crate::input::history::HistoryEntry> = Vec::new();
        let mut b = Buffer::new();
        b.set_restored_text("/provider".to_string());
        assert!(b.menu_suppressed());
        // Any interaction (here: a Backspace edit) lifts the suppression so
        // the command list reopens as the user edits.
        b.apply(Action::Backspace, &history, &reg);
        assert!(!b.menu_suppressed(), "editing reopens the menu");
    }

    #[test]
    fn submit_with_backslash_mid_buffer_inserts_newline_at_cursor() {
        let reg = CommandRegistry::builtin();
        let history: Vec<crate::input::history::HistoryEntry> = Vec::new();
        let mut b = Buffer::new();
        b.text = "abc\\def".to_string();
        b.cursor = 4; // right after the backslash
        let r = b.apply(Action::Submit, &history, &reg);
        assert!(matches!(r, BufferResult::Redraw));
        assert_eq!(b.text, "abc\ndef");
        assert_eq!(b.cursor, 4);
    }

    #[test]
    fn submit_without_trailing_backslash_commits_normally() {
        let reg = CommandRegistry::builtin();
        let history: Vec<crate::input::history::HistoryEntry> = Vec::new();
        let mut b = Buffer::new();
        b.text = "ship it".to_string();
        b.cursor = b.text.len();
        let r = b.apply(Action::Submit, &history, &reg);
        match r {
            BufferResult::Commit(s) => assert_eq!(s, "ship it"),
            _ => panic!("expected Commit"),
        }
    }

    #[test]
    fn submit_with_backslash_not_before_cursor_commits_normally() {
        // Backslash exists in the buffer but cursor isn't right after
        // it — Enter should still submit, not insert a newline.
        let reg = CommandRegistry::builtin();
        let history: Vec<crate::input::history::HistoryEntry> = Vec::new();
        let mut b = Buffer::new();
        b.text = "abc\\def".to_string();
        b.cursor = b.text.len(); // at end, byte before is 'f'
        let r = b.apply(Action::Submit, &history, &reg);
        match r {
            BufferResult::Commit(s) => assert_eq!(s, "abc\\def"),
            _ => panic!("expected Commit"),
        }
    }
}

#[cfg(test)]
mod menu_tests {
    use super::*;
    use crate::custom_commands::CustomCommandRegistry;

    #[test]
    fn non_slash_input_returns_no_menu() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        assert!(build_menu_items("hello world", 0, &reg, &custom, None, None).is_none());
    }

    #[test]
    fn slash_prefix_returns_all_commands() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let items =
            build_menu_items("/", 0, &reg, &custom, None, None).expect("menu should show for '/'");
        assert!(!items.is_empty(), "builtin registry should have commands");
    }

    #[test]
    fn slash_with_filter_narrows_list() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let all = build_menu_items("/", 0, &reg, &custom, None, None).unwrap();
        let filtered = build_menu_items("/he", 0, &reg, &custom, None, None).unwrap_or_default();
        assert!(
            filtered.len() < all.len(),
            "prefix '/he' should filter builtin commands"
        );
        // Every filtered entry must start with "he".
        for (name, _) in &filtered {
            assert!(
                name.starts_with("he"),
                "prefix filter leaked non-matching '{}'",
                name
            );
        }
    }

    #[test]
    fn whitespace_after_slash_closes_menu() {
        // Once the user types args, menu goes away so arrow keys don't
        // start navigating a stale palette.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        assert!(build_menu_items("/cd ", 0, &reg, &custom, None, None).is_none());
        assert!(build_menu_items("/cd /tmp", 0, &reg, &custom, None, None).is_none());
    }

    #[test]
    fn slash_with_no_matches_returns_none() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        assert!(build_menu_items("/zzznomatch", 0, &reg, &custom, None, None).is_none());
    }

    #[test]
    fn at_directory_completion_can_build_child_menu() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let index = file_index::FileIndex::from_entries(
            std::path::PathBuf::from("/tmp"),
            vec![
                file_index::Entry {
                    rel_path: "crates/".into(),
                    is_dir: true,
                    depth: 1,
                },
                file_index::Entry {
                    rel_path: "crates/atomcode-bridge/".into(),
                    is_dir: true,
                    depth: 2,
                },
                file_index::Entry {
                    rel_path: "crates/atomcode-bridge/src/".into(),
                    is_dir: true,
                    depth: 3,
                },
                file_index::Entry {
                    rel_path: "crates/atomcode-bridge/Cargo.toml".into(),
                    is_dir: false,
                    depth: 3,
                },
            ],
        );
        let replacement = file_index::format_at_mention_replacement("crates/atomcode-bridge/");

        let items = build_menu_items(
            &replacement,
            replacement.len(),
            &reg,
            &custom,
            None,
            Some(&index),
        )
        .expect("directory completion should immediately show child entries");

        assert_eq!(
            items.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(),
            vec![
                "crates/atomcode-bridge/src/",
                "crates/atomcode-bridge/Cargo.toml",
            ]
        );
    }

    fn skill_fixture(name: &str, desc: &str, user_invocable: bool) -> atomcode_core::skill::Skill {
        atomcode_core::skill::Skill {
            name: name.to_string(),
            description: desc.to_string(),
            template: "do thing".to_string(),
            disable_model_invocation: false,
            user_invocable,
            argument_hint: None,
            allowed_tools: vec![],
            skill_dir: std::path::PathBuf::new(),
            source_path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn top_level_hides_individual_skills() {
        // Regression for the two-level palette: typing /bra or any
        // bare-name prefix must NOT surface skills. They live behind
        // the `/skills` gateway so the top palette stays uncluttered.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        let lock = std::sync::RwLock::new(skills);

        // /bra — no skill should appear; /bra falls through to "no
        // matches" since no built-in starts with bra either.
        assert!(
            build_menu_items("/bra", 0, &reg, &custom, Some(&lock), None).is_none(),
            "individual skills must not leak into the top-level menu"
        );

        // /skills — only the built-in gateway entry, never the
        // individual skills.
        let items = build_menu_items("/skills", 0, &reg, &custom, Some(&lock), None)
            .expect("/skills must include the built-in gateway");
        assert!(items.iter().any(|(n, _)| n == "skills"));
        for (n, _) in &items {
            assert!(
                !n.contains(':'),
                "namespaced skill leaked into top-level: {}",
                n
            );
        }
    }

    #[test]
    fn skills_sub_mode_lists_skills_under_bare_names() {
        // Once the user has typed `/skills ` (trailing space, normally
        // injected by the needs_args path on Enter), the palette
        // switches to second-level: bare skill names, ready to commit
        // as `/skills <name>`.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        let lock = std::sync::RwLock::new(skills);

        let items = build_menu_items("/skills ", 0, &reg, &custom, Some(&lock), None)
            .expect("/skills (with space) must list skills");
        // Sub-mode lists BARE names (the dispatcher re-qualifies to `skills:<name>` on
        // submit) — matches build_skill_menu_items_lists_unique_bare_names + the documented design.
        assert!(items.iter().any(|(n, _)| n == "brainstorming"));
        assert!(items.iter().any(|(n, _)| n == "web-access"));
        for (n, _) in &items {
            assert!(!n.contains(':'), "sub-mode names are bare: {}", n);
        }
    }

    #[test]
    fn skills_sub_mode_filters_by_bare_prefix() {
        // /skills bra narrows to brainstorming. /skills web narrows
        // to web-access. /skills zz returns no menu at all.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        let lock = std::sync::RwLock::new(skills);

        let bra = build_menu_items("/skills bra", 0, &reg, &custom, Some(&lock), None)
            .expect("filter must produce a result");
        assert_eq!(bra.len(), 1);
        assert_eq!(bra[0].0, "brainstorming");

        let web = build_menu_items("/skills web", 0, &reg, &custom, Some(&lock), None)
            .expect("filter must produce a result");
        assert_eq!(web.len(), 1);
        assert_eq!(web[0].0, "web-access");

        assert!(build_menu_items("/skills zz", 0, &reg, &custom, Some(&lock), None).is_none());
    }

    #[test]
    fn skills_sub_mode_hides_after_skill_name() {
        // /skills brainstorming why X — user is typing skill args now,
        // menu should disappear so arrow keys don't navigate stale entries.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        let lock = std::sync::RwLock::new(skills);

        assert!(build_menu_items(
            "/skills brainstorming why",
            0,
            &reg,
            &custom,
            Some(&lock),
            None
        )
        .is_none());
    }

    #[test]
    fn skills_sub_mode_excludes_hidden_skills() {
        // user_invocable=false skills must not surface in the sub-menu
        // either — they're LLM-only via the use_skill tool.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:visible", "shown", true));
        skills.register(skill_fixture("skills:hidden", "hidden", false));
        let lock = std::sync::RwLock::new(skills);

        let items = build_menu_items("/skills ", 0, &reg, &custom, Some(&lock), None)
            .expect("at least one visible skill should produce a menu");
        assert!(items.iter().any(|(n, _)| n == "visible"));
        assert!(
            !items.iter().any(|(n, _)| n == "hidden"),
            "user_invocable=false skill leaked into sub-menu"
        );
    }

    #[test]
    fn effort_top_level_is_gateway_only() {
        // `/effort` at the top level surfaces the gateway entry, not the
        // individual high/max/off choices (those live behind `/effort `).
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let items = build_menu_items("/effort", 0, &reg, &custom, None, None)
            .expect("/effort gateway must appear");
        assert!(items.iter().any(|(n, _)| n == "effort"));
        assert!(!items
            .iter()
            .any(|(n, _)| n == "high" || n == "max" || n == "off"));
    }

    #[test]
    fn effort_sub_mode_lists_and_filters_choices() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        // `/effort ` (trailing space) → all three choices.
        let all = build_menu_items("/effort ", 0, &reg, &custom, None, None)
            .expect("/effort sub-mode must list choices");
        let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"high") && names.contains(&"max") && names.contains(&"off"),
            "got: {names:?}"
        );
        // Prefix narrows.
        let hi = build_menu_items("/effort hi", 0, &reg, &custom, None, None)
            .expect("`hi` must match high");
        assert_eq!(hi.len(), 1);
        assert_eq!(hi[0].0, "high");
        // No match → no menu.
        assert!(build_menu_items("/effort zz", 0, &reg, &custom, None, None).is_none());
        // A chosen value followed by a space (typing past) hides the menu.
        assert!(build_menu_items("/effort high ", 0, &reg, &custom, None, None).is_none());
    }

    #[test]
    fn no_skill_registry_is_no_op() {
        // Ensures the legacy call path (None) keeps working.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let with_none = build_menu_items("/", 0, &reg, &custom, None, None).unwrap();
        let empty_skills = std::sync::RwLock::new(atomcode_core::skill::SkillRegistry::new());
        let with_empty =
            build_menu_items("/", 0, &reg, &custom, Some(&empty_skills), None).unwrap();
        assert_eq!(
            with_none.len(),
            with_empty.len(),
            "empty registry must produce same menu as None"
        );
    }

    #[test]
    fn build_skill_menu_items_lists_unique_bare_names() {
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        skills.register(skill_fixture("skills:hidden", "no", false));
        let lock = std::sync::RwLock::new(skills);

        let all = build_skill_menu_items(Some(&lock), "");
        assert!(all.iter().any(|(n, _)| n == "brainstorming"));
        assert!(all.iter().any(|(n, _)| n == "web-access"));
        assert!(!all.iter().any(|(n, _)| n == "hidden"));

        let filtered = build_skill_menu_items(Some(&lock), "bra");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "brainstorming");

        assert!(build_skill_menu_items(Some(&lock), "zz").is_empty());
        assert!(build_skill_menu_items(None, "").is_empty());
    }

    #[test]
    fn dollar_trigger_lists_all_user_invocable_skills() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        skills.register(skill_fixture("skills:hidden", "no", false));
        let lock = std::sync::RwLock::new(skills);

        let items =
            build_menu_items("$", 0, &reg, &custom, Some(&lock), None).expect("$ must list skills");
        assert!(items.iter().any(|(n, _)| n == "brainstorming"));
        assert!(items.iter().any(|(n, _)| n == "web-access"));
        assert!(!items.iter().any(|(n, _)| n == "hidden"));
        for (n, _) in &items {
            assert!(!n.contains('/'), "row leaked slash syntax: {}", n);
        }
    }

    #[test]
    fn dollar_trigger_filters_and_parity_with_skills_submode() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        let lock = std::sync::RwLock::new(skills);

        let bra = build_menu_items("$bra", 0, &reg, &custom, Some(&lock), None)
            .expect("filter must match");
        assert_eq!(bra.len(), 1);
        assert_eq!(bra[0].0, "brainstorming");

        let via_dollar = build_menu_items("$web", 0, &reg, &custom, Some(&lock), None);
        let via_slash = build_menu_items("/skills web", 0, &reg, &custom, Some(&lock), None);
        assert_eq!(via_dollar, via_slash);

        assert!(build_menu_items("$zz", 0, &reg, &custom, Some(&lock), None).is_none());
    }

    #[test]
    fn dollar_trigger_only_at_start_and_closes_on_space() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        let lock = std::sync::RwLock::new(skills);

        assert!(build_menu_items("hi $bra", 0, &reg, &custom, Some(&lock), None).is_none());
        assert!(build_menu_items("$brainstorming ", 0, &reg, &custom, Some(&lock), None).is_none());
    }

    // Regression: HistoryPrev used to leave the cursor at end-of-text,
    // so a recalled `/session foo` from history would `is_in_history()`
    // true AND have the slash prefix — without the call-site gate, the
    // menu would auto-pop, trapping Up/Down inside it. The fix is twofold
    // (caller skips menu while in history; cursor parks at 0 to signal
    // "this is recalled, scroll again"). These two unit tests pin both.
    #[test]
    fn history_prev_parks_cursor_at_zero_and_marks_history_mode() {
        let mut buf = Buffer::new();
        let reg = CommandRegistry::builtin();
        let history = vec![crate::input::history::HistoryEntry { text: "/session foo".into(), images: vec![], pastes: vec![] }];

        let _ = buf.apply(Action::HistoryPrev, &history, &reg);

        assert_eq!(buf.text, "/session foo");
        assert_eq!(buf.cursor, 0, "cursor must park at 0 to suppress menu");
        assert!(buf.is_in_history(), "buffer must report history mode");
    }

    #[test]
    fn cursor_line_up_walks_lines_then_signals_history_at_top() {
        // "1\n2\n3" with cursor after the trailing "3". Up should
        // walk: end-of-3 → end-of-2 → end-of-1 → start-of-1 → false.
        // The start-of-1 snap is the rescue step: even on a single-
        // line draft, Up first parks at column 0 before history nav
        // kicks in, so a fat-fingered Up can't silently swallow what
        // the user just typed.
        let mut buf = Buffer::new();
        buf.text = "1\n2\n3".into();
        buf.cursor = buf.text.len();

        assert!(buf.cursor_line_up(), "first Up moves up from line 3");
        assert_eq!(&buf.text[..buf.cursor], "1\n2");
        assert!(buf.cursor_line_up(), "second Up moves up from line 2");
        assert_eq!(&buf.text[..buf.cursor], "1");
        assert!(
            buf.cursor_line_up(),
            "on the first line Up snaps to byte 0 before yielding"
        );
        assert_eq!(buf.cursor, 0);
        assert!(
            !buf.cursor_line_up(),
            "already at byte 0 → caller falls through to HistoryPrev"
        );
    }

    #[test]
    fn cursor_line_down_walks_lines_then_signals_history_at_bottom() {
        let mut buf = Buffer::new();
        buf.text = "1\n2\n3".into();
        buf.cursor = 0;

        assert!(buf.cursor_line_down(), "Down from line 1 → line 2");
        assert_eq!(&buf.text[..buf.cursor], "1\n");
        assert!(buf.cursor_line_down(), "Down from line 2 → line 3");
        assert_eq!(&buf.text[..buf.cursor], "1\n2\n");
        assert!(
            buf.cursor_line_down(),
            "on the last line Down snaps to end-of-buffer before yielding"
        );
        assert_eq!(buf.cursor, buf.text.len());
        assert!(
            !buf.cursor_line_down(),
            "already at end → caller falls through to HistoryNext"
        );
    }

    #[test]
    fn cursor_line_up_snaps_to_start_on_single_line() {
        // Single-line draft: Up should pull the cursor to column 0
        // first; only the next Up after that surfaces history.
        let mut buf = Buffer::new();
        buf.text = "hello world".into();
        buf.cursor = 7; // inside the word "world"

        assert!(buf.cursor_line_up(), "Up snaps to byte 0 on single line");
        assert_eq!(buf.cursor, 0);
        assert!(!buf.cursor_line_up(), "second Up yields to history");
    }

    #[test]
    fn cursor_line_down_snaps_to_end_on_single_line() {
        let mut buf = Buffer::new();
        buf.text = "hello world".into();
        buf.cursor = 4;

        assert!(buf.cursor_line_down(), "Down snaps to end on single line");
        assert_eq!(buf.cursor, buf.text.len());
        assert!(!buf.cursor_line_down(), "second Down yields to history");
    }

    #[test]
    fn cursor_line_up_clamps_to_shorter_line() {
        // Column-preservation: cursor at col 5 on line 2 ("hello"),
        // line 1 is only "ab" — Up clamps to end of "ab".
        let mut buf = Buffer::new();
        buf.text = "ab\nhello".into();
        buf.cursor = buf.text.len(); // after final 'o'

        assert!(buf.cursor_line_up());
        assert_eq!(buf.cursor, 2, "cursor clamps to end of shorter prev line");
    }

    #[test]
    fn cursor_line_up_handles_cjk_width() {
        // 你好 = 2 chars but 4 display cells. Target column on line
        // 2 lands inside line 1's CJK run — should pick a char
        // boundary (no panic) and preserve visual column.
        let mut buf = Buffer::new();
        buf.text = "你好world\nabcd".into();
        // Move cursor to end of line 2 (col 4 → display width 4 →
        // lands at "你好" exactly on line 1).
        buf.cursor = buf.text.len();
        assert!(buf.cursor_line_up());
        // "你好" is the first 6 bytes (3 bytes per CJK char in UTF-8).
        assert_eq!(buf.cursor, 6);
    }

    #[test]
    fn history_next_back_to_stash_restores_cursor_to_end() {
        let mut buf = Buffer::new();
        let reg = CommandRegistry::builtin();
        let history = vec![crate::input::history::HistoryEntry { text: "/session foo".into(), images: vec![], pastes: vec![] }];

        // Type a partial draft, then scroll into history and back out.
        let _ = buf.apply(Action::Insert('h'), &history, &reg);
        let _ = buf.apply(Action::Insert('i'), &history, &reg);
        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        assert!(buf.is_in_history());
        let _ = buf.apply(Action::HistoryNext, &history, &reg);

        // Past newest entry → restored stash with cursor at the end so
        // the user can keep typing where they left off.
        assert_eq!(buf.text, "hi");
        assert_eq!(buf.cursor, 2);
        assert!(!buf.is_in_history());
    }

    #[test]
    fn typing_clears_history_mode() {
        // Sanity check — Insert resets history_idx, so the menu can
        // re-appear naturally once the user starts editing the recall.
        let mut buf = Buffer::new();
        let reg = CommandRegistry::builtin();
        let history = vec![crate::input::history::HistoryEntry { text: "/session foo".into(), images: vec![], pastes: vec![] }];

        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        assert!(buf.is_in_history());
        let _ = buf.apply(Action::Insert('x'), &history, &reg);
        assert!(!buf.is_in_history());
    }

    #[test]
    fn sync_recalled_attachments_mirrors_buffer_history_idx() {
        use crate::input::history::{HistoryEntry, HistoryImageRef};
        let history: Vec<HistoryEntry> = vec![
            HistoryEntry { text: "no img".into(), images: vec![], pastes: vec![] },
            HistoryEntry {
                text: "with img".into(),
                images: vec![HistoryImageRef {
                    hash: "deadbeef12345678".into(),
                    mt: "image/png".into(),
                    n: 1,
                }],
                pastes: vec![],
            },
        ];
        let reg = CommandRegistry::builtin();
        let mut buf = Buffer::new();
        let mut state = UiState::new();
        // ↑ once → newest entry (idx=1, has image).
        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        super::sync_recalled_attachments(&mut state, &buf, &history);
        assert_eq!(state.pending_recalled_attachments.len(), 1);
        // ↑ again → idx=0 (no images) → wholesale replace empties the vec.
        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        super::sync_recalled_attachments(&mut state, &buf, &history);
        assert!(state.pending_recalled_attachments.is_empty());
        // Type a char on an empty-images entry → history_idx clears
        // but the retain pass keeps the (already empty) vec empty.
        let _ = buf.apply(Action::Insert('a'), &history, &reg);
        super::sync_recalled_attachments(&mut state, &buf, &history);
        assert!(state.pending_recalled_attachments.is_empty());
    }

    /// Regression: arrow-up recalls `[Image #1]这是什么？`, user appends
    /// ` 现在不清楚为啥...`, submits — the trailing edit must NOT drop
    /// the recalled image. Pre-fix, `Insert` cleared `history_idx` and
    /// the wholesale `clear()` wiped `pending_recalled_attachments`,
    /// so the marker text reached the model as literal `[Image #1]`
    /// without bytes. Post-fix, the retain pass keeps refs whose
    /// marker is still in `buf.text`.
    #[test]
    fn sync_recalled_attachments_retains_on_edit_when_marker_present() {
        use crate::input::history::{HistoryEntry, HistoryImageRef};
        let history: Vec<HistoryEntry> = vec![HistoryEntry {
            text: "[Image #1]hello".into(),
            images: vec![HistoryImageRef {
                hash: "deadbeef12345678".into(),
                mt: "image/png".into(),
                n: 1,
            }],
            pastes: vec![],
        }];
        let reg = CommandRegistry::builtin();
        let mut buf = Buffer::new();
        let mut state = UiState::new();
        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        super::sync_recalled_attachments(&mut state, &buf, &history);
        assert_eq!(state.pending_recalled_attachments.len(), 1);
        // Append a char — history_idx clears, but `[Image #1]` is still
        // in buf, so the recalled ref must survive.
        let _ = buf.apply(Action::Insert('!'), &history, &reg);
        super::sync_recalled_attachments(&mut state, &buf, &history);
        assert_eq!(
            state.pending_recalled_attachments.len(),
            1,
            "edit that leaves marker intact must preserve recalled ref"
        );
    }

    /// Companion to the retain-on-edit test: when the user backspaces
    /// over the `[Image #N]` marker itself, the recalled ref tied to
    /// that marker should drop — otherwise `hydrate_recalled_attachments`
    /// would inject orphan bytes the user explicitly removed.
    #[test]
    fn sync_recalled_attachments_drops_when_marker_removed() {
        use crate::input::history::{HistoryEntry, HistoryImageRef};
        let history: Vec<HistoryEntry> = vec![HistoryEntry {
            text: "[Image #1]hi".into(),
            images: vec![HistoryImageRef {
                hash: "deadbeef12345678".into(),
                mt: "image/png".into(),
                n: 1,
            }],
            pastes: vec![],
        }];
        let reg = CommandRegistry::builtin();
        let mut buf = Buffer::new();
        let mut state = UiState::new();
        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        super::sync_recalled_attachments(&mut state, &buf, &history);
        assert_eq!(state.pending_recalled_attachments.len(), 1);
        // Replace the buffer text so the marker is gone — simulates the
        // user backspacing over `[Image #1]`. We use a direct edit
        // through Action::Insert + delete is overkill; mutating the
        // buf's text via a fresh Buffer simulates the same end state.
        // Drop history_idx by inserting a char then verify retain
        // strips the ref since the marker is no longer present.
        // We force buf.text to a no-marker string by replaying from
        // empty + Insert sequence:
        let mut buf2 = Buffer::new();
        let _ = buf2.apply(Action::Insert('h'), &history, &reg);
        let _ = buf2.apply(Action::Insert('i'), &history, &reg);
        // pending_recalled_attachments still has the entry from earlier
        // (state isn't reset between buffer swaps in the real loop —
        // sync runs on each apply).
        super::sync_recalled_attachments(&mut state, &buf2, &history);
        assert!(
            state.pending_recalled_attachments.is_empty(),
            "removed marker must drop the matching recalled ref"
        );
    }

    #[test]
    fn cache_write_image_writes_and_is_idempotent() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let img = atomcode_core::conversation::message::ImagePart {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(b"hello"),
        };
        super::cache_write_image(dir.path(), &img, 0xdead_beef_1234_5678);
        let p = dir.path().join("deadbeef12345678.png");
        assert!(p.exists());
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        // Calling again must not error and not change the file mtime.
        let mtime1 = std::fs::metadata(&p).unwrap().modified().unwrap();
        super::cache_write_image(dir.path(), &img, 0xdead_beef_1234_5678);
        let mtime2 = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "second call must short-circuit on exists");
    }

    #[test]
    fn hydrate_renumbers_and_rewrites_line() {
        use crate::input::history::HistoryImageRef;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_path_buf();
        // Write a fake cache file at hash=deadbeef12345678.
        std::fs::write(cache_dir.join("deadbeef12345678.png"), b"\x89PNG").unwrap();

        let mut state = UiState::new();
        state.session_image_count = 5; // current session already used #1..#5
        state.pending_recalled_attachments.push(HistoryImageRef {
            hash: "deadbeef12345678".into(),
            mt: "image/png".into(),
            n: 2, // recalled marker number from the saved entry
        });
        let mut line = "look at [Image #2] please".to_string();
        let notice = super::hydrate_recalled_attachments(&mut state, &mut line, &cache_dir);

        assert_eq!(notice.len(), 0, "no cache miss expected");
        assert_eq!(line, "look at [Image #6] please", "marker renumbered to #6");
        assert_eq!(state.pending_images.len(), 1);
        assert_eq!(state.pending_image_markers, vec![6]);
        assert_eq!(state.session_image_count, 6);
        assert!(state.pending_recalled_attachments.is_empty());
    }

    #[test]
    fn hydrate_strips_marker_on_cache_miss() {
        use crate::input::history::HistoryImageRef;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_path_buf();
        // No cache file written → cache miss.
        let mut state = UiState::new();
        state.pending_recalled_attachments.push(HistoryImageRef {
            hash: "0000000000000000".into(),
            mt: "image/png".into(),
            n: 3,
        });
        let mut line = "before [Image #3] after".to_string();
        let notice = super::hydrate_recalled_attachments(&mut state, &mut line, &cache_dir);

        assert_eq!(line, "before  after", "marker stripped on cache miss");
        assert_eq!(state.pending_images.len(), 0);
        assert!(state.pending_recalled_attachments.is_empty());
        assert_eq!(notice.len(), 1, "expected one cache-miss notice");
        assert!(notice[0].contains("[Image #3]"));
        assert!(notice[0].contains("缓存"));
    }

    #[test]
    fn paste_submit_recall_submit_rehydrates_image() {
        use crate::input::history::{History, HistoryEntry, HistoryImageRef};
        use base64::Engine;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("image-cache");
        std::fs::create_dir(&cache_dir).unwrap();
        let hist_path = tmp.path().join("hist");
        let mut history = History::load_with_cache(&hist_path, cache_dir.clone());

        // ── Turn 1: paste image, submit ────────────────────────────────
        let raw_bytes = b"\x89PNG\r\n\x1a\nfake".to_vec();
        let img = atomcode_core::conversation::message::ImagePart {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(&raw_bytes),
        };
        let hash: u64 = 0xdead_beef_1234_5678;
        super::cache_write_image(&cache_dir, &img, hash);
        history.push(HistoryEntry {
            text: "describe [Image #1]".into(),
            images: vec![HistoryImageRef {
                hash: format!("{:016x}", hash),
                mt: img.media_type.clone(),
                n: 1,
            }],
            pastes: vec![],
        });
        history.save().unwrap();
        // GC must NOT delete our file (it's referenced).
        assert!(cache_dir.join(format!("{:016x}.png", hash)).exists());

        // ── Reload (new "session") ─────────────────────────────────────
        let history2 = History::load_with_cache(&hist_path, cache_dir.clone());
        assert_eq!(history2.entries().len(), 1);
        assert_eq!(history2.entries()[0].images.len(), 1);

        // ── Turn 2: simulate up-arrow + submit ─────────────────────────
        let mut state = UiState::new();
        // Up-arrow handler would do this:
        state.pending_recalled_attachments = history2.entries()[0].images.clone();
        let mut line = history2.entries()[0].text.clone();
        let notices = super::hydrate_recalled_attachments(&mut state, &mut line, &cache_dir);
        assert!(notices.is_empty(), "cache hit, no notice expected");
        assert_eq!(state.pending_images.len(), 1, "image rehydrated");
        let rehydrated = base64::engine::general_purpose::STANDARD
            .decode(&state.pending_images[0].data)
            .unwrap();
        assert_eq!(rehydrated, raw_bytes, "bytes round-trip exact");
        // Marker renumbered (recalled was #1, new session also starts at #1
        // but session_image_count was 0 → bumped to 1, so new marker = 1).
        assert_eq!(line, "describe [Image #1]");
        assert_eq!(state.pending_image_markers, vec![1]);
    }

    #[test]
    fn hydrate_runs_for_streaming_queued_submit_too() {
        // Regression: handle_streaming_key's Commit branch must also
        // hydrate `pending_recalled_attachments` so a user who pressed
        // ↑ during streaming and queued the recalled message travels
        // with their image. Pre-fix, the queue carried empty images.
        use crate::input::history::HistoryImageRef;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_path_buf();
        std::fs::write(cache_dir.join("deadbeef12345678.png"), b"\x89PNG").unwrap();

        let mut state = UiState::new();
        state.pending_recalled_attachments.push(HistoryImageRef {
            hash: "deadbeef12345678".into(),
            mt: "image/png".into(),
            n: 4,
        });
        let mut line = "describe [Image #4]".to_string();
        let _ = super::hydrate_recalled_attachments(&mut state, &mut line, &cache_dir);
        // After hydrate, the line + pending state should match what the
        // queued-submit pending-drain loop expects to see.
        assert_eq!(state.pending_images.len(), 1);
        assert_eq!(line, "describe [Image #1]"); // first paste this session
        assert_eq!(state.pending_image_markers, vec![1]);
        assert!(
            line.contains("[Image #1]"),
            "marker survives in line for the survival filter"
        );
    }

    #[test]
    fn parse_dollar_line_splits_name_and_args() {
        assert_eq!(
            parse_dollar_line("$brainstorming"),
            Some(("brainstorming".to_string(), String::new()))
        );
        assert_eq!(
            parse_dollar_line("$brainstorming why is X"),
            Some(("brainstorming".to_string(), "why is X".to_string()))
        );
        assert_eq!(
            parse_dollar_line("$brainstorming  spaced "),
            Some(("brainstorming".to_string(), "spaced".to_string()))
        );
        assert_eq!(parse_dollar_line("hello"), None);
        assert_eq!(parse_dollar_line("/skills x"), None);
        assert_eq!(parse_dollar_line("$"), None);
        assert_eq!(parse_dollar_line("$   "), None);
    }
}

#[cfg(test)]
mod tool_format_tests {
    use super::*;

    #[test]
    fn fmt_elapsed_under_one_minute_uses_seconds_only() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(999), "0s");
        assert_eq!(fmt_elapsed(1_000), "1s");
        assert_eq!(fmt_elapsed(45_500), "45s");
    }

    #[test]
    fn fmt_elapsed_above_one_minute_splits_minutes_and_seconds() {
        assert_eq!(fmt_elapsed(60_000), "1m0s");
        assert_eq!(fmt_elapsed(141_000), "2m21s");
        assert_eq!(fmt_elapsed(342_000), "5m42s");
    }

    #[test]
    fn display_tool_name_snake_to_pascal() {
        assert_eq!(display_tool_name("read_file"), "ReadFile");
        assert_eq!(display_tool_name("search_replace"), "SearchReplace");
        assert_eq!(display_tool_name("bash"), "Bash");
    }

    #[test]
    fn display_tool_name_handles_edge_cases() {
        assert_eq!(display_tool_name(""), "");
        assert_eq!(display_tool_name("x"), "X");
        assert_eq!(display_tool_name("x_"), "X");
        assert_eq!(display_tool_name("_x"), "X");
    }

    /// MCP tool names arrive on the wire as `mcp__<server>__<tool>` —
    /// the double underscores carry meaning. Naive PascalCase folds the
    /// three parts into one blob (`McpZouwuQueryRequirements`), which
    /// the issue reporter (#299) couldn't visually parse. Render with
    /// middle-dot separators instead.
    #[test]
    fn display_tool_name_splits_mcp_server_and_tool() {
        assert_eq!(
            display_tool_name("mcp__zouwu__query"),
            "mcp · zouwu · query"
        );
        assert_eq!(
            display_tool_name("mcp__zouwu-mcp-server__query_requirements"),
            "mcp · zouwu-mcp-server · query_requirements"
        );
    }

    /// Defensive: an `mcp__`-prefixed name that's missing the second
    /// `__` boundary (e.g. partial / malformed wire name) falls back
    /// to the generic PascalCase path rather than panicking on a
    /// missing split.
    #[test]
    fn display_tool_name_mcp_missing_second_separator_falls_back() {
        assert_eq!(display_tool_name("mcp__lonely"), "McpLonely");
    }

    /// Short form strips the redundant noun suffix so batch UI shows
    /// `Read(mod.rs)` instead of `ReadFile(mod.rs)` — matches CC's
    /// function-call-style tool labels. Strip list is generic
    /// (`_file`, `_files`, `_directory`); other suffixes pass through
    /// untouched so `search_replace` stays `SearchReplace` (no
    /// disambiguation lost).
    #[test]
    fn display_tool_name_short_strips_redundant_noun() {
        assert_eq!(display_tool_name_short("read_file"), "Read");
        assert_eq!(display_tool_name_short("write_file"), "Write");
        assert_eq!(display_tool_name_short("edit_file"), "Edit");
        assert_eq!(display_tool_name_short("create_file"), "Create");
        assert_eq!(display_tool_name_short("list_directory"), "List");
        assert_eq!(
            display_tool_name_short("parallel_edit_files"),
            "ParallelEdit"
        );
        // Suffixes not in strip list pass through.
        assert_eq!(display_tool_name_short("bash"), "Bash");
        assert_eq!(display_tool_name_short("grep"), "Grep");
        assert_eq!(display_tool_name_short("search_replace"), "SearchReplace");
        assert_eq!(display_tool_name_short("web_fetch"), "WebFetch");
        assert_eq!(display_tool_name_short("blast_radius"), "BlastRadius");
    }

    /// The short form must NOT strip `_file`/`_files`/`_directory` from an
    /// MCP tool name — that suffix is part of the real tool, not a redundant
    /// noun. `mcp__fs__read_file` stays `mcp · fs · read_file`, not
    /// `mcp · fs · read`.
    #[test]
    fn display_tool_name_short_keeps_mcp_suffix() {
        assert_eq!(
            display_tool_name_short("mcp__fs__read_file"),
            "mcp · fs · read_file"
        );
        assert_eq!(
            display_tool_name_short("mcp__playwright-mcp-server__browser_snapshot"),
            "mcp · playwright-mcp-server · browser_snapshot"
        );
    }

    #[test]
    fn format_tool_detail_read_file_basename() {
        let args = r#"{"file_path":"/abs/path/to/foo.rs"}"#;
        assert_eq!(format_tool_detail("read_file", args), "foo.rs");
    }

    #[test]
    fn format_tool_detail_edit_file_omits_old_string_preview() {
        let args = r#"{"file_path":"/abs/path/to/test.txt","old_string":"4","new_string":"1888"}"#;
        assert_eq!(format_tool_detail("edit_file", args), "test.txt");
    }

    #[test]
    fn format_tool_detail_edit_file_repairs_unescaped_newline() {
        let args = concat!(
            r#"{"file_path":"/abs/path/to/test.txt","old_string":"old","new_string":"line 1"#,
            "\n",
            r#"line 2"}"#
        );
        assert_eq!(format_tool_detail("edit_file", args), "test.txt");
    }

    #[test]
    fn format_tool_detail_read_symbol_combines_symbol_and_file() {
        let args = r#"{"symbol":"parse","file_path":"src/lexer.rs"}"#;
        assert_eq!(format_tool_detail("read_symbol", args), "parse in lexer.rs");
    }

    #[test]
    fn format_tool_detail_bash_truncates_at_500() {
        let args = format!(r#"{{"command":"{}"}}"#, "a".repeat(600));
        let out = format_tool_detail("bash", &args);
        // `truncate_with_ellipsis` preserves `max_cols-1` display columns
        // (499) then appends '…' (display width 1, 3 UTF-8 bytes).
        // Display width = 500, byte length = 502.
        assert_eq!(out.len(), 502, "byte length: 499 'a' + 3-byte '…'");
        assert!(out.ends_with('…'), "should end with Unicode ellipsis");
        assert_eq!(&out[..499], "a".repeat(499));
    }

    #[test]
    fn format_tool_detail_bash_preserves_short_command() {
        let args = format!(r#"{{"command":"{}"}}"#, "a".repeat(500));
        let out = format_tool_detail("bash", &args);
        // Full command preserved — `push_body_prefixed` handles wrapping
        // for the committed body, and `build_inflight_tool_row` clips the
        // live spinner row to terminal width.
        assert_eq!(out, "a".repeat(500));
    }

    #[test]
    fn format_tool_detail_unknown_tool_falls_back_to_common_keys() {
        // Unknown tool but args carry `file_path` — fallback uses it.
        let args = r#"{"file_path":"/tmp/a.txt","extra":"x"}"#;
        let out = format_tool_detail("my_custom_tool", args);
        assert!(!out.is_empty(), "fallback should find file_path");
    }

    #[test]
    fn format_tool_detail_invalid_json_returns_empty() {
        let out = format_tool_detail("read_file", "not json");
        assert_eq!(out, "");
    }

    #[test]
    fn format_tool_detail_todo_add_shows_content() {
        let args = r#"{"action":"add","content":"Write tests"}"#;
        let out = format_tool_detail("todo", args);
        assert_eq!(out, "Write tests");
    }

    #[test]
    fn format_tool_detail_todo_update_shows_id_and_status() {
        let args = r#"{"action":"update","id":2,"status":"completed"}"#;
        let out = format_tool_detail("todo", args);
        assert_eq!(out, "#2 → completed");
    }

    #[test]
    fn parse_todo_titles_extracts_id_to_content() {
        let mut map = std::collections::HashMap::new();
        // The full-list shape every todo result returns, with a preamble
        // line that must be ignored.
        let output = "Task #4 'Write tests' updated to 'completed'\n\
                      [x] 4. Write tests\n\
                      [>] 5. Fix the parser\n\
                      [ ] 6. Update docs";
        parse_todo_titles_into(&mut map, output);
        assert_eq!(map.get(&4).map(String::as_str), Some("Write tests"));
        assert_eq!(map.get(&5).map(String::as_str), Some("Fix the parser"));
        assert_eq!(map.get(&6).map(String::as_str), Some("Update docs"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn enrich_todo_detail_splices_title_into_update() {
        let mut titles = std::collections::HashMap::new();
        titles.insert(4u64, "Write tests".to_string());
        let args = r#"{"action":"update","id":4,"status":"completed"}"#;
        let base = format_tool_detail("todo", args); // "#4 → completed"
        let out = enrich_todo_detail("todo", args, &base, &titles);
        assert_eq!(out, "#4 Write tests \u{2192} completed");
    }

    #[test]
    fn enrich_todo_detail_falls_back_without_title() {
        let titles = std::collections::HashMap::new();
        let args = r#"{"action":"update","id":4,"status":"completed"}"#;
        let base = format_tool_detail("todo", args);
        // Unknown id ⇒ unchanged id-only detail.
        assert_eq!(enrich_todo_detail("todo", args, &base, &titles), base);
        // Non-update actions and non-todo tools pass through untouched.
        assert_eq!(enrich_todo_detail("todo", r#"{"action":"add","content":"x"}"#, "x", &titles), "x");
        assert_eq!(enrich_todo_detail("bash", r#"{"command":"ls"}"#, "ls", &titles), "ls");
    }

    #[test]
    fn format_tool_detail_todo_list_shows_list_all() {
        let args = r#"{"action":"list"}"#;
        let out = format_tool_detail("todo", args);
        assert_eq!(out, "list all");
    }

    #[test]
    fn format_tool_detail_search_replace_shows_arrow() {
        let args = r#"{"search":"bg-blue-600","replace":"bg-violet-600","glob":"*.vue"}"#;
        let out = format_tool_detail("search_replace", args);
        assert!(
            out.contains("bg-blue-600"),
            "should contain search term: got {:?}",
            out
        );
        assert!(
            out.contains("bg-violet-600"),
            "should contain replace term: got {:?}",
            out
        );
        assert!(
            out.contains("→"),
            "should contain arrow separator: got {:?}",
            out
        );
        assert!(
            out.contains("glob: *.vue"),
            "should contain glob info: got {:?}",
            out
        );
    }

    #[test]
    fn format_tool_detail_search_replace_without_glob() {
        let args = r#"{"search":"oldFunc","replace":"newFunc"}"#;
        let out = format_tool_detail("search_replace", args);
        assert_eq!(out, "oldFunc → newFunc");
    }

    #[test]
    fn format_tool_detail_search_replace_with_path() {
        let args = r#"{"search":"foo","replace":"bar","path":"/some/dir"}"#;
        let out = format_tool_detail("search_replace", args);
        assert!(
            out.contains("path: dir"),
            "should contain path basename: got {:?}",
            out
        );
    }

    #[test]
    fn format_tool_detail_search_replace_dot_path_omitted() {
        let args = r#"{"search":"foo","replace":"bar","path":"."}"#;
        let out = format_tool_detail("search_replace", args);
        assert!(
            !out.contains("path:"),
            "default '.' path should be omitted: got {:?}",
            out
        );
    }

    #[test]
    fn format_tool_detail_parallel_edit_files_shows_basenames() {
        let args = r#"{"files":[{"path":"/src/server/api.rs","instruction":"add log"},{"path":"/src/client/mod.rs","instruction":"add log"},{"path":"/src/config/mod.rs","instruction":"add log"}],"contract":"use tracing"}"#;
        let out = format_tool_detail("parallel_edit_files", args);
        assert_eq!(out, "api.rs, mod.rs, mod.rs");
    }

    #[test]
    fn format_tool_detail_parallel_edit_files_two_files() {
        let args = r#"{"files":[{"path":"a.rs","instruction":"add X"},{"path":"b.rs","instruction":"wire X"}]}"#;
        let out = format_tool_detail("parallel_edit_files", args);
        assert_eq!(out, "a.rs, b.rs");
    }

    #[test]
    fn format_tool_detail_parallel_edit_files_repairs_unescaped_instruction_newline() {
        let args = concat!(
            r#"{"files":[{"path":"a.rs","instruction":"line 1"#,
            "\n",
            r#"line 2"},{"path":"b.rs","instruction":"change b"}]}"#
        );
        let out = format_tool_detail("parallel_edit_files", args);
        assert_eq!(out, "a.rs, b.rs");
    }

    #[test]
    fn format_tool_detail_parallel_edit_files_empty_files_array() {
        let args = r#"{"files":[]}"#;
        let out = format_tool_detail("parallel_edit_files", args);
        assert_eq!(out, "");
    }

    #[test]
    fn format_tool_detail_parallel_edit_files_missing_files_key() {
        let args = r#"{"contract":"use tracing"}"#;
        let out = format_tool_detail("parallel_edit_files", args);
        assert_eq!(out, "");
    }

    // ── disambiguate_batch_details tests (issue #437) ──

    #[test]
    fn disambiguate_no_duplicates_returns_as_is() {
        let names = vec!["read_file", "read_file"];
        let args = vec![
            r#"{"file_path":"/a/foo.rs"}"#,
            r#"{"file_path":"/b/bar.rs"}"#,
        ];
        let details = vec!["foo.rs".to_string(), "bar.rs".to_string()];
        let result = disambiguate_batch_details(&names, &args, &details);
        assert_eq!(result, vec!["foo.rs", "bar.rs"]);
    }

    #[test]
    fn disambiguate_same_basename_adds_parent_dir() {
        // Issue #437: three SKILL.md files in different directories
        let names = vec!["read_file", "read_file", "read_file"];
        let args = vec![
            r#"{"file_path":"/home/.atomcode/skills/atomcode-automation-recommender/SKILL.md"}"#,
            r#"{"file_path":"/home/.atomcode/skills/tosshub-skill/SKILL.md"}"#,
            r#"{"file_path":"/home/.atomcode/skills/zouwu-skill/SKILL.md"}"#,
        ];
        let details = vec![
            "SKILL.md".to_string(),
            "SKILL.md".to_string(),
            "SKILL.md".to_string(),
        ];
        let result = disambiguate_batch_details(&names, &args, &details);
        // Each should include its parent directory to disambiguate
        assert_eq!(
            result,
            vec![
                "atomcode-automation-recommender/SKILL.md",
                "tosshub-skill/SKILL.md",
                "zouwu-skill/SKILL.md",
            ]
        );
    }

    #[test]
    fn disambiguate_partial_duplicates_only_touches_dups() {
        // Two same-name files + one unique file
        let names = vec!["read_file", "read_file", "read_file"];
        let args = vec![
            r#"{"file_path":"/a/mod.rs"}"#,
            r#"{"file_path":"/b/mod.rs"}"#,
            r#"{"file_path":"/c/unique.rs"}"#,
        ];
        let details = vec![
            "mod.rs".to_string(),
            "mod.rs".to_string(),
            "unique.rs".to_string(),
        ];
        let result = disambiguate_batch_details(&names, &args, &details);
        // unique.rs is left unchanged
        assert_eq!(result[2], "unique.rs");
        // mod.rs entries get parent dir
        assert_eq!(result[0], "a/mod.rs");
        assert_eq!(result[1], "b/mod.rs");
    }

    #[test]
    fn disambiguate_no_path_uses_hash_suffix() {
        // Non-file tools that produce duplicate details
        let names = vec!["bash", "bash"];
        let args = vec![r#"{"command":"echo hi"}"#, r#"{"command":"echo hi"}"#];
        let details = vec!["echo hi".to_string(), "echo hi".to_string()];
        let result = disambiguate_batch_details(&names, &args, &details);
        // First stays the same, second gets #2 suffix
        assert_eq!(result[0], "echo hi");
        assert_eq!(result[1], "echo hi #2");
    }

    #[test]
    fn tail_path_basic() {
        assert_eq!(tail_path("a/b/c/SKILL.md", 0), "SKILL.md");
        assert_eq!(tail_path("a/b/c/SKILL.md", 1), "c/SKILL.md");
        assert_eq!(tail_path("a/b/c/SKILL.md", 2), "b/c/SKILL.md");
        assert_eq!(tail_path("a/b/c/SKILL.md", 3), "a/b/c/SKILL.md");
        // depth exceeding path depth returns full path
        assert_eq!(tail_path("a/b/c/SKILL.md", 10), "a/b/c/SKILL.md");
    }

    #[test]
    fn tail_path_no_parent() {
        // File with no directory component
        assert_eq!(tail_path("foo.rs", 0), "foo.rs");
        assert_eq!(tail_path("foo.rs", 1), "foo.rs");
    }

    #[test]
    fn disambiguate_long_path_is_truncated() {
        // Very deep duplicate paths should be truncated to 100 display cols
        let long_seg = "a".repeat(30); // 30 chars each
        let path1 = format!("/x/{}/{}/mod.rs", long_seg, long_seg);
        let path2 = format!("/y/{}/{}/mod.rs", long_seg, long_seg);
        let names = vec!["read_file", "read_file"];
        let args: Vec<String> = vec![
            format!(r#"{{"file_path":"{}"}}"#, path1),
            format!(r#"{{"file_path":"{}"}}"#, path2),
        ];
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let details = vec!["mod.rs".to_string(), "mod.rs".to_string()];
        let result = disambiguate_batch_details(&names, &args_refs, &details);
        // Both entries should be truncated — no entry exceeds 100 display width
        for entry in &result {
            assert!(
                crate::width::display_width(entry) <= 100,
                "entry too wide ({} cols): {}",
                crate::width::display_width(entry),
                entry,
            );
        }
        // And they should still be different from each other
        assert_ne!(result[0], result[1]);
    }

    #[test]
    fn summarise_single_line_returned_as_is() {
        assert_eq!(summarise("ok"), "ok");
    }

    #[test]
    fn web_search_domains_from_ddg_output() {
        // DuckDuckGo backend: explicit URL lines under each numbered result.
        let out = "Search results for \"北京建筑大学 录取分数线\":\n\n\
            1. 北京建筑大学2024录取分数\n   https://www.eol.cn/beijing/123\n   snippet\n\n\
            2. 院校库\n   https://gaokao.com/school/456?ref=x\n   snippet\n\n\
            3. 阳光高考\n   https://gaokao.com/school/789\n   snippet\n";
        // www. stripped, gaokao.com deduped, first-seen order preserved.
        assert_eq!(web_search_source_domains(out), vec!["eol.cn", "gaokao.com"]);
    }

    #[test]
    fn web_search_domains_from_inline_text() {
        // Exa-style blob: URLs embedded mid-sentence with trailing punctuation.
        let out = "See (https://bjeea.cn/page) and http://dxsbb.com/a, also https://bjeea.cn/other.";
        assert_eq!(web_search_source_domains(out), vec!["bjeea.cn", "dxsbb.com"]);
    }

    #[test]
    fn web_search_domains_empty_when_no_links() {
        // A link-less Exa text blob or an error message yields no domains.
        assert!(web_search_source_domains("Exa returned a prose summary with no links.").is_empty());
        assert!(web_search_source_domains("").is_empty());
    }

    #[test]
    fn web_search_suffix_collapses_extra_sources() {
        let out = "a https://eol.cn/1 b https://gaokao.com/2 c https://bjeea.cn/3 d https://dxsbb.com/4";
        // Two shown, the rest collapse into +N.
        assert_eq!(
            web_search_result_suffix(out).as_deref(),
            Some("eol.cn, gaokao.com +2")
        );
    }

    #[test]
    fn web_search_suffix_none_without_links() {
        assert_eq!(web_search_result_suffix("no links here"), None);
    }

    #[test]
    fn plan_mode_block_reason_detects_gate_blocks() {
        // A PlanModeGate block → calm hint (reason without the `blocked: ` prefix).
        assert_eq!(
            plan_mode_block_reason(
                "blocked: plan mode is active — `write_file` would modify the workspace",
                false
            ),
            Some("plan mode is active — `write_file` would modify the workspace")
        );
        // Successes, non-block failures, and OTHER middleware blocks (e.g. approval
        // deny) are NOT plan-mode hints → normal ✗ result render.
        assert_eq!(plan_mode_block_reason("ok", true), None);
        assert_eq!(plan_mode_block_reason("Error: file not found", false), None);
        assert_eq!(
            plan_mode_block_reason("blocked: denied by approval policy: bash", false),
            None
        );
    }

    #[test]
    fn summarise_multi_line_adds_line_count() {
        let out = summarise("first line\nsecond line\nthird line");
        assert!(out.starts_with("first line"));
        assert!(out.contains("(3 lines)"));
    }

    #[test]
    fn summarise_empty_string_has_fallback() {
        let out = summarise("");
        // Empty input: `lines()` yields nothing, so first falls back
        // to "(no output)" and n==0 means no " (N lines)" suffix.
        assert!(out.contains("(no output)"), "got: {}", out);
    }

    /// A long diagnostic line (e.g. a deep WSL path) must survive intact —
    /// no pre-truncation. The old code capped at 80/200 cols here; now the
    /// renderer fits the line to the live screen width, so summarise hands
    /// back the full text.
    #[test]
    fn summarise_keeps_long_path_intact() {
        let err = "Error: old_string not found in \
            /mnt/d/docs/work/cangjie/projects/fountain/f_store.";
        let out = summarise(err);
        assert_eq!(out, err, "the full line must survive un-truncated");
        assert!(!out.contains('…'));
    }

    /// The actual bug fix: a 200-col first line must NOT be pre-truncated
    /// (the old 80-col success cap chopped it and wasted wide screens).
    /// The renderer now fits it to the live width, so summarise returns it
    /// whole.
    #[test]
    fn summarise_does_not_pretruncate_wide_line() {
        let line: String = "x".repeat(200);
        let out = summarise(&line);
        assert_eq!(out, line, "200-col line must survive un-truncated");
        assert!(!out.contains('…'));
    }

    /// The remaining 512-col cap is a pure safety bound, not a display
    /// decision — it only trips for a pathological multi-KB single line,
    /// and when it does the cut is marked with `…` and stays bounded.
    #[test]
    fn summarise_caps_pathological_line_with_ellipsis() {
        let long: String = "x".repeat(600);
        let out = summarise(&long);
        assert!(
            out.ends_with('…'),
            "safety cap must mark the cut. got len {}",
            out.chars().count()
        );
        assert!(out.chars().count() <= 512);
    }

    /// Multi-line output keeps the line-count suffix.
    #[test]
    fn summarise_multi_line_still_appends_count() {
        let err = "Error: foo\nbar\nbaz";
        let out = summarise(err);
        assert!(out.starts_with("Error: foo"));
        assert!(out.contains("(3 lines)"));
    }
}

pub(crate) enum BufferResult {
    NoOp,
    Redraw,
    Commit(String),
    Exit,
}

fn prev_boundary(s: &str, mut p: usize) -> usize {
    p -= 1;
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_boundary(s: &str, mut p: usize) -> usize {
    p += 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// All the per-session UI state that flows through key/event handlers.
///
/// Before this aggregation, handlers took 7–9 `&mut` parameters each
/// and the call sites filled a paragraph. Now the handlers take
/// `(&mut App, &mut LoopCtx, &mut dyn Renderer, …event)` — the LoopCtx
/// stays separate because the tokio `select!` in `run_loop` needs to
/// borrow `ctx.input_rx`, `ctx.runtime_event_rx`, `ctx.wake_rx`
/// independently, and bundling them into App would fight the borrow
/// checker on every arm.
pub struct App {
    pub state: UiState,
    pub buf: Buffer,
    pub menu: MenuState,
    /// Exactly one overlay at a time — /model, /provider, /resume all
    /// push into the same slot. The Modal trait owns draw + key handling
    /// so adding a fourth overlay is `Some(Box::new(X))`, not a new
    /// field + new dispatch branch.
    pub active_modal: Option<Box<dyn crate::modals::Modal>>,
    /// Messages the user submitted while a turn was already running.
    /// Drained one-at-a-time from the head whenever the current turn
    /// finishes. Matches CC's "type-ahead" UX — queue the next prompt
    /// while the model is still thinking and it fires automatically.
    pub message_queue: VecDeque<crate::state::QueuedMessage>,
    /// Streaming-state `<think>…</think>` stripper. Kept on App (not
    /// a local in the streaming arm) because it carries state across
    /// agent events — a tag straddling two chunks would break if the
    /// stripper were re-constructed each event.
    pub think: ThinkStripper,
    /// call_id → (tool_name, detail, call_rendered). Populated on
    /// ToolCallStarted, read by `ApprovalNeeded` (which renders the
    /// `▸ Tool(detail)` line eagerly so the user sees *what* they're
    /// being asked to approve), and consumed on ToolCallResult. The
    /// `call_rendered` flag prevents rendering the tool-call line
    /// twice when ApprovalNeeded fired first.
    pub pending_tools: std::collections::HashMap<String, (String, String, bool)>,
    /// Timestamp of the first Ctrl+C press on an empty idle buffer.
    /// Requires a second press within `CTRL_C_EXIT_WINDOW` to actually
    /// exit — protects against accidental single-tap exits.
    pub exit_pending: Option<std::time::Instant>,
    /// Timestamp of the first bare Esc press on an empty idle buffer.
    /// A second Esc within `DOUBLE_ESC_UNDO_WINDOW` triggers `/undo`.
    pub esc_undo_pending: Option<std::time::Instant>,
    /// Set by `/fixissue <url>` while the agent is resolving that issue.
    /// On `TurnComplete` the text buffered in `fixissue_buffer` is posted
    /// back as an issue comment + the `fixed` label is applied. Cleared
    /// on TurnComplete / TurnCancelled / Error so a subsequent normal
    /// message doesn't accidentally trigger a post-back.
    pub fixissue_pending: Option<atomcode_core::atomgit::IssueRef>,
    /// Accumulates every visible `AssistantText` delta produced during a
    /// fixissue turn, verbatim. Sent as the AtomGit comment body on
    /// successful completion.
    pub fixissue_buffer: String,
    /// True while a setup skill turn is in flight. On `TurnComplete`,
    /// skill/command registries are reloaded so newly-created skills
    /// become visible to the LLM immediately. Cleared on
    /// TurnComplete / TurnCancelled / Error.
    pub setup_pending: bool,
    /// Accumulates reasoning/thinking content for display in verbose mode.
    /// Flushed on newline or when buffer exceeds threshold.
    pub reasoning_buffer: String,
    /// Guards the one-shot `/setup` hint so it fires at most once per
    /// session. Flipped to `true` after the first render; subsequent
    /// redraws skip the check entirely.
    pub setup_hint_shown: bool,
}

/// How long the "press Ctrl+C again to exit" confirmation stays armed.
const CTRL_C_EXIT_WINDOW: Duration = Duration::from_secs(2);
const DOUBLE_ESC_UNDO_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyEscIntercept {
    Consumed,
    TriggerUndo,
}

fn second_esc_triggers_undo(pending: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    pending.is_some_and(|t| now.duration_since(t) <= DOUBLE_ESC_UNDO_WINDOW)
}

fn intercept_empty_bare_esc(
    pending: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) -> EmptyEscIntercept {
    if second_esc_triggers_undo(*pending, now) {
        *pending = None;
        EmptyEscIntercept::TriggerUndo
    } else {
        *pending = Some(now);
        EmptyEscIntercept::Consumed
    }
}

/// Grace period after a quit request before the force-exit watchdog fires. The
/// graceful path (engine teardown closes `cmd_tx`) normally completes in well
/// under a second; the bridge bounds its own kernel-teardown wait at 5s, so 8s
/// here only ever trips when even that fails — at which point hard-exiting is
/// strictly better than trapping the user. See [`arm_shutdown_watchdog`].
const SHUTDOWN_WATCHDOG: Duration = Duration::from_secs(8);

impl App {
    fn new(caps: &crate::terminal::TerminalCaps) -> Self {
        Self {
            state: UiState::with_unicode(caps.unicode_symbols),
            buf: Buffer::new(),
            menu: MenuState::new(),
            active_modal: None,
            message_queue: VecDeque::new(),
            think: ThinkStripper::new(),
            pending_tools: std::collections::HashMap::new(),
            exit_pending: None,
            esc_undo_pending: None,
            fixissue_pending: None,
            fixissue_buffer: String::new(),
            setup_pending: false,
            reasoning_buffer: String::new(),
            setup_hint_shown: false,
        }
    }
}

/// Why the event loop exited. Callers (currently just `atomcode-tuix::run`)
/// use this to decide whether to re-exec into the new binary after an
/// in-place upgrade or just terminate normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// Normal termination (user quit, Ctrl+C, etc.).
    Normal,
    /// `/upgrade` or `/upgrade rollback` succeeded; the live binary has been
    /// replaced and the caller should `re_exec_self` to start the new version.
    ///
    /// Carries the *original* exe path (e.g. `atomcode.exe`) captured
    /// **before** `replace_binary` renamed the running binary. On Windows,
    /// `std::env::current_exe()` returns the renamed path after the swap,
    /// so callers MUST use this path for `re_exec_self` instead of
    /// `current_exe()`.
    UpgradeRestart { exe: std::path::PathBuf },
}

pub async fn run_loop(mut ctx: LoopCtx, renderer: &mut dyn Renderer) -> Result<ExitReason> {
    let mut app = App::new(&ctx.caps);

    crate::tuix_trace!(
        "SES",
        "run_loop start model={} cwd={}",
        ctx.model_name,
        ctx.working_dir.display()
    );

    // Draw welcome + initial prompt
    let dir_display = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
    renderer.render(UiLine::Welcome {
        model: ctx.model_name.clone(),
        working_dir: dir_display.clone(),
    });
    // If this process was spawned by `apply_pending_upgrade` → `re_exec_self`,
    // an env var carries the version we just upgraded from. Surface one line
    // on the welcome screen so the user knows the upgrade succeeded, then
    // clear the var so any subprocesses we spawn don't inherit a stale hint.
    if let Ok(prev) = std::env::var("ATOMCODE_UPGRADED_FROM") {
        std::env::remove_var("ATOMCODE_UPGRADED_FROM");
        let current = format!("v{}", env!("CARGO_PKG_VERSION"));
        renderer.render(UiLine::CommandOutput(
            crate::i18n::t(crate::i18n::Msg::UpgradeSuccess {
                from: &prev,
                to: &current,
            })
            .into_owned(),
        ));
    }
    // Warn the user when --dangerously-skip-permissions / -y is active.
    // The status bar shows a ⚠ BYPASS badge, but a scrollback banner
    // is harder to miss and persists even if the user clears the status row.
    if ctx.dangerously_skip_permissions {
        renderer.render(UiLine::CommandOutput(
            crate::i18n::t(crate::i18n::Msg::BypassWarningBanner).into_owned(),
        ));
    }
    // Warn when running with admin/root privileges so the user is aware
    // the model can access system files beyond the project directory.
    if ctx.is_admin {
        renderer.render(UiLine::CommandOutput(
            crate::i18n::t(crate::i18n::Msg::AdminWarningBanner).into_owned(),
        ));
    }
    // Same env-var handoff from `atomcode codingplan` (see CLI `run()`):
    // the subcommand stashes its rendered SetupReport here instead of
    // printing to stdout, so the user sees the ✓/✗ lines in the chat
    // scrollback rather than scrolled off above the welcome banner.
    if let Ok(report) = std::env::var("ATOMCODE_CODINGPLAN_REPORT") {
        std::env::remove_var("ATOMCODE_CODINGPLAN_REPORT");
        if !report.is_empty() {
            renderer.render(UiLine::CommandOutput(report));
        }
    }

    // Terminal keyboard hint: shown when crossterm couldn't negotiate
    // the Kitty keyboard protocol (CSI u). The previous copy claimed
    // "Shift+Enter won't work" — but Kitty is only ONE of several ways
    // a terminal can disambiguate modifier+Enter. Windows Terminal,
    // VSCode (xterm.js), mintty/Git Bash, and modern PowerShell hosts
    // all forward Shift/Alt/Ctrl+Enter via VT modifyOtherKeys without
    // ever negotiating CSI u, so the user sees Shift+Enter work in
    // their daily session yet boots into a banner asserting it can't.
    // Re-frame as informational guidance rather than a definitive
    // "won't work" claim, and surface `\<Enter>` as the universal
    // fallback so legacy-conhost users (where modifier+Enter IS
    // genuinely swallowed at the OS layer) have a guaranteed path.
    let kbd_hint_set = std::env::var("ATOMCODE_KBD_NOT_ENHANCED").is_ok();
    if kbd_hint_set {
        std::env::remove_var("ATOMCODE_KBD_NOT_ENHANCED");
    }
    // Emit a single universal hint pointing at `\<Enter>` whenever the
    // keyboard-enhanced negotiation failed.
    //
    // Why the universal-fallback message instead of per-terminal
    // chord recommendations: the previous helper detected MSYSTEM /
    // WT_SESSION / ConEmuPID / TERM_PROGRAM and named the most
    // reliable chord per terminal, but the detection misfires
    // whenever the env vars don't survive (e.g. PowerShell sessions
    // launched in Windows Terminal that lose WT_SESSION through a
    // helper process — observed in user feedback 2026-05-09). The
    // `\<Enter>` line continuation is implemented at the buffer
    // layer (event_loop/mod.rs Action::Submit handler), so it
    // works on EVERY terminal regardless of keyboard protocol or
    // env var fidelity. Modifier+Enter chords stay supported in
    // `key_action.rs::classify`; users who know they have them just
    // use them. The startup hint targets the user who doesn't know,
    // and for them a guaranteed-works recommendation beats a
    // sometimes-wrong terminal-specific one.
    if kbd_hint_set {
        renderer.render(UiLine::CommandOutput(
            crate::i18n::t(crate::i18n::Msg::HintMultiLineInput).into_owned(),
        ));
    }

    // The legacy-Windows-conhost fallback banner used to fire here
    // (gated on ATOMCODE_LEGACY_CONHOST_FALLBACK set by lib.rs). It
    // walked the user through wheel-scroll, PageUp/Down, third-party
    // terminal alternatives, and the ATOMCODE_PLAIN / ATOMCODE_RETAIN
    // bypass switches. Removed in v4.22 once alt-screen on conhost
    // shipped working wheel + PageUp/Down + SGR mouse: the wall of
    // text became dead weight (every conhost user immediately
    // wanted it gone). Newline guidance still reaches them via the
    // universal `\<Enter>` block above (kbd_hint_set arm), which is
    // one line and terminal-agnostic.

    // Bind the initial session's persistent id onto the agent + telemetry so
    // even a brand-new session uses its session-file id for the
    // x-atomcode-session-id header (not Agent::new's bootstrap). Makes a later
    // /resume reuse the SAME id. The -c replay block below rebinds if present.
    commands::bind_telemetry_to_session(&ctx, &ctx.current_session);

    // Auto-continue: if the CLI loaded the most recent session for this
    // working dir (via `atomcode -c` / `--continue`), replay its messages
    // into scrollback AND restore the agent's model context so follow-up
    // questions can reference prior conversation. This mirrors the `/resume`
    // slash command's behaviour: visual replay + AgentCommand::SetConversation.
    if let Some(session) = ctx.replay_on_start.take() {
        if !session.messages.is_empty() {
            crate::modals::session_picker::replay_session(renderer, &session, false);
            // Sync messages into the agent loop so the LLM has full context.
            ctx.agent
                .cmd_tx
                .send(AgentCommand::SetConversation(
                    session.to_conversation_snapshot(),
                ))
                .ok();
            // Continue accumulating into the same session file — future
            // TurnComplete saves overwrite it instead of creating a new one.
            // Header + telemetry = this continued session's persistent id, so
            // `-c` reuses the saved session's id (not a fresh per-process one).
            commands::bind_telemetry_to_session(&ctx, &session);
            ctx.current_session = session;
            app.state.on_turn_complete();
        }
    }

    // First-run onboarding: no providers configured AND no OAuth login
    // on disk means the user has never set this up — open the
    // OnboardingWizard. Users with a config or prior OAuth auth are
    // never shown this and boot straight to idle. Plain renderer
    // (CI / pipe / non-TTY) is also gated out — the bordered box
    // would just garble its output channel with no human to see it.
    if should_auto_show_onboarding(&ctx) {
        // Modal trait imported so `wizard.draw(...)` resolves; the
        // OnboardingWizard's Modal impl owns the per-step box drawing.
        use crate::modals::Modal;
        renderer.clear_screen();
        // First-launch fast path: single-page QR + URL. Background
        // poll thread (PR 1b) watches `/auth/check` and auto-closes
        // the modal the moment AtomGit reports authorisation, then
        // the `OauthEvent::Authorized` branch in the main `select!`
        // flips `pending_run_login_setup` so `/codingplan` claims
        // immediately — zero keystrokes after the user finishes the
        // browser flow. The legacy 3-step Intro / Language / Setup
        // wizard stays intact for `/welcome` — `new_qr_fast_path` is
        // ONLY used here. /welcome's command arm still uses `new()`
        // / `new_with_confirm()` so users who explicitly re-run the
        // wizard see the familiar language + setup path.
        let mut wizard = crate::modals::OnboardingWizard::new_qr_fast_path();
        // Pull the LoginSession out of the wizard before boxing — the
        // background poll thread owns it from here. wizard.draw still
        // has access to `qr_login_url` so the QR keeps rendering.
        if let Some(session) = wizard.take_pending_session() {
            oauth_poll::spawn_oauth_poll(
                session,
                Some(std::sync::Arc::clone(&ctx.telemetry)),
                ctx.oauth_event_tx.clone(),
                ctx.wake_tx.clone(),
            );
        }
        wizard.draw(&app.buf, &app.state, &ctx, renderer);
        app.active_modal = Some(Box::new(wizard));
    } else {
        // One-shot /setup hint — only on first boot into this project,
        // gated by preferences + setup-state presence.
        if !app.setup_hint_shown && should_auto_show_setup(&ctx) {
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::CmdSetupTip).into_owned(),
            ));
            app.setup_hint_shown = true;
        }
        // One-shot legacy-conhost scroll hint. The classic Windows console
        // host snaps the viewport back to the bottom on every write, so the
        // live footer repaint during a running task makes scrolling up to
        // read history impossible until the task ends — a conhost limitation
        // we don't fix in-app. Show it once here at startup (this block runs
        // exactly once per session) and ONLY on legacy conhost; Windows
        // Terminal and every other terminal set `legacy_conhost = false`.
        // Gated out of the plain renderer (CI / pipe) where there's no live
        // footer and no human to read the hint.
        if !ctx.is_plain_renderer && ctx.caps.legacy_conhost {
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::ConhostScrollHint).into_owned(),
            ));
        }
        renderer.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: build_status(&app.state, &ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }

    // Startup CodingPlan drift check. Without this, a user who ran
    // `/codingplan` days ago and now sees a new model in the plan lineup
    // wouldn't learn until they typed a message — the mid-turn trigger
    // at the submit-path only fires on user action. Gating:
    //
    //   * Only when the active provider is an AtomGit* (CodingPlan)
    //     provider — non-CodingPlan users do zero network work on boot.
    //   * Still respects the 15-min cooldown against `monitor_last_check_at`
    //     so rapid restarts (e.g. crash-loop during development) don't
    //     spam the API gateway.
    //
    // The check itself is fully async (`spawn_check` returns immediately
    // and runs on a tokio task); the event loop entering its main tick
    // loop below isn't blocked, and the warning — when it arrives a
    // second or two later — wakes the loop via `wake_tx` so the status
    // row repaints without the user needing to press a key.
    if monitor::is_codingplan_provider(&ctx.config.default_provider) {
        let cooled = ctx
            .monitor_last_check_at
            .map(|t| t.elapsed() >= monitor::CHECK_COOLDOWN)
            .unwrap_or(true);
        if cooled {
            ctx.monitor_last_check_at = Some(std::time::Instant::now());
            monitor::spawn_check(
                ctx.config.clone(),
                ctx.model_name.clone(),
                ctx.monitor_warning.clone(),
                ctx.wake_tx.clone(),
            );
        }
        // Startup usage check (separate cooldown — 30s vs drift's 15min).
        // Always fires once at startup so the user sees current quota
        // immediately if they're already over 80%.
        ctx.usage_last_check_at = Some(std::time::Instant::now());
        usage_monitor::spawn_check(ctx.usage_slot.clone(), ctx.wake_tx.clone());
    }

    // Spinner tick channel — a background task fires a tick every 100ms
    // into a bounded (cap 1) mpsc. The main loop recv's this in the
    // `tokio::select!` alongside the agent-event channel, so spinner
    // ticks compete fairly with agent events (both are channel reads
    // rather than a time-interval future that the runtime can skip
    // over when other branches are always ready).
    //
    // Cap 1 + try_send means if the main loop is mid-event and a tick
    // can't land in the channel, we silently drop it — no burst of
    // queued frames when control eventually returns. The post-event
    // pump (below) complements this by advancing the spinner as soon
    // as a slow handler finishes, even if the next scheduled tick is
    // still 50ms away.
    let (spin_tx, mut spin_rx) = tokio::sync::mpsc::channel::<()>(1);
    let spin_task = {
        let spin_tx = spin_tx.clone();
        tokio::spawn(async move {
            use tokio::sync::mpsc::error::TrySendError;
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await; // discard the immediate tick
            loop {
                interval.tick().await;
                match spin_tx.try_send(()) {
                    Ok(_) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Closed(_)) => break,
                }
            }
        })
    };
    drop(spin_tx); // only the task needs the sender

    // Deferred-render tick: 50fps. The renderer throttles InputPrompt /
    // StreamingBox redraws to 20ms windows so Mac Terminal.app doesn't
    // choke on back-to-back full footer payloads, but the trailing
    // edge of a burst needs someone to paint it — that someone is this
    // tick. No-op when nothing is pending.
    // 5ms matches the InputThrottle window (see render::throttle) —
    // tick == window means the max visible lag from "burst ended" to
    // "parked paint landed" is ~10ms, imperceptible. Previously 20ms
    // which compounded with the 20ms throttle window to ~40ms lag,
    // visible for IME commit bursts.
    let mut deferred_render_tick = tokio::time::interval(Duration::from_millis(5));
    deferred_render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    deferred_render_tick.tick().await; // consume the immediate fire

    // Last emitted integer percent for the /upgrade download line.
    // Gate on change so we don't spam the renderer with a progress
    // line for every chunk (a 10 MB binary at 64 KiB chunks would be
    // 160 redraws). `-1` means "no download active yet".
    let mut upgrade_last_pct: i32 = -1;
    // True once Done fired successfully — the loop exits after the
    // current pending message finishes so the user sees the success
    // line before the TUI shuts down.
    let mut upgrade_done: Option<std::path::PathBuf> = None;

    // DEVIATION from plan:
    // 1. plan uses `SignalKind::terminal_stop()` which does not exist in tokio 1.x.
    //    Using `SignalKind::from_raw(libc::SIGTSTP)` instead.
    // 2. tokio::select! does not support #[cfg(...)] on individual arms, so signal
    //    handling is split into a cfg-gated loop variant below.
    #[cfg(unix)]
    let mut sigtstp =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP))?;
    #[cfg(unix)]
    let mut sigcont =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGCONT))?;

    // Windows-only OS-level Ctrl+C fallback. The keyboard path
    // (crossterm KeyEvent → handle_input → 2-press confirm) is the
    // primary route, but on legacy conhost the Ctrl+C keystroke is
    // sometimes swallowed before reaching the input buffer when raw
    // mode + ENABLE_VIRTUAL_TERMINAL_INPUT are both active — users
    // report "completely no reaction" with no hint shown.
    // `tokio::signal::windows::ctrl_c` hooks SetConsoleCtrlHandler so
    // the OS signal still lands here regardless of whether the
    // keystroke ever made it into the console input queue. Single-press
    // exit on this path: when the keypress chain is broken, this is the
    // user's only escape — a 2-press confirm would just trap them.
    #[cfg(windows)]
    let mut win_ctrl_c = tokio::signal::windows::ctrl_c()?;

    sync_reasoning_effort_from_provider(&mut ctx);

    // Terminal tab/window title mirrors the current session name. Tracked
    // here so we only emit an OSC title write when it actually changes
    // (startup, first-message auto-name, `/rename`, `/resume`, `/new`) rather
    // than every loop iteration. `None` forces the first pass to emit.
    let mut last_terminal_title: Option<String> = None;

    loop {
        sync_terminal_title(&ctx, renderer, &mut last_terminal_title);

        #[cfg(unix)]
        tokio::select! {
            // Biased ordering: spinner first so whenever a tick is
            // pending in spin_rx we draw it before racing with agent
            // events. Without `biased` tokio picks a ready branch
            // randomly, so under heavy agent traffic the spinner gets
            // chosen ~50% of the time its tick is ready, dropping the
            // effective frame rate to ~5 fps and looking like "frozen
            // then jumps".
            biased;

            // ── Deferred-render trailing edge ──
            // Drains any InputPrompt / StreamingBox payload the
            // renderer parked during its 20ms throttle window. No-op
            // when nothing is pending.
            _ = deferred_render_tick.tick() => {
                renderer.flush_deferred();
            }

            // ── Spinner tick (from background task) ──
            // Skip the spinner repaint while a capturing modal (the password
            // prompt) is up — the spinner repaints the same footer/input
            // region the modal draws, and would clobber the masked line.
            Some(()) = spin_rx.recv(), if matches!(app.state.phase, UiPhase::Streaming)
                && app.active_modal.as_ref().map_or(true, |m| !m.captures_all_keys()) => {
                draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
            }

            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(&mut app, &mut ctx, renderer, ev)?;
            }

            // ── Version-check wake ──
            // Fires once when the detached startup check resolves with a
            // positive result. Idle-only: in Streaming the spinner tick
            // redraws frequently enough that the hint picks up naturally.
            // Preserve an active `/` command menu — don't blindly call
            // `redraw_idle_plain(menu: None)` which would erase it.
            Some(()) = ctx.wake_rx.recv(), if matches!(app.state.phase, UiPhase::Idle) => {
                let items = menu_for_display(&app.buf, &ctx);
                if let Some(items) = items {
                    redraw_with_menu(
                        &app.buf,
                        &items,
                        app.menu.selected,
                        &app.state,
                        &ctx,
                        renderer,
                    );
                } else {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── OAuth poll thread results ──
            // Emitted by `event_loop::oauth_poll::spawn_oauth_poll`
            // once per QR-fast-path session. Authorized → close the
            // wizard + flip `pending_run_login_setup` so the existing
            // /codingplan driver picks up the just-written auth.toml
            // and claims the plan. Failed → close the wizard too and
            // surface the reason in scrollback with a retry hint;
            // leaving the modal open would require a Modal trait
            // extension (as_any_mut + downcast) we don't yet have.
            Some(ev) = ctx.oauth_event_rx.recv() => {
                use oauth_poll::OauthEvent;
                let was_modal_open = app.active_modal.is_some();
                if was_modal_open {
                    app.active_modal = None;
                    renderer.clear_screen();
                }
                match ev {
                    OauthEvent::Authorized => {
                        // Banner FIRST, /codingplan output below — per
                        // user direction: AtomCode chrome should anchor
                        // the top of scrollback, the codingplan claim
                        // output is verbose detail underneath. Model
                        // bullet is blank at this point because the
                        // claim hasn't picked a default provider yet —
                        // refreshed below once the claim writes
                        // ctx.model_name.
                        crate::modals::onboarding_wizard::paint_welcome(&ctx, renderer);
                        // `pending_run_login_setup` is only drained by the
                        // keystroke-handler path (handle_input → modal
                        // close → drain flag). The OAuth poll path doesn't
                        // route through there, so just call the codingplan
                        // driver directly — same effect, runs in this
                        // select! arm's scope where renderer + ctx are
                        // already mutable.
                        if let Err(e) = crate::event_loop::commands::run_login_flow(renderer, &mut ctx) {
                            renderer.render(crate::render::UiLine::Error(
                                format!("CodingPlan 自动配置失败: {e:#}。可运行 /login 手动重试。"),
                            ));
                            renderer.flush();
                        }
                        // Splice the resolved model name into the
                        // banner painted above. `run_login_flow`
                        // updates `ctx.model_name` from the picked
                        // default provider (see commands.rs:2906) — at
                        // this point the banner's cached model="" is
                        // stale, so refresh in place.
                        let dir_display = crate::platform::collapse_home(
                            &ctx.working_dir.to_string_lossy(),
                        );
                        renderer.refresh_welcome_banner(&ctx.model_name, &dir_display);
                        // QR-fast-path onboarding bypasses the regular
                        // first-boot idle render (see ~line 2506), so
                        // the one-shot /setup tip never fires for users
                        // who land through the scan flow. Surface it
                        // here under the same gates: in-session
                        // once-only + `should_auto_show_setup` (no
                        // setup-state.json or missing recommender
                        // skill).
                        if !app.setup_hint_shown && should_auto_show_setup(&ctx) {
                            renderer.render(crate::render::UiLine::CommandOutput(
                                crate::i18n::t(crate::i18n::Msg::CmdSetupTip).into_owned(),
                            ));
                            renderer.flush();
                            app.setup_hint_shown = true;
                        }
                    }
                    OauthEvent::Failed(reason) => {
                        renderer.render(crate::render::UiLine::Error(
                            format!(
                                "登录失败: {reason}。运行 /login 可重试。",
                            ),
                        ));
                        renderer.flush();
                    }
                }
            }

            // ── MCP connection events ──
            // Render connection success/failure into scrollback as they arrive.
            // Also register tools dynamically when servers connect.
            Some(ev) = async {
                if let Some(rx) = ctx.mcp_connect_rx.as_mut() {
                    rx.recv().await
                } else {
                    None
                }
            }, if ctx.mcp_connect_rx.is_some() => {
                use atomcode_core::mcp::{McpConnectEvent, register_mcp_tools_async};
                match &ev {
                    McpConnectEvent::Connected { name } => {
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::McpServerConnected { name }).into_owned(),
                        ));
                        // Register tools from this newly connected server.
                        // Important: do this in a background task so a slow `tools/list`
                        // can't block the TUI event loop and freeze input.
                        if let Some(registry) = &ctx.mcp_registry {
                            let registry = registry.clone();
                            let tools = ctx.agent.tool_registry.clone();
                            let name = name.clone();
                            let tx = registry.event_sender();
                            tokio::spawn(async move {
                                let list_timeout = registry.list_tools_timeout(&name).await;
                                let server_tools = match tokio::time::timeout(
                                    list_timeout,
                                    registry.list_tools_for_server(&name),
                                )
                                .await
                                {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Some(tx) = tx {
                                            let _ = tx.send(McpConnectEvent::Warning {
                                                name,
                                                message: format!(
                                                    "tools/list timed out after {}s during auto-registration",
                                                    list_timeout.as_secs()
                                                ),
                                            });
                                        }
                                        return;
                                    }
                                };
                                if !server_tools.is_empty() {
                                    register_mcp_tools_async(&tools, registry, server_tools).await;
                                }
                            });
                        }
                    }
                    McpConnectEvent::Failed { name, error } => {
                        renderer.render(UiLine::Error(
                            crate::i18n::t(crate::i18n::Msg::McpServerFailed { name, error }).into_owned(),
                        ));
                    }
                    McpConnectEvent::Warning { name, message } => {
                        // Default: keep MCP startup/runtime noise out of scrollback.
                        //
                        // Exception: `/mcp tools <server>` uses Warning events to return the tool list
                        // (and related timeouts) from a background task. Those should be user-visible.
                        if message.starts_with("tools:\n")
                            || message.contains("tools/list timed out")
                            || message.contains("tools/list failed")
                        {
                            renderer.render(UiLine::CommandOutput(format!(
                                "  [mcp:{}] {}\n",
                                name,
                                message.trim_end()
                            )));
                        } else {
                            // Route to the opt-in tuix trace log instead (safe for raw-mode TUI).
                            crate::tuix_trace!("MCP", "server='{}' warning: {}", name, message);
                        }
                    }
                }

                // `/mcp reload` progress: once every configured server has reported a
                // terminal state (Connected/Failed), emit a summary line.
                if let Some(p) = ctx.mcp_reload.as_mut() {
                    match &ev {
                        McpConnectEvent::Connected { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.connected = p.connected.saturating_add(1)
                        }
                        McpConnectEvent::Failed { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.failed = p.failed.saturating_add(1)
                        }
                        McpConnectEvent::Warning { .. } => {}
                    }
                    if p.done >= p.total {
                        let elapsed_ms = p.started_at.elapsed().as_millis();
                        renderer.render(UiLine::CommandOutput(format!(
                            "  MCP reload complete: {} connected, {} failed ({}ms)\n",
                            p.connected, p.failed, elapsed_ms
                        )));
                        ctx.mcp_reload = None;
                    }
                }
                renderer.flush();
            }

            // ── LSP server start / failure ──
            // Mirrors the MCP arm above. Without this, `LspManager`'s
            // raw `eprintln!` on a failed server start would land in the
            // input box (TUI owns the screen, stderr-fd writes hit
            // wherever the cursor sits — between the cyan rules).
            // Started → ✓ in scrollback. Failed → ✗ as an Error line.
            // Warning is non-actionable noise (e.g. shutdown teardown
            // errors) and routed to the trace log instead.
            Some(ev) = async {
                if let Some(rx) = ctx.lsp_connect_rx.as_mut() {
                    rx.recv().await
                } else {
                    None
                }
            }, if ctx.lsp_connect_rx.is_some() => {
                use atomcode_core::lsp::LspConnectEvent;
                match &ev {
                    LspConnectEvent::Started { command, ext } => {
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::LspServerStarted { name: command, ext }).into_owned(),
                        ));
                    }
                    LspConnectEvent::Failed { command, ext, error } => {
                        renderer.render(UiLine::Error(
                            crate::i18n::t(crate::i18n::Msg::LspServerFailed { name: command, ext, error }).into_owned(),
                        ));
                    }
                    LspConnectEvent::Warning { ext, message } => {
                        crate::tuix_trace!("LSP", "ext='{}' warning: {}", ext, message);
                    }
                }
                renderer.flush();
            }

            // ── /upgrade progress ──
            Some(ev) = ctx.upgrade_rx.recv() => {
                handle_upgrade_event(ev, &mut upgrade_last_pct, &mut upgrade_done, &mut ctx, renderer);
                if upgrade_done.is_some() { break; }
                if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── /plugin async job result ──
            Some(ev) = ctx.plugin_job_rx.recv() => {
                // Let an open modal (the interactive /plugin manager) refresh
                // its cached lists from this job's result first.
                if let Some(m) = app.active_modal.as_mut() {
                    m.on_plugin_event(&ev);
                }
                handle_plugin_job_event(ev, &mut ctx, &mut app.state, renderer);
                // The job result rendered to scrollback above; restore the
                // bottom prompt. Redraw the modal if one is open (else
                // redraw_idle_plain would paint over it), otherwise the idle box.
                if let Some(m) = app.active_modal.as_ref() {
                    m.draw(&app.buf, &app.state, &ctx, renderer);
                } else if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── Agent events ──
            // Consumed regardless of phase. Gating on Streaming missed
            // the TurnComplete that arrives *after* an Error event: the
            // Error handler flips phase to Idle, so the very next event
            // on the channel is stuck until the user submits again —
            // which is what "得发两次你好才结束" looked like in the UI.
            // Phase-specific behaviour (spinner redraw, type-ahead queue
            // drain) lives inside the match arms on `app.state.phase`.
            maybe = ctx.runtime_event_rx.recv() => {
                let Some(runtime_event) = maybe else { break };
                if runtime_event.runtime_id == ctx.foreground_runtime_id {
                    let pre_phase = app.state.phase;
                    handle_agent_event(runtime_event.event, &mut app.state, &mut app.think, renderer, &mut app.pending_tools, &mut ctx, &mut app.fixissue_pending, &mut app.fixissue_buffer, &mut app.setup_pending, &mut app.reasoning_buffer, &mut app.buf);
                    // A turn ending with a password modal still up = orphan (the sudo/ssh
                    // that asked for it finished or timed out); dismiss it so `Password:`
                    // doesn't linger in the input box.
                    dismiss_orphan_capturing_modal(&mut app, &ctx, renderer);
                    if pre_phase != app.state.phase {
                        crate::tuix_trace!("PH", "{:?} -> {:?}", pre_phase, app.state.phase);
                    }
                    if matches!(app.state.phase, UiPhase::Streaming)
                    {
                        draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                    }
                    if matches!(app.state.phase, UiPhase::Idle) {
                        // Turn just ended — drain the type-ahead queue.
                        // Pop the oldest queued message, echo as a User
                        // line, dispatch to the agent, and transition
                        // back to Streaming. Remaining queue entries
                        // fire in order on subsequent completions.
                        if let Some(queued) = app.message_queue.pop_front() {
                            crate::tuix_trace!("QUE", "pop_front remaining={}", app.message_queue.len());
                            if let Some(live) = &ctx.sync_session {
                                // 同步模式：投 LiveSession，不本地渲染用户行。
                                use atomcode_core::live::UserInput;
                                live.send_input(UserInput { text: queued.text, images: queued.images });
                                app.state.on_submit();
                            } else {
                                ctx.agent.cmd_tx.send(AgentCommand::SendMessage {
                                    text: queued.text,
                                    images: queued.images,
                                    image_markers: queued.image_markers,
                                }).ok();
                                app.state.on_submit();
                            }
                            draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                        } else {
                            crate::tuix_trace!("PH", "turn_end -> Idle, queue empty, redraw_idle");
                            redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                        }
                    }
                } else {
                    ctx.bg_manager.apply_background_event(
                        runtime_event.runtime_id,
                        runtime_event.event,
                        &ctx.session_manager,
                    );
                }
            }

            // ── Askpass password prompt ──
            // Fired by the askpass server when a child process (sudo/ssh)
            // asks for a password. Installs the PasswordModal so the user
            // can type their password without it ever touching the main
            // input buffer or history. When `askpass_rx` is `None` (the
            // common case before Task 10 wires it, or after the channel
            // closes) the async block resolves to `std::future::pending()`
            // which never fires, making the arm completely inert at zero cost.
            //
            // The arm binds the raw `Option<AskpassPrompt>` so we can also
            // handle the `None` / channel-closed case: once the sender half
            // is dropped we set `askpass_rx = None` so subsequent iterations
            // skip the inner `rx.recv()` and go straight back to `pending()`.
            maybe_p = async {
                match ctx.askpass_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match maybe_p {
                    Some(p) => {
                        install_password_modal(&mut app, p.prompt, p.reply);
                        if let Some(m) = app.active_modal.as_ref() {
                            m.draw(&app.buf, &app.state, &ctx, renderer);
                        }
                    }
                    None => {
                        // Channel closed — degrade back to the inert pending() path.
                        ctx.askpass_rx = None;
                    }
                }
            }

            // ── Suspend ──
            _ = sigtstp.recv() => {
                renderer.render(UiLine::ClearTransient);
                renderer.shutdown();
                app.state.on_suspend();
                // Disable raw mode before SIGSTOP so shell gets a sane terminal.
                let _ = crossterm::terminal::disable_raw_mode();
                unsafe { libc::raise(libc::SIGSTOP); }
            }

            // ── Resume ──
            _ = sigcont.recv() => {
                let _ = crossterm::terminal::enable_raw_mode();
                app.state.on_resume();
                match app.state.phase {
                    UiPhase::Streaming => {
                        draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                    }
                    _ => {
                        redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                    }
                }
            }
        }

        // Was `cfg(not(unix))` to bracket the whole non-Unix select.
        // Narrowed to `cfg(windows)` because the only arm that needs
        // this branch (`win_ctrl_c.recv()`) is itself Windows-only,
        // and tokio's `select!` macro doesn't accept arm-level
        // `#[cfg(...)]` attributes — it tries to expand them inside
        // its own ruleset and fails with "no rules expected `#`".
        // We only support Unix + Windows, so cfg(not(unix)) ≡
        // cfg(windows) for our build matrix anyway.
        #[cfg(windows)]
        tokio::select! {
            biased;

            // ── Windows OS-level Ctrl+C ──
            // Fallback for conhost configurations where the keystroke
            // never lands in the input buffer. Healthy terminals fire
            // the keypress arm in `handle_input` first; this only wins
            // when that path is silent. Single-press exit: skips the
            // 2-press confirm because if we got here, the user has no
            // working keyboard route to confirm with anyway.
            Some(()) = win_ctrl_c.recv() => {
                if matches!(app.state.phase, UiPhase::Streaming) {
                    // In Streaming phase, Ctrl+C should cancel the
                    // running turn (matching keyboard-path behaviour)
                    // rather than shut down the whole application.
                    crate::tuix_trace!("KEY", "windows ctrl_c signal -> Cancel (streaming)");
                    cancel_active_turn(&ctx);
                    clear_capturing_modal_on_cancel(&mut app);
                    restore_cancelled_message_to_buf(&mut app, renderer, &ctx);
                } else {
                    crate::tuix_trace!("KEY", "windows ctrl_c signal -> Shutdown");
                    arm_shutdown_watchdog(&mut ctx);
                }
            }

            // ── Deferred-render trailing edge ──
            // Drains any InputPrompt / StreamingBox payload the
            // renderer parked during its 20ms throttle window. No-op
            // when nothing is pending.
            _ = deferred_render_tick.tick() => {
                renderer.flush_deferred();
            }

            // ── Spinner tick (from background task) ──
            // Skip the spinner repaint while a capturing modal (the password
            // prompt) is up — the spinner repaints the same footer/input
            // region the modal draws, and would clobber the masked line.
            Some(()) = spin_rx.recv(), if matches!(app.state.phase, UiPhase::Streaming)
                && app.active_modal.as_ref().map_or(true, |m| !m.captures_all_keys()) => {
                draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
            }

            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(&mut app, &mut ctx, renderer, ev)?;
            }

            // ── Version-check wake ──
            // Must check for an active `/` command menu before calling
            // `redraw_idle_plain` — otherwise the menu gets erased when
            // this fires a second or two after the user types `/`.
            Some(()) = ctx.wake_rx.recv(), if matches!(app.state.phase, UiPhase::Idle) => {
                let items = menu_for_display(&app.buf, &ctx);
                if let Some(items) = items {
                    redraw_with_menu(
                        &app.buf,
                        &items,
                        app.menu.selected,
                        &app.state,
                        &ctx,
                        renderer,
                    );
                } else {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── OAuth poll thread results ──
            // Emitted by `event_loop::oauth_poll::spawn_oauth_poll`
            // once per QR-fast-path session. Authorized → close the
            // wizard + flip `pending_run_login_setup` so the existing
            // /codingplan driver picks up the just-written auth.toml
            // and claims the plan. Failed → close the wizard too and
            // surface the reason in scrollback with a retry hint;
            // leaving the modal open would require a Modal trait
            // extension (as_any_mut + downcast) we don't yet have.
            Some(ev) = ctx.oauth_event_rx.recv() => {
                use oauth_poll::OauthEvent;
                let was_modal_open = app.active_modal.is_some();
                if was_modal_open {
                    app.active_modal = None;
                    renderer.clear_screen();
                }
                match ev {
                    OauthEvent::Authorized => {
                        // Banner FIRST, /codingplan output below — per
                        // user direction: AtomCode chrome should anchor
                        // the top of scrollback, the codingplan claim
                        // output is verbose detail underneath. Model
                        // bullet is blank at this point because the
                        // claim hasn't picked a default provider yet —
                        // refreshed below once the claim writes
                        // ctx.model_name.
                        crate::modals::onboarding_wizard::paint_welcome(&ctx, renderer);
                        // `pending_run_login_setup` is only drained by the
                        // keystroke-handler path (handle_input → modal
                        // close → drain flag). The OAuth poll path doesn't
                        // route through there, so just call the codingplan
                        // driver directly — same effect, runs in this
                        // select! arm's scope where renderer + ctx are
                        // already mutable.
                        if let Err(e) = crate::event_loop::commands::run_login_flow(renderer, &mut ctx) {
                            renderer.render(crate::render::UiLine::Error(
                                format!("CodingPlan 自动配置失败: {e:#}。可运行 /login 手动重试。"),
                            ));
                            renderer.flush();
                        }
                        // Splice the resolved model name into the
                        // banner painted above. `run_login_flow`
                        // updates `ctx.model_name` from the picked
                        // default provider (see commands.rs:2906) — at
                        // this point the banner's cached model="" is
                        // stale, so refresh in place.
                        let dir_display = crate::platform::collapse_home(
                            &ctx.working_dir.to_string_lossy(),
                        );
                        renderer.refresh_welcome_banner(&ctx.model_name, &dir_display);
                        // QR-fast-path onboarding bypasses the regular
                        // first-boot idle render (see ~line 2506), so
                        // the one-shot /setup tip never fires for users
                        // who land through the scan flow. Surface it
                        // here under the same gates: in-session
                        // once-only + `should_auto_show_setup` (no
                        // setup-state.json or missing recommender
                        // skill).
                        if !app.setup_hint_shown && should_auto_show_setup(&ctx) {
                            renderer.render(crate::render::UiLine::CommandOutput(
                                crate::i18n::t(crate::i18n::Msg::CmdSetupTip).into_owned(),
                            ));
                            renderer.flush();
                            app.setup_hint_shown = true;
                        }
                    }
                    OauthEvent::Failed(reason) => {
                        renderer.render(crate::render::UiLine::Error(
                            format!(
                                "登录失败: {reason}。运行 /login 可重试。",
                            ),
                        ));
                        renderer.flush();
                    }
                }
            }

            // ── MCP connection events ──
            // Render connection success/failure into scrollback as they arrive.
            // Also register tools dynamically when servers connect.
            Some(ev) = async {
                if let Some(rx) = ctx.mcp_connect_rx.as_mut() {
                    rx.recv().await
                } else {
                    None
                }
            }, if ctx.mcp_connect_rx.is_some() => {
                use atomcode_core::mcp::{McpConnectEvent, register_mcp_tools_async};
                match &ev {
                    McpConnectEvent::Connected { name } => {
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::McpServerConnected { name }).into_owned(),
                        ));
                        // Register tools from this newly connected server (backgrounded).
                        if let Some(registry) = &ctx.mcp_registry {
                            let registry = registry.clone();
                            let tools = ctx.agent.tool_registry.clone();
                            let name = name.clone();
                            let tx = registry.event_sender();
                            tokio::spawn(async move {
                                let list_timeout = registry.list_tools_timeout(&name).await;
                                let server_tools = match tokio::time::timeout(
                                    list_timeout,
                                    registry.list_tools_for_server(&name),
                                )
                                .await
                                {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Some(tx) = tx {
                                            let _ = tx.send(McpConnectEvent::Warning {
                                                name,
                                                message: format!(
                                                    "tools/list timed out after {}s during auto-registration",
                                                    list_timeout.as_secs()
                                                ),
                                            });
                                        }
                                        return;
                                    }
                                };
                                if !server_tools.is_empty() {
                                    register_mcp_tools_async(&tools, registry, server_tools).await;
                                }
                            });
                        }
                    }
                    McpConnectEvent::Failed { name, error } => {
                        renderer.render(UiLine::Error(
                            crate::i18n::t(crate::i18n::Msg::McpServerFailed { name, error }).into_owned(),
                        ));
                    }
                    McpConnectEvent::Warning { name, message } => {
                        // Default: keep MCP startup/runtime noise out of scrollback.
                        //
                        // Exception: `/mcp tools <server>` uses Warning events to return the tool list
                        // (and related timeouts) from a background task. Those should be user-visible.
                        if message.starts_with("tools:\n")
                            || message.contains("tools/list timed out")
                            || message.contains("tools/list failed")
                        {
                            renderer.render(UiLine::CommandOutput(format!(
                                "  [mcp:{}] {}\n",
                                name,
                                message.trim_end()
                            )));
                        } else {
                            // Route to the opt-in tuix trace log instead (safe for raw-mode TUI).
                            crate::tuix_trace!("MCP", "server='{}' warning: {}", name, message);
                        }
                    }
                }

                // `/mcp reload` progress: once every configured server has reported a
                // terminal state (Connected/Failed), emit a summary line.
                if let Some(p) = ctx.mcp_reload.as_mut() {
                    match &ev {
                        McpConnectEvent::Connected { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.connected = p.connected.saturating_add(1)
                        }
                        McpConnectEvent::Failed { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.failed = p.failed.saturating_add(1)
                        }
                        McpConnectEvent::Warning { .. } => {}
                    }
                    if p.done >= p.total {
                        let elapsed_ms = p.started_at.elapsed().as_millis();
                        renderer.render(UiLine::CommandOutput(format!(
                            "  MCP reload complete: {} connected, {} failed ({}ms)\n",
                            p.connected, p.failed, elapsed_ms
                        )));
                        ctx.mcp_reload = None;
                    }
                }
                renderer.flush();
            }

            // ── LSP server start / failure ──
            // Mirrors the MCP arm above. Without this, `LspManager`'s
            // raw `eprintln!` on a failed server start would land in the
            // input box (TUI owns the screen, stderr-fd writes hit
            // wherever the cursor sits — between the cyan rules).
            // Started → ✓ in scrollback. Failed → ✗ as an Error line.
            // Warning is non-actionable noise (e.g. shutdown teardown
            // errors) and routed to the trace log instead.
            Some(ev) = async {
                if let Some(rx) = ctx.lsp_connect_rx.as_mut() {
                    rx.recv().await
                } else {
                    None
                }
            }, if ctx.lsp_connect_rx.is_some() => {
                use atomcode_core::lsp::LspConnectEvent;
                match &ev {
                    LspConnectEvent::Started { command, ext } => {
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::LspServerStarted { name: command, ext }).into_owned(),
                        ));
                    }
                    LspConnectEvent::Failed { command, ext, error } => {
                        renderer.render(UiLine::Error(
                            crate::i18n::t(crate::i18n::Msg::LspServerFailed { name: command, ext, error }).into_owned(),
                        ));
                    }
                    LspConnectEvent::Warning { ext, message } => {
                        crate::tuix_trace!("LSP", "ext='{}' warning: {}", ext, message);
                    }
                }
                renderer.flush();
            }

            // ── /upgrade progress ──
            Some(ev) = ctx.upgrade_rx.recv() => {
                handle_upgrade_event(ev, &mut upgrade_last_pct, &mut upgrade_done, &mut ctx, renderer);
                if upgrade_done.is_some() { break; }
                if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── /plugin async job result ──
            Some(ev) = ctx.plugin_job_rx.recv() => {
                // Let an open modal (the interactive /plugin manager) refresh
                // its cached lists from this job's result first.
                if let Some(m) = app.active_modal.as_mut() {
                    m.on_plugin_event(&ev);
                }
                handle_plugin_job_event(ev, &mut ctx, &mut app.state, renderer);
                // The job result rendered to scrollback above; restore the
                // bottom prompt. Redraw the modal if one is open (else
                // redraw_idle_plain would paint over it), otherwise the idle box.
                if let Some(m) = app.active_modal.as_ref() {
                    m.draw(&app.buf, &app.state, &ctx, renderer);
                } else if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── Agent events ──
            // Consumed regardless of phase. Gating on Streaming missed
            // the TurnComplete that arrives *after* an Error event: the
            // Error handler flips phase to Idle, so the very next event
            // on the channel is stuck until the user submits again —
            // which is what "得发两次你好才结束" looked like in the UI.
            // Phase-specific behaviour (spinner redraw, type-ahead queue
            // drain) lives inside the match arms on `app.state.phase`.
            maybe = ctx.runtime_event_rx.recv() => {
                let Some(runtime_event) = maybe else { break };
                if runtime_event.runtime_id == ctx.foreground_runtime_id {
                    let pre_phase = app.state.phase;
                    handle_agent_event(runtime_event.event, &mut app.state, &mut app.think, renderer, &mut app.pending_tools, &mut ctx, &mut app.fixissue_pending, &mut app.fixissue_buffer, &mut app.setup_pending, &mut app.reasoning_buffer, &mut app.buf);
                    // A turn ending with a password modal still up = orphan (the sudo/ssh
                    // that asked for it finished or timed out); dismiss it so `Password:`
                    // doesn't linger in the input box.
                    dismiss_orphan_capturing_modal(&mut app, &ctx, renderer);
                    if pre_phase != app.state.phase {
                        crate::tuix_trace!("PH", "{:?} -> {:?}", pre_phase, app.state.phase);
                    }
                    if matches!(app.state.phase, UiPhase::Streaming)
                    {
                        draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                    }
                    if matches!(app.state.phase, UiPhase::Idle) {
                        if let Some(queued) = app.message_queue.pop_front() {
                            crate::tuix_trace!("QUE", "pop_front remaining={}", app.message_queue.len());
                            if let Some(live) = &ctx.sync_session {
                                // 同步模式：投 LiveSession，不本地渲染用户行。
                                use atomcode_core::live::UserInput;
                                live.send_input(UserInput { text: queued.text, images: queued.images });
                                app.state.on_submit();
                            } else {
                                ctx.agent.cmd_tx.send(AgentCommand::SendMessage {
                                    text: queued.text,
                                    images: queued.images,
                                    image_markers: queued.image_markers,
                                }).ok();
                                app.state.on_submit();
                            }
                            draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                        } else {
                            crate::tuix_trace!("PH", "turn_end -> Idle, queue empty, redraw_idle");
                            redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                        }
                    }
                } else {
                    ctx.bg_manager.apply_background_event(
                        runtime_event.runtime_id,
                        runtime_event.event,
                        &ctx.session_manager,
                    );
                }
            }
        }

        if matches!(app.state.phase, UiPhase::Idle) && ctx.agent.cmd_tx.is_closed() {
            break;
        }

        // Force-exit watchdog. The graceful break above is the normal path; this
        // only fires when a quit was requested (deadline armed by
        // `arm_shutdown_watchdog`) but the engine teardown never closed `cmd_tx`
        // in time — a wedged kernel/bridge await would otherwise trap the user at
        // the prompt no matter how many times they press /quit. Re-checked every
        // ~5ms via `deferred_render_tick`, so it trips promptly once the deadline
        // passes. Restore the terminal first, then hard-exit (skips Drop, which is
        // exactly why a wedged await can't stop us).
        if ctx.shutdown_deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            crate::tuix_trace!("EXIT", "shutdown watchdog fired -> hard exit (teardown wedged)");
            let _ = ctx.history.save();
            renderer.render(UiLine::ClearTransient);
            renderer.shutdown();
            // `renderer.shutdown()` restores mouse / Kitty-keyboard / scroll-region
            // state, but NOT bracketed paste — disabling that is `TerminalGuard::Drop`'s
            // job (lib.rs), and `process::exit(0)` skips every Drop. Emit `?2004l`
            // directly so a force-exit doesn't strand the user's shell wrapping every
            // paste in literal `200~`/`201~` markers.
            {
                use std::io::Write as _;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(b"\x1b[?2004l");
                let _ = out.flush();
            }
            let _ = crossterm::terminal::disable_raw_mode();
            std::process::exit(0);
        }
    }

    // Stop the background spinner task. Dropping `spin_rx` at scope
    // exit would let it self-terminate on the next try_send, but abort
    // is immediate and has no downside — the task holds no resources
    // beyond the interval timer.
    spin_task.abort();
    let _ = ctx.history.save();

    // Determine the exit reason. If the upgrade_done flag was set,
    // the loop exited because /upgrade (or /upgrade rollback) succeeded
    // and the live binary has been replaced — the caller should re-exec.
    if let Some(exe) = upgrade_done {
        Ok(ExitReason::UpgradeRestart { exe })
    } else {
        Ok(ExitReason::Normal)
    }
}

/// If another atomcode process just ran `/codingplan` (i.e. the shared
/// sync marker file advanced since we last looked), pull the fresh
/// config from disk, clear our stale drift warning, and hand the new
/// config to the agent. Cheap on every keystroke: a single file-read
/// + serde parse. Idempotent — when no other process has synced, the
/// early return skips all work.
fn refresh_after_cross_process_codingplan_sync(ctx: &mut LoopCtx) {
    let current = atomcode_core::coding_plan::read_last_sync();
    let advanced = match (current, ctx.monitor_last_sync_seen) {
        (Some(new), Some(old)) => new > old,
        (Some(_), None) => true, // marker just appeared
        _ => false,
    };
    if !advanced {
        return;
    }
    ctx.monitor_last_sync_seen = current;

    // Hot-reload the config file. Fail silently: if the other process
    // wrote a malformed config (shouldn't happen — it would have
    // rejected its own reload), leave our in-memory snapshot alone.
    let path = atomcode_core::config::Config::default_path();
    if let Ok(fresh) = atomcode_core::config::Config::load(&path) {
        ctx.config = fresh;
        ctx.runtime_factory.set_config(ctx.config.clone());
        if let Some(p) = ctx.config.providers.get(&ctx.config.default_provider) {
            ctx.model_name = p.model.clone();
        }
        let _ = ctx
            .agent
            .cmd_tx
            .send(AgentCommand::ReloadConfig(ctx.config.clone()));
    }

    // Sync marker = another process just reconciled config with
    // server, so any drift warning we're still showing is stale by
    // definition. Reset the cooldown too so the next drift check
    // (if needed) fires immediately instead of waiting 15 min from
    // whenever we last checked.
    if let Ok(mut g) = ctx.monitor_warning.lock() {
        *g = None;
    }
    ctx.monitor_last_check_at = None;
    // Same logic for the usage slot — a cross-process /codingplan
    // re-sync may also have rotated the quota window. Clear + reset
    // so the next opportunity fetches fresh.
    if let Ok(mut g) = ctx.usage_slot.lock() {
        *g = None;
    }
    ctx.usage_last_check_at = None;
}

/// Common attach-orchestration shared by every "I just got an image
/// from somewhere" entry point: bracketed-paste with empty payload
/// (clipboard image), bracketed-paste with file-path payload (iTerm2
/// Cmd+V on image / Finder drag-and-drop), and the explicit Ctrl+V
/// keystroke that pulls the clipboard image without going through any
/// paste event at all (the iTerm2 default-Cmd+V case where iTerm2
/// sends nothing through the PTY for image-only clipboards).
///
/// `img_hash` is the result of whichever provider the caller used —
/// `None` means no image was found and the caller should fall through
/// to its own non-image handling. When `Some`, this function takes
/// over: capability-checks the active model, emits a `[Image #N]`
/// marker into the input buffer, pushes the image bytes to
/// `pending_images` (drained at submit), writes the bytes to the
/// shared image cache so /resume can rehydrate, and triggers a
/// redraw appropriate to the current phase.
///
/// Returns:
///   - `Ok(true)`  — image was attached OR rejected with an error
///                    message; caller must `return Ok(())`.
///   - `Ok(false)` — no image to attach (`img_hash == None`); caller
///                    continues with its non-image flow.
fn attach_image_to_input(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    img_hash: Option<(ImagePart, u64)>,
) -> Result<bool> {
    let Some((img, hash)) = img_hash else {
        return Ok(false);
    };
    if !ctx.config.can_handle_attached_images() {
        renderer.render(UiLine::Error(
            crate::i18n::t(crate::i18n::Msg::ModelNoImageSupport {
                model: &ctx.model_name,
            })
            .into_owned(),
        ));
        renderer.flush();
        if matches!(app.state.phase, UiPhase::Idle) {
            redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
        }
        return Ok(true);
    }
    // N comes from `session_image_count` (monotonic across turns), NOT
    // `pending_images.len()+1` — otherwise turn 1's first paste and
    // turn 2's first paste would both render as `[Image #1]` in
    // scrollback, ambiguous when scrolling back.
    app.state.session_image_count += 1;
    let n = app.state.session_image_count;
    app.state.pending_images.push(img.clone());
    app.state.pending_image_hashes.push(hash);
    app.state.pending_image_markers.push(n);
    cache_write_image(&crate::platform::image_cache_dir(), &img, hash);
    let marker = format!("[Image #{}]", n);
    app.buf.text.insert_str(app.buf.cursor, &marker);
    app.buf.cursor += marker.len();
    if matches!(app.state.phase, UiPhase::Streaming) {
        draw_spinner_now(
            &mut app.state,
            &app.buf,
            ctx,
            renderer,
            app.message_queue.len(),
            app.menu.selected,
        );
    } else {
        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
    }
    Ok(true)
}

/// Submit-time image-path recognition.
///
/// `try_attach_image_from_path` only runs on `InputEvent::Paste`. Paths that
/// arrive as plain keystrokes never hit that branch: a user typing the path,
/// or — the common case — a Windows paste that conhost / Windows Terminal
/// delivers as individual key events instead of a bracketed paste. Those would
/// otherwise be sent to the model as a literal path string, leaving it to
/// fumble with `OpenFile` / `Bash dir` / base64 to read the bytes.
///
/// Scan the outgoing `text` for whitespace-separated tokens that resolve to a
/// real local image file (same strict checks as the paste path: absolute +
/// known image extension + existing file ≤ `MAX_PATH_IMAGE_BYTES`), read them
/// as image attachments, and replace the path token with an `[Image #N]`
/// marker so the payload matches the paste/drag flow. No-op when the model
/// can't take images — the path is left as text for the model to handle.
fn attach_typed_image_paths(
    app: &mut App,
    ctx: &mut LoopCtx,
    text: &mut String,
    images: &mut Vec<ImagePart>,
    kept_markers: &mut Vec<usize>,
) {
    if !ctx.config.can_handle_attached_images() {
        return;
    }
    // Snapshot tokens before mutating `text`. The `/` `\` pre-filter avoids a
    // filesystem stat on every word of a long message — an absolute path
    // always carries a separator.
    let tokens: Vec<String> = text
        .split_whitespace()
        .filter(|t| t.contains('/') || t.contains('\\'))
        .map(str::to_string)
        .collect();
    for tok in tokens {
        let Some((img, _hash)) = try_attach_image_from_path(&tok) else {
            continue;
        };
        app.state.session_image_count += 1;
        let n = app.state.session_image_count;
        *text = text.replacen(&tok, &format!("[Image #{}]", n), 1);
        // No render here: the caller groups the collected `kept_markers` into a
        // single `UiLine::UserWithAttachments` echo (the viewport-overflow fix),
        // so this helper only collects.
        images.push(img);
        kept_markers.push(n);
    }
}

/// `/paste` slash-command handler. Exists for Windows users whose
/// Ctrl+V is intercepted by Windows Terminal / conhost before the
/// keystroke reaches atomcode — the terminal-layer `paste` action
/// only forwards `CF_UNICODETEXT`, so an image-only clipboard never
/// triggers the in-app `KeyCode::Char('v') + CONTROL` branch.
/// `/paste` invokes the same `try_paste_clipboard_image` →
/// `attach_image_to_input` pipeline directly, bypassing the
/// terminal's keybinds. Works on every platform — Windows / macOS /
/// Linux / git-bash — so it doubles as a discoverable backup
/// regardless of how Ctrl+V is configured locally. Falls back to a
/// scrollback error line when the clipboard has no image so the
/// user isn't left wondering whether the command did anything.
fn handle_paste_command(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    let img_hash = try_paste_clipboard_image();
    if img_hash.is_none() {
        renderer.render(UiLine::Error(
            crate::i18n::t(crate::i18n::Msg::CmdPasteNoImage).into_owned(),
        ));
        renderer.flush();
        if matches!(app.state.phase, UiPhase::Idle) {
            redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
        }
        return Ok(());
    }
    attach_image_to_input(app, ctx, renderer, img_hash)?;
    Ok(())
}

fn handle_input(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    ev: InputEvent,
) -> Result<()> {
    use crate::modals::ModalAction;

    // Pick up any cross-process `/codingplan` that ran since the last
    // input — hot-reloads config + clears stale drift hint before we
    // act on the current keystroke.
    refresh_after_cross_process_codingplan_sync(ctx);

    crate::tuix_trace!(
        "IN",
        "phase={:?} modal={} qlen={} ev={}",
        app.state.phase,
        app.active_modal.is_some(),
        app.message_queue.len(),
        match &ev {
            InputEvent::Paste(t) => format!("paste({})", t.len()),
            InputEvent::Eof => "eof".into(),
            InputEvent::Key(k) => format!("key({:?},{:?})", k.kind, k.code),
            InputEvent::Resize(w, h) => format!("resize({}x{})", w, h),
            InputEvent::MouseScroll(d) => format!("mouse_scroll({})", d),
        }
    );

    match ev {
        InputEvent::MouseScroll(delta) => {
            // Mouse wheel is a no-op: SGR mouse capture (`?1002h` /
            // `?1006h`) is intentionally NOT enabled, so wheel ticks
            // resolve at the terminal level (native scrollback) before
            // reaching us. This arm survives only as a defensive
            // catch-all for terminals that forward wheel events
            // outside the SGR mouse protocol.
            renderer.scroll_body(delta);
        }
        InputEvent::Resize(mut cols, mut rows) => {
            // Coalesce burst-fired SIGWINCH events. gnome-terminal /
            // alacritty / iTerm2 send a Resize per pixel during a
            // window drag — a 200ms drag fires 30+ events. Without
            // coalescing each one runs `on_resize` (per-row CUP+EL
            // wipe + body re-emit + footer repaint), which the user
            // sees as flicker / 刷屏 (Linux Mint bug report).
            //
            // Drain whatever is already queued in `input_rx`:
            //   - adjacent Resize events collapse to the latest size
            //     (intermediate sizes are discarded — only the final
            //     geometry matters)
            //   - non-Resize events are buffered and dispatched AFTER
            //     `on_resize` settles, so they read `screen.width()` /
            //     `screen.height()` at the new geometry rather than
            //     an in-flight intermediate.
            //
            // Forward to the renderer so DECSTBM-based backends can
            // re-issue their scroll region and repaint the footer at
            // the new geometry. Fire-and-forget; the render worker
            // serialises this against in-flight content writes.
            let mut deferred: Vec<InputEvent> = Vec::new();
            while let Ok(next) = ctx.input_rx.try_recv() {
                match next {
                    InputEvent::Resize(w, h) => {
                        cols = w;
                        rows = h;
                    }
                    other => deferred.push(other),
                }
            }
            renderer.on_resize(cols, rows);
            // A resize invalidates any open modal's cached overlay
            // geometry (it was built for the old size). Rebuild it now so
            // the window re-centres at the new dimensions instead of
            // lingering stale / mispositioned until the next keypress.
            if let Some(m) = app.active_modal.as_ref() {
                m.draw(&app.buf, &app.state, ctx, renderer);
            }
            for ev in deferred {
                handle_input(app, ctx, renderer, ev)?;
            }
        }
        InputEvent::Paste(text) => {
            // Route paste to the active modal when one is installed — the
            // provider/model/session wizards all have text-input steps
            // where pasting URLs / API keys / tokens is the natural UX.
            // Modals that don't want paste can override `handle_paste`
            // to drop it; the default inserts into `buf` + redraws.
            //
            // A capturing modal (the password prompt) installs mid-turn while a
            // tool runs (phase == Streaming), so route paste to it regardless of
            // phase — otherwise the pasted password leaks into `buf`. Non-
            // capturing modals keep their Idle-only routing below.
            if app.active_modal.as_ref().is_some_and(|m| m.captures_all_keys()) {
                let streaming = matches!(app.state.phase, UiPhase::Streaming);
                if let Some(modal) = app.active_modal.as_mut() {
                    let action =
                        modal.handle_paste(&text, &mut app.buf, &mut app.state, ctx, renderer)?;
                    if matches!(action, crate::modals::ModalAction::Close) {
                        app.active_modal = None;
                        if streaming {
                            draw_spinner_now(
                                &mut app.state,
                                &app.buf,
                                ctx,
                                renderer,
                                app.message_queue.len(),
                                app.menu.selected,
                            );
                        } else {
                            redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                        }
                    }
                }
                return Ok(());
            }
            if matches!(app.state.phase, UiPhase::Idle) {
                if let Some(modal) = app.active_modal.as_mut() {
                    let action =
                        modal.handle_paste(&text, &mut app.buf, &mut app.state, ctx, renderer)?;
                    if matches!(action, crate::modals::ModalAction::Close) {
                        app.active_modal = None;
                        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    }
                    return Ok(());
                }
            }
            // No modal: paste goes into the type-ahead buffer just like
            // keyboard input (Idle or Streaming, both consume it).
            if matches!(app.state.phase, UiPhase::Idle | UiPhase::Streaming) {
                // Image-paste detection — two parallel providers, mutually
                // exclusive on `text` shape:
                //   * `text` empty → terminal sent bracketed paste with
                //     no payload because the system clipboard holds image
                //     bytes, not text. Pull via `arboard`. Terminals with
                //     bracketed paste enabled go through here on Cmd+V.
                //   * `text` non-empty + parses as an image filesystem
                //     path → iTerm2 Cmd+V on image clipboard (saves to a
                //     temp file under
                //     `/var/folders/.../T/com.googlecode.iterm2/` and
                //     pastes the path instead of bytes), Finder
                //     drag-and-drop, kitty/wezterm drag-and-drop. Without
                //     this branch the user just sees the literal path
                //     string land in their input buffer — Cmd+V on iTerm2
                //     felt broken vs. Claude Code / Aider, which all do
                //     this same path-recognition.
                let image_paste: Option<(ImagePart, u64)> = if text.trim().is_empty() {
                    try_paste_clipboard_image()
                } else {
                    try_attach_image_from_path(&text)
                };
                if attach_image_to_input(app, ctx, renderer, image_paste)? {
                    return Ok(());
                }
                app.buf.insert_paste(text);
                if matches!(app.state.phase, UiPhase::Streaming) {
                    draw_spinner_now(
                        &mut app.state,
                        &app.buf,
                        ctx,
                        renderer,
                        app.message_queue.len(),
                        app.menu.selected,
                    );
                } else {
                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                }
            }
        }
        InputEvent::Eof => {}
        // Act on Press AND Repeat. Release is dropped (it would double-fire
        // every handler on Windows, where crossterm emits all three kinds
        // per keystroke).
        //
        // Repeat is what the Kitty protocol's `REPORT_EVENT_TYPES` bit
        // (enabled in lib.rs) turns OS key autorepeat into — without
        // accepting it, holding Left/Right/Backspace only moves one step
        // because every autorepeat tick gets dropped here. Accepting it
        // also doesn't cause runaway Submit on a held Enter: Submit
        // transitions to Streaming phase, and Streaming's Enter handler
        // doesn't submit again.
        //
        // Terminals that don't support `REPORT_EVENT_TYPES` (iTerm2 3.5+,
        // Apple Terminal) leak autorepeat as repeated Press events
        // instead; the reader-level `MODIFIER_ENTER_DEDUP` handles the
        // one case where that's harmful (modifier+Enter → spurious
        // newlines).
        InputEvent::Key(KeyEvent {
            kind: KeyEventKind::Press | KeyEventKind::Repeat,
            code,
            modifiers,
            ..
        }) => {
            // A capturing modal (the password prompt) installs mid-turn while a
            // tool runs (phase == Streaming). It must receive EVERY key
            // regardless of phase — otherwise typed chars leak into the
            // type-ahead `buf` and Esc/Ctrl+C cancel the whole turn instead of
            // the modal, so the modal's oneshot never fires and the askpass
            // server hangs. Route here BEFORE any phase dispatch. Non-capturing
            // modals (the default) keep their Idle-only routing below.
            if app.active_modal.as_ref().is_some_and(|m| m.captures_all_keys()) {
                let streaming = matches!(app.state.phase, UiPhase::Streaming);
                if let Some(modal) = app.active_modal.as_mut() {
                    let action = modal.handle_key(
                        code,
                        modifiers,
                        &mut app.buf,
                        &mut app.state,
                        ctx,
                        renderer,
                    )?;
                    if matches!(action, ModalAction::Close) {
                        app.active_modal = None;
                        if streaming {
                            draw_spinner_now(
                                &mut app.state,
                                &app.buf,
                                ctx,
                                renderer,
                                app.message_queue.len(),
                                app.menu.selected,
                            );
                        } else {
                            redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                        }
                    }
                }
                return Ok(());
            }
            // Modal trumps phase handlers when it's installed — /model,
            // /provider, /resume all install a modal and the event loop
            // funnels every keystroke through it until it reports Close.
            //
            // Exception: Ctrl+C is a global exit shortcut and must NOT
            // be trappable by any modal. The OnboardingWizard's Intro
            // screen explicitly promises "Ctrl+C exits anytime" — and
            // more broadly, the universal keyboard escape hatch should
            // never depend on whichever modal happens to be open
            // forwarding it. Dismiss the modal and send Shutdown so
            // the run-loop tears down cleanly.
            if matches!(app.state.phase, UiPhase::Idle)
                && code == KeyCode::Char('c')
                && modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                && app.active_modal.is_some()
            {
                app.active_modal = None;
                arm_shutdown_watchdog(ctx);
                return Ok(());
            }
            if matches!(app.state.phase, UiPhase::Idle) {
                if let Some(modal) = app.active_modal.as_mut() {
                    let action = modal.handle_key(
                        code,
                        modifiers,
                        &mut app.buf,
                        &mut app.state,
                        ctx,
                        renderer,
                    )?;
                    if matches!(action, ModalAction::Close) {
                        app.active_modal = None;
                        // IssueWizard signals a staged title+body via
                        // `ctx.pending_new_issue`. Drain + POST to the
                        // AtomGit API here and echo the created-issue
                        // URL into scrollback. Blocking call — the
                        // wizard is modal so UI freezing briefly is
                        // expected / acceptable.
                        if let Some(draft) = ctx.pending_new_issue.take() {
                            match atomcode_core::atomgit::Client::from_stored_auth().and_then(|c| {
                                c.create_issue(&draft.owner, &draft.repo, &draft.title, &draft.body)
                            }) {
                                Ok(created) => {
                                    let shown_url = created.html_url.clone().unwrap_or_else(|| {
                                        format!(
                                            "https://atomgit.com/{}/{}/issues/{}",
                                            draft.owner, draft.repo, created.number
                                        )
                                    });
                                    renderer.render(UiLine::CommandOutput(
                                        crate::i18n::t(crate::i18n::Msg::IssueCreated {
                                            number: created.number,
                                            title: &created.title,
                                            url: &shown_url,
                                        })
                                        .into_owned(),
                                    ));
                                }
                                Err(e) => {
                                    renderer.render(UiLine::CommandOutput(
                                        crate::i18n::t(crate::i18n::Msg::IssueCreateFailed {
                                            error: &format!("{:#}", e),
                                        })
                                        .into_owned(),
                                    ));
                                }
                            }
                            renderer.flush();
                        }
                        // OnboardingWizard signals its follow-up via two bool
                        // flags. Drain one, execute it here — the
                        // CodingPlan flow (which internally handles
                        // OAuth login when needed) needs suspend/resume
                        // of raw mode (only event-loop scope can drive
                        // that safely), and opening ProviderWizard is a
                        // Modal-to-Modal swap that needs mutable
                        // `active_modal` access the modals themselves
                        // don't have.
                        if std::mem::take(&mut ctx.pending_run_login_setup) {
                            crate::event_loop::commands::run_login_flow(renderer, ctx)?;
                        }
                        if std::mem::take(&mut ctx.pending_open_provider_wizard) {
                            let pw = crate::modals::ProviderWizard::MainMenu { selected: 0 };
                            app.active_modal = Some(Box::new(pw));
                            if let Some(m) = app.active_modal.as_mut() {
                                m.draw(&app.buf, &app.state, ctx, renderer);
                            }
                            // ProviderWizard owns the next frame now; skip
                            // the idle redraw below so we don't clobber it.
                            return Ok(());
                        }
                        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    }
                    return Ok(());
                }
            }
            // PageUp / PageDown / Home / End: scroll the body
            // viewport. Universal across phases — same as a terminal's
            // own scrollback navigation. RetainedRenderer and
            // PlainRenderer rely on the host terminal's native
            // scrollback, so these keys default to a no-op there.
            // We intercept BEFORE phase dispatch so scrolling works in
            // Idle / Streaming alike.

            // Ctrl+V: pull the system clipboard image and attach as
            // `[Image #N]` — independent of whether the host terminal
            // forwarded a Paste event for the keystroke. The status
            // line hint "Image in clipboard · ctrl+v to paste"
            // already promises this chord, but iTerm2's default Cmd+V
            // on an image-only clipboard sends NOTHING through the
            // PTY (no plaintext to paste, so iTerm2's Paste action
            // becomes a no-op), which made Cmd+V feel broken vs.
            // Claude Code on the same setup. Catching the literal
            // Ctrl+V (\x16, KeyCode::Char('v') + CONTROL) here closes
            // the gap on every terminal in one place — no per-host
            // OSC negotiation needed.
            //
            // For users who want Cmd+V muscle memory: remap iTerm2's
            // Cmd+V to "Send: 0x16" in Preferences → Profiles → Keys
            // → Key Mappings, then Cmd+V → Ctrl+V → this handler.
            //
            // Gated to Idle / Streaming. Approval and Suspended don't
            // accept input; modals (handled above) get first refusal.
            // Shift / Alt with Ctrl+V are excluded so reserved chords
            // (e.g. terminal-emulator-defined Ctrl+Shift+V "Paste as
            // Plain Text") still pass through to whatever else might
            // bind them in the future.
            if matches!(app.state.phase, UiPhase::Idle | UiPhase::Streaming)
                && code == crossterm::event::KeyCode::Char('v')
                && modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                && !modifiers.contains(crossterm::event::KeyModifiers::SHIFT)
                && !modifiers.contains(crossterm::event::KeyModifiers::ALT)
            {
                let img_hash = try_paste_clipboard_image();
                if attach_image_to_input(app, ctx, renderer, img_hash)? {
                    return Ok(());
                }
                // No image — fall back to clipboard text. Reaching this
                // branch means the host terminal forwarded Ctrl+V as a
                // real `\x16` key event rather than intercepting it as
                // bracketed paste or character injection (classic
                // conhost / older Windows Terminal configs / WT after
                // the user removed the `paste` keybind per our Windows
                // docs all hit this path). Without this fallback the
                // keystroke is silently swallowed and the user's text
                // paste disappears — a regression from before the
                // Ctrl+V → image handler existed.
                //
                // Routing through `InputEvent::Paste` instead of
                // `app.buf.insert_paste` directly so we get the modal-
                // first dispatch, the image-from-path check, and the
                // Streaming-vs-Idle redraw branching for free.
                if let Some(text) = try_paste_clipboard_text() {
                    return handle_input(app, ctx, renderer, InputEvent::Paste(text));
                }
                // Empty clipboard — Ctrl+V has no other binding
                // (key_action::classify maps it to NoOp), so swallow
                // silently rather than insert a literal `v`.
                return Ok(());
            }

            if let Some(handled) = handle_scroll_key(code, modifiers, renderer, &app.buf) {
                if handled {
                    return Ok(());
                }
            }
            match app.state.phase {
                UiPhase::Idle => handle_idle_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Streaming => handle_streaming_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Approval => handle_approval_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Suspended => {}
            }
        }
        // Release key events: drop on the floor. Press / Repeat are handled
        // above; Release is noise on Windows.
        InputEvent::Key(_) => {}
    }
    Ok(())
}

/// Try handling a scroll-related key (PageUp/PageDown/Home/End).
/// Returns:
///   - `Some(true)`  → key consumed; caller should skip phase dispatch
///   - `Some(false)` → key was a scroll key but not consumed (e.g.
///     Home/End with text in input buffer, where they should move
///     cursor instead)
///   - `None`        → not a scroll key at all
///
/// RetainedRenderer implements the scroll-related trait methods;
/// PlainRenderer uses the trait no-op defaults and silently falls
/// through to the existing phase dispatch (e.g. End-of-line cursor
/// movement during input).
fn handle_scroll_key(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    renderer: &mut dyn crate::render::Renderer,
    buf: &Buffer,
) -> Option<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};
    // Don't intercept Home/End when the user is editing a non-empty
    // buffer — those should move the cursor, not jump scrollback.
    // PageUp/PageDown and Shift+Up/Shift+Down always scroll regardless
    // (they're explicit scroll commands, not in-line editing keys).
    let buf_empty = buf.text.is_empty();
    let has_shift = modifiers.contains(KeyModifiers::SHIFT);
    match code {
        // Page-step. macOS keyboards: Fn+Up / Fn+Down generate
        // PageUp / PageDown. iTerm2 / Windows have dedicated keys.
        KeyCode::PageUp => {
            renderer.scroll_body(-10);
            Some(true)
        }
        KeyCode::PageDown => {
            renderer.scroll_body(10);
            Some(true)
        }
        // Message-jump scrolls. Alt+Up/Down jumps to prev/next message.
        // Ctrl+Up/Down jumps to prev/next user message.
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) && !has_shift => {
            renderer.scroll_to_prev_message();
            Some(true)
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) && !has_shift => {
            renderer.scroll_to_next_message();
            Some(true)
        }
        KeyCode::Up if modifiers.contains(KeyModifiers::CONTROL) && !has_shift => {
            renderer.scroll_to_prev_user_message();
            Some(true)
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::CONTROL) && !has_shift => {
            renderer.scroll_to_next_user_message();
            Some(true)
        }
        // Line-step. Shift+Up / Shift+Down is the cross-keyboard
        // alternative for users without a dedicated PageUp/Down key.
        // Bare Up/Down stays bound to input-history navigation
        // (Action::HistoryPrev/Next via key_action::map) for backward
        // compat with retained mode.
        KeyCode::Up if has_shift => {
            renderer.scroll_body(-1);
            Some(true)
        }
        KeyCode::Down if has_shift => {
            renderer.scroll_body(1);
            Some(true)
        }
        KeyCode::Home if buf_empty && modifiers.is_empty() => {
            renderer.scroll_body_to_top();
            Some(true)
        }
        KeyCode::End if buf_empty && modifiers.is_empty() => {
            renderer.scroll_body_to_bottom();
            Some(true)
        }
        _ => None,
    }
}

/// Slash-command palette state. Active whenever buf starts with '/'.
pub struct MenuState {
    pub selected: usize,
}

impl MenuState {
    pub fn new() -> Self {
        Self { selected: 0 }
    }
}

// `ModelPicker` moved to `crate::modals::model_picker`; re-exported at
// `crate::modals::ModelPicker` for existing call sites (execute_slash_command).
pub use crate::modals::ModelPicker;

// `SessionPicker` moved to `crate::modals::session_picker`; re-exported
// at `crate::modals::SessionPicker` for existing call sites.
pub use crate::modals::SessionPicker;

// `ProviderWizard` + `WizardStep` + `DraftProvider` moved to
// `crate::modals::provider_wizard`; re-exported at `crate::modals` for
// existing call sites (execute_slash_command).
pub use crate::modals::ProviderWizard;

/// Parse a committed `$<name> [args]` line into `(name, args)`. Returns `None`
/// when the line is not `$`-prefixed or carries no skill name. Mirrors
/// `parse_slash_line` but for the `$` skills trigger.
fn parse_dollar_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix('$')?;
    let trimmed = rest.trim_start();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let args = parts.next().unwrap_or("").trim();
    Some((name.to_string(), args.to_string()))
}

/// Build the second-level skills palette: user-invocable skills whose bare
/// name or fully-qualified `<ns>:<name>` starts with `prefix_lower`. A bare
/// name is shown when it is unique across the registry; otherwise the
/// qualified name is shown to disambiguate. Sorted for stable navigation.
/// Shared by the `/skills ` sub-mode and the `$` trigger so both stay in lockstep.
fn build_skill_menu_items(
    skill_registry: Option<&std::sync::RwLock<atomcode_core::skill::SkillRegistry>>,
    prefix_lower: &str,
) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = Vec::new();
    if let Some(reg) = skill_registry {
        if let Ok(reg) = reg.read() {
            let skills: Vec<_> = reg.user_invocable().collect();
            for skill in &skills {
                let bare = skill
                    .name
                    .split_once(':')
                    .map(|(_, s)| s)
                    .unwrap_or(skill.name.as_str());
                let full_lower = skill.name.to_ascii_lowercase();
                let bare_lower = bare.to_ascii_lowercase();
                if bare_lower.contains(prefix_lower) || full_lower.contains(prefix_lower) {
                    let bare_is_unique = skills.iter().all(|other| {
                        other.name == skill.name
                            || other
                                .name
                                .split_once(':')
                                .map(|(_, s)| s)
                                .unwrap_or(other.name.as_str())
                                != bare
                    });
                    let display = if bare_is_unique {
                        bare.to_string()
                    } else {
                        skill.name.clone()
                    };
                    items.push((display, skill.description.clone()));
                }
            }
        }
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

/// Filter the command registry by the buf's prefix after '/'. Returns the
/// (name, desc) pairs matching, or None if menu shouldn't show (buf doesn't
/// start with '/' or has whitespace, meaning the user has moved on to args).
/// Custom commands are appended after built-in matches; duplicates (custom
/// command with the same name as a built-in) are suppressed.
fn build_menu_items(
    buf: &str,
    cursor: usize,
    commands: &CommandRegistry,
    custom: &crate::custom_commands::CustomCommandRegistry,
    skill_registry: Option<&std::sync::RwLock<atomcode_core::skill::SkillRegistry>>,
    file_index: Option<&file_index::FileIndex>,
) -> Option<Vec<(String, String)>> {
    // `@`-mention branch — checked first so it takes priority over any
    // `/` interpretation.
    if let (Some(idx), Some(token)) = (file_index, file_index::detect_at_mention(buf, cursor)) {
        let (scope_dir, filter) = file_index::split_token(&token);
        let entries = idx.filter(&scope_dir, &filter);
        if entries.is_empty() {
            return None;
        }
        // Show the FULL relative path (including the scope prefix) so the
        // user always sees where they are. e.g. when scope is `crates/`,
        // list `crates/atomcode-cli/` not just `atomcode-cli/`.
        return Some(
            entries
                .into_iter()
                .map(|e| (e.rel_path, String::new()))
                .collect(),
        );
    }

    // `$`-trigger skills picker. A fast shortcut to the `/skills` palette:
    // `$` at the start of the buffer lists user-invocable skills under bare
    // names; `$bra` filters. A space (user typing args) closes the menu, the
    // same way `/skills <name> ` does. Shares `build_skill_menu_items` with
    // the `/skills ` sub-mode so contents stay identical.
    if let Some(after) = buf.strip_prefix('$') {
        if after.contains(char::is_whitespace) {
            return None;
        }
        let items = build_skill_menu_items(skill_registry, &after.to_ascii_lowercase());
        return if items.is_empty() { None } else { Some(items) };
    }

    if !buf.starts_with('/') {
        return None;
    }

    // Two-level palette for skills.
    //
    // Level 1 (top): the built-in `/skills` entry acts as a gateway —
    // it does NOT expand into individual skills here, so it cannot
    // crowd or collide with built-in / custom commands.
    //
    // Level 2 (sub-mode): once the user has typed `/skills ` (with a
    // trailing space, usually injected by the needs_args path on
    // Enter), this branch fires and lists user-invocable skills under
    // their bare names. Submission rewrites the committed line back
    // to `/skills <name>` so the `skills` arm in execute_slash_command
    // looks up `skills:<name>` in the registry and dispatches.
    if let Some(after) = buf.strip_prefix("/skills ") {
        if after.contains(char::is_whitespace) {
            return None;
        }
        let items = build_skill_menu_items(skill_registry, &after.to_ascii_lowercase());
        return if items.is_empty() { None } else { Some(items) };
    }

    // Two-level palette for `/effort` (same gateway pattern as `/skills`).
    // Once `/effort ` (trailing space) is in the buffer, list the three
    // reasoning-effort choices; submission commits `/effort <choice>`.
    if let Some(after) = buf.strip_prefix("/effort ") {
        if after.contains(char::is_whitespace) {
            return None;
        }
        let prefix = after.to_ascii_lowercase();
        let items: Vec<(String, String)> = [
            ("high", "Deeper reasoning (DeepSeek V4)"),
            ("max", "Maximum reasoning depth (DeepSeek V4)"),
            ("off", "Use the API default"),
        ]
        .into_iter()
        .filter(|(n, _)| n.starts_with(prefix.as_str()))
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .collect();
        return if items.is_empty() { None } else { Some(items) };
    }

    let rest = &buf[1..];
    // Once a space appears (user is typing args), stop showing menu.
    if rest.contains(char::is_whitespace) {
        return None;
    }
    let prefix_lower = rest.to_ascii_lowercase();
    // Top-level: built-ins (which now include the `/skills` gateway)
    // followed by custom commands. Individual skills are intentionally
    // hidden from this level — users access them via `/skills <name>`.
    let mut matches: Vec<(String, String)> = commands
        .matching_prefix(rest)
        .into_iter()
        .map(|c| {
            let desc = crate::commands::cmd_desc_i18n(c.name)
                .map(|cow| cow.into_owned())
                .unwrap_or_else(|| c.desc.to_string());
            (c.name.to_string(), desc)
        })
        .collect();
    for (name, desc) in custom.command_names_and_descriptions() {
        if name.starts_with(&prefix_lower) && !matches.iter().any(|(n, _)| *n == name) {
            matches.push((name, desc));
        }
    }
    let _ = skill_registry; // referenced only inside the sub-mode branch above
    if matches.is_empty() {
        None
    } else {
        Some(matches)
    }
}

/// Build the slash / @-mention menu for the current buffer, honoring the
/// buffer's suppression state: `is_in_history()` (a recalled `/session foo`)
/// and `menu_suppressed()` (a `/command` restored into the box on Esc-cancel).
/// All display paths go through this so the menu never pops for text the user
/// didn't type, regardless of which redraw fires.
fn menu_for_display(buf: &Buffer, ctx: &LoopCtx) -> Option<Vec<(String, String)>> {
    if buf.is_in_history() || buf.menu_suppressed() {
        return None;
    }
    build_menu_items(
        &buf.text,
        buf.cursor,
        &ctx.commands,
        &ctx.custom_commands,
        Some(&ctx.skill_registry),
        Some(&ctx.file_index),
    )
}

fn handle_idle_key(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Result<()> {
    // GOAL ESCAPE HATCH (Idle). A `/goal` continuation is driven SERVER-SIDE,
    // so the TUI can legitimately sit in Idle while the agent keeps looping
    // rounds. From Idle, Esc/Ctrl+C otherwise just clear the input / arm exit —
    // they never reach the cancel path (that lives in `handle_streaming_key`),
    // which is why a goal felt uninterruptible. When a goal is active, route
    // Ctrl+C and a bare Esc (empty buffer — don't steal Esc from clearing a
    // draft the user is editing to nudge the goal) to `Cancel`: the bridge
    // turns that into "clear the goal + cancel the running turn", and the
    // follow-up GoalUpdate(active=false) resets the local goal state. Belt and
    // suspenders alongside `on_thinking` keeping the TUI in Streaming.
    if app.state.goal_condition.is_some() {
        let is_ctrl_c = code == KeyCode::Char('c')
            && modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
        let is_bare_esc = code == KeyCode::Esc && app.buf.text.is_empty();
        if is_ctrl_c || is_bare_esc {
            cancel_active_turn(ctx);
            clear_capturing_modal_on_cancel(app);
            crate::tuix_trace!(
                "KEY",
                "idle goal-active {} -> Cancel",
                if is_ctrl_c { "Ctrl+C" } else { "Esc" }
            );
            return Ok(());
        }
    }
    // If the menu is active (buf starts with '/'), intercept navigation keys.
    // Suppress while scrolling history / right after a restore (see
    // `menu_for_display`) — otherwise a recalled `/se…` immediately re-pops.
    let menu_items = menu_for_display(&app.buf, ctx);
    if let Some(items) = &menu_items {
        // Clamp selection in range.
        if app.menu.selected >= items.len() {
            app.menu.selected = items.len() - 1;
        }
        match (code, modifiers) {
            (KeyCode::Up, _) => {
                // Wrap to the last item (mirror Down's modular wrap below).
                // The menu is fully modal — to reach input history with a
                // partial slash buffer like `/se`, press Esc or Backspace
                // to clear the buffer first.  Previously Up at index 0
                // cleared the buffer and fell through to history nav,
                // which felt like the menu had silently swallowed your
                // text and dumped you somewhere unexpected.
                app.menu.selected = if app.menu.selected == 0 {
                    items.len() - 1
                } else {
                    app.menu.selected - 1
                };
                redraw_with_menu(
                    &app.buf,
                    items,
                    app.menu.selected,
                    &app.state,
                    ctx,
                    renderer,
                );
                return Ok(());
            }
            (KeyCode::Down, _) => {
                app.menu.selected = (app.menu.selected + 1) % items.len();
                redraw_with_menu(
                    &app.buf,
                    items,
                    app.menu.selected,
                    &app.state,
                    ctx,
                    renderer,
                );
                return Ok(());
            }
            (KeyCode::Enter | KeyCode::Tab, m)
                if !m.contains(crossterm::event::KeyModifiers::SHIFT) =>
            {
                // Tab and Enter both pick the highlighted entry, but
                // they diverge on no-arg top-level commands:
                //   * Enter   → execute immediately (legacy behavior).
                //   * Tab     → complete only — rewrite the buffer to
                //               `/name ` and park the cursor, mirroring
                //               shell tab-completion. The user reviews
                //               the line and presses Enter to fire.
                // For @-mentions, `needs_args` commands, and the
                // `/skills` palette, both keys behave identically
                // because those branches were already complete-only.
                // Shift+Enter (hard newline) is excluded by the
                // modifier guard; crossterm reports Shift+Tab as
                // `KeyCode::BackTab` so it doesn't match this arm.
                //
                // `@`-mention selection: insert `@<full_path>` at the
                // token range. Directory candidates keep their trailing
                // slash so the token stays active for drill-down.
                if !items.is_empty() {
                    if let Some((at_pos, end)) =
                        file_index::detect_at_mention_range(&app.buf.text, app.buf.cursor)
                    {
                        // `items[selected].0` is the full relative path
                        // (e.g. `crates/atomcode-cli/`); prepend `@` without
                        // adding whitespace so directories can keep completing.
                        let selected_path = items[app.menu.selected].0.clone();
                        let replacement =
                            file_index::format_at_mention_replacement(&selected_path);
                        app.buf.text.replace_range(at_pos..end, &replacement);
                        app.buf.cursor = at_pos + replacement.len();
                        app.menu.selected = 0;
                        if let Some(next_items) = build_menu_items(
                            &app.buf.text,
                            app.buf.cursor,
                            &ctx.commands,
                            &ctx.custom_commands,
                            Some(&ctx.skill_registry),
                            Some(&ctx.file_index),
                        ) {
                            redraw_with_menu(&app.buf, &next_items, 0, &app.state, ctx, renderer);
                        } else {
                            redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                        }
                        return Ok(());
                    }
                }

                // Accept the highlighted command. Two shapes:
                //   * arg-less commands (e.g. /help, /quit, /login) → execute
                //     immediately on Enter, as before.
                //   * commands that require an arg (e.g. /background <task>) →
                //     auto-complete the name + trailing space and park the
                //     cursor so the user types the arg next. A SECOND Enter
                //     (once the arg is filled in) commits normally through
                //     the regular BufferResult::Commit → execute_slash_command
                //     path at the bottom of this function.

                // `$`-mode: items carry bare skill names. Tab completes to
                // `$name ` (review, then Enter); Enter invokes immediately.
                if app.buf.text.starts_with('$') && !items.is_empty() {
                    let name = items[app.menu.selected].0.clone();
                    app.menu.selected = 0;
                    if code == KeyCode::Tab {
                        app.buf.text = format!("${} ", name);
                        app.buf.cursor = app.buf.text.len();
                        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                        return Ok(());
                    }
                    // Enter → invoke now via the shared skills arm.
                    let committed = format!("${}", name);
                    renderer.render(UiLine::ClearTransient);
                    renderer.render(UiLine::User(committed.clone()));
                    app.buf.text.clear();
                    app.buf.cursor = 0;
                    ctx.history.push(crate::input::history::HistoryEntry {
                        text: committed.clone(),
                        images: Vec::new(),
                        pastes: Vec::new(),
                    });
                    app.state.last_submitted_message = Some(committed.clone());
                    execute_slash_command(
                        "skills",
                        &name,
                        &mut app.state,
                        ctx,
                        renderer,
                        &mut app.active_modal,
                        &mut app.fixissue_pending,
                        &mut app.fixissue_buffer,
                        &mut app.setup_pending,
                    )?;
                    if matches!(app.state.phase, UiPhase::Idle) {
                        app.state.last_submitted_message = None;
                        redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                    }
                    return Ok(());
                }

                let name = items[app.menu.selected].0.clone();
                let needs_args = ctx
                    .commands
                    .find(&name)
                    .map(|c| c.needs_args)
                    .unwrap_or(false);
                app.menu.selected = 0;

                if needs_args {
                    // Rewrite buffer to `/name ` and park cursor at the end.
                    // Menu rebuilds on next keystroke — with the trailing
                    // space parse_slash_line returns `Some(("name", ""))`
                    // so build_menu_items correctly hides the menu.
                    app.buf.text = format!("/{} ", name);
                    app.buf.cursor = app.buf.text.len();

                    // The `/skills` gateway is special: build_menu_items
                    // recognises the `/skills ` prefix and returns the
                    // second-level palette of skills. Render that
                    // immediately so the user doesn't see the menu blink
                    // out and reappear.
                    if name == "skills" {
                        if let Some(items) = build_menu_items(
                            &app.buf.text,
                            app.buf.cursor,
                            &ctx.commands,
                            &ctx.custom_commands,
                            Some(&ctx.skill_registry),
                            Some(&ctx.file_index),
                        ) {
                            app.menu.selected = 0;
                            redraw_with_menu(&app.buf, &items, 0, &app.state, ctx, renderer);
                            return Ok(());
                        }
                        // Empty sub-mode: build_menu_items returned None
                        // for the `/skills ` form, which at this point
                        // can only mean the registry has zero
                        // user-invocable skills (the filter is empty —
                        // we just appended a space — so there's no
                        // "no matches" case here, only "no skills").
                        // Without feedback the user sees `/skills `
                        // with no menu and concludes the feature is
                        // broken (reported by a Windows user with a
                        // clean install). Emit a one-time scrollback
                        // hint pointing at the install paths so they
                        // know what to do next; keep the buffer at
                        // `/skills ` so backspace still recovers.
                        renderer.render(UiLine::CommandOutput(
                            "  \u{24d8} No user-invocable skills installed yet.\n    \
                            \u{2022} Drop SKILL.md into ~/.atomcode/skills/<name>/ \n      \
                              (Windows: %USERPROFILE%\\.atomcode\\skills\\<name>\\)\n    \
                            \u{2022} Or install a plugin that ships skills via /plugin install <git-url>\n\n"
                                .into(),
                        ));
                    }

                    // `/effort` gateway: render the high/max/off sub-menu
                    // immediately so it doesn't blink out and reappear.
                    if name == "effort" {
                        if let Some(items) = build_menu_items(
                            &app.buf.text,
                            app.buf.cursor,
                            &ctx.commands,
                            &ctx.custom_commands,
                            Some(&ctx.skill_registry),
                            Some(&ctx.file_index),
                        ) {
                            app.menu.selected = 0;
                            redraw_with_menu(&app.buf, &items, 0, &app.state, ctx, renderer);
                            return Ok(());
                        }
                    }

                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    return Ok(());
                }

                // Sub-mode submit: items in the skills palette carry
                // bare names (e.g. "brainstorming"). Mirror the
                // `needs_args` branch above — Enter from the palette
                // auto-completes to `/skills <name> ` and parks the
                // cursor at the end so the user can append args
                // (passed to `/use_skill` as `argument`). A second
                // Enter (with or without args) commits through the
                // regular BufferResult::Commit path. Without this,
                // skills always fired without args, and there was no
                // way to pass `argument` into the skill from the
                // picker.
                // Sub-mode submit for `/effort`: the selected item is one of
                // high|max|off. Tab completes to `/effort <choice> ` (parked,
                // consistent with the top-level Tab≠Enter rule); Enter commits
                // `/effort <choice>` and executes via the regular dispatch path.
                let in_effort_sub_mode = app.buf.text.starts_with("/effort ");
                if in_effort_sub_mode {
                    if code == KeyCode::Tab {
                        app.buf.text = format!("/effort {} ", name);
                        app.buf.cursor = app.buf.text.len();
                        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                        return Ok(());
                    }
                    let committed = format!("/effort {}", name);
                    renderer.render(UiLine::ClearTransient);
                    renderer.render(UiLine::User(committed.clone()));
                    app.buf.text.clear();
                    app.buf.cursor = 0;
                    ctx.history.push(crate::input::history::HistoryEntry {
                        text: committed.clone(),
                        images: Vec::new(),
                        pastes: Vec::new(),
                    });
                    if let Some((cmd, arg)) = parse_slash_line(&committed) {
                        execute_slash_command(
                            cmd,
                            arg,
                            &mut app.state,
                            ctx,
                            renderer,
                            &mut app.active_modal,
                            &mut app.fixissue_pending,
                            &mut app.fixissue_buffer,
                            &mut app.setup_pending,
                        )?;
                        if matches!(app.state.phase, UiPhase::Idle) {
                            redraw_after_slash(
                                &app.buf,
                                &app.state,
                                ctx,
                                &app.active_modal,
                                renderer,
                            );
                        }
                    }
                    return Ok(());
                }

                let in_skills_sub_mode = app.buf.text.starts_with("/skills ");
                if in_skills_sub_mode {
                    app.buf.text = format!("/skills {} ", name);
                    app.buf.cursor = app.buf.text.len();
                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    return Ok(());
                }

                // Top-level no-arg command (e.g. /quit, /help).
                // Tab → complete-only: insert `/name ` and park the
                // cursor so the user can review/edit before pressing
                // Enter to fire. The trailing space causes
                // build_menu_items to hide the menu on the next redraw
                // (parse_slash_line treats `/name ` as a fully-named
                // command with empty arg).
                if code == KeyCode::Tab {
                    app.buf.text = format!("/{} ", name);
                    app.buf.cursor = app.buf.text.len();
                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    return Ok(());
                }

                // Enter: execute immediately, as before.
                let committed = format!("/{}", name);
                renderer.render(UiLine::ClearTransient);
                renderer.render(UiLine::User(committed.clone()));
                app.buf.text.clear();
                app.buf.cursor = 0;
                // Mirror the regular-message and queued-message paths
                // below: pushing the just-submitted line into `ctx.history`
                // is what Up-arrow recall reads from. Without this, a
                // slash command executed from the menu vanishes from
                // history the moment it runs.
                ctx.history.push(crate::input::history::HistoryEntry {
                    text: committed.clone(),
                    images: Vec::new(),
                    pastes: Vec::new(),
                });
                if let Some((cmd, arg)) = parse_slash_line(&committed) {
                    if cmd.eq_ignore_ascii_case("paste") {
                        // `/paste` needs `&mut app.buf` to insert the
                        // `[Image #N]` marker at the cursor, which the
                        // `execute_slash_command` signature doesn't
                        // expose; short-circuit to the local handler.
                        handle_paste_command(app, ctx, renderer)?;
                    } else {
                        execute_slash_command(
                            cmd,
                            arg,
                            &mut app.state,
                            ctx,
                            renderer,
                            &mut app.active_modal,
                            &mut app.fixissue_pending,
                            &mut app.fixissue_buffer,
                            &mut app.setup_pending,
                        )?;
                    }
                    if matches!(app.state.phase, UiPhase::Idle) {
                        redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                    }
                }
                return Ok(());
            }
            (KeyCode::Esc, _) => {
                // Close menu by clearing buffer.
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                return Ok(());
            }
            _ => {} // fall through to buffer edits
        }
    }

    // Tab toggles Plan/Build mode when no completion menu is visible —
    // there is nothing to complete, so the key is repurposed for mode
    // switching instead.
    if code == KeyCode::Tab && menu_items.is_none() {
        app.state.agent_mode = app.state.agent_mode.toggle();
        let is_plan = matches!(app.state.agent_mode, crate::state::AgentMode::Plan);
        ctx.agent
            .cmd_tx
            .send(AgentCommand::SetPlanMode(is_plan))
            .ok();
        renderer.render(UiLine::CommandOutput(format!(
            "  Switched to {} mode.\n",
            app.state.agent_mode.label()
        )));
        renderer.flush();
        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
        return Ok(());
    }

    if code == KeyCode::Esc
        && modifiers.is_empty()
        && menu_items.is_none()
        && app.buf.text.is_empty()
    {
        match intercept_empty_bare_esc(&mut app.esc_undo_pending, std::time::Instant::now()) {
            EmptyEscIntercept::Consumed => {
                app.exit_pending = None;
                // Surface the second-press affordance, mirroring the
                // "press Ctrl+C again to exit" hint — otherwise the first
                // Esc is silently swallowed and the undo gesture is
                // undiscoverable (and a reflexive double-tap surprises).
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::EscAgainToUndo).into_owned(),
                ));
                renderer.flush();
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                return Ok(());
            }
            EmptyEscIntercept::TriggerUndo => {
                app.exit_pending = None;
                dispatch_undo("", &app.state, ctx, renderer);
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                return Ok(());
            }
        }
    }

    // Ctrl+V: try clipboard image first, fall back to text paste.
    if code == KeyCode::Char('v') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        if let Some((img, hash)) = try_paste_clipboard_image() {
            // Refuse to attach an image when there is no path for it to
            // reach a vision-capable model — neither the active provider
            // accepts images, nor a vision_preprocessor is configured to
            // OCR them first. Without this gate, sending burns a turn on
            // a 400 from the upstream's param validator (e.g.
            // ModelArts.81001 "message[N].content[0] has invalid
            // field(s): text, type" for GLM-5.1). Helper in
            // `Config::can_handle_attached_images`.
            if !ctx.config.can_handle_attached_images() {
                renderer.render(UiLine::Error(
                    crate::i18n::t(crate::i18n::Msg::ModelNoImageSupport {
                        model: &ctx.model_name,
                    })
                    .into_owned(),
                ));
                renderer.flush();
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                return Ok(());
            }
            // Insert the `[Image #N]` marker into the input buffer at
            // cursor — same pattern as `insert_paste` for long text.
            // The marker echoes through to scrollback on submit; image
            // bytes are stashed in `pending_images` and drained then.
            // N comes from `session_image_count` (monotonic across
            // turns), NOT `pending_images.len()+1` — otherwise turn 1's
            // first paste and turn 2's first paste would both render as
            // `[Image #1]` in scrollback, ambiguous when scrolling back.
            app.state.session_image_count += 1;
            let n = app.state.session_image_count;
            app.state.pending_images.push(img.clone());
            app.state.pending_image_hashes.push(hash);
            app.state.pending_image_markers.push(n);
            cache_write_image(&crate::platform::image_cache_dir(), &img, hash);
            let marker = format!("[Image #{}]", n);
            app.buf.text.insert_str(app.buf.cursor, &marker);
            app.buf.cursor += marker.len();
            redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
            return Ok(());
        }
        // No image in clipboard — fall through to normal key handling
        // (the `v` char will be inserted as a regular character via classify).
    }

    // Ctrl+T cycles reasoning_effort
    if code == KeyCode::Char('t') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        if !reasoning_effort_applicable_on_provider(ctx) {
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::ReasoningEffortNoEffect).into_owned(),
            ));
            renderer.flush();
            redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
            return Ok(());
        }
        let new_val = app.state.cycle_reasoning_effort();
        ctx.reasoning_effort = new_val.map(|s| s.to_string());
        persist_reasoning_effort(ctx);
        let msg = match new_val {
            Some(v) => format!("  reasoning_effort → {}\n", v),
            None => "  reasoning_effort cleared (API default)\n".into(),
        };
        renderer.render(UiLine::CommandOutput(msg));
        renderer.flush();
        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
        return Ok(());
    }

    // Multi-line cursor nav (idle path). Mirror of the streaming-mode
    // handler: in a buffer with embedded newlines, plain Up/Down walks
    // through the lines first; only when the cursor is already on the
    // first/last line does it surface as HistoryPrev/Next. Gated to
    // "no modifiers" so Shift+Up (body scroll) and other compound keys
    // still classify normally.
    if modifiers.is_empty() {
        match code {
            KeyCode::Up if app.buf.cursor_line_up() => {
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                return Ok(());
            }
            KeyCode::Down if app.buf.cursor_line_down() => {
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                return Ok(());
            }
            _ => {}
        }
    }

    let action = classify(code, modifiers);

    let result = app.buf.apply(action, ctx.history.entries(), &ctx.commands);
    sync_recalled_attachments(&mut app.state, &app.buf, ctx.history.entries());
    crate::tuix_trace!(
        "KEY",
        "idle result={} buf_len={} cursor={}",
        match &result {
            BufferResult::NoOp => "NoOp",
            BufferResult::Redraw => "Redraw",
            BufferResult::Commit(_) => "Commit",
            BufferResult::Exit => "Exit",
        },
        app.buf.text.len(),
        app.buf.cursor
    );
    // Any key that's not the Ctrl+C-on-empty-buffer exit path resets the
    // "press again to exit" arming — otherwise the prompt would stick around
    // across arbitrary edits, defeating the point of a short time window.
    if !matches!(result, BufferResult::Exit) {
        app.exit_pending = None;
    }
    if code != KeyCode::Esc || !modifiers.is_empty() || !app.buf.text.is_empty() {
        app.esc_undo_pending = None;
    }
    match result {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            // Rebuild menu after buf change. Same suppression gate as above
            // (history recall / restored command) via `menu_for_display`.
            let items = menu_for_display(&app.buf, ctx);
            if let Some(items) = items {
                if app.menu.selected >= items.len() {
                    app.menu.selected = 0;
                }
                redraw_with_menu(
                    &app.buf,
                    &items,
                    app.menu.selected,
                    &app.state,
                    ctx,
                    renderer,
                );
            } else {
                app.menu.selected = 0;
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
            }
        }
        BufferResult::Commit(line) => {
            renderer.render(UiLine::ClearTransient);
            app.buf.text.clear();
            app.buf.cursor = 0;
            // NB: `app.buf.clear_pastes()` is deferred until AFTER the
            // submit path calls `expand_pastes(&line)` — wiping the
            // paste Vec here used to leave `expand_pastes` with
            // nothing to substitute, so the agent received the raw
            // `[Pasted #N +M lines]` placeholder instead of the
            // pasted body and answered "I don't see any pasted
            // content". Mirrors the queue branch below which already
            // clears AFTER expansion.
            app.menu.selected = 0;
            // `$<name> [args]` committed (Tab-parked then Enter, or typed in
            // full). Resolve to a user-invocable skill and dispatch through the
            // same `skills` arm as `/skills <name> <args>`; the user-visible
            // echo stays `$name` so `/skills` never appears.
            if let Some((skill_name, skill_args)) = parse_dollar_line(&line) {
                let is_user_skill = ctx
                    .skill_registry
                    .read()
                    .ok()
                    .and_then(|r| r.get(&skill_name).map(|s| s.user_invocable))
                    .unwrap_or(false);
                if is_user_skill {
                    renderer.render(UiLine::User(line.clone()));
                    ctx.history.push(crate::input::history::HistoryEntry {
                        text: line.clone(),
                        images: Vec::new(),
                        // Registry still live here (cleared after dispatch);
                        // carry it so recall re-expands any folded paste.
                        pastes: app.buf.pastes.clone(),
                    });
                    app.state.last_submitted_message = Some(line.clone());
                    let arg = if skill_args.is_empty() {
                        skill_name.clone()
                    } else {
                        format!("{} {}", skill_name, skill_args)
                    };
                    execute_slash_command(
                        "skills",
                        &arg,
                        &mut app.state,
                        ctx,
                        renderer,
                        &mut app.active_modal,
                        &mut app.fixissue_pending,
                        &mut app.fixissue_buffer,
                        &mut app.setup_pending,
                    )?;
                    if matches!(app.state.phase, UiPhase::Idle) {
                        app.state.last_submitted_message = None;
                        redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                    }
                    app.buf.clear_pastes();
                    return Ok(());
                }
                // Not a known skill: fall through so `$foo` is sent as a
                // normal message (e.g. "$5 budget" keeps working).
            }
            // Only treat `/name …` as a slash command when `name` is
            // actually registered. Unrecognised `/foo …` (e.g. the user
            // typed `/test 文件下有哪些文件` meaning to *ask about*
            // `/test`, or just `/test` as a question) falls through to
            // the regular message path — better than the old
            // "Unknown command: /foo" dead-end.
            let as_slash = parse_slash_line(&line).filter(|(cmd, _)| {
                ctx.commands.find(cmd).is_some()
                    // Use `resolve()` (not exact-key `get()`) so a plugin
                    // command keyed `plugin:name` is recognised when typed as
                    // the bare `/name` — matching how dispatch renders it.
                    // Otherwise `/wechat` (keyed `weixin:wechat`) fails the
                    // gate and falls through to the agent as plain text.
                    || ctx.custom_commands.resolve(cmd).is_some()
                    || ctx
                        .skill_registry
                        .read()
                        .ok()
                        .and_then(|r| r.get(cmd).map(|s| s.user_invocable))
                        .unwrap_or(false)
            });
            if let Some(bash_cmd) = parse_bash_command(&line) {
                // `!cmd` — user-invoked bash mode. Echo the line, hand off
                // to the agent loop (executes + records context, no turn).
                renderer.render(UiLine::User(line.clone()));
                ctx.agent
                    .cmd_tx
                    .send(AgentCommand::LocalShell {
                        cmd: bash_cmd.to_string(),
                    })
                    .ok();
                // `!` lines carry no pastes/images; submit consumes the buffer.
                app.buf.clear_pastes();
                if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                }
            } else if let Some((cmd, arg)) = as_slash {
                // Slash commands carry no image markers — echo the
                // user line as-typed, before dispatch.
                renderer.render(UiLine::User(line.clone()));
                // Push into the Up-arrow recall buffer so the just-typed
                // command isn't lost the moment it executes. Mirrors the
                // regular-message branch below; History::push dedups
                // against the previous entry and ignores empty text.
                ctx.history.push(crate::input::history::HistoryEntry {
                    text: line.clone(),
                    images: Vec::new(),
                    // `/goal <pasted body>` recall must re-expand too — the
                    // registry is still live here (cleared after dispatch).
                    pastes: app.buf.pastes.clone(),
                });
                // Stash for Esc-restore, same as regular messages. Most slash
                // commands run synchronously (cleared in the Idle branch
                // below); the streaming ones (e.g. `/skills <name>` running a
                // subagent) keep it so cancelling restores the command line.
                app.state.last_submitted_message = Some(line.clone());
                if cmd.eq_ignore_ascii_case("paste") {
                    // See `handle_paste_command` — short-circuited
                    // here because the dispatcher signature can't
                    // hand it `&mut app.buf`.
                    handle_paste_command(app, ctx, renderer)?;
                } else {
                    // Expand `[Pasted #N …]` placeholders in the argument
                    // before dispatch, exactly like the regular-message
                    // path below. Without this, `/goal <pasted body>`
                    // hands the command the literal placeholder string
                    // (e.g. "[Pasted #1 +69 lines]") instead of the real
                    // pasted text. The paste registry is still live here —
                    // it's cleared a few lines down, after dispatch.
                    let arg = app.buf.expand_pastes(arg);
                    execute_slash_command(
                        cmd,
                        &arg,
                        &mut app.state,
                        ctx,
                        renderer,
                        &mut app.active_modal,
                        &mut app.fixissue_pending,
                        &mut app.fixissue_buffer,
                        &mut app.setup_pending,
                    )?;
                }
                if matches!(app.state.phase, UiPhase::Idle) {
                    // Finished synchronously — nothing running to cancel, so
                    // drop the restore stash (avoid a stale command lingering).
                    app.state.last_submitted_message = None;
                    redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                } else if matches!(app.state.phase, UiPhase::Approval) {
                    // After /bg <N> resume into an approval-waiting session,
                    // redraw the footer with an empty input box. Don't use
                    // draw_spinner_now because spinner_label was cleared by
                    // on_turn_complete() — it would show "◓ …" which is
                    // misleading. The next agent event (ApprovalNeeded /
                    // TurnComplete) will update the footer naturally.
                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                }
                // Slash commands don't consume pastes (they take a
                // single short arg, not a pasted body), but the submit
                // semantically consumes the buffer — drop them so the
                // next message starts with a clean paste registry.
                app.buf.clear_pastes();
            } else {
                // Hydrate recalled attachments BEFORE echoing the user
                // line, so `[Image #N]` markers in the visible body
                // match the renumbered markers that the
                // `└ [Image #N]` post-submit echo (and the actual
                // submit payload) use. Without this, an arrow-up
                // recall + edit would render `[Image #1]` in the body
                // while the echo + payload carry `[Image #2]` — the
                // user reasonably reads that as a bug ("two different
                // numbers for the same image").
                let cache_dir = crate::platform::image_cache_dir();
                let mut line = line; // shadow as mutable so hydrate can rewrite it
                for n in hydrate_recalled_attachments(&mut app.state, &mut line, &cache_dir) {
                    renderer.render(UiLine::Warning(n));
                }
                let mut expanded = app.buf.expand_pastes(&line);
                // Snapshot the live paste registry BEFORE clearing it, so
                // the history entry can carry the original paste bodies.
                // Up-arrow recall rehydrates `Buffer.pastes` from this so a
                // recalled message's `[Pasted #N …]` placeholder expands to
                // the real content instead of being sent literally (#843).
                let submitted_pastes = app.buf.pastes.clone();
                // Pastes have now been substituted into `expanded`;
                // safe to drop the registry. Doing it any earlier
                // (e.g. up at the buf.text.clear() prep) was the
                // exact bug that made the agent see only the
                // `[Pasted #N]` placeholder.
                app.buf.clear_pastes();
                // Cache the full expanded form before dispatch. If the
                // user hits Ctrl+C / Esc mid-stream, `handle_streaming_key`
                // takes this Option and restores it to `app.buf.text`
                // so the cancelled message can be edited and resent.
                app.state.last_submitted_message = Some(expanded.clone());
                // Only attach images whose `[Image #N]` marker survived
                // editing — if the user deleted the marker from the input
                // buffer, the corresponding image must not be sent. Echo
                // the kept images as `└ [Image #N]` sub-lines so scrollback
                // shows what was actually sent.
                let pending = std::mem::take(&mut app.state.pending_images);
                let pending_markers = std::mem::take(&mut app.state.pending_image_markers);
                let pending_hashes = std::mem::take(&mut app.state.pending_image_hashes);
                let mut images: Vec<ImagePart> = Vec::with_capacity(pending.len());
                let mut kept_markers: Vec<usize> = Vec::with_capacity(pending.len());
                let mut kept_refs: Vec<crate::input::history::HistoryImageRef> =
                    Vec::with_capacity(pending.len());
                // Use the marker `n` recorded at paste time, NOT the index.
                // Once `session_image_count` became monotonic, paste-time
                // markers diverge from positional indices — using the index
                // would silently drop every image after the first turn that
                // had a paste.
                for ((img, n), hash) in pending
                    .into_iter()
                    .zip(pending_markers.into_iter())
                    .zip(pending_hashes.into_iter())
                {
                    if line.contains(&format!("[Image #{}]", n)) {
                        kept_refs.push(crate::input::history::HistoryImageRef {
                            hash: format!("{:016x}", hash),
                            mt: img.media_type.clone(),
                            n,
                        });
                        images.push(img);
                        kept_markers.push(n);
                    }
                }
                // Recognize bare image paths that were typed / pasted as
                // keystrokes (notably Windows conhost paste) and never went
                // through the `InputEvent::Paste` image detection. Mutates
                // `expanded` (path token -> `[Image #N]`) and appends to
                // `images` / `kept_markers`. `last_submitted_message` was
                // already cached above from the raw form, so Ctrl+C edit
                // restores the editable path, not the marker.
                attach_typed_image_paths(
                    app,
                    ctx,
                    &mut expanded,
                    &mut images,
                    &mut kept_markers,
                );
                if ctx.sync_session.is_none() {
                    // Echo the EXACT text the agent receives (`expanded`): the
                    // full pasted body with `[Pasted #N …]` placeholders expanded
                    // and typed image paths already rewritten to `[Image #N]`
                    // markers — never the opaque placeholder. The input box and
                    // Up-arrow history below stay folded (compact `line`). Sync
                    // mode echoes via AgentEvent::UserEcho, so this branch is the
                    // local-TUI path only.
                    renderer.render(UiLine::UserWithAttachments {
                        text: expanded.clone(),
                        attachments: kept_markers.clone(),
                    });
                }
                renderer.flush();
                ctx.history.push(crate::input::history::HistoryEntry {
                    text: line.clone(),
                    images: kept_refs,
                    pastes: submitted_pastes,
                });
                // Clear stale hook warning at the start of each turn.
                if let Ok(mut slot) = ctx.hook_warning_hint.lock() {
                    *slot = None;
                }
                if let Some(live) = &ctx.sync_session {
                    // 同步模式：投递到 LiveSession。
                    use atomcode_core::live::UserInput;
                    live.send_input(UserInput {
                        text: expanded,
                        images,
                    });
                    app.state.on_submit();
                } else {
                    // —— 原有逻辑，原样保留 ——
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::SendMessage {
                            text: expanded,
                            images,
                            image_markers: kept_markers,
                        })
                        .ok();
                    app.state.on_submit();
                    // CodingPlan drift check — fire before every turn sent
                    // to a CodingPlan-managed provider, gated by a 15-min
                    // cooldown so rapid-fire messages don't spam the API.
                    // Non-CodingPlan users skip entirely (zero network).
                    if monitor::is_codingplan_provider(&ctx.config.default_provider) {
                        let cooled = ctx
                            .monitor_last_check_at
                            .map(|t| t.elapsed() >= monitor::CHECK_COOLDOWN)
                            .unwrap_or(true);
                        if cooled {
                            ctx.monitor_last_check_at = Some(std::time::Instant::now());
                            monitor::spawn_check(
                                ctx.config.clone(),
                                ctx.model_name.clone(),
                                ctx.monitor_warning.clone(),
                                ctx.wake_tx.clone(),
                            );
                        }
                    }
                }
            }
        }
        BufferResult::Exit => {
            let now = std::time::Instant::now();
            let armed = app
                .exit_pending
                .is_some_and(|t| now.duration_since(t) <= CTRL_C_EXIT_WINDOW);
            if armed {
                arm_shutdown_watchdog(ctx);
            } else {
                app.exit_pending = Some(now);
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::CtrlCAgainToExit).into_owned(),
                ));
                renderer.flush();
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
            }
        }
    }
    Ok(())
}

fn redraw_with_menu(
    buf: &Buffer,
    items: &[(String, String)],
    selected: usize,
    state: &UiState,
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
) {
    let kind = if file_index::detect_at_mention_range(&buf.text, buf.cursor).is_some() {
        crate::render::MenuKind::AtMention
    } else if buf.text.starts_with('$') {
        crate::render::MenuKind::Skill
    } else {
        crate::render::MenuKind::SlashCommand
    };
    let payload = crate::render::MenuPayload {
        items: items.to_vec(),
        selected,
        kind,
    };
    let attachments = compute_input_attachments(state, &buf.text);
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu: Some(payload),
        status: build_status(state, ctx),
        attachments,
    });
    // Footer-only UiLines (InputPrompt / StreamingBox) update widget
    // state and set `dirty = true` but emit NO bytes — the real paint
    // happens on the next `flush_deferred` tick (~5ms cadence,
    // configured in event_loop's select loop). Calling `renderer.flush()`
    // here would queue a `RenderCmd::Flush` that hits an empty BufWriter,
    // which on Linux/macOS terminals is a sub-µs no-op but on Windows
    // OpenConsole / xterm.js costs 1–3ms per arrow keypress (each Flush
    // forces a WriteFile syscall + VT-parser cycle in the host). The
    // 5ms paint tick is well below human perception, so dropping the
    // explicit flush has zero UX cost and removes the per-key syscall.
}

/// Synchronize `state.pending_recalled_attachments` with whatever
/// history entry the buffer is currently showing. Called after every
/// `buf.apply()` so:
///   - HistoryPrev/Next sets the recalled attachments to the new entry
///   - Insert/Delete (which clear `history_idx` to None) only drop the
///     refs whose `[Image #N]` marker is no longer in `buf.text`. A
///     user who arrow-up'd a `[Image #1]这是什么？` entry and then
///     appended `还有一个问题` should keep the image attached on
///     submit — the marker is still there, so `hydrate_recalled_attachments`
///     can still match it. Wiping wholesale (the prior behaviour) sent
///     the literal `[Image #1]` as text and silently dropped the bytes.
pub(crate) fn sync_recalled_attachments(
    state: &mut UiState,
    buf: &Buffer,
    history: &[crate::input::history::HistoryEntry],
) {
    match buf.history_idx() {
        Some(i) if i < history.len() => {
            state.pending_recalled_attachments = history[i].images.clone();
        }
        _ => {
            state
                .pending_recalled_attachments
                .retain(|r| buf.text.contains(&format!("[Image #{}]", r.n)));
        }
    }
}

/// Emit the terminal tab/window title for the current session, but only when
/// it differs from what was last emitted (`last`). Called at the top of every
/// event-loop iteration; `last` starts as `None` so the first pass always
/// emits (startup fallback), and each session-name change (auto-name, `/rename`,
/// `/resume`, `/new`) is picked up on the next iteration.
///
/// Fallback for un-named / brand-new sessions is `atomcode v<version>`, so a
/// fresh tab shows the running version instead of whatever stale string the
/// launcher/shortcut left behind (the original `atomcode-v4.25.6`-lingering bug).
fn sync_terminal_title(ctx: &LoopCtx, renderer: &mut dyn Renderer, last: &mut Option<String>) {
    const VERSION_FALLBACK: &str = concat!("atomcode v", env!("CARGO_PKG_VERSION"));
    let title = crate::title::session_terminal_title(&ctx.current_session.name, VERSION_FALLBACK);
    if last.as_deref() != Some(title.as_str()) {
        renderer.set_title(title.clone());
        *last = Some(title);
    }
}

/// Idle prompt without any menu/picker — used by the common
/// "Redraw" path and the post-event-loop fallback after an agent
/// event returns the UI to Idle.
fn redraw_idle_plain(buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
    let attachments = compute_input_attachments(state, &buf.text);
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu: None,
        status: build_status(state, ctx),
        attachments,
    });
    // No explicit flush — see `redraw_with_menu` for the rationale
    // (InputPrompt is footer-only, the 5ms `flush_deferred` tick owns
    // the actual paint, and `out.flush()` on every keystroke is a
    // measurable per-keypress syscall on Windows OpenConsole / xterm.js).
}

/// True iff startup should auto-open the OnboardingWizard:
/// no providers configured AND no OAuth login on disk AND we're
/// running in an interactive renderer. Plain mode (CI / pipe /
/// non-TTY) falls through to the "no provider configured" status
/// hint instead — the bordered-panel wizard can't sensibly run
/// without a human watching keystrokes.
pub(crate) fn should_auto_show_onboarding(ctx: &LoopCtx) -> bool {
    if ctx.is_plain_renderer {
        return false;
    }
    ctx.config.providers.is_empty() && atomcode_core::auth::get_stored_auth().is_none()
}

/// True iff startup should show the one-shot `/setup` hint in scrollback.
/// Returns `true` when either:
///   - `setup-state.json` doesn't exist (never ran `/setup`), OR
///   - the recommender skill directory is missing (user deleted it after setup).
fn should_auto_show_setup(ctx: &LoopCtx) -> bool {
    let state = atomcode_core::setup::state::load_setup_state(&ctx.working_dir);
    if state.is_none() {
        return true; // never ran setup → show hint
    }

    // setup-state.json exists but the skill may have been deleted manually.
    // Path must match SkillRegistry::reload's scan path: the unified
    // Config::config_dir() (== ATOMCODE_HOME when set, else ~/.atomcode).
    let skill_dir = atomcode_core::config::Config::config_dir()
        .join("skills")
        .join("atomcode-automation-recommender");
    !skill_dir.exists()
}

/// Extract current + latest version from the `ALREADY_LATEST` error
/// body. The shape is fixed by `self_update.rs`:
///   `already on {current} (latest is {latest}). Pass --force to reinstall.`
/// Returns None if the format ever drifts — caller falls back to "?"
/// placeholders so the localized sentence still renders cleanly.
fn parse_already_latest_versions(s: &str) -> Option<(&str, &str)> {
    let after_on = s.strip_prefix("already on ")?;
    let (current, rest) = after_on.split_once(" (latest is ")?;
    let latest = rest.strip_suffix(". Pass --force to reinstall.")?;
    let latest = latest.strip_suffix(')')?;
    Some((current, latest))
}

#[cfg(test)]
mod parse_already_latest_versions_tests {
    use super::parse_already_latest_versions;
    #[test]
    fn extracts_both_versions() {
        let s = "already on v4.22.2 (latest is v4.22.2). Pass --force to reinstall.";
        assert_eq!(
            parse_already_latest_versions(s),
            Some(("v4.22.2", "v4.22.2"))
        );
    }
    #[test]
    fn rejects_unrelated_strings() {
        assert!(parse_already_latest_versions("something else entirely").is_none());
    }
}

#[cfg(test)]
mod image_marker_tests {
    use super::image_markers_in_order as marks;

    #[test]
    fn extracts_markers_in_order_deduped() {
        assert_eq!(marks("[Image #1] hi"), vec![1]);
        assert_eq!(marks("[Image #2] and [Image #5] and [Image #2]"), vec![2, 5]);
        assert_eq!(marks("no images here"), Vec::<usize>::new());
        // Malformed markers are ignored.
        assert_eq!(marks("[Image #] [Image #x] [Image #3]"), vec![3]);
    }
}

#[cfg(test)]
mod streaming_slash_tests {
    use super::streaming_executable_slash as exec;

    #[test]
    fn goal_halt_subcommands_run_mid_stream() {
        // The whole point: `/goal clear` (and its aliases) must execute while a
        // turn is running, because a server-driven goal keeps the TUI in
        // Streaming where commands are otherwise blocked.
        for sub in ["clear", "stop", "off", "reset", "none", "cancel"] {
            let got = exec(&format!("/goal {sub}"));
            assert_eq!(got, Some(("goal".to_string(), sub.to_string())), "sub={sub}");
        }
    }

    #[test]
    fn bg_no_arg_runs_but_setting_a_goal_does_not() {
        assert_eq!(exec("/bg"), Some(("bg".to_string(), String::new())));
        // Backgrounding a NEW message and SETTING a new goal must NOT run mid-stream.
        assert_eq!(exec("/bg go do a thing"), None);
        assert_eq!(exec("/goal write all the tests"), None);
        assert_eq!(exec("/goal"), None); // bare /goal = status, not whitelisted
    }

    #[test]
    fn quit_and_exit_run_mid_stream() {
        assert_eq!(exec("/quit"), Some(("quit".to_string(), String::new())));
        assert_eq!(exec("/exit"), Some(("exit".to_string(), String::new())));
        assert_eq!(exec("/quit now"), None);
    }

    #[test]
    fn unrelated_commands_and_non_slash_are_blocked() {
        assert_eq!(exec("/model"), None);
        assert_eq!(exec("/clear"), None);
        assert_eq!(exec("just a message"), None);
        // Case-insensitive command + sub.
        assert_eq!(exec("/GOAL Clear"), Some(("goal".to_string(), "Clear".to_string())));
    }
}

/// Redraw after running a slash command. If the command installed a
/// modal, delegate the draw to it so the modal's menu appears; otherwise
/// fall through to the plain idle prompt.
///
/// Replaces the old per-picker `redraw_idle` that hard-coded payload
/// construction for model/session. New modals just implement `draw`.
fn redraw_after_slash(
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    active_modal: &Option<Box<dyn crate::modals::Modal>>,
    renderer: &mut dyn Renderer,
) {
    if let Some(modal) = active_modal.as_ref() {
        modal.draw(buf, state, ctx, renderer);
    } else {
        redraw_idle_plain(buf, state, ctx, renderer);
    }
}

/// Persist config changes and notify the daemon to pick them up.
/// Refresh the plugin-derived registries on `LoopCtx` after a
/// `/plugin` install / uninstall / marketplace mutation. Re-walks the
/// skill / custom-command sources from disk so newly-installed plugin
/// assets become visible to the slash-command palette and the agent
/// loop within the same session.
///
/// Hook executor is NOT rebuilt here: in this codebase the executor
/// lives entirely on the agent side (see `agent::mod` lines around
/// 718–722) and is reconstructed per `cd`. New hook plugins therefore
/// pick up at the next `/cd` (or process restart). Per spec §8 this
/// deferred behavior is acceptable.
/// Returns `(skills_loaded, skip_warnings)`. Caller decides how (and
/// whether) to surface the warnings — the TUI gates them behind verbose
/// mode (Ctrl+O) and always shows a `N loaded / M skipped` summary on
/// /plugin install. Non-summary callers can ignore both values.
pub(crate) fn reload_plugins(ctx: &mut LoopCtx) -> (usize, Vec<String>) {
    let mut loaded = 0usize;
    let mut warnings = Vec::new();
    if let Ok(mut guard) = ctx.skill_registry.write() {
        warnings = guard.reload(&ctx.working_dir);
        loaded = guard.all().count();
    }
    ctx.custom_commands = crate::custom_commands::CustomCommandRegistry::load(&ctx.working_dir);
    // Hook executor lives on the agent loop. Send a one-shot rebuild signal
    // so plugin-contributed hooks (especially UserPromptSubmit) fire on the
    // next user message rather than waiting for /cd or restart.
    let _ = ctx
        .agent
        .cmd_tx
        .send(atomcode_core::agent::AgentCommand::ReloadHooks);
    (loaded, warnings)
}

pub(crate) fn save_and_reload(ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
    let path = Config::default_path();
    match ctx.config.save(&path) {
        Ok(()) => {
            atomcode_core::proxy::apply_process_proxy_config(&ctx.config.network.proxy);
            ctx.runtime_factory.set_config(ctx.config.clone());
            let _ = ctx
                .agent
                .cmd_tx
                .send(AgentCommand::ReloadConfig(ctx.config.clone()));
        }
        Err(e) => {
            renderer.render(UiLine::Error(
                crate::i18n::t(crate::i18n::Msg::ConfigSaveFailed {
                    error: &format!("{}", e),
                })
                .into_owned(),
            ));
            renderer.flush();
        }
    }
}

/// On Ctrl+C / Esc during streaming, pull the running message back
/// into the input buffer so the user can edit and resend without
/// re-typing. Also drops any type-ahead queue entries: a user
/// pulling the escape cord doesn't want queued messages to
/// auto-fire after the current one dies. The actual `TurnCancelled`
/// event (plus the flip back to Idle + footer redraw) arrives later
/// via the agent round-trip — but the spinner tick at 80ms+ redraws
/// the StreamingBox with `buf.text`, so the restored message shows
/// up within a frame.
fn restore_cancelled_message_to_buf(app: &mut App, renderer: &mut dyn Renderer, ctx: &LoopCtx) {
    app.message_queue.clear();
    if let Some(msg) = app.state.last_submitted_message.take() {
        // Cursor at the end (edit-and-resend), but suppress the slash menu
        // for one frame so a restored `/command` doesn't re-pop the list.
        // Preserve any draft the user typed while the turn was running —
        // prepend the cancelled prompt instead of clobbering the draft.
        app.buf.restore_cancelled_text(msg);
        app.menu.selected = 0;
        // Force an immediate StreamingBox repaint so the restored
        // text shows in the input box on this frame, not the next
        // spinner tick.
        draw_spinner_now(
            &mut app.state,
            &app.buf,
            ctx,
            renderer,
            app.message_queue.len(),
            app.menu.selected,
        );
    }
}

/// Slash commands allowed to EXECUTE while a turn is running. Everything else is
/// blocked with the "disabled while a turn is running" hint. Returns the
/// `(command, args)` to run, or `None` to fall through to the block/queue.
///
/// Minimal whitelist:
///   - `/bg` (no args) — background the current turn.
///   - `/quit` and `/exit` — cancel the current turn, then shut down the TUI.
///   - `/goal clear|stop|off|reset|none|cancel` — halt a server-driven `/goal`
///     loop. Load-bearing: a goal keeps the TUI in Streaming (see `on_thinking`)
///     where commands are otherwise blocked, so without this a typed
///     `/goal clear` never reaches the bridge and the goal is uninterruptible by
///     command (Esc/Ctrl+C bypass the command system; a typed command does not).
///
/// A NEW goal (`/goal <condition>`) and `/goal status` are intentionally NOT
/// whitelisted — only the halt sub-commands.
fn streaming_executable_slash(line: &str) -> Option<(String, String)> {
    let (cmd, arg) = parse_slash_line(line)?;
    if cmd.eq_ignore_ascii_case("bg") && arg.trim().is_empty() {
        return Some(("bg".to_string(), String::new()));
    }
    if matches!(cmd.to_ascii_lowercase().as_str(), "quit" | "exit") && arg.trim().is_empty() {
        return Some((cmd.to_ascii_lowercase(), String::new()));
    }
    if cmd.eq_ignore_ascii_case("goal") {
        let head = arg.trim().split_whitespace().next().unwrap_or("");
        if matches!(
            head.to_ascii_lowercase().as_str(),
            "clear" | "stop" | "off" | "reset" | "none" | "cancel"
        ) {
            return Some(("goal".to_string(), arg.trim().to_string()));
        }
    }
    None
}

fn handle_streaming_key(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Result<()> {
    // Ctrl+O toggles verbose mode (real-time tool output + reasoning visibility)
    if code == KeyCode::Char('o') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        app.state.toggle_tool_output();
        // Show feedback to the user about the current state
        // Use muted style matching ToolResult's summary_style:
        // light theme → SGR 90 (DarkGrey), dark theme → SGR 2 (faint)
        let reset = "\x1b[0m";
        let mute = if crate::highlight::theme::is_light_for_render() {
            "\x1b[90m"
        } else {
            "\x1b[2m"
        };
        let status = if app.state.show_tool_output {
            format!("{mute}  ○ Verbose mode enabled (tool output + reasoning visible) (Ctrl+o to hide){reset}\n")
        } else {
            format!("{mute}  ○ Verbose mode disabled (Ctrl+o to show tool output + reasoning){reset}\n")
        };
        renderer.render(UiLine::CommandOutput(status));
        renderer.flush();
        draw_spinner_now(
            &mut app.state,
            &app.buf,
            ctx,
            renderer,
            app.message_queue.len(),
            app.menu.selected,
        );
        return Ok(());
    }

    // Ctrl+C always cancels the running turn — highest priority so
    // users have a reliable escape hatch even mid-edit. Also drops
    // the type-ahead queue: a user yanking the escape cord doesn't
    // want queued messages to auto-fire after the current one dies.
    if code == KeyCode::Char('c') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        let send_ok = cancel_active_turn(ctx);
        clear_capturing_modal_on_cancel(app);
        crate::tuix_trace!(
            "KEY",
            "streaming Ctrl+C -> Cancel send_ok={} spinner={:?}",
            send_ok,
            app.state.spinner_label
        );
        restore_cancelled_message_to_buf(app, renderer, ctx);
        return Ok(());
    }

    // Esc also cancels a running turn (CC-style). Placed before the
    // menu-nav block so Streaming + menu-open Esc still cancels the
    // stream — mid-stream the higher-value action is "stop the agent",
    // not "clear an unsubmitted slash token" (users can Ctrl+U for that).
    if code == KeyCode::Esc {
        let send_ok = cancel_active_turn(ctx);
        clear_capturing_modal_on_cancel(app);
        crate::tuix_trace!(
            "KEY",
            "streaming Esc -> Cancel send_ok={} spinner={:?}",
            send_ok,
            app.state.spinner_label
        );
        restore_cancelled_message_to_buf(app, renderer, ctx);
        return Ok(());
    }

    // When the menu is active (buf starts with `/`), intercept nav keys
    // so the user can browse candidate commands mid-stream. Execution
    // is still blocked below — Enter falls through to the commit arm,
    // which emits the "disabled while a turn is running" hint.
    let menu_items = menu_for_display(&app.buf, ctx);
    if let Some(items) = &menu_items {
        if app.menu.selected >= items.len() {
            app.menu.selected = items.len() - 1;
        }
        match code {
            KeyCode::Up => {
                app.menu.selected = app.menu.selected.saturating_sub(1);
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            KeyCode::Down => {
                if app.menu.selected + 1 < items.len() {
                    app.menu.selected += 1;
                }
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            KeyCode::Esc => {
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            _ => {} // fall through to buffer edits
        }
    }

    // Multi-line cursor nav: in a buffer with embedded newlines, plain
    // Up/Down should walk through the lines first; only when the
    // cursor is already on the first/last line does it surface as
    // HistoryPrev/Next. Matches the convention from fish / Cursor /
    // Claude Code — losing a multi-line draft to "I was just trying
    // to fix line 2" is far worse than the historical single-line
    // shortcut.  Gated to "no modifiers" so Shift+Up (selection in
    // some terminals) and other compound keys still classify normally.
    if modifiers.is_empty() {
        match code {
            KeyCode::Up if app.buf.cursor_line_up() => {
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            KeyCode::Down if app.buf.cursor_line_down() => {
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            _ => {}
        }
    }

    let action = classify(code, modifiers);
    let apply_result = app.buf.apply(action, ctx.history.entries(), &ctx.commands);
    sync_recalled_attachments(&mut app.state, &app.buf, ctx.history.entries());
    match apply_result {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            // Menu shape may have changed — reset selection if it
            // now points past the (possibly shorter) list.
            if let Some(items) = menu_for_display(&app.buf, ctx) {
                if app.menu.selected >= items.len() {
                    app.menu.selected = 0;
                }
            } else {
                app.menu.selected = 0;
            }
            draw_spinner_now(
                &mut app.state,
                &app.buf,
                ctx,
                renderer,
                app.message_queue.len(),
                app.menu.selected,
            );
        }
        BufferResult::Commit(line) => {
            // Slash commands are not queued — they need ctx access
            // that only makes sense between turns. Show a hint and
            // leave the buf alone. Gate strictly on *registered*
            // commands; unrecognised `/foo …` falls through to the
            // type-ahead queue as a regular message.
            //
            // EXCEPT a small whitelist that must RUN mid-stream (see
            // `streaming_executable_slash`): `/bg` (background the current turn)
            // and `/goal`'s halt sub-commands — a server-driven `/goal` keeps the
            // TUI in Streaming, so without this a typed `/goal clear` could never
            // reach the bridge and the goal was uninterruptible by command.
            if let Some((cmd, arg)) = streaming_executable_slash(&line) {
                if matches!(cmd.as_str(), "quit" | "exit") {
                    cancel_active_turn(ctx);
                    clear_capturing_modal_on_cancel(app);
                }
                commands::execute_slash_command(
                    &cmd,
                    &arg,
                    &mut app.state,
                    ctx,
                    renderer,
                    &mut app.active_modal,
                    &mut app.fixissue_pending,
                    &mut app.fixissue_buffer,
                    &mut app.setup_pending,
                )?;
                app.message_queue.clear();
                app.pending_tools.clear();
                app.think.reset();
                app.reasoning_buffer.clear();
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                return Ok(());
            }
            let is_known_slash = parse_slash_line(&line)
                .map(|(cmd, _)| ctx.commands.find(cmd).is_some())
                .unwrap_or(false);
            if is_known_slash {
                renderer.render(UiLine::CommandOutput(
                    "  (slash commands are disabled while a turn is running)\n".into(),
                ));
                renderer.flush();
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            // Hydrate recalled attachments BEFORE building the queue
            // payload — same prelude as the idle submit path, so a user
            // who pressed ↑ during streaming sees their recalled images
            // travel with the queued message instead of being silently
            // dropped on dispatch.
            let mut line = line;
            let cache_dir_for_hydrate = crate::platform::image_cache_dir();
            for n in hydrate_recalled_attachments(&mut app.state, &mut line, &cache_dir_for_hydrate)
            {
                renderer.render(UiLine::Warning(n));
            }
            let expanded = app.buf.expand_pastes(&line);
            // Mirror the main submit path's image filtering: only
            // attachments whose `[Image #N]` marker survived editing
            // travel with this submission, both into the queue
            // payload and into the persisted history entry.
            let pending = std::mem::take(&mut app.state.pending_images);
            let pending_markers = std::mem::take(&mut app.state.pending_image_markers);
            let pending_hashes = std::mem::take(&mut app.state.pending_image_hashes);
            let mut q_images: Vec<ImagePart> = Vec::with_capacity(pending.len());
            let mut q_markers: Vec<usize> = Vec::with_capacity(pending.len());
            let mut q_refs: Vec<crate::input::history::HistoryImageRef> =
                Vec::with_capacity(pending.len());
            for ((img, n), hash) in pending
                .into_iter()
                .zip(pending_markers.into_iter())
                .zip(pending_hashes.into_iter())
            {
                if line.contains(&format!("[Image #{}]", n)) {
                    q_refs.push(crate::input::history::HistoryImageRef {
                        hash: format!("{:016x}", hash),
                        mt: img.media_type.clone(),
                        n,
                    });
                    q_images.push(img);
                    q_markers.push(n);
                }
            }
            if ctx.sync_session.is_none() {
                // Same as the idle submit path: echo the EXACT text queued for
                // the agent (`expanded`) — the full pasted body, no
                // `[Pasted #N …]` placeholder — while history keeps the folded
                // `line` for compact Up-arrow recall.
                renderer.render(UiLine::UserWithAttachments {
                    text: expanded.clone(),
                    attachments: q_markers.clone(),
                });
            }
            ctx.history.push(crate::input::history::HistoryEntry {
                text: line.clone(),
                images: q_refs,
                // Snapshot before the clear_pastes() below so recall of a
                // queued message re-expands its folded paste (#843).
                pastes: app.buf.pastes.clone(),
            });
            app.message_queue.push_back(crate::state::QueuedMessage {
                text: expanded,
                images: q_images,
                image_markers: q_markers,
            });
            crate::tuix_trace!("QUE", "push_back len={}", app.message_queue.len());
            app.buf.text.clear();
            app.buf.cursor = 0;
            app.buf.clear_pastes();
            // Echo as a queued entry so the user sees it landed.
            renderer.render(UiLine::CommandOutput(format!("  ↳ queued: {}\n", line)));
            renderer.flush();
            draw_spinner_now(
                &mut app.state,
                &app.buf,
                ctx,
                renderer,
                app.message_queue.len(),
                app.menu.selected,
            );
        }
        BufferResult::Exit => {
            // Ctrl+C on empty buf during streaming — treat as cancel
            // (consistent with the explicit Ctrl+C branch above).
            cancel_active_turn(ctx);
            clear_capturing_modal_on_cancel(app);
            restore_cancelled_message_to_buf(app, renderer, ctx);
        }
    }
    Ok(())
}

/// Request agent shutdown AND arm the force-exit watchdog. Use this for EVERY
/// user-initiated quit (`/quit`, `/exit`, a confirmed Ctrl+C) instead of a bare
/// `cmd_tx.send(Shutdown)`. The graceful exit (the run-loop breaks once the
/// agent task ends and closes `cmd_tx`) still wins whenever it can; the deadline
/// only matters if a wedged teardown await never closes the channel — then the
/// loop hard-exits at [`SHUTDOWN_WATCHDOG`] rather than trapping the user.
///
/// NOT for the `/upgrade` restart path: that must exit via
/// `ExitReason::UpgradeRestart` to re-exec, so it sends `Shutdown` directly.
fn arm_shutdown_watchdog(ctx: &mut LoopCtx) {
    ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
    // Don't re-arm (push the deadline back) on a repeated /quit — keep the
    // earliest deadline so spamming the key can't indefinitely defer the exit.
    if ctx.shutdown_deadline.is_none() {
        ctx.shutdown_deadline = Some(std::time::Instant::now() + SHUTDOWN_WATCHDOG);
    }
}

/// Drop a capturing modal (the password prompt) when the turn it is attached to
/// is being cancelled. Dropping `PasswordModal` drops its `reply` Sender, which
/// resolves the askpass server's `reply_rx.await` to `Err` → the helper (and
/// thus sudo) gets a clean failure instead of hanging forever, and the server
/// task doesn't leak. No-op for non-capturing modals and when none is installed.
fn clear_capturing_modal_on_cancel(app: &mut App) {
    if app
        .active_modal
        .as_ref()
        .is_some_and(|m| m.captures_all_keys())
    {
        app.active_modal = None;
    }
}

/// After a turn ends (phase back to `Idle`), a still-installed capturing modal is an
/// ORPHAN: the sudo/ssh that requested the password finished or timed out, so the prompt
/// can never be answered. Dismiss it (drop → the askpass server's `reply_rx` resolves to
/// `Err` → clean failure, no task leak) and repaint the idle prompt so the stale
/// `Password:` line is cleared. No-op unless `Idle` with a capturing modal up.
fn dismiss_orphan_capturing_modal(app: &mut App, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
    if matches!(app.state.phase, UiPhase::Idle)
        && app.active_modal.as_ref().is_some_and(|m| m.captures_all_keys())
    {
        app.active_modal = None;
        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
    }
}

/// Cancel the executor that owns the foreground turn.
///
/// In sync mode the turn belongs to `LiveSession`; the local agent is idle and
/// sending it `AgentCommand::Cancel` is a no-op. Outside sync mode the local
/// agent remains the owner and keeps the existing command-channel path.
fn cancel_active_turn(ctx: &LoopCtx) -> bool {
    if ctx.sync_forwarder.is_some() {
        let Some(session) = atomcode_daemon::current_live_session() else {
            return false;
        };
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(session.cancel_current_turn())
        })
    } else {
        ctx.agent.cmd_tx.send(AgentCommand::Cancel).is_ok()
    }
}

/// Deliver a tool-approval decision to whichever turn is actually waiting.
///
/// In **sync mode** the running turn belongs to the in-process LiveSession
/// coordinator (`DaemonTurnExecutor`), not this TUI's own agent — so the
/// decision must go to the LiveSession's approver slot
/// (`current_live_session().approve`), exactly like the webui's
/// `/live/permission`. Sending it to the TUI agent (which isn't running this
/// turn) leaves the tool blocked forever: the "Running … 141s, and the webui
/// approval card never closes" bug. `ApproveToolAlways` sends `AllowAlways` so
/// the LiveSession's decider persists a session grant (grant_session /
/// grant_session_scope), exactly like the webui's "always allow this session"
/// path. In normal (non-sync) mode the decision goes to the TUI agent
/// as before.
fn deliver_approval(ctx: &mut LoopCtx, cmd: AgentCommand) {
    if ctx.sync_forwarder.is_some() {
        let decision = approval_command_to_decision(&cmd);
        if let Some(session) = atomcode_daemon::current_live_session() {
            let _ = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(session.approve(decision))
            });
        }
    } else {
        ctx.agent.cmd_tx.send(cmd).ok();
    }
}

/// Map an approval `AgentCommand` to the `PermissionDecision` the sync-mode
/// LiveSession applies. Pulled out of `deliver_approval` as a pure function so
/// the command↔decision contract (notably that an auto-approve sends `Allow`,
/// not `Deny`/`Ask`) is unit-testable without building a `LoopCtx`.
fn approval_command_to_decision(cmd: &AgentCommand) -> atomcode_core::tool::PermissionDecision {
    use atomcode_core::tool::PermissionDecision;
    match cmd {
        AgentCommand::ApproveTool => PermissionDecision::Allow,
        AgentCommand::ApproveToolAlways => PermissionDecision::AllowAlways,
        _ => PermissionDecision::Deny,
    }
}

/// Whether an incoming `ApprovalNeeded` should be auto-approved (true) instead
/// of surfaced as a prompt (false), given the TUI's bypass flag.
///
/// In LOCAL mode the core decider short-circuits and never emits the event, so
/// this only bites in SYNC mode, where the daemon's LiveSession always asks.
/// Honoring bypass here is what keeps `--dangerously-skip-permissions` working
/// for daemon-backed turns (e.g. a `/skills` expansion that drives tool calls).
fn approval_needed_is_auto_bypassed(dangerously_skip_permissions: bool) -> bool {
    dangerously_skip_permissions
}

/// The approval command a BYPASS-mode TUI issues for an `ApprovalNeeded` it
/// auto-resolves. A named function so the `handle_agent_event` short-circuit
/// and its test share one source of truth.
fn bypass_approval_command() -> AgentCommand {
    AgentCommand::ApproveTool
}

#[cfg(test)]
mod bypass_approval_tests {
    use super::{
        approval_command_to_decision, approval_needed_is_auto_bypassed, bypass_approval_command,
        AgentCommand,
    };
    use atomcode_core::tool::PermissionDecision;

    // The fix's core invariant: with `--dangerously-skip-permissions` on, an
    // incoming `ApprovalNeeded` is auto-approved rather than prompted. This is
    // what was broken in SYNC mode — the daemon always asks, and the TUI used
    // to show the prompt regardless of bypass (e.g. on `/skills` tool calls).
    #[test]
    fn bypass_on_auto_approves() {
        assert!(approval_needed_is_auto_bypassed(true));
    }

    // Without bypass, the event must NOT be short-circuited — the normal
    // prompt path runs. Guards against an accidental inversion of the flag.
    #[test]
    fn bypass_off_still_prompts() {
        assert!(!approval_needed_is_auto_bypassed(false));
    }

    // End-to-end on the pure seam: the command BYPASS issues must resolve to a
    // genuine `Allow`. If either the issued command or the mapping drifts so
    // that bypass yields `Deny`/`Ask`, the tool would stall/deny instead of
    // sailing through — exactly the regression this test pins.
    #[test]
    fn bypass_command_resolves_to_allow() {
        let decision = approval_command_to_decision(&bypass_approval_command());
        assert!(
            matches!(decision, PermissionDecision::Allow),
            "BYPASS must auto-approve with Allow, got {decision:?}"
        );
    }

    // The full command↔decision contract used by sync-mode `deliver_approval`.
    #[test]
    fn approval_command_decision_mapping() {
        assert!(matches!(
            approval_command_to_decision(&AgentCommand::ApproveTool),
            PermissionDecision::Allow
        ));
        assert!(matches!(
            approval_command_to_decision(&AgentCommand::ApproveToolAlways),
            PermissionDecision::AllowAlways
        ));
        assert!(matches!(
            approval_command_to_decision(&AgentCommand::DenyTool),
            PermissionDecision::Deny
        ));
    }
}

fn handle_approval_key(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Result<()> {
    // Ctrl+C: first press denies the tool and arms exit confirmation;
    // second press within the window actually exits.
    if code == KeyCode::Char('c') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        let now = std::time::Instant::now();
        let armed = app
            .exit_pending
            .is_some_and(|t| now.duration_since(t) <= CTRL_C_EXIT_WINDOW);
        if armed {
            arm_shutdown_watchdog(ctx);
        } else {
            // First Ctrl+C: deny the current tool and arm the exit confirmation.
            // The goal (if any) continues; Claude Code's /goal works the same way.
            // A second Ctrl+C within the window triggers Shutdown above.
            app.exit_pending = Some(now);
            renderer.pop_approval_prompt();
            deliver_approval(ctx, AgentCommand::DenyTool);
            app.state.on_approval_resolved();
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::CtrlCAgainToExit).into_owned(),
            ));
            renderer.flush();
        }
        return Ok(());
    }

    // Any other key resets the exit confirmation
    app.exit_pending = None;

    let cmd = match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => AgentCommand::ApproveTool,
        KeyCode::Char('a') | KeyCode::Char('A') => AgentCommand::ApproveToolAlways,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => AgentCommand::DenyTool,
        _ => return Ok(()),
    };
    // Retract the "Waiting for approval" body row now that the user
    // responded — without this, the prompt stays in scrollback next to
    // the tool result, creating visual noise.
    renderer.pop_approval_prompt();
    // Per Claude Code semantics, denying a tool does NOT stop the active
    // /goal — the evaluator will see the user's refusal on the next round
    // and either change tactics or judge the goal complete. The user can
    // explicitly halt with `/goal clear` or by pressing Esc outside this
    // approval prompt (inside the prompt, Esc denies the tool instead).
    deliver_approval(ctx, cmd);
    app.state.on_approval_resolved();
    Ok(())
}

/// Render one streamed upgrade event. Mutates the percent tracker so
/// Downloading lines only redraw on whole-percent changes (see caller's
/// `upgrade_last_pct` reasoning). Sets `done = true` when the upgrade
/// succeeds, so the main loop can break after rendering the success
/// line — the user must restart to load the new binary.
/// Render the result of an async /plugin operation. Mirrors the messages
/// emitted by the previous synchronous path in `handle_plugin` so users see
/// the same wording — only the timing changes.
pub(super) fn handle_plugin_job_event(
    ev: atomcode_core::plugin::PluginJobEvent,
    ctx: &mut LoopCtx,
    state: &mut crate::state::UiState,
    renderer: &mut dyn Renderer,
) {
    use atomcode_core::plugin::PluginJobEvent;
    match ev {
        PluginJobEvent::MarketplaceAdded(info) => {
            // Marketplace add by itself doesn't load any skills (those come
            // from installed plugins) — show only the marketplace summary.
            // `✓` prefix + col-0 alignment mirrors the MCP-connect toast
            // (`McpServerConnected`) emitted from the same body region, so
            // every "background install completed" line lands at the same
            // left edge regardless of which subsystem owns it.
            let _ = reload_plugins(ctx);
            let short_commit = &info.git_commit[..7.min(info.git_commit.len())];
            // Adding a marketplace does NOT install its plugins — list them + the install
            // command so the user isn't left thinking a plugin command (e.g. /wechat) is
            // already usable (a common confusion: `marketplace add` ≠ install).
            let plugin_list = info.plugins.join(", ");
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::PluginMarketplaceAdded {
                    name: &info.name,
                    commit: short_commit,
                    count: info.plugins.len(),
                    plugins: &plugin_list,
                })
                .into_owned(),
            ));
        }
        PluginJobEvent::MarketplaceUpdated(info) => {
            let _ = reload_plugins(ctx);
            let short_commit = &info.git_commit[..7.min(info.git_commit.len())];
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::PluginMarketplaceUpdated {
                    name: &info.name,
                    commit: short_commit,
                })
                .into_owned(),
            ));
        }
        PluginJobEvent::PluginInstalled(info) => {
            let (loaded, warnings) = reload_plugins(ctx);
            // Verbose mode (Ctrl+O) dumps the per-skill rejection reasons,
            // so users can debug a misnamed SKILL.md without restarting.
            // Default mode prints only the count summary — no cursor races.
            //
            // Sub-detail warning rows keep a 2-col indent: they are
            // children of the install summary line, indenting communicates
            // that subordination.
            if state.show_tool_output {
                for w in &warnings {
                    renderer.render(UiLine::CommandOutput(format!("  {}", w)));
                }
            }
            let show_details_hint = !warnings.is_empty() && !state.show_tool_output;
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::PluginInstallDone {
                    plugin: &info.plugin,
                    marketplace: &info.marketplace,
                    loaded,
                    skipped: warnings.len(),
                    show_details_hint,
                })
                .into_owned(),
            ));

            // If `/guide <topic>` was waiting for this install, auto-invoke
            // the "ask" skill with the stashed topic.
            if let Some(topic) = ctx.pending_guide_topic.take() {
                if let Some(rendered) = commands::expand_skill(ctx, "ask", &topic) {
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::CmdGuideAutoInvoke { topic: &topic })
                            .into_owned(),
                    ));
                    renderer.flush();
                    commands::submit_agent_turn(ctx, state, rendered);
                } else {
                    renderer.render(UiLine::Error(
                        crate::i18n::t(crate::i18n::Msg::CmdGuideSkillNotFound).into_owned(),
                    ));
                    renderer.flush();
                }
            }
        }
        PluginJobEvent::Failed { op, msg } => {
            // Clean up pending guide topic so future /guide commands work.
            if ctx.pending_guide_topic.take().is_some() {
                renderer.render(UiLine::Error(
                    crate::i18n::t(crate::i18n::Msg::CmdGuideInstallFailed { error: &msg })
                        .into_owned(),
                ));
                renderer.flush();
            }
            renderer.render(UiLine::Error(format!("{}: {}", op, msg)));
        }
        PluginJobEvent::GitNotFound => {
            // Not an error — a friendly hint to guide the user to install git.
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::PluginGitNotFound).into_owned(),
            ));
            renderer.flush();
        }
        PluginJobEvent::PluginAlreadyInstalled { id } => {
            // Stale install? Try reload + expand so the user still
            // gets an answer if the plugin was installed but not loaded.
            if let Some(topic) = ctx.pending_guide_topic.take() {
                let _ = reload_plugins(ctx);
                if let Some(rendered) = commands::expand_skill(ctx, "ask", &topic) {
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::CmdGuideAutoInvoke { topic: &topic })
                            .into_owned(),
                    ));
                    renderer.flush();
                    commands::submit_agent_turn(ctx, state, rendered);
                }
            }
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::PluginAlreadyInstalled { id: &id }).into_owned(),
            ));
        }
    }
    renderer.flush();
}

pub(super) fn handle_upgrade_event(
    ev: atomcode_core::self_update::UpgradeEvent,
    last_pct: &mut i32,
    done: &mut Option<std::path::PathBuf>,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
) {
    use atomcode_core::self_update::UpgradeEvent;
    match ev {
        UpgradeEvent::ManifestFetched { version } => {
            *last_pct = -1;
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::UpgradeManifestFetched { version: &version })
                    .into_owned(),
            ));
        }
        UpgradeEvent::Downloading { bytes, total } => {
            let pct = if total == 0 {
                0
            } else {
                ((bytes * 100) / total) as i32
            };
            if pct != *last_pct {
                *last_pct = pct;
                // Emit at 25/50/75/100 to keep output tidy. Finer-grained
                // progress would flood the append-only renderer with lines
                // since there's no in-place update here.
                if pct == 25 || pct == 50 || pct == 75 || pct == 100 {
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::UpgradeDownloading { pct, bytes, total })
                            .into_owned(),
                    ));
                }
            }
        }
        UpgradeEvent::Verifying => {
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::UpgradeVerifying).into_owned(),
            ));
        }
        UpgradeEvent::Replacing => {
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::UpgradeReplacing).into_owned(),
            ));
        }
        UpgradeEvent::Done {
            version,
            backup,
            exe,
        } => {
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::UpgradeDone {
                    version: &version,
                    backup: &backup.display().to_string(),
                })
                .into_owned(),
            ));
            // Push the hint in the status bar to match the new reality —
            // the little "↑ vX" arrow goes away for this session.
            if let Ok(mut g) = ctx.update_hint.lock() {
                *g = None;
            }
            // Store the *original* exe path so `re_exec_self` uses it
            // instead of `current_exe()` (which on Windows returns the
            // renamed `.atomcode.rolling` after `replace_binary`).
            *done = Some(exe);
            // Tell the agent to shut down so the loop exits cleanly.
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
        UpgradeEvent::Failed(msg) => {
            if msg.contains(atomcode_core::self_update::PACKAGE_MANAGED) {
                // HarmonyBrew-managed build: self-update is intentionally
                // disabled. Not an error — render as command output.
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::UpgradePackageManaged).into_owned(),
                ));
            } else if msg.contains(atomcode_core::self_update::ALREADY_LATEST) {
                // Friendly path — not an error, just "nothing to do".
                // self_update.rs's anyhow!() error is fixed-format
                // English: "already on {current} (latest is {latest}).
                // Pass --force to reinstall." Pull the two version
                // strings out so each locale formats the full sentence
                // itself instead of pasting English into a translated
                // wrapper.
                let friendly = msg.replace(
                    &format!("{}: ", atomcode_core::self_update::ALREADY_LATEST),
                    "",
                );
                let (current, latest) =
                    parse_already_latest_versions(&friendly).unwrap_or(("?", "?"));
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::UpgradeAlreadyLatest { current, latest })
                        .into_owned(),
                ));
            } else {
                renderer.render(UiLine::Error(
                    crate::i18n::t(crate::i18n::Msg::UpgradeFailed { error: &msg }).into_owned(),
                ));
            }
        }
        UpgradeEvent::RolledBack { exe, backup } => {
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::UpgradeRolledBack {
                    exe: &exe.display().to_string(),
                    backup: &backup.display().to_string(),
                })
                .into_owned(),
            ));
            *done = Some(exe);
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
    }
    renderer.flush();
}

/// Flush a buffered turn-end separator. `as_goal_end=true` is used when the
/// caller is about to render (or just rendered) a `✓ Goal met` / `⚠ Goal
/// stopped` banner immediately above; in that case the separator drops the
/// `↻ goal round N` / `✓ done · N rounds` prefix and shows just the stats —
/// the verdict banner already told the user what happened, the line below
/// only needs the cost & duration. No-op when no separator is pending.
/// Normal (non-goal) turn-end separator label — i18n, and Error-aware
/// (✗ "stopped" on an errored turn vs the celebratory ✓ "done" otherwise).
/// Shared by the immediate-render path (no active goal) and the deferred
/// `flush_pending_separator` path so both stay localized and consistent.
fn turn_summary_label(
    state: &mut UiState,
    errored: bool,
    turn_count: usize,
    tool_call_count: usize,
    total_tokens: usize,
    cached_pct: Option<u8>,
    dur: &str,
) -> String {
    if errored {
        // An errored turn already rendered a red Error line just above; a
        // celebratory "✓ Nailed it" under it is contradictory, and we don't
        // burn a DONE_LABELS rotation slot on a failure.
        crate::i18n::t(crate::i18n::Msg::TurnSummaryError {
            turn_count,
            tool_call_count,
            duration: dur,
            total_tokens,
        })
        .into_owned()
    } else {
        let done = state.next_done_label();
        crate::i18n::t(crate::i18n::Msg::TurnSummary {
            done,
            turn_count,
            tool_call_count,
            duration: dur,
            total_tokens,
            cached_pct,
        })
        .into_owned()
    }
}

fn flush_pending_separator(state: &mut UiState, renderer: &mut dyn Renderer, as_goal_end: bool) {
    let Some(ps) = state.pending_separator.take() else { return };
    let dur = crate::render::fmt_dur(ps.duration);
    let cached = ps
        .cached_pct
        .map(|p| format!(" · {p}% cached"))
        .unwrap_or_default();
    let label = if as_goal_end {
        format!(
            "{} tools · {} · {} tokens{}",
            ps.tool_call_count,
            dur,
            crate::i18n::fmt_tokens(ps.total_tokens),
            cached,
        )
    } else if ps.was_goal_round {
        format!(
            "↻ goal round {} · {} tools · {} · {} tokens{}",
            state.goal_round.max(1),
            ps.tool_call_count,
            dur,
            crate::i18n::fmt_tokens(ps.total_tokens),
            cached,
        )
    } else {
        // Reached only if a non-goal turn was ever buffered (today they
        // render immediately). Kept as a correct fallback either way.
        turn_summary_label(
            state,
            ps.errored,
            ps.turn_count,
            ps.tool_call_count,
            ps.total_tokens,
            ps.cached_pct,
            &dur,
        )
    };
    renderer.render(UiLine::TurnSeparator { label });
    renderer.flush();
}

/// If an approval prompt is still showing when the agent moves on (a tool result arrives,
/// the turn ends), the approval was resolved WITHOUT a user keypress — a headless timeout
/// fail-close, a displaced second approval, or a cancel. Retract the orphaned "Waiting for
/// approval" body row with the SAME cleanup the Y/A/N keypath does (`pop_approval_prompt` +
/// `on_approval_resolved`), so it can't linger above the result. Returns false (no-op) when
/// no approval is pending — the normal path, where the keypress already cleared it.
fn retract_stale_approval(state: &mut UiState, renderer: &mut dyn Renderer) -> bool {
    if matches!(state.phase, UiPhase::Approval) {
        renderer.pop_approval_prompt();
        state.on_approval_resolved();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod approval_retract_tests {
    use super::retract_stale_approval;
    use crate::render::{Renderer, UiLine};
    use crate::state::{UiPhase, UiState};

    #[derive(Default)]
    struct CountingRenderer {
        pops: usize,
    }
    impl Renderer for CountingRenderer {
        fn render(&mut self, _line: UiLine) {}
        fn flush(&mut self) {}
        fn shutdown(&mut self) {}
        fn reset(&mut self) {}
        fn clear_screen(&mut self) {}
        fn suspend_for_external(&mut self) {}
        fn resume_from_external(&mut self) {}
        fn flush_deferred(&mut self) {}
        fn pop_approval_prompt(&mut self) {
            self.pops += 1;
        }
    }

    #[test]
    fn retracts_orphaned_approval_when_resolved_without_keypress() {
        let mut state = UiState::new();
        state.on_approval_needed("EditFile");
        assert!(matches!(state.phase, UiPhase::Approval));
        let mut r = CountingRenderer::default();
        // A result/cancel arriving while the prompt is still up = resolved without a keypress.
        let retracted = retract_stale_approval(&mut state, &mut r);
        assert!(retracted, "a still-pending approval must be retracted");
        assert_eq!(r.pops, 1, "the orphaned 'Waiting for approval' body row must be popped");
        assert!(!matches!(state.phase, UiPhase::Approval), "phase must leave Approval");
    }

    #[test]
    fn noop_when_no_approval_pending() {
        let mut state = UiState::new(); // not in Approval phase
        let mut r = CountingRenderer::default();
        let retracted = retract_stale_approval(&mut state, &mut r);
        assert!(!retracted, "nothing to retract when no prompt is up");
        assert_eq!(r.pops, 0, "must not pop when no approval prompt is showing");
    }
}

/// Decide the notice to surface when a turn completes having produced NO
/// user-visible answer. Returns `Some(message)` when the turn ended NATURALLY
/// (finish_reason=stop), made no tool calls, and rendered no visible text —
/// e.g. a reasoning-only / `<think>`-only completion that the TUI would
/// otherwise show as a blank bubble (the "压缩了你怎么给我结果" symptom).
/// Returns `None` when there is a visible answer, tools ran, the reasoning is
/// already on screen (`show_reasoning`), or the turn errored / was cancelled /
/// hit a budget — those all have their own surfacing.
fn empty_completion_notice(
    rendered_visible_text: bool,
    tool_call_count: usize,
    saw_reasoning: bool,
    show_reasoning: bool,
    stop_reason: atomcode_core::agent::TurnStopReason,
) -> Option<String> {
    use atomcode_core::agent::TurnStopReason;
    // Only a NATURAL stop (finish_reason=stop) can be silently blank. Budget,
    // error, and cancel finishes already render their own labels/banners.
    if !matches!(stop_reason, TurnStopReason::Natural) {
        return None;
    }
    // A rendered answer, or tool calls (text-free tool turns are normal), means
    // the turn was not blank from the user's perspective.
    if rendered_visible_text || tool_call_count > 0 {
        return None;
    }
    // Reasoning already on screen (Ctrl+O enabled) => the turn isn't blank.
    if saw_reasoning && show_reasoning {
        return None;
    }
    Some(if saw_reasoning {
        "本轮模型只输出了推理、未给出正文。按 Ctrl+O 可查看推理内容；可直接重试或换个问法。".to_string()
    } else {
        "本轮模型未输出任何正文内容。可直接重试或换个问法。".to_string()
    })
}

#[cfg(test)]
mod empty_completion_notice_tests {
    use super::empty_completion_notice;
    use atomcode_core::agent::TurnStopReason;

    #[test]
    fn reasoning_only_hidden_gets_ctrl_o_hint() {
        let n = empty_completion_notice(false, 0, true, false, TurnStopReason::Natural)
            .expect("a blank reasoning-only turn must surface a notice");
        assert!(n.contains("Ctrl+O"), "hidden reasoning should hint at Ctrl+O, got {n:?}");
    }

    #[test]
    fn blank_no_reasoning_gets_generic_notice() {
        let n = empty_completion_notice(false, 0, false, false, TurnStopReason::Natural)
            .expect("a blank turn must surface a notice");
        assert!(!n.contains("Ctrl+O"), "no reasoning => no Ctrl+O hint, got {n:?}");
    }

    #[test]
    fn visible_text_suppresses_notice() {
        assert!(empty_completion_notice(true, 0, true, false, TurnStopReason::Natural).is_none());
    }

    #[test]
    fn tool_calls_suppress_notice() {
        assert!(empty_completion_notice(false, 1, false, false, TurnStopReason::Natural).is_none());
    }

    #[test]
    fn errored_turn_suppresses_notice() {
        assert!(empty_completion_notice(false, 0, true, false, TurnStopReason::Error).is_none());
    }

    #[test]
    fn cancelled_turn_suppresses_notice() {
        assert!(empty_completion_notice(false, 0, true, false, TurnStopReason::Cancelled).is_none());
    }

    #[test]
    fn reasoning_already_visible_suppresses_notice() {
        // show_reasoning=true => the reasoning is on screen; the turn is not blank.
        assert!(empty_completion_notice(false, 0, true, true, TurnStopReason::Natural).is_none());
    }
}

/// Raw name of the dispatch (fan-out child agents) tool whose per-child progress
/// should stream live without Ctrl+O. Matched by DISPLAY name (via
/// `display_tool_name`) so it's robust to the snake→Pascal transform; only couples
/// to the tool's contract name. If the tool is renamed, progress just falls back
/// to Ctrl+O-gated (graceful).
const DISPATCH_TOOL_RAW_NAME: &str = "parallel_edit_files";

/// Whether a tool's live `ToolOutputChunk` should stream to scrollback BY DEFAULT
/// (i.e. without Ctrl+O verbose). True for: verbose mode on; a user-invoked `!`
/// shell (`local-shell-…`, ran precisely to see output); or the dispatch tool,
/// whose per-child ↻/✓/✗ progress is the whole point of running it.
fn streams_tool_output_by_default(
    show_tool_output: bool,
    call_id: &str,
    tool_display: Option<&str>,
) -> bool {
    show_tool_output
        || call_id.starts_with("local-shell-")
        || tool_display.is_some_and(|d| d == display_tool_name(DISPATCH_TOOL_RAW_NAME))
}

#[cfg(test)]
mod tool_output_stream_gate_tests {
    use super::{display_tool_name, streams_tool_output_by_default, DISPATCH_TOOL_RAW_NAME};

    #[test]
    fn dispatch_tool_streams_without_verbose() {
        let disp = display_tool_name(DISPATCH_TOOL_RAW_NAME);
        assert!(
            streams_tool_output_by_default(false, "call-1", Some(&disp)),
            "dispatch tool's progress must stream by default"
        );
    }

    #[test]
    fn verbose_and_shell_stream_other_tools_dont() {
        assert!(streams_tool_output_by_default(true, "call-1", Some("ReadFile")));
        assert!(streams_tool_output_by_default(false, "local-shell-7", None));
        assert!(!streams_tool_output_by_default(false, "call-1", Some("ReadFile")));
        assert!(!streams_tool_output_by_default(false, "call-1", None));
    }
}

/// Determine whether the Ctrl+O hint should be shown for a tool call.
///
/// The hint appears only for bash commands when real-time output is
/// disabled and the call is not a local-shell (-prefixed) invocation.
/// Extracted as a pure function so the gating logic can be unit-tested
/// independently of 's rendering infrastructure.
fn should_show_ctrl_o_hint(tool_name: &str, verbose: bool, call_id: &str) -> bool {
    tool_name == "bash" && !verbose && !call_id.starts_with("local-shell-")
}

#[cfg(test)]
mod ctrl_o_hint_gating_tests {
    use super::should_show_ctrl_o_hint;

    #[test]
    fn shown_for_bash_without_verbose() {
        // Normal case: bash command, verbose off → show hint
        assert!(should_show_ctrl_o_hint("bash", false, "call-1"));
    }

    #[test]
    fn suppressed_when_verbose_on() {
        // Verbose already on → hint not needed
        assert!(!should_show_ctrl_o_hint("bash", true, "call-1"));
    }

    #[test]
    fn suppressed_for_non_bash_tools() {
        // Non-bash tools don't stream output
        assert!(!should_show_ctrl_o_hint("read_file", false, "call-1"));
        assert!(!should_show_ctrl_o_hint("edit_file", false, "call-1"));
        assert!(!should_show_ctrl_o_hint("grep", false, "call-1"));
        assert!(!should_show_ctrl_o_hint("glob", false, "call-1"));
    }

    #[test]
    fn suppressed_for_local_shell() {
        // Local shell () always streams — no hint needed
        assert!(!should_show_ctrl_o_hint("bash", false, "local-shell-7"));
    }

    #[test]
    fn suppressed_when_verbose_and_local_shell() {
        // Both conditions simultaneously
        assert!(!should_show_ctrl_o_hint("bash", true, "local-shell-3"));
    }
}

fn handle_agent_event(
    ev: AgentEvent,
    state: &mut UiState,
    think: &mut ThinkStripper,
    renderer: &mut dyn Renderer,
    pending_tools: &mut std::collections::HashMap<String, (String, String, bool)>,
    ctx: &mut LoopCtx,
    fixissue_pending: &mut Option<atomcode_core::atomgit::IssueRef>,
    fixissue_buffer: &mut String,
    setup_pending: &mut bool,
    reasoning_buffer: &mut String,
    buf: &mut Buffer,
) {
    // Any foreground event means the stream is alive — refresh the stall clock so
    // the spinner only warns "network may be down" after genuine silence.
    state.note_stream_activity();

    // Whitelist which events should flush a buffered turn-end separator
    // BEFORE we handle them. The buffered separator was deferred at
    // `TurnComplete` precisely so that — if the goal is about to end —
    // the `✓ Goal met` banner can render ABOVE the line. So we only
    // flush on events that signal "a new action is starting" (next round
    // beginning, next tool call, next user-bound stream, etc.). Passive
    // events like `PhaseChange(Idle)` / `TokenUsage` come right after
    // `TurnComplete` but BEFORE the wrapper's `GoalUpdate(active=false)`;
    // flushing on them would render the line above the banner — the bug
    // this whitelist exists to prevent.
    //
    // `GoalUpdate(active=false)` is intentionally absent here — its
    // handler renders the banner and then flushes the separator itself
    // (with a stripped, stats-only label).
    let should_flush_now = matches!(
        &ev,
        AgentEvent::TextDelta(_)
            | AgentEvent::ReasoningDelta(_)
            | AgentEvent::ToolCallStreaming { .. }
            | AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ApprovalNeeded { .. }
            | AgentEvent::PhaseChange(atomcode_core::agent::AgentPhase::Thinking)
            | AgentEvent::PhaseChange(atomcode_core::agent::AgentPhase::CallingTool(_))
            | AgentEvent::PhaseChange(atomcode_core::agent::AgentPhase::WaitingApproval)
            | AgentEvent::GoalUpdate { active: true, .. }
            | AgentEvent::TurnCancelled { .. }
            | AgentEvent::Error { .. }
    );
    if should_flush_now {
        flush_pending_separator(state, renderer, /* as_goal_end */ false);
    }

    match ev {
        AgentEvent::TextDelta(text) => {
            let visible = think.feed(&text);
            if !visible.is_empty() {
                // Mark that this turn produced a real visible answer, so the
                // TurnComplete handler does not surface a "blank turn" notice.
                state.turn_rendered_visible_text = true;
                // Keep the raw reply markdown for `/copy`. Clear-on-finalize:
                // the first delta of a new turn wipes the sealed prior reply,
                // so between turns the buffer still holds the last reply.
                if state.response_finalized {
                    state.last_assistant_response.clear();
                    state.response_finalized = false;
                }
                state.last_assistant_response.push_str(&visible);
                if fixissue_pending.is_some() {
                    fixissue_buffer.push_str(&visible);
                }
                renderer.render(UiLine::AssistantText(visible));
                renderer.flush();
            }
        }
        AgentEvent::ReasoningDelta(text) => {
            // Record that reasoning was produced this turn REGARDLESS of
            // visibility — the blank-turn notice uses it to say "only reasoning,
            // press Ctrl+O" vs "no output at all".
            state.turn_saw_reasoning = true;
            // Display reasoning/thinking content in verbose mode (Ctrl+O)
            // Only show when the user has enabled it
            if state.show_reasoning {
                reasoning_buffer.push_str(&text);
                // Flush on newline or when buffer gets large
                if reasoning_buffer.contains('\n') || reasoning_buffer.len() > 80 {
                    let output = std::mem::take(reasoning_buffer);
                    // Render as gray/dimmed text with automatic line wrapping
                    renderer.render(UiLine::ReasoningText(output));
                    renderer.flush();
                }
            }
        }
        AgentEvent::ToolCallStreaming { name, .. } => {
            state.on_tool_call_streaming(&display_tool_name(&name));
        }
        AgentEvent::ToolCallStarted {
            id,
            name,
            arguments,
        } => {
            let detail = format_tool_detail(&name, &arguments);
            let detail = enrich_todo_detail(&name, &arguments, &detail, &state.todo_titles);
            let display = display_tool_name(&name);

            // If this call is part of an active batch, the
            // ToolBatchStarted handler already rendered the group header
            // + child rows — skip the standalone ▸ ToolCallInFlight
            // line. Still record into `pending_tools` so the matching
            // ToolCallResult knows the display name + detail and skips
            // its own ▸ render too.
            // Preserve any existing entry (from ToolBatchStarted) which
            // carries the disambiguated detail — don't overwrite with
            // the raw basename (issue #439).
            if state.call_id_to_batch.contains_key(&id) {
                pending_tools
                    .entry(id)
                    .or_insert((display.clone(), detail, true));
                state.on_tool_call_started(&display);
                return;
            }

            // The v2 kernel asks for approval from middleware BEFORE it emits
            // ToolStarted. ApprovalNeeded may therefore have already rendered the
            // static `● Tool(detail)` row for this same call id. In that case the
            // started event is only a state transition: rendering a fresh
            // ToolCallInFlight row would duplicate the tool line.
            if let Some((stored_display, stored_detail, true)) = pending_tools.get_mut(&id) {
                *stored_display = display.clone();
                *stored_detail = detail.clone();
                state.on_tool_call_started(&display);
                return;
            }

            // Emit the ▸ line immediately so users can see what command
            // is running, especially for long-running bash commands.
            renderer.render(UiLine::AssistantLineBreak);
            renderer.render(UiLine::ToolCallInFlight {
                id: id.clone(),
                name: display.clone(),
                detail: detail.clone(),
            });
            // Show hint for bash commands if real-time output is disabled.
            // Placed HERE (at ToolCallStarted) rather than at ToolCallResult
            // (where it was before), so users see it WHILE the command is
            // running — pressing Ctrl+O actually streams live chunks.
            // The old placement after ToolCallResult meant the command had
            // already finished and no more ToolOutputChunk events were coming,
            // so pressing Ctrl+O showed the "enabled" banner but no output.
            if should_show_ctrl_o_hint(&name, state.show_tool_output, &id) {
                let reset = "[0m";
                let mute = if crate::highlight::theme::is_light_for_render() {
                    "[90m"
                } else {
                    "[2m"
                };
                renderer.render(UiLine::CommandOutput(format!(
                    "{mute}  ○ Press Ctrl+o to show real-time output while running{reset}
",
                )));
            }
            renderer.flush();

            // Mark as rendered so ToolCallResult doesn't emit it again.
            pending_tools.insert(id, (display.clone(), detail, true));
            state.on_tool_call_started(&display);
        }
        AgentEvent::ToolOutputChunk { call_id, chunk } => {
            // Display real-time tool output (e.g., bash stdout/stderr). Normally
            // gated behind Ctrl+O verbose mode, but user-invoked `!` shell commands
            // and the dispatch tool's per-child progress always stream (see
            // `streams_tool_output_by_default`).
            let tool_display = pending_tools.get(&call_id).map(|(d, ..)| d.as_str());
            if streams_tool_output_by_default(state.show_tool_output, &call_id, tool_display) {
                // Append to the scrollback as command output
                renderer.render(UiLine::CommandOutput(chunk));
                renderer.flush();
            }
        }
        AgentEvent::ToolCallResult {
            call_id,
            name,
            output,
            success,
            ..
        } => {
            // A result for this call arrived while an approval prompt is still up ⇒ the
            // approval was resolved WITHOUT the user answering (headless timeout fail-close,
            // a displaced second approval, or a cancel). Retract the orphaned "Waiting for
            // approval" row first so it doesn't linger above the result.
            retract_stale_approval(state, renderer);
            // Learn task id → content from every todo result (each action
            // returns the full list). This is what later `todo update`
            // rows look up to show the task NAME instead of a bare id.
            // Parsed before the batch early-return below so both batched
            // and single todo results feed the map.
            if name == "todo" {
                parse_todo_titles_into(&mut state.todo_titles, &output);
            }
            // If this call belongs to an active batch, the group header
            // already accounts for it; emit a single-line `  ↳ ✓ / ✗`
            // child completion and skip the full ▸ + ⎿ body render.
            // The model still gets the full output via the ToolResult
            // message in the conversation. Task 1.3 will upgrade this
            // to in-place checkmarks on the existing child rows instead
            // of appending new lines.
            if let Some(batch_id) = state.call_id_to_batch.get(&call_id).cloned() {
                // CC-style result-data update: `⎿ Read(mod.rs) → 200 lines`.
                // The result snippet is generic line count of the
                // output (works across read/grep/glob/bash without
                // per-tool extraction). Failure shows `→ ✗` so the
                // user can spot the broken child without reading
                // bytes-of-output.
                //
                // Renderer's ToolGroupChildUpdate finds the row by
                // call_id and CUPs to its terminal position. Falls
                // back to no-op if the group has been frozen —
                // model still gets the full ToolResult through the
                // conversation.
                // └ (U+2514 Box Drawing), → (U+2192 Arrows), ✗ (U+2717
                // Dingbats), ● (U+25CF Geometric Shapes): all in WGL4 so
                // every Windows monospace font (Consolas, NSimSun,
                // Cascadia, Microsoft YaHei) ships them. Hardcoded —
                // no `unicode_symbols` ASCII fallback — matching the
                // single-tool-call ● treatment for visual parity
                // between batched and single tool-call paths.
                let child_glyph = "\u{2514}";
                let arrow = "\u{2192}";
                // web_search shows the result SOURCE domains (e.g. `→ eol.cn,
                // gaokao.com +3`) so the user can see which sites the data came
                // from; every other tool (and a link-less search result) falls
                // back to the generic line count.
                let web_sources = (name == "web_search" && success)
                    .then(|| web_search_result_suffix(&output))
                    .flatten();
                let suffix = if !success {
                    format!(" {} \u{2717}", arrow)
                } else if let Some(srcs) = web_sources {
                    format!(" {} {}", arrow, srcs)
                } else {
                    let n = output.lines().count().max(1);
                    let unit = if n == 1 { "line" } else { "lines" };
                    format!(" {} {} {}", arrow, n, unit)
                };
                // Reuse the original Tool(arg) prefix the
                // ToolBatchStarted handler painted. pending_tools
                // holds (display, detail) — strip the previous "name
                // detail" join and rebuild as Short(detail) for
                // visual consistency with the initial child row.
                let prefix = pending_tools
                    .remove(&call_id)
                    .map(|(_, det, _)| format!("{}({})", display_tool_name_short(&name), det))
                    .unwrap_or_else(|| display_tool_name_short(&name));
                renderer.render(UiLine::ToolGroupChildUpdate {
                    batch_id,
                    call_id: call_id.clone(),
                    new_text: format!("  {} {}{}", child_glyph, prefix, suffix),
                });
                renderer.flush();
                return;
            }

            // Close any in-flight assistant line before emitting the pair.
            renderer.render(UiLine::AssistantLineBreak);
            // Freeze the animated in-flight tool-call row to its final
            // static `▸` icon before the `⎿ result` body row lands beneath
            // it. Pass the call_id so we only freeze if the inflight_tool matches.
            // This prevents freezing a different tool's spinner when multiple
            // tools are in flight (e.g., WriteFile result arrives while Bash spinner is active).
            renderer.render(UiLine::ToolCallCommit {
                call_id: Some(call_id.clone()),
            });

            // Prefer the display-name we stored at ToolCallStarted time;
            // fall back to converting the raw name if we missed the Start
            // (e.g. protocol surfaced a Result without a matching Start).
            let (display_name, detail, call_rendered) = pending_tools
                .remove(&call_id)
                .unwrap_or_else(|| (display_tool_name(&name), String::new(), false));

            // Filter empty tool names (model occasionally emits malformed
            // tool calls with "" as the name; agent surfaces the error via
            // a ToolCallResult but there's no useful ▸ line to render).
            let safe_name = if display_name.is_empty() {
                "(invalid)".to_string()
            } else {
                display_name
            };

            // ParallelEditFiles already streamed a per-task line tree
            // and an aggregate summary line via the SubAgentDispatch*
            // events — the ToolResult body would just repeat the same
            // info as a markdown table, doubling vertical space and
            // truncating mid-word at terminal boundaries. Skip both
            // the ▸ tool-call line and the ⎿ result line; the model
            // still receives full output via the ToolResult message
            // in the conversation.
            let suppress_body_echo = name == "parallel_edit_files";

            // Only emit the tool-call line here if ApprovalNeeded didn't
            // already render it — otherwise we'd print it twice.
            if !call_rendered && !suppress_body_echo {
                renderer.render(UiLine::ToolCall {
                    name: safe_name.clone(),
                    detail: detail.clone(),
                });
            }
            if !suppress_body_echo {
                // A plan-mode interception isn't a failure — render it as a calm `○`
                // hint (with the gate's reason) instead of a ✗ error, so the user
                // sees WHY the tool didn't run and that they should review the plan.
                if let Some(reason) = plan_mode_block_reason(&output, success) {
                    renderer.render(UiLine::CommandOutput(format!("  ○ {reason}\n")));
                } else {
                    // A single web_search call gets the same source-domain
                    // summary as the parallel child rows (`sources: eol.cn,
                    // gaokao.com +3`); fall back to the generic snippet when no
                    // links are present or on failure.
                    let summary = if name == "web_search" && success {
                        web_search_result_suffix(&output)
                            .map(|s| format!("sources: {s}"))
                            .unwrap_or_else(|| summarise(&output))
                    } else {
                        summarise(&output)
                    };
                    renderer.render(UiLine::ToolResult { success, summary });
                }
            }
            // Collect diff lines into a single batch — N individual
            // DiffLine renders each trigger a full footer redraw and
            // tens of KB of ANSI, which blocks the event loop long
            // enough to stall the spinner during edit tool results.
            //
            // Gated on edit-class tools because the `+ ` / `- ` line
            // detection is purely textual: markdown bullet lists
            // (`- item`) inside non-edit tool outputs trip the same
            // pattern. The deepseek-v4-flash screenshot symptom was a
            // 162-line `UseSkill(brainstorming)` template — every `- `
            // bullet got rendered as a removed-diff line and the whole
            // skill body leaked into the scrollback. Restricting to
            // tools that actually emit diff payloads (`edit_file`,
            // `write_file`, `create_file`, `search_replace`) closes
            // that without losing the diff render where it's wanted.
            let emits_diff = matches!(
                name.as_str(),
                "edit_file" | "write_file" | "create_file" | "search_replace"
            );
            if emits_diff {
                let diff_entries: Vec<crate::render::DiffEntry> = output
                    .lines()
                    .take(120)
                    .filter_map(|line| {
                        if let Some(rest) = line.strip_prefix("+ ") {
                            Some(crate::render::DiffEntry {
                                added: true,
                                text: rest.to_string(),
                            })
                        } else if let Some(rest) = line.strip_prefix("- ") {
                            Some(crate::render::DiffEntry {
                                added: false,
                                text: rest.to_string(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                if !diff_entries.is_empty() {
                    renderer.render(UiLine::DiffBlock(diff_entries));
                }
            }
            // Flush any pending diff renders before the next event.
            renderer.flush();
            let _ = name;
        }
        AgentEvent::ApprovalNeeded {
            tool_name, call, snapshot, ..
        } => {
            // BYPASS mode (`--dangerously-skip-permissions`): auto-approve and
            // skip the prompt entirely. In LOCAL mode the core decider already
            // short-circuits, so this event never fires there. But in SYNC mode
            // the daemon's LiveSession runs an InteractivePermissionDecider
            // (auto_approve=false, see live_api.rs) that ALWAYS asks and forwards
            // ApprovalRequested → here (live_sync.rs); the TUI's bypass flag would
            // otherwise be ignored, surfacing prompts whenever a turn drives tool
            // calls — e.g. a `/skills` expansion. Honor bypass at the TUI seam
            // rather than flipping the shared daemon executor (which would leak
            // bypass to webui peers attached to the same LiveSession).
            // `deliver_approval` routes to LiveSession.approve(Allow) in sync mode
            // and to the local agent otherwise.
            if approval_needed_is_auto_bypassed(ctx.dangerously_skip_permissions) {
                deliver_approval(ctx, bypass_approval_command());
                return;
            }
            // Persist mid-turn messages to session so /bg can recover
            // the conversation even when the turn hasn't finished yet.
            if !snapshot.messages.is_empty() {
                apply_session_snapshot(&mut ctx.current_session, snapshot);
                ctx.bg_manager
                    .set_foreground_session(ctx.current_session.clone());
            }

            // Emit the `▸ Tool(detail)` row BEFORE the approval prompt
            // so the user sees what they're approving.
            let display = display_tool_name(&tool_name);
            // Prefer the disambiguated detail from `pending_tools` (populated
            // by ToolBatchStarted for parallel batches) over the raw basename
            // from format_tool_detail. Without this, parallel batch approvals
            // show "ReadFile(SKILL.md)" for every call, making it impossible
            // to tell which file is being approved (issue #439).
            let detail = pending_tools
                .get(&call.id)
                .map(|(_, det, _)| det.clone())
                .unwrap_or_else(|| format_tool_detail(&tool_name, &call.arguments));

            // Check if ToolCallStarted already rendered this tool call as a
            // dynamic ToolCallInFlight spinner. If so, we need to freeze it
            // to a static `▸` row before showing the approval prompt.
            if let Some(entry) = pending_tools.get_mut(&call.id) {
                let (disp, det, rendered) = entry;
                if *rendered {
                    // ToolCallInFlight is animating — commit it to a static row
                    // so the approval prompt appears below a frozen `▸ Bash(...)`.
                    // Pass the call_id to ensure we only freeze the matching tool.
                    renderer.render(UiLine::ToolCallCommit {
                        call_id: Some(call.id.clone()),
                    });
                } else {
                    // Not yet rendered, emit it now
                    renderer.render(UiLine::ToolCall {
                        name: disp.clone(),
                        detail: det.clone(),
                    });
                    *rendered = true;
                }
            } else {
                // No entry from ToolCallStarted, render and insert
                renderer.render(UiLine::ToolCall {
                    name: display.clone(),
                    detail: detail.clone(),
                });
                pending_tools.insert(call.id.clone(), (display.clone(), detail.clone(), true));
            }

            renderer.render(UiLine::ApprovalPrompt {
                tool: display.clone(),
                detail: detail.clone(),
            });
            renderer.flush();
            atomcode_core::notify::notify(
                &ctx.config.notifications,
                atomcode_core::notify::NotificationEvent::ApprovalNeeded(
                    atomcode_core::notify::ApprovalNotification {
                        tool_name: &display_tool_name(&tool_name),
                        detail: Some(&format_tool_detail(&tool_name, &call.arguments)),
                        working_dir: Some(&ctx.working_dir),
                    },
                ),
            );
            // `display` is the already-PascalCased name (e.g. "Bash",
            // "ReadFile"); on_approval_needed stashes the current
            // "Running X" label so on_approval_resolved can restore it.
            state.on_approval_needed(&display);
            // Redraw the footer (input box) so the user can type
            // Y/A/N in response. Without this, a prior
            // on_approval_resolved() transition to Streaming may
            // have left the footer stale — especially when
            // TurnRunner dispatches the second approval before any
            // spinner tick fires (issue #455: "待审批输入 Y 后，
            // 输入框没了"). Use redraw_idle_plain instead of
            // draw_spinner_now because the spinner label may be
            // stale/misleading in the approval phase (mirrors the
            // /bg resume path).
            redraw_idle_plain(buf, state, ctx, renderer);
        }
        AgentEvent::PhaseChange(AgentPhase::Thinking) => state.on_thinking(),
        AgentEvent::PhaseChange(AgentPhase::CallingTool(name)) => {
            state.on_tool_call_streaming(&display_tool_name(&name));
        }
        AgentEvent::PhaseChange(_) => {}
        AgentEvent::TurnComplete {
            duration,
            total_tokens,
            turn_count,
            tool_call_count,
            stop_reason,
            snapshot,
        } => {
            // Seal the assistant-reply buffer so `/copy` reads this completed
            // reply until the next turn's first delta starts a fresh one.
            state.response_finalized = true;
            atomcode_core::notify::notify(
                &ctx.config.notifications,
                atomcode_core::notify::NotificationEvent::TurnFinished(
                    atomcode_core::notify::TurnNotification {
                        duration,
                        turn_count,
                        tool_call_count,
                        total_tokens: Some(total_tokens),
                        stop_reason,
                        working_dir: Some(&ctx.working_dir),
                    },
                ),
            );
            // A turn that finished NATURALLY but produced no visible answer
            // (reasoning-only / `<think>`-only) would otherwise render as a
            // blank bubble — surface a notice so the user isn't left staring at
            // an empty reply with a big token count (the "怎么不给我结果" case).
            if let Some(notice) = empty_completion_notice(
                state.turn_rendered_visible_text,
                tool_call_count,
                state.turn_saw_reasoning,
                state.show_reasoning,
                stop_reason,
            ) {
                renderer.render(UiLine::Warning(notice));
            }
            renderer.render(UiLine::AssistantLineBreak);
            pending_tools.clear();
            let errored = matches!(stop_reason, atomcode_core::agent::TurnStopReason::Error);
            // Footer token count: bill output + UNCACHED input (re-reading the
            // cached prefix each round is near-free). The event's `total_tokens`
            // is the v2 gross sum (prompt+completion per round) which overstates
            // usage ~10-100× on long multi-round turns — recompute from the
            // per-turn tallies instead. Falls back to the event value if no
            // per-round usage arrived (turn_prompt 0).
            let (total_tokens, cached_pct) = if state.turn_prompt_tokens > 0 {
                crate::state::turn_token_summary(
                    state.turn_prompt_tokens,
                    state.turn_completion_tokens,
                    state.turn_cached_tokens,
                )
            } else {
                (total_tokens, None)
            };
            if state.goal_condition.is_some() {
                // A /goal is active: DEFER the separator so the next event can
                // choose its form — a `✓ Goal met` banner ABOVE a stats-only
                // line when the goal ends (GoalUpdate active=false), or the
                // `↻ goal round N` banner mid-goal (flushed by should_flush_now).
                // `errored` rides along as a defensive fallback.
                state.pending_separator = Some(crate::state::PendingSeparator {
                    duration,
                    turn_count,
                    tool_call_count,
                    total_tokens,
                    was_goal_round: true,
                    errored,
                    cached_pct,
                });
            } else {
                // No active goal: render the turn summary immediately, exactly
                // as before the /goal merge — the line lands at the bottom of
                // the turn that just finished instead of waiting for the next
                // event. Same i18n + Error-aware label as the deferred path.
                let dur = crate::render::fmt_dur(duration);
                let label = turn_summary_label(
                    state,
                    errored,
                    turn_count,
                    tool_call_count,
                    total_tokens,
                    cached_pct,
                    &dur,
                );
                renderer.render(UiLine::TurnSeparator { label });
            }
            renderer.flush();
            state.on_turn_complete();

            // Reset the think stripper between turns. If the previous turn
            // left an unclosed `<think>` in flight (cancelled mid-stream,
            // model never emitted `</think>`, provider switch that doesn't
            // use `<think>` tags like Kimi thinking-mode via reasoning_content),
            // the stripper stays `inside=true` and silently swallows every
            // TextDelta of the NEXT turn — user sees blank assistant bubbles
            // while datalog proves the model did return text.
            think.reset();

            // Clear reasoning buffer between turns
            reasoning_buffer.clear();

            // Record this turn's stats (anchored by message count) so /resume
            // can re-render the same `✓ … 工具 · tokens` divider between turns —
            // sessions persist only `messages`, so without this the per-turn
            // token/duration numbers are lost on reload and turns butt together.
            ctx.current_session.turn_stats.push(atomcode_core::session::TurnStat {
                after_message: snapshot.messages.len(),
                turn_count,
                tool_call_count,
                duration_ms: duration.as_millis() as u64,
                total_tokens,
                errored: matches!(stop_reason, atomcode_core::agent::TurnStopReason::Error),
            });
            // Persist session after every completed turn so /resume can
            // find it after a clean exit — the whole point of sessions.
            persist_current_session(ctx, snapshot, renderer);

            // CodingPlan usage refresh — fire after each completed turn
            // (with cooldown) so the right-aligned hint reflects the
            // tokens the turn just consumed. Gated to CodingPlan users
            // only; non-CodingPlan paths skip all network activity.
            if monitor::is_codingplan_provider(&ctx.config.default_provider) {
                let cooled = ctx
                    .usage_last_check_at
                    .map(|t| t.elapsed() >= usage_monitor::USAGE_COOLDOWN)
                    .unwrap_or(true);
                if cooled {
                    ctx.usage_last_check_at = Some(std::time::Instant::now());
                    usage_monitor::spawn_check(ctx.usage_slot.clone(), ctx.wake_tx.clone());
                }
            }

            // fixissue post-run side effects — only on successful TurnComplete
            // (TurnCancelled / Error arms below clear `fixissue_pending`
            // without posting). Takes the IssueRef out so only this turn's
            // completion triggers the post-back.
            if let Some(issue_ref) = fixissue_pending.take() {
                let body = std::mem::take(fixissue_buffer);
                if body.trim().is_empty() {
                    renderer.render(UiLine::CommandOutput(format!(
                        "  [fixissue] agent produced no text; skipping comment + label on issue #{}\n",
                        issue_ref.number
                    )));
                } else {
                    match atomcode_core::atomgit::fixissue::post_completion(&issue_ref, &body) {
                        Ok(()) => renderer.render(UiLine::CommandOutput(format!(
                            "  [fixissue] ✓ posted summary + applied 'fixed' label to issue #{}\n",
                            issue_ref.number
                        ))),
                        Err(e) => renderer.render(UiLine::CommandOutput(format!(
                            "  [fixissue] ✗ post-back failed (local fix still saved): {:#}\n",
                            e
                        ))),
                    }
                }
                renderer.flush();
            }

            // setup post-run side effects — only on successful TurnComplete.
            // Reload skills/commands so newly-created skills become visible
            // to the LLM immediately.
            if std::mem::take(setup_pending) {
                let (skills_loaded, warnings) = reload_plugins(ctx);
                let warn_count = warnings.len();
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::SetupAutoReloaded {
                        skills: skills_loaded,
                        warnings: warn_count,
                    })
                    .into_owned(),
                ));
                if !warnings.is_empty() {
                    for w in &warnings {
                        renderer.render(UiLine::Error(w.clone()));
                    }
                }
                renderer.flush();
            }
        }
        AgentEvent::TurnCancelled { snapshot } => {
            // Seal the reply buffer (partial reply still copyable via `/copy`).
            state.response_finalized = true;
            atomcode_core::notify::notify(
                &ctx.config.notifications,
                atomcode_core::notify::NotificationEvent::TurnFinished(
                    atomcode_core::notify::TurnNotification {
                        duration: state.turn_elapsed().unwrap_or_default(),
                        turn_count: 0,
                        tool_call_count: pending_tools.len(),
                        total_tokens: None,
                        stop_reason: atomcode_core::agent::TurnStopReason::Cancelled,
                        working_dir: Some(&ctx.working_dir),
                    },
                ),
            );
            // Render any in-flight tool calls that never got a result
            // as "(cancelled)" so the user sees what was mid-flight.
            for (_id, (name, detail, call_rendered)) in pending_tools.drain() {
                let safe_name = if name.is_empty() {
                    "(invalid)".into()
                } else {
                    name
                };
                if !call_rendered {
                    renderer.render(UiLine::ToolCall {
                        name: safe_name,
                        detail,
                    });
                }
                renderer.render(UiLine::ToolResult {
                    success: false,
                    summary: "(cancelled)".into(),
                });
            }
            renderer.render(UiLine::TurnCancelled);
            renderer.flush();
            state.on_turn_cancelled();
            // Cancellation = agent didn't finish; don't post a comment
            // against an incomplete "fix".
            fixissue_pending.take();
            fixissue_buffer.clear();
            *setup_pending = false;
            // Same reset rationale as TurnComplete: a cancelled turn is the
            // single most common way for `<think>` to go unclosed, so this
            // branch is even more important for the stripper's hygiene.
            think.reset();
            // Save what we did have — a user who Ctrl+C'd mid-stream
            // should still be able to /resume the cleaned conversation.
            persist_current_session(ctx, snapshot, renderer);
        }
        AgentEvent::ConversationTruncated {
            snapshot,
            restored_prompt,
            target_n,
            prompts_before,
        } => {
            let new_len = snapshot.messages.len();
            // Persist the truncated conversation: messages + prune stale
            // per-turn dividers (anchored by message-count) so /resume won't
            // replay dividers for removed turns.
            ctx.current_session.update_from_conversation_snapshot(snapshot);
            ctx.current_session
                .turn_stats
                .retain(|s| s.after_message <= new_len);
            ctx.current_session.touch();
            ctx.bg_manager
                .set_foreground_session(ctx.current_session.clone());
            if let Err(e) = ctx.session_manager.save(&ctx.current_session) {
                renderer.render(UiLine::Error(
                    crate::i18n::t(crate::i18n::Msg::SessionSaveFailed {
                        error: &e.to_string(),
                    })
                    .into_owned(),
                ));
            }
            // Redraw scrollback from the truncated history — reuses the
            // /resume replay path (clears screen, re-renders turn dividers).
            crate::modals::session_picker::replay_session(renderer, &ctx.current_session, true);
            // Put the rolled-back prompt back in the input box for editing.
            buf.set_restored_text(restored_prompt);
            // Confirmation + disk-divergence warning.
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::CmdUndoDone {
                    target: target_n,
                    last: prompts_before,
                })
                .into_owned(),
            ));
            renderer.render(UiLine::CommandOutput(
                crate::i18n::t(crate::i18n::Msg::CmdUndoDiskWarning).into_owned(),
            ));
            renderer.flush();
            state.on_turn_complete();
        }
        AgentEvent::UndoFailed {
            requested,
            available,
        } => {
            let line = if available == 0 {
                crate::i18n::t(crate::i18n::Msg::CmdUndoNoTurns).into_owned()
            } else {
                crate::i18n::t(crate::i18n::Msg::CmdUndoOutOfRange {
                    requested,
                    available,
                })
                .into_owned()
            };
            renderer.render(UiLine::CommandOutput(line));
            renderer.flush();
        }
        AgentEvent::Error { error, snapshot } => {
            // Seal the reply buffer (any text streamed before the error stays
            // copyable via `/copy`).
            state.response_finalized = true;
            renderer.render(UiLine::Error(error));
            renderer.flush();
            fixissue_pending.take();
            fixissue_buffer.clear();
            *setup_pending = false;
            state.on_error();
            // Same reset rationale as TurnComplete / TurnCancelled — an
            // aborted turn is another way to leave `<think>` half-open.
            think.reset();
            // Persist on Error too — without this, a first-turn LLM
            // failure (auth, rate limit, gateway 5xx, our own 5-min
            // total-request timeout, etc.) silently drops the user's
            // typed message from disk so the next `/resume` shows
            // nothing for that conversation. Empty `messages` from
            // the streaming-error forwarder is treated as a no-op
            // by persist_current_session.
            persist_current_session(ctx, snapshot, renderer);
        }
        AgentEvent::Warning(w) => {
            // Non-fatal — flush a yellow advisory line and let the turn
            // continue. Don't touch state/think/buffers; the warning is
            // purely informational. Used today for the OpenAI provider's
            // truncation detector (`prompt_tokens` reported by the proxy
            // is implausibly low for the body we sent).
            renderer.render(UiLine::Warning(w));
            renderer.flush();
        }
        AgentEvent::CompactionUi(kind) => match kind {
            atomcode_core::agent::CompactionUiKind::Begin => {
                state.compacting = true;
                // A standalone manual `/compact` runs from Idle (its command
                // handler does NOT call on_submit), so the spinner wouldn't
                // animate. Force Streaming so "Compacting…" shows, and record
                // it so completion restores Idle. An auto-compaction is already
                // mid-turn (Streaming) — leave its phase to the turn lifecycle.
                if !matches!(state.phase, UiPhase::Streaming) {
                    state.compaction_forced_streaming = true;
                    state.phase = UiPhase::Streaming;
                    state.phase_started_at = Some(std::time::Instant::now());
                }
                state.note_stream_activity();
            }
            atomcode_core::agent::CompactionUiKind::End => {
                state.compacting = false;
                if state.compaction_forced_streaming {
                    state.compaction_forced_streaming = false;
                    state.phase = UiPhase::Idle;
                    state.spinner_label.clear();
                }
            }
            atomcode_core::agent::CompactionUiKind::Mark(label) => {
                state.compacting = false;
                renderer.render(UiLine::CompactionMark(label));
                renderer.flush();
                if state.compaction_forced_streaming {
                    state.compaction_forced_streaming = false;
                    state.phase = UiPhase::Idle;
                    state.spinner_label.clear();
                }
            }
        },
        AgentEvent::HookWarningHint(msg) => {
            if let Ok(mut slot) = ctx.hook_warning_hint.lock() {
                *slot = Some(msg);
            }
        }
        AgentEvent::VisionPreprocessSuccess { vl_key, char_count } => {
            // Format here (not in agent) so we can localize / restyle
            // without bumping the AgentEvent contract. Char count helps
            // users notice degenerate near-zero VL outputs that would
            // mislead the main model into "image failed" responses.
            let msg = crate::i18n::t(crate::i18n::Msg::VisionPreprocessSuccess { char_count })
                .into_owned();
            renderer.render(UiLine::VisionPreprocessSuccess { msg, model: vl_key });
            renderer.flush();
        }
        AgentEvent::RestorePendingImages { images, markers } => {
            // VL preprocessing failed — re-attach the user's images to
            // the input state so they can retry without re-pasting from
            // clipboard.
            //
            // Restore the full original message text (including the text
            // between [Image #N] markers) from `last_submitted_message`.
            // Without this, only markers get re-inserted into the cleared
            // buffer and the user's caption text is silently lost — the
            // user sees blank space where their text should be (bug report:
            // "多张图片发送后丢失文字，只保留了最后一张").
            if let Some(restore) = state.last_submitted_message.take() {
                buf.text = restore;
                buf.cursor = buf.text.len();
            }
            //
            // Hash table is rebuilt as best-effort: we hash the base64
            // payload (not raw RGBA), which means a fresh clipboard copy
            // of the same image won't dedupe against this restored entry.
            // Minor UX nit, not a correctness issue.
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            // markers length should match images length (agent passed them
            // back); zip is best-effort if it doesn't (truncates).
            for (img, marker) in images.into_iter().zip(markers.into_iter()) {
                let mut hasher = DefaultHasher::new();
                img.data.hash(&mut hasher);
                let h = hasher.finish();
                cache_write_image(&crate::platform::image_cache_dir(), &img, h);
                state.pending_image_hashes.push(h);
                state.pending_images.push(img);
                state.pending_image_markers.push(marker);
                // Only insert the marker if it's NOT already in the
                // restored message text — `last_submitted_message` above
                // already carries the original markers in the correct
                // positions alongside the user's text.
                let marker_text = format!("[Image #{}]", marker);
                if !buf.text.contains(&marker_text) {
                    buf.text.insert_str(buf.cursor, &marker_text);
                    buf.cursor += marker_text.len();
                }
            }
            // Don't redraw — TUI is in Streaming phase here (turn isn't
            // over yet); the next idle/streaming redraw picks up the new
            // pending state on its own.
        }
        AgentEvent::TokenUsage(u) => {
            state.prompt_tokens += u.prompt_tokens;
            state.completion_tokens += u.completion_tokens;
            state.cached_tokens += u.cached_tokens;
            state.total_tokens += u.completion_tokens;
            // Per-turn tallies for the footer's billable count + cache annotation
            // (reset in on_turn_complete / on_turn_cancelled).
            state.turn_prompt_tokens += u.prompt_tokens;
            state.turn_completion_tokens += u.completion_tokens;
            state.turn_cached_tokens += u.cached_tokens;
        }
        AgentEvent::WorkingDirChanged(new_dir) => {
            // Fires when a tool (change_dir / bash cd) or an AgentCommand::ChangeDir
            // mutated the shared cwd. Sync the footer's view so the status row
            // reflects the new directory on the next redraw (spinner tick if
            // streaming, idle redraw after turn complete). Without this the
            // footer is stuck on the old path until the user types `/cd` or
            // restarts the session.
            if ctx.working_dir != new_dir {
                ctx.previous_dir = Some(std::mem::replace(&mut ctx.working_dir, new_dir.clone()));
                ctx.runtime_factory.set_working_dir(new_dir.clone());
                commands::push_recent_dir(&mut ctx.recent_dirs, new_dir);
            }
        }
        AgentEvent::ProjectSwitched(new_dir) => {
            // A webui /cd switched the project directory (delivered via the
            // live-sync forwarder in sync mode). Follow it: change cwd like
            // `/cd` (updates runtime_factory + @-file index + recent dirs +
            // tells the running agent), THEN open a fresh session in the new
            // dir like `/session`. Distinct from WorkingDirChanged (agent's own
            // `cd`, conversation preserved). No-op when already there to avoid
            // resetting on a redundant broadcast.
            if ctx.working_dir != new_dir {
                commands::apply_cd(ctx, new_dir.clone());
                // 新项目的会话要存进新项目的 hash 目录。session_manager 只在启动
                // 时按初始目录构建，不重建的话切项目后的新会话会写进旧项目，
                // /resume 与手机端项目列表都会在错误的项目下看到它。
                ctx.session_manager = atomcode_core::session::SessionManager::new(&new_dir);
                commands::reset_to_new_session(ctx, state, renderer);
            }
        }
        AgentEvent::RemoteSlashCommand(line) => {
            // 手机端发来的斜杠命令：只放行只读信息类白名单（status/cost/whoami/
            // diff），在桌面同样渲染一份（让桌面用户知道手机做了什么），输出经
            // CommandOutput 广播回手机。交互式/桌面专属命令礼貌拒绝。
            let display = format!("/{}", line.trim().trim_start_matches('/'));
            match commands::run_remote_command(ctx, state, &line) {
                Some(txt) => {
                    renderer.render(UiLine::CommandOutput(format!(
                        "（手机端执行 {display}）"
                    )));
                    renderer.render(UiLine::CommandOutput(txt.clone()));
                    renderer.flush();
                    if let Some(live) = &ctx.sync_session {
                        live.notify_command_output(format!("{display}\n{txt}"));
                    }
                }
                None => {
                    if let Some(live) = &ctx.sync_session {
                        live.notify_command_output(format!(
                            "{display}\n  该命令需要在桌面端执行（手机端仅支持 \
                             /status /cost /whoami /diff）"
                        ));
                    }
                }
            }
        }
        AgentEvent::ContextStats {
            system_tokens,
            sent_tokens,
            dropped_tokens: _,
            working_set_tokens: _,
            total_messages,
            tool_defs_tokens,
            cold_zone_tokens,
            ctx_window,
            ctx_name,
            system_prompt,
        } => {
            state.on_context_stats(
                system_tokens,
                sent_tokens,
                tool_defs_tokens,
                cold_zone_tokens,
                total_messages,
                ctx_window,
                &ctx_name,
                &system_prompt,
            );
            // If `/context` is waiting for fresh stats, the rich emission
            // (ctx_window > 0) is the signal to render. Narrow emissions
            // from TurnRunner leave ctx_window at 0 and must not trigger
            // a report render (they'd race ahead of the pending refresh
            // and print partial data). Clears the flag on fire so a
            // single dispatch yields a single render even when multiple
            // rich emissions follow (e.g. inside a long multi-round turn).
            if ctx_window > 0 {
                if let Some(show_prompt) = state.pending_context_render.take() {
                    renderer.render(UiLine::CommandOutput(commands::render_context_report(
                        state,
                        ctx,
                        show_prompt,
                    )));
                    renderer.flush();
                }
            }
        }
        AgentEvent::ToolBatchStarted { batch_id, calls } => {
            // Header label: "Reading 4 files in parallel" when all calls
            // share a tool name (common case for batched read_file /
            // grep / glob); otherwise generic "Running 4 tools in
            // parallel". No tech-stack hardcoding — tool names come from
            // the model's own tool_calls.name.
            let count = calls.len();
            let unique_names: std::collections::HashSet<&str> =
                calls.iter().map(|c| c.name.as_str()).collect();
            // Generic header — no per-tool verb table inside the
            // framework. Same-name batches surface the model's own
            // tool name; mixed batches use "tools". This avoids
            // a `match tool_name { "bash" => "Running" ... }` table
            // that drifts whenever new tools land or models invent
            // names (mcp.foo, custom plugins).
            let label = if unique_names.len() == 1 {
                let single = unique_names.iter().next().copied().unwrap_or("tool");
                format!("Running {} {} calls in parallel", count, single)
            } else {
                format!("Running {} tools in parallel", count)
            };
            // Header alone — child rows are NOT pre-rendered. Each
            // child surfaces as a `  ↳ ✓ name` line when its
            // ToolCallResult arrives. Trade-off:
            // - PRO: zero duplication; children "trickle in" as they
            //   complete, so user sees real progress on slow batches
            //   (4 reads finishing within 1s look near-atomic; a 4-call
            //   batch where 3 are fast + 1 is `cargo check` shows the
            //   slow tail clearly).
            // - PRO: avoids the retained-renderer's "in-place mutation
            //   of older body rows" problem (rows already scrolled into
            //   native terminal scrollback can't be modified).
            // - CON: user doesn't see batch contents until first child
            //   completes. Acceptable: footer spinner conveys "working",
            //   contents become visible immediately on first result.
            // Glyphs: ● (BLACK CIRCLE U+25CF) for batch header,
            // └ (BOX DRAWINGS LIGHT UP AND RIGHT U+2514) for each
            // child row. Picked over ⏺/⎿ because Cascadia Code
            // (Windows VSCode default) renders ⏺ as a flat oval and
            // ⎿ as a backslash-shaped fallback -- both are widely
            // supported monospace glyphs that survive the same fonts
            // where the dental-symbols block tofu's. Aligns with the
            // single-tool-call ● glyph (retained::ToolCall arm) so
            // batched and single calls share one visual anchor, and
            // with `└` for tool-result rows below the call. Both
            // glyphs are in WGL4 (Consolas, NSimSun, Cascadia, Microsoft
            // YaHei all ship them), so no `unicode_symbols` ASCII
            // fallback — matches the single-tool-call hardcoded ● for
            // visual parity between batched and single tool-call paths.
            let head_glyph = "\u{25cf}";
            let child_glyph = "\u{2514}";
            // Build header + child rows; renderer keeps the group
            // "live" while it's the bottom of body_lines, so each
            // ToolCallResult below can update the matching child row
            // in place (CC-style result data light-up).
            //
            // Child format: `⎿ Read(mod.rs)`. Tool name is the short
            // form (Read not ReadFile); detail is wrapped in parens
            // (Tool(arg) reads as a function call, mirroring CC).
            let header_text = format!("{} {}", head_glyph, label);
            // Build child rows with disambiguation: when multiple calls
            // produce the same detail (e.g. 3 × Read(SKILL.md) from
            // different directories), show enough parent path to tell
            // them apart (issue #437).
            let raw_details: Vec<String> = calls
                .iter()
                .map(|c| format_tool_detail(&c.name, &c.arguments))
                .collect();
            let disambiguated = disambiguate_batch_details(
                &calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                &calls
                    .iter()
                    .map(|c| c.arguments.as_str())
                    .collect::<Vec<_>>(),
                &raw_details,
            );
            // For todo add calls, prepend batch-sequential task numbers
            // (#1, #2, …) so users can see task ids at a glance in the
            // parallel batch display (issue #697).
            let mut todo_add_counter: usize = 0;
            let final_details: Vec<String> = calls
                .iter()
                .zip(disambiguated.iter())
                .map(|(c, detail)| {
                    if c.name == "todo" {
                        // Parse the action from arguments JSON rather than
                        // string-matching, because model-generated JSON may
                        // contain whitespace around colons/commas.
                        let action = serde_json::from_str::<serde_json::Value>(&c.arguments)
                            .ok()
                            .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(str::to_string));
                        match action.as_deref() {
                            Some("add") => {
                                todo_add_counter += 1;
                                return format!("#{} {}", todo_add_counter, detail);
                            }
                            // Upgrade `#4 → completed` to `#4 Write tests →
                            // completed` using the title learned from earlier
                            // todo results (issue: parallel updates showed no
                            // task name).
                            Some("update") => {
                                return enrich_todo_detail(
                                    &c.name,
                                    &c.arguments,
                                    detail,
                                    &state.todo_titles,
                                );
                            }
                            _ => {}
                        }
                    }
                    detail.clone()
                })
                .collect();
            let children: Vec<crate::render::ToolGroupChild> = calls
                .iter()
                .zip(final_details.iter())
                .map(|(c, detail)| crate::render::ToolGroupChild {
                    call_id: c.id.clone(),
                    text: format!(
                        "  {} {}({})",
                        child_glyph,
                        display_tool_name_short(&c.name),
                        detail
                    ),
                })
                .collect();
            renderer.render(UiLine::AssistantLineBreak);
            renderer.render(UiLine::ToolGroupRender {
                batch_id: batch_id.clone(),
                header: header_text,
                children,
            });
            renderer.flush();

            let call_ids: Vec<String> = calls.iter().map(|c| c.id.clone()).collect();
            for cid in &call_ids {
                state.call_id_to_batch.insert(cid.clone(), batch_id.clone());
            }
            // Pre-populate `pending_tools` with the final (potentially
            // todo-numbered) detail so that subsequent ToolCallStarted /
            // ApprovalNeeded events use the disambiguated / numbered
            // path (e.g. "a/SKILL.md", "#1 创建demo3") instead of the
            // raw basename ("SKILL.md", "创建demo3"). Without this,
            // parallel batch approvals show identical "ReadFile(SKILL.md)"
            // prompts and the user can't tell which file they're
            // approving (issue #439 / #697).
            for (c, detail) in calls.iter().zip(final_details.iter()) {
                pending_tools.insert(
                    c.id.clone(),
                    (display_tool_name_short(&c.name), detail.clone(), true),
                );
            }
            state.active_tool_batches.insert(
                batch_id.clone(),
                crate::state::ActiveToolBatch { call_ids },
            );
            // Anchor the spinner clock to the batch start. The interleaved
            // per-tool events that follow won't reset it (they no-op the reset
            // while a batch is active), so the elapsed-ms ticks steadily instead
            // of flickering 0→N→0.
            state.on_tool_batch_started();
        }
        AgentEvent::GoalUpdate { active, round, condition, last_reason, .. } => {
            if active {
                state.goal_condition = Some(condition);
                state.goal_round = round;
                if state.goal_started_at.is_none() {
                    state.goal_started_at = Some(std::time::Instant::now());
                }
            } else {
                // Goal ended. Render order: banner (CommandOutput, bypasses
                // markdown) ABOVE a stats-only separator. The user wanted
                // the verdict to read top-down: assistant output → ✓ Goal
                // met → quiet horizontal line with timing. The earlier
                // TurnComplete buffered its stats into `pending_separator`
                // precisely so we could re-render them in this stripped
                // form here.
                if state.goal_condition.is_some() {
                    if let Some(reason) = last_reason.as_deref() {
                        let banner = if reason.contains("cancelled") {
                            // Cancel already gets its own UiLine via
                            // TurnCancelled — skip to avoid double banner.
                            None
                        } else if reason.contains("evaluator unavailable")
                            || reason.contains("cleared by user")
                        {
                            Some(format!("  ⚠ Goal stopped: {reason}\n"))
                        } else {
                            Some(format!("  ✓ Goal met: {reason}\n"))
                        };
                        if let Some(line) = banner {
                            renderer.render(UiLine::CommandOutput(line));
                            renderer.flush();
                        }
                    }
                }
                state.goal_condition = None;
                state.goal_round = 0;
                state.goal_started_at = None;
                // Now flush the buffered separator as a stats-only line —
                // the verdict above already conveys what happened, the
                // separator just visually closes the turn.
                flush_pending_separator(state, renderer, /* as_goal_end */ true);
            }
        }
        AgentEvent::ToolBatchCompleted {
            batch_id,
            ok: _,
            total: _,
            elapsed_ms: _,
        } => {
            // CC-style: NO standalone batch-summary row. Each child
            // row already shows its own `→ N lines` / `→ ✗`, so an
            // aggregate `batch 4/4 ok · Xs wall` line would just be
            // visual noise repeating what's already visible above.
            //
            // SubAgentDispatchEnd (different code path) STILL emits
            // its `▸ ParallelEditFiles · ...` summary because sub-agent
            // turns/elapsed per-task is hidden by Task 3's collapse —
            // that summary is the only place the user can see how
            // long it took.
            //
            // Just clear batch state so subsequent per-call events
            // fall back to the standard single-tool render path.
            if let Some(b) = state.active_tool_batches.remove(&batch_id) {
                for cid in b.call_ids {
                    state.call_id_to_batch.remove(&cid);
                }
            }
        }
        AgentEvent::SubAgentDispatchStart { tasks } => {
            // Header line: announce the dispatch. The model gets this
            // same fact in the ToolResult; the UI line tells the user
            // "the wait is intentional, not a hang". Per-task running/
            // done lines are suppressed (Task 3 — CC alignment); the
            // footer spinner conveys mid-flight progress, the
            // DispatchEnd summary lands the final count.
            renderer.render(UiLine::CommandOutput(format!(
                "Dispatching {} sub-agents in parallel...",
                tasks.len()
            )));
            renderer.flush();
            state.on_sub_agent_dispatch_start(tasks);
        }
        AgentEvent::SubAgentTaskStarted { index: _ } => {
            // Per-task running lines suppressed for CC-style collapsed
            // view. State tracking still happens via DispatchStart's
            // task list. Nothing to render here.
        }
        AgentEvent::SubAgentTaskDone {
            index: _,
            elapsed_ms: _,
            turns: _,
            summary: _,
        } => {
            // Per-task done lines suppressed — final count shows in
            // DispatchEnd summary. Still tick the counter so the
            // aggregate `N/M ok` reflects this completion.
            state.on_sub_agent_task_done();
        }
        AgentEvent::SubAgentTaskFailed {
            index,
            elapsed_ms,
            turns: _,
            reason,
        } => {
            // Failures KEEP their per-task line. Rationale: the user
            // needs to know which sub-agent failed for diagnosis;
            // collapsing into "1 fail" leaves them blind. Successes
            // collapse silently (no actionable info per success).
            state.on_sub_agent_task_failed();
            if let Some(info) = state.sub_agent_tasks.get(index) {
                let cross = "\u{2717}";
                let short_reason = reason.lines().next().unwrap_or("").trim();
                renderer.render(UiLine::CommandOutput(format!(
                    "  {} {}{} — {} · {}",
                    cross,
                    info.path,
                    info.dedup_suffix,
                    fmt_elapsed(elapsed_ms),
                    if short_reason.is_empty() {
                        "failed"
                    } else {
                        short_reason
                    }
                )));
                renderer.flush();
            }
        }
        AgentEvent::SubAgentDispatchEnd => {
            // Compute the aggregate before clearing state. This is the
            // single line that replaces the old multi-row pipe-table
            // result block — the model still sees the full breakdown
            // in the ToolResult content, but the UI only needs the
            // bottom line.
            let total = state.sub_agent_total;
            let failed = state.sub_agent_failed;
            let ok = total.saturating_sub(failed);
            let elapsed = state
                .sub_agent_started_at
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            if total > 0 {
                let arrow = "\u{25cf}";
                let summary = if failed == 0 {
                    format!(
                        "{} ParallelEditFiles · {}/{} ok · {} wall",
                        arrow,
                        ok,
                        total,
                        fmt_elapsed(elapsed)
                    )
                } else {
                    format!(
                        "{} ParallelEditFiles · {} ok · {} fail · {} wall",
                        arrow,
                        ok,
                        failed,
                        fmt_elapsed(elapsed)
                    )
                };
                renderer.render(UiLine::ToolGroupSummary { text: summary });
                renderer.flush();
            }
            state.on_sub_agent_dispatch_end();
        }
        AgentEvent::BackgroundComplete {
            summary,
            files_edited,
            turns,
            success,
        } => {
            let header = if success {
                crate::i18n::t(crate::i18n::Msg::BackgroundComplete { turns }).into_owned()
            } else {
                crate::i18n::t(crate::i18n::Msg::BackgroundFailed { turns }).into_owned()
            };
            let mut body = String::from(&header);
            body.push_str("  ");
            body.push_str(&summary);
            if !body.ends_with('\n') {
                body.push('\n');
            }
            if !files_edited.is_empty() {
                body.push_str(&crate::i18n::t(crate::i18n::Msg::BackgroundFilesEdited));
                for f in &files_edited {
                    body.push_str(&format!("    - {}\n", f));
                }
            }
            if success {
                renderer.render(UiLine::CommandOutput(body));
            } else {
                renderer.render(UiLine::Error(body));
            }
            renderer.flush();
        }
        AgentEvent::MessagesSync { snapshot } => {
            // Response to AgentCommand::SyncMessages. Persist the
            // snapshot to the current session so /bg can recover
            // the conversation state.
            if !snapshot.messages.is_empty() {
                apply_session_snapshot(&mut ctx.current_session, snapshot);
                ctx.bg_manager
                    .set_foreground_session(ctx.current_session.clone());
            }
        }
        AgentEvent::UserEcho(text) => {
            let markers = image_markers_in_order(&text);
            renderer.render(UiLine::UserWithAttachments {
                text,
                attachments: markers,
            });
            renderer.flush();
        }
        AgentEvent::PeerBusy(running) => {
            // Live-sync: mirror the peer's busy state so TUI input is
            // visually disabled while the other side's turn is running.
            if running {
                state.on_submit();
            } else {
                // 对端轮次结束:把流式累积的助手行收尾落地(等价本地 TurnComplete
                // 的第一步 AssistantLineBreak),否则短回复(如"在的!")一直挂在
                // 流式当前行不提交,要等下一轮才一起刷出 —— sync 模式下的空助手气泡
                // bug。同时 reset think-stripper,避免上一轮残留吞掉下一轮文本。
                renderer.render(UiLine::AssistantLineBreak);
                renderer.flush();
                think.reset();
                state.on_turn_complete();
            }
        }
        AgentEvent::ProviderChanged(provider) => {
            // Live-sync: another view (webui dropdown) switched the model —
            // mirror it into the TUI's active provider + header. Skip when it's
            // already our provider (the echo of the TUI's own /model switch, which
            // already applied + persisted). Persistence is done by whoever
            // originated the switch (webui → /live/provider endpoint; TUI → the
            // /model picker's save_and_reload), so here we only sync in-memory
            // state and notify the agent — no second disk write.
            if ctx.config.default_provider != provider
                && ctx.config.providers.contains_key(&provider)
            {
                ctx.config.default_provider = provider.clone();
                ctx.model_name = ctx
                    .config
                    .providers
                    .get(&provider)
                    .map(|p| p.model.clone())
                    .unwrap_or(provider);
                // Footer context window follows the mirrored switch too (see
                // model_picker) — otherwise the denominator lags a turn behind
                // a webui-driven model change.
                state.on_model_window_changed(ctx.config.default_context_window());
                ctx.runtime_factory.set_config(ctx.config.clone());
                let _ = ctx
                    .agent
                    .cmd_tx
                    .send(AgentCommand::ReloadConfig(ctx.config.clone()));
                let dir_display =
                    crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
                renderer.refresh_welcome_banner(&ctx.model_name, &dir_display);
                renderer.flush();
            }
        }
        AgentEvent::SessionSwitched(session_id) => {
            // 另一端（webui 新建对话 / webui 侧栏切到已存在会话）切换了会话，
            // 同步模式的 TUI 跟随。按 session_id 跨项目定位会话文件：
            //  - webui「新建」：文件已落盘但为空 → 回放空历史 = 干净空白会话；
            //  - webui 切到「已存在」会话：把该会话的历史一并加载、回放进 TUI。
            // 二者统一走 load_any，省掉「新建/切换」分支。找不到文件（罕见）才退回
            // 到「按指定 id 建空白会话」的旧行为。
            crate::tuix_trace!("TUI", "SessionSwitched: session_id={}, sync_session={}", session_id, ctx.sync_session.is_some());
            let sid = atomcode_core::session::SessionId::from_string(session_id);
            // 自回声守卫：TUI 自己发起的切换（/new、/resume 等，见 sync_local_session_switch）
            // 会经 live_switch_session 广播、再经转发器回流到这里。此时本地切换已完成、
            // ctx.current_session.id 已是目标 id，无需重复清场/回放/重挂——直接忽略，避免
            // 双重渲染。仅「另一端发起」的切换（id 与当前不同）才走下面的完整跟随逻辑。
            // 与 ProviderChanged 臂同款的自回声去重。
            if ctx.current_session.id == sid {
                crate::tuix_trace!("TUI", "SessionSwitched: self-echo for current session, ignoring");
                return;
            }
            let loaded = atomcode_core::session::SessionManager::load_any(&sid).ok();

            // 重置对话与计数（无论加载成功与否都先清场）。
            ctx.agent.cmd_tx.send(AgentCommand::ClearConversation).ok();
            state.total_tokens = 0;
            state.prompt_tokens = 0;
            state.completion_tokens = 0;
            state.cached_tokens = 0;
            state.last_context = None;
            state.pending_context_render = None;
            state.thinking_idx = 0;
            state.on_turn_complete();

            // 目标会话所属目录：已存在会话用其自身 working_dir；建不出来时沿用当前目录。
            let target_session = match loaded {
                Some(session) => {
                    // 该会话属于另一个项目 → 先像 /cd 一样切目录（与 webui handleSelectSession
                    // 的 setCwd 对齐），保证后续回合在正确项目里执行、@-索引/会话列表也跟随。
                    if ctx.working_dir != session.working_dir {
                        commands::apply_cd(ctx, session.working_dir.clone());
                    }
                    ctx.current_session_id = Some(sid.clone());
                    session
                }
                None => {
                    // 罕见：磁盘上找不到该会话（如 webui 刚新建、广播早于落盘）。
                    // 退回旧行为：用指定 id 建一个空白会话，保证三端落同一文件。
                    ctx.current_session_id = None;
                    let mut new_session =
                        atomcode_core::session::Session::default_session(ctx.working_dir.clone());
                    new_session.id = sid;
                    new_session
                }
            };

            // 把历史灌进 agent 会话，使后续回合带上下文（空会话则等价于清空）。
            ctx.agent
                .cmd_tx
                .send(AgentCommand::SetConversation(
                    target_session.to_conversation_snapshot(),
                ))
                .ok();
            commands::bind_telemetry_to_session(ctx, &target_session);
            ctx.current_session = target_session;
            ctx.bg_manager
                .set_foreground_session(ctx.current_session.clone());

            // 重绘画布并回放目标会话历史（/resume 同款干净回放，不带「同步快照」分隔）。
            renderer.begin_sync();
            renderer.reset();
            if ctx.current_session.messages.is_empty() {
                let dir_display =
                    crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
                renderer.render(UiLine::Welcome {
                    model: ctx.model_name.clone(),
                    working_dir: dir_display,
                });
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::CmdNewSession).into_owned(),
                ));
            } else {
                crate::modals::session_picker::replay_session(
                    renderer,
                    &ctx.current_session,
                    true,
                );
            }
            renderer.flush();
            renderer.end_sync();

            // 同步模式：用「带历史」的 session_id 重新附着 LiveSession，使三端
            // （TUI / webui / 磁盘）落到同一会话、同一对话。render_snapshot=false：
            // 历史已在上面 replay 过，避免重复刷。
            if ctx.sync_session.is_some() {
                let session = atomcode_daemon::ensure_live_session(
                    ctx.working_dir.clone(),
                    ctx.telemetry.clone(),
                    Some(ctx.current_session.id.clone()),
                    ctx.current_session.messages.clone(),
                );
                crate::tuix_trace!("TUI", "SessionSwitched: attaching LiveSession ptr={:#x}", std::sync::Arc::as_ptr(&session) as usize);
                attach_live_session(ctx, renderer, session, false);
            }
        }
        AgentEvent::SessionRenamed { name } => {
            // AI session-namer renamed the active session (fire-and-forget after
            // the first turn, or mirrored from a daemon session). Apply only if
            // the session is still auto-named and the user hasn't renamed it —
            // never clobber an explicit `/rename`. The next loop iteration's
            // `sync_terminal_title` picks up the new name for the tab title.
            crate::tuix_trace!("TUI", "SessionRenamed: name={}", name);
            apply_ai_session_name(ctx, name, renderer);
        }
        AgentEvent::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming } => {
            // Non-error pause line: dim/plain body row, never red.
            // auto_resuming=true  (WaitAndRetry): "⏳ 限流，Ns 后自动继续…"
            // auto_resuming=false (Pause, has time): "⏸ 5小时窗口已用尽，约 HH:MM 恢复…"
            // auto_resuming=false (Pause, no time):  "⏸ 5小时窗口已用尽，稍后恢复…"
            // UiLine::Muted: dim DarkGrey, no forced prefix, non-bold.
            // Rate-limit is a pause, not an error/warning — it must not
            // render with the yellow `! ` prefix that Warning applies.
            let line = format_rate_limited_line(&reset_at_display, &reset_label, secs_until_reset, auto_resuming);
            renderer.render(UiLine::Muted(line));
            renderer.flush();
        }
    }
}

/// Apply an AI-generated session name if the session is still auto-named and
/// not user-renamed. Saves directly (NOT via `apply_session_snapshot`, which
/// would re-truncate the name). Auto-name semantics: does NOT set
/// `user_renamed`, so a later explicit `/rename` still wins.
fn apply_ai_session_name(ctx: &mut LoopCtx, name: String, renderer: &mut dyn Renderer) {
    if !atomcode_core::agent::session_title::should_accept_ai_name(
        ctx.current_session.user_renamed,
        ctx.current_session.ai_named,
    ) {
        return;
    }
    ctx.current_session.name = name;
    ctx.current_session.ai_named = true;
    ctx.current_session.touch();
    ctx.bg_manager
        .set_foreground_session(ctx.current_session.clone());
    if let Err(e) = ctx.session_manager.save(&ctx.current_session) {
        renderer.render(UiLine::Error(format!("session save failed: {e}")));
        renderer.flush();
    }
}

/// Copy the latest conversation into `ctx.current_session`, auto-name
/// the session from the first real user message, and write the session
/// file to disk. Called on every TurnComplete and TurnCancelled so
/// `/resume` can find the conversation after a quit. No-op when the
/// conversation is empty (don't save a blank session).
fn persist_current_session(
    ctx: &mut LoopCtx,
    snapshot: atomcode_core::conversation::ConversationSnapshot,
    renderer: &mut dyn Renderer,
) {
    if snapshot.messages.is_empty() {
        return;
    }
    apply_session_snapshot(&mut ctx.current_session, snapshot);
    ctx.bg_manager
        .set_foreground_session(ctx.current_session.clone());
    // Surface save failures instead of silently swallowing them.
    // Previously this was `let _ = session_manager.save(...)`, which
    // hid disk-full / permission / read-only / invalid-path errors —
    // users would `/resume` on the next launch, see nothing, and
    // assume "the session was lost" with no idea anything went wrong.
    if let Err(e) = ctx.session_manager.save(&ctx.current_session) {
        renderer.render(UiLine::Error(
            crate::i18n::t(crate::i18n::Msg::SessionSaveFailed {
                error: &e.to_string(),
            })
            .into_owned(),
        ));
        renderer.flush();
    }
}

pub(crate) fn apply_session_snapshot(
    session: &mut atomcode_core::session::Session,
    snapshot: atomcode_core::conversation::ConversationSnapshot,
) {
    if snapshot.messages.is_empty() {
        return;
    }
    session.update_from_conversation_snapshot(snapshot);
    session.touch();
    // Triggers for renaming:
    //   * `default` / `session-<ts>` — never renamed yet
    //   * leading `[` — previous rename grabbed a synthetic system-meta
    //     marker (`[System meta · not a user message]`,
    //     `[You are stuck — ...]`, etc.) that the agent injects as a
    //     Role::User message for plumbing reasons. Re-derive from the
    //     next non-synthetic user turn so the /resume picker stops
    //     showing those as session titles.
    //   * `user_renamed` — if the user explicitly renamed (via /rename),
    //     never auto-name, regardless of name format.
    let should_rename = !session.user_renamed
        && (session.name == "default"
            || session.name.starts_with("session-")
            || session.name.trim_start().starts_with('['));
    if should_rename {
        use atomcode_core::conversation::message::Role;
        // Primary signal: `Message.synthetic` field (accurate for sessions
        // saved after the field landed). Secondary signal: bracket-prefix
        // legacy heuristic for sessions saved before the field existed
        // and so default-loaded as `synthetic = false`.
        let first_real_user = session
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User) && !m.synthetic)
            .find_map(|m| m.text().filter(|t| !is_synthetic_user_text(t)));
        if let Some(text) = first_real_user {
            let name: String = text.lines().next().unwrap_or("").chars().take(40).collect();
            if !name.is_empty() {
                session.name = name;
            }
        }
        // Else: leave the existing default/session-<ts>/[...]-marker
        // name. Better to keep a generic placeholder than to commit to
        // a synthetic injection as the title.
    }
}

/// True when `text` looks like a synthetic user-channel injection
/// (atomcode plumbs system-meta control signals through `add_user_message`
/// and tags them with a leading `[...]` bracket marker on the first line:
/// `[System meta · not a user message]`, `[You are stuck — ...]`, etc.).
/// Used by session naming to skip these so `/resume` titles stay
/// human-meaningful.
fn is_synthetic_user_text(text: &str) -> bool {
    text.trim_start().starts_with('[')
}

#[cfg(test)]
mod session_naming_tests {
    use super::{apply_session_snapshot, is_synthetic_user_text};

    #[test]
    fn apply_session_snapshot_renames_from_first_real_user() {
        use atomcode_core::conversation::message::{Message, Role};
        let mut session = atomcode_core::session::Session::default_session(
            std::path::PathBuf::from("/tmp/project"),
        );
        let messages = vec![
            Message::new(Role::User, "[System meta · not a user message]\nread this"),
            Message::new(Role::User, "implement background sessions\nwith tests"),
        ];

        apply_session_snapshot(
            &mut session,
            atomcode_core::conversation::ConversationSnapshot {
                messages,
                cold_summaries: Vec::new(),
            },
        );

        assert_eq!(session.name, "implement background sessions");
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn apply_session_snapshot_preserves_custom_name() {
        use atomcode_core::conversation::message::{Message, Role};
        let mut session = atomcode_core::session::Session::default_session(
            std::path::PathBuf::from("/tmp/project"),
        );
        session.name = "manual name".to_string();

        apply_session_snapshot(
            &mut session,
            atomcode_core::conversation::ConversationSnapshot {
                messages: vec![Message::new(Role::User, "new task")],
                cold_summaries: Vec::new(),
            },
        );

        assert_eq!(session.name, "manual name");
    }

    #[test]
    fn apply_session_snapshot_preserves_cold_summaries() {
        use atomcode_core::conversation::{
            message::{Message, Role},
            ConversationSnapshot,
        };
        let mut session = atomcode_core::session::Session::default_session(
            std::path::PathBuf::from("/tmp/project"),
        );

        apply_session_snapshot(
            &mut session,
            ConversationSnapshot {
                messages: vec![Message::new(Role::User, "new task")],
                cold_summaries: vec!["compressed context".to_string()],
            },
        );

        assert_eq!(session.cold_summaries, vec!["compressed context"]);
    }

    #[test]
    fn synthetic_system_meta_is_detected() {
        assert!(is_synthetic_user_text(
            "[System meta · not a user message]\n12 calls..."
        ));
    }

    #[test]
    fn synthetic_stuck_warning_is_detected() {
        assert!(is_synthetic_user_text(
            "[You are stuck — read foo.rs repeatedly without making progress.]"
        ));
    }

    #[test]
    fn leading_whitespace_does_not_hide_synthetic_marker() {
        assert!(is_synthetic_user_text("   [System meta] body"));
    }

    #[test]
    fn real_user_message_is_not_synthetic() {
        assert!(!is_synthetic_user_text("Fix the auth bug in login.rs"));
        assert!(!is_synthetic_user_text("Continue."));
        assert!(!is_synthetic_user_text("(why does this break?)"));
    }

    #[test]
    fn ai_rename_applies_unless_user_renamed_or_already_ai_named() {
        use atomcode_core::agent::session_title::should_accept_ai_name;
        // Not user-renamed and not yet AI-named → accept (the AI title wins
        // over the first-turn truncation the auto-namer set).
        assert!(should_accept_ai_name(false, false));
        // A deliberate /rename is never overwritten.
        assert!(!should_accept_ai_name(true, false));
        // Already AI-named → don't re-name on reconnect/resume.
        assert!(!should_accept_ai_name(false, true));
    }
}

/// Build the persistent status line shown directly below the input box.
/// Pulls model name from ctx, cwd from ctx.working_dir (with $HOME
/// collapsed to `~`), and running token count from state.
/// Probe the system clipboard for an image, memoising the result inside
/// the supplied cache for `CLIPBOARD_HINT_TTL_MS`. `build_status` calls
/// this on every redraw, so without caching every spinner tick (~12/s
/// during streaming) would round-trip to the platform clipboard API.
const CLIPBOARD_HINT_TTL_MS: u64 = 1500;

fn clipboard_image_hash(cache: &std::sync::Mutex<ClipboardCheckState>) -> Option<u64> {
    let mut state = cache.lock().unwrap_or_else(|e| e.into_inner());
    let stale = state
        .last_checked
        .map(|t| t.elapsed() >= std::time::Duration::from_millis(CLIPBOARD_HINT_TTL_MS))
        .unwrap_or(true);
    if stale {
        state.image_hash = arboard::Clipboard::new()
            .and_then(|mut c| c.get_image())
            .ok()
            .map(|img| rgba_fingerprint(img.width, img.height, img.bytes.as_ref()));
        state.last_checked = Some(std::time::Instant::now());
    }
    state.image_hash
}

pub(crate) fn build_status(state: &UiState, ctx: &LoopCtx) -> crate::render::StatusLine {
    let cwd = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
    // Priority:
    //   1. No provider configured + not logged in — show "configure" nudge.
    //      This wins over the upgrade hint because without a provider the
    //      app literally cannot answer any message; the user needs to know
    //      why before they're told to upgrade.
    //   2. Upgrade-available hint (existing behavior).
    //   3. None.
    let no_provider =
        ctx.config.providers.is_empty() && atomcode_core::auth::get_stored_auth().is_none();
    // Open-source build pointed at an AtomGit gateway: any chat will
    // fail-fast with `CpOfficialBuildRequired`. Surface that diagnosis
    // up front (red, beats every other hint) so the user doesn't have
    // to type a message to discover the dead-end — `/login` won't help,
    // only switching to the official build will.
    let active_base_url = ctx
        .config
        .active_provider(None)
        .ok()
        .and_then(|p| p.base_url.clone())
        .unwrap_or_default();
    let needs_official_build = !atomcode_core::coding_plan::signer_available()
        && atomcode_core::coding_plan::is_atomgit_gateway(&active_base_url);
    // Priority: needs-official-build (Warning red) > no-provider (Warning
    // red) > CodingPlan drift monitor (Warning red) > CodingPlan
    // token-usage hint (Info ≥80%, Warning ≥95%) > upgrade banner
    // (Info dim). Usage outranks upgrade because ">80% in this rolling
    // window" is more actionable than "new version available". Only one
    // hint renders at a time (right-aligned on the status row).
    let hint: Option<(String, crate::render::HintSeverity)> = if needs_official_build {
        Some((
            crate::i18n::t(crate::i18n::Msg::StatusOfficialBuildRequired).into_owned(),
            crate::render::HintSeverity::Warning,
        ))
    } else if no_provider {
        Some((
            crate::i18n::t(crate::i18n::Msg::StatusNoProvider).into_owned(),
            crate::render::HintSeverity::Warning,
        ))
    } else if let Some(warning) = monitor::is_codingplan_provider(&ctx.config.default_provider)
        .then(|| ctx.monitor_warning.lock().ok().and_then(|g| g.clone()))
        .flatten()
    {
        // Only surface the CodingPlan drift warning while a CodingPlan-managed
        // (AtomGit*) provider is active. A warning set on an AtomGit provider
        // must not linger after the user switches to a custom provider via a
        // path that doesn't clear the slot (e.g. `/provider`) — the hint is
        // meaningless for non-CodingPlan models.
        Some((warning.display_text(), crate::render::HintSeverity::Warning))
    } else if let Some(hook_msg) = ctx.hook_warning_hint.lock().ok().and_then(|g| g.clone()) {
        Some((hook_msg, crate::render::HintSeverity::Warning))
    } else if let Some(usage) =
        usage_monitor::build_usage_hint(&ctx.usage_slot, &ctx.config.default_provider)
    {
        Some(usage)
    } else if let Some(h) = clipboard_image_hash(&ctx.clipboard_check)
        .filter(|h| !state.pending_image_hashes.contains(h))
    {
        // Transient cue — beats the upgrade banner because the action
        // window is "now" (the image is in the clipboard right now).
        // Suppressed when the clipboard's image fingerprint matches one
        // already in `pending_images`: the input box already shows
        // `[Image #N]`, prompting another paste of the same image would
        // just attach a dup. A NEW image (different fingerprint) appears
        // here as a fresh hint so the user can attach it too.
        let _ = h;
        // Windows Terminal / conhost swallow Ctrl+V (they bind it to
        // their own `paste` action that only forwards CF_UNICODETEXT,
        // so an image-only clipboard never reaches the in-app
        // handler). Surface the `/paste` fallback on Windows; macOS /
        // Linux terminals pass Ctrl+V through cleanly, so they keep
        // the snappier keybind hint.
        let hint_msg = if cfg!(target_os = "windows") {
            crate::i18n::Msg::StatusClipboardImageHintSlash
        } else {
            crate::i18n::Msg::StatusClipboardImageHint
        };
        Some((
            crate::i18n::t(hint_msg).into_owned(),
            crate::render::HintSeverity::Info,
        ))
    } else if let Some(v) = ctx.update_hint.lock().ok().and_then(|g| g.clone()) {
        let text = if atomcode_core::self_update::is_package_managed() {
            crate::i18n::t(crate::i18n::Msg::StatusUpgradeHintPm { version: &v }).into_owned()
        } else {
            crate::i18n::t(crate::i18n::Msg::StatusUpgradeHint { version: &v }).into_owned()
        };
        Some((text, crate::render::HintSeverity::Info))
    } else {
        // Lowest-priority fallback: surface the `/webui` browser-UI entry
        // point, which is otherwise easy to miss. Yields the slot to every
        // higher-priority hint above (warnings / usage / upgrade).
        Some((
            crate::i18n::t(crate::i18n::Msg::StatusWebuiHint).into_owned(),
            crate::render::HintSeverity::Info,
        ))
    };
    // Pre-configure, `ctx.model_name` is a dummy from the startup fallback
    // (empty string or "not-configured") — showing that raw in the status
    // line reads as a glitch. Replace with an explicit placeholder so the
    // user sees the state, not a rendering artifact.
    let model = if no_provider {
        crate::i18n::t(crate::i18n::Msg::StatusModelNotConfigured).into_owned()
    } else {
        ctx.model_name.clone()
    };
    // Mode indicator: only rendered when the user has explicitly
    // switched away from the default Build mode. Plan disables file
    // edits + shell, so making it prominent in the status line guards
    // against the user being confused why the agent refuses to write
    // files. Default Build = None, no visual noise.
    let mode_indicator = match state.agent_mode {
        crate::state::AgentMode::Plan => Some("PLAN".to_string()),
        crate::state::AgentMode::Build => None,
    };
    // Bypass indicator: right-aligned warning badge when
    // --dangerously-skip-permissions / -y is active. Placed on the
    // right side of the status row so it does not displace the PLAN
    // mode indicator on the left.
    let bypass_indicator = if ctx.dangerously_skip_permissions {
        Some(crate::i18n::t(crate::i18n::Msg::BypassBadge).into_owned())
    } else {
        None
    };
    // Pull current ctx usage from the last ContextStats emission. Pre-
    // first-turn `last_context` is None — render shows nothing then.
    // Using `sent_tokens` (what was actually sent to the model on the
    // last turn) instead of cumulative `total_tokens` because the user
    // cares about "how close to overflow am I", not "how many tokens
    // has this session burned in total". See render::StatusLine docs.
    let (ctx_used, ctx_window) = match state.last_context.as_ref() {
        Some(snap) => (snap.sent_tokens, snap.ctx_window),
        None => (0, 0),
    };
    // Session-name badge: surfaced only when the user has explicitly
    // renamed the conversation. Auto-named sessions (default /
    // session-* / first-message-derived) intentionally stay badge-less
    // so the chrome stays quiet on fresh conversations.
    let session_name = if ctx.current_session.user_renamed {
        let name = ctx.current_session.name.trim();
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    } else {
        None
    };
    // Active /goal → the dedicated footer goal row (width-truncated by the
    // renderer). Carries the condition text so the user can SEE what the goal
    // is, not just that one is running.
    let goal = state.goal_condition.as_ref().map(|cond| crate::render::GoalStatus {
        condition: cond.clone(),
        // Display 1-based: the engine's round is 0 on the first attempt.
        round: state.goal_round + 1,
        elapsed_secs: state.goal_started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0),
    });
    crate::render::StatusLine {
        model,
        cwd,
        ctx_used,
        ctx_window,
        hint,
        mode_indicator,
        bypass_indicator,
        reasoning_effort: if reasoning_effort_applicable_on_provider(ctx) {
            ctx.reasoning_effort.clone()
        } else {
            None
        },
        session_name,
        goal,
    }
}

/// Render one spinner frame. Used from both the interval-driven tick
/// path and the opportunistic "post-event" pump path that guards
/// against agent-event floods starving the interval tick.
///
/// When the type-ahead buffer starts with `/`, the slash-command palette
/// is attached so the user can see candidate commands mid-stream (the
/// renderer then shows the menu in place of the spinner).
fn draw_spinner_now(
    state: &mut UiState,
    buf: &Buffer,
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
    queue_len: usize,
    menu_selected: usize,
) {
    let frame = state.tick_spinner();
    // Same source + applicability gate as the status bar's `[high]`, so the
    // spinner's effort hint and the status line never disagree. (Reading
    // `state.reasoning_effort` here showed nothing when effort came from the
    // provider config / webui rather than a Ctrl+T cycle.)
    let effort = if reasoning_effort_applicable_on_provider(ctx) {
        ctx.reasoning_effort.as_deref()
    } else {
        None
    };
    let label = format_spinner_label(state, queue_len, effort);
    let status = build_status(state, ctx);
    let menu = menu_for_display(buf, ctx).map(|items| {
        let selected = menu_selected.min(items.len().saturating_sub(1));
        let kind = if file_index::detect_at_mention_range(&buf.text, buf.cursor).is_some() {
            crate::render::MenuKind::AtMention
        } else if buf.text.starts_with('$') {
            crate::render::MenuKind::Skill
        } else {
            crate::render::MenuKind::SlashCommand
        };
        crate::render::MenuPayload {
            items,
            selected,
            kind,
        }
    });
    let attachments = compute_input_attachments(state, &buf.text);
    renderer.render(UiLine::StreamingBox {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        frame,
        label,
        status,
        menu,
        attachments,
    });
    renderer.flush();
}

/// Build the spinner line shown in the footer —
/// `"{label}… · {elapsed} · {N} queued"`. State stores only the bare
/// word (e.g. `Pondering`, `Running ReadFile`); ellipsis + elapsed +
/// queued suffixes are appended here so format is consistent across
/// every call site.
fn format_spinner_label(state: &UiState, queue_len: usize, reasoning_effort: Option<&str>) -> String {
    // Compaction takes over the spinner: show "Compacting…" (or a slow variant
    // past the stall threshold) with the phase elapsed appended, instead of a
    // thinking label. Unified for auto + manual /compact.
    if state.compacting {
        let base = if state.compaction_stalled() {
            crate::i18n::t(crate::i18n::Msg::CompactingSlow)
        } else {
            crate::i18n::t(crate::i18n::Msg::Compacting)
        };
        let mut out = base.into_owned();
        if let Some(d) = state.phase_elapsed() {
            out.push_str(&format!(" · {}", crate::render::fmt_dur(d)));
        }
        return out;
    }
    // `display_spinner_label` maps `Running X` / `Preparing X` back to the
    // thinking word so the footer never flashes tool names — tool progress is
    // carried by the body `▸ Tool(detail)` rows. Other labels pass through.
    let base = state.display_spinner_label();
    let mut out = format!("{}{}", base, state.ellipsis());
    // Order matters. The phase clock (`· 372ms`) ticks every frame, and any
    // segment AFTER a rapidly-changing field shifts on every redraw — which
    // read as flicker when the elapsed sat in the middle (user report:
    // `Cogitating… · 372ms · thinking with high effort` jittered the effort
    // text). So: static segments first, the ticking elapsed dead last.
    //
    // Reasoning-effort hint (deepseek-v4 high/max), mirroring CC's
    // `… · thinking with high effort`. The value comes from the caller (the
    // ctx-sourced, applicability-gated effort — the SAME source as the status
    // bar's `[high]`, so the two never disagree). Placed FIRST among the
    // metadata so `spinner_meta_suffix` can splice it out (a tool isn't
    // "thinking") while still forwarding the trailing time/queue anchors.
    if let Some(effort) = reasoning_effort {
        out.push_str(&format!(" · thinking with {} effort", effort));
    }
    if queue_len > 0 {
        out.push_str(&format!(" · {} queued", queue_len));
    }
    // (The mid-stream "· esc to cancel" stall hint was removed by request — esc
    // still cancels, it's just no longer advertised in the spinner. The stall
    // machinery (`stream_stalled`) stays for the compaction-slow variant.)
    // Phase elapsed (NOT total turn elapsed) — `Pondering… 8s`,
    // `Running ReadFile… 4s`. CC behaviour: timer resets on every phase
    // transition so the user reads "this thing has been running for N
    // seconds", not "the whole turn so far is 1301s". LAST, so its per-frame
    // width changes never shift anything after it.
    if let Some(d) = state.phase_elapsed() {
        out.push_str(&format!(" · {}", crate::render::fmt_dur(d)));
    }
    out
}

/// Convert a snake_case tool name to PascalCase for display. The agent
/// protocol uses `read_file`, `edit_file`, `web_fetch` etc.; the UI shows
/// `ReadFile`, `EditFile`, `WebFetch` — a CC-style convention that reads
/// more cleanly at a glance.
///
/// MCP tools arrive on the wire as `mcp__<server>__<tool>`. Naive
/// PascalCase collapses the three parts into `McpZouwuQueryRequirements`
/// where the server / tool boundary disappears. Render them with a
/// middle-dot separator so users can tell at a glance which part is the
/// server and which is the tool (#299).
pub fn display_tool_name(snake: &str) -> String {
    if let Some(rest) = snake.strip_prefix("mcp__") {
        if let Some((server, tool)) = rest.split_once("__") {
            return format!("mcp · {} · {}", server, tool);
        }
    }
    pascal_case(snake)
}

fn pascal_case(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    for word in snake.split('_') {
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// CC-style short tool name. Strips the redundant `_file` /
/// `_directory` / `_files` suffixes (the noun is implicit from the
/// arg) before PascalCase conversion. Generic — no per-tool match
/// arms; works for any future tool that follows the
/// `<verb>_<noun>` convention.
///
/// Examples:
/// - `read_file` → `Read`
/// - `write_file` → `Write`
/// - `list_directory` → `List`
/// - `parallel_edit_files` → `ParallelEdit`
/// - `bash` → `Bash` (no suffix to strip)
/// - `search_replace` → `SearchReplace` (suffix `_replace` not in
///    strip list, kept verbatim → preserves disambiguation)
pub fn display_tool_name_short(snake: &str) -> String {
    // MCP wire names (`mcp__server__tool`) carry their suffix as part of the
    // real tool name — stripping `_file`/`_files`/`_directory` here would turn
    // `mcp__fs__read_file` into `mcp · fs · read`. Hand the full name to
    // display_tool_name so the `mcp · server · tool` split stays verbatim.
    if snake.starts_with("mcp__") {
        return display_tool_name(snake);
    }
    const STRIP_SUFFIXES: &[&str] = &["_files", "_file", "_directory"];
    let trimmed = STRIP_SUFFIXES
        .iter()
        .find_map(|s| snake.strip_suffix(s))
        .unwrap_or(snake);
    display_tool_name(trimmed)
}

/// Parse the full task list every `todo` result returns into `map`
/// (id → content). The list renders as `{icon} {id}. {content}` lines
/// (see `todo.rs::render`); we match that shape and ignore everything
/// else (the "Added task #N: …" / "Task #N '…' updated" preamble lines
/// don't match and are skipped). Both the core and capabilities todo
/// tools share this line format, so one parser covers both.
pub(crate) fn parse_todo_titles_into(
    map: &mut std::collections::HashMap<u64, String>,
    output: &str,
) {
    for line in output.lines() {
        let line = line.trim_start();
        // "[<icon>] <id>. <content>"
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((_icon, after)) = rest.split_once("] ") else {
            continue;
        };
        let Some((id_str, content)) = after.split_once(". ") else {
            continue;
        };
        let Ok(id) = id_str.trim().parse::<u64>() else {
            continue;
        };
        let content = content.trim();
        if !content.is_empty() {
            map.insert(id, content.to_string());
        }
    }
}

/// Splice the known task title into a `todo update` detail so the row
/// shows the NAME, not just the id: `#4 → completed` becomes
/// `#4 Write tests → completed`. Returns `base` unchanged for non-todo
/// tools, non-update actions, or ids whose title we haven't learned yet
/// (graceful fallback to the id-only form).
pub(crate) fn enrich_todo_detail(
    name: &str,
    args_json: &str,
    base: &str,
    titles: &std::collections::HashMap<u64, String>,
) -> String {
    if name != "todo" {
        return base.to_string();
    }
    let repaired = atomcode_core::turn::json_repair::repair_tool_args(name, args_json);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&repaired) else {
        return base.to_string();
    };
    if v.get("action").and_then(|a| a.as_str()) != Some("update") {
        return base.to_string();
    }
    let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
        return base.to_string();
    };
    let Some(title) = titles.get(&id) else {
        return base.to_string();
    };
    let title = crate::width::truncate_with_ellipsis(title, 80);
    // `base` is `#<id> → <status>` (or bare `#<id>`); insert the title
    // right after the id so the arrow keeps pointing at the status.
    match base.split_once(" \u{2192} ") {
        Some((head, tail)) => format!("{} {} \u{2192} {}", head, title, tail),
        None => format!("{} {}", base, title),
    }
}

pub(crate) fn format_tool_detail(name: &str, args_json: &str) -> String {
    let repaired_args = atomcode_core::turn::json_repair::repair_tool_args(name, args_json);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&repaired_args) else {
        return String::new();
    };
    let get_str = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let basename = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();

    match name {
        "read_file" | "edit_file" | "write_file" | "create_file" | "list_symbols" => {
            // Single-call path: basename only (compact). Batch disambiguation
            // is handled by `disambiguate_batch_details` which runs after
            // all child details are computed and can compare them.
            get_str("file_path")
                .map(|p| basename(&p))
                .unwrap_or_default()
        }
        "read_symbol" => {
            let sym = get_str("symbol").unwrap_or_default();
            let file = get_str("file_path")
                .map(|p| basename(&p))
                .unwrap_or_default();
            if sym.is_empty() {
                file
            } else if file.is_empty() {
                sym
            } else {
                format!("{} in {}", sym, file)
            }
        }
        "glob" => get_str("pattern")
            .map(|p| crate::width::truncate_with_ellipsis(&p, 100))
            .unwrap_or_default(),
        "grep" => get_str("pattern")
            .map(|p| crate::width::truncate_with_ellipsis(&p, 100))
            .unwrap_or_default(),
        "bash" => get_str("command")
            .map(|c| crate::width::truncate_with_ellipsis(&c, 500))
            .unwrap_or_default(),
        "list_directory" | "change_dir" => get_str("path").unwrap_or_else(|| ".".into()),
        "code_review" => {
            let base = get_str("base");
            let staged = v.get("staged").and_then(|x| x.as_bool()).unwrap_or(false);
            match (base, staged) {
                (Some(b), true) => format!("base: \"{}\", staged: true", b),
                (Some(b), false) => format!("base: \"{}\"", b),
                (None, true) => "staged: true".to_string(),
                (None, false) => String::new(),
            }
        }
        "web_fetch" => get_str("url")
            .map(|u| crate::width::truncate_with_ellipsis(&u, 150))
            .unwrap_or_default(),
        "web_search" => get_str("query")
            .map(|q| crate::width::truncate_with_ellipsis(&q, 100))
            .unwrap_or_default(),
        "find_references" | "trace_callees" | "trace_callers" => {
            get_str("symbol").unwrap_or_default()
        }
        "trace_chain" => {
            // trace_chain takes `from`/`to`, not `symbol` — keep this branch
            // separate so the detail isn't blank. See trace_chain.rs Args.
            let from = get_str("from").unwrap_or_default();
            let to = get_str("to").unwrap_or_default();
            if from.is_empty() || to.is_empty() {
                String::new()
            } else {
                format!("{} → {}", from, to)
            }
        }
        "blast_radius" | "file_dependencies" => {
            // Same as above: basename for single-call; batch disambiguation
            // handled by `disambiguate_batch_details`.
            get_str("file").map(|p| basename(&p)).unwrap_or_default()
        }
        "search_replace" => {
            // SearchReplaceArgs uses search/replace/glob/path (not
            // file_path/file/pattern/old). Show "search → replace" so
            // the approval prompt tells the user WHAT will be replaced.
            let search = get_str("search");
            let replace = get_str("replace");
            let glob = get_str("glob");
            let path = get_str("path");
            match (&search, &replace) {
                (Some(s), Some(r)) => {
                    let arrow = format!(
                        "{} → {}",
                        crate::width::truncate_with_ellipsis(s, 60),
                        crate::width::truncate_with_ellipsis(r, 60)
                    );
                    let mut parts = vec![arrow];
                    if let Some(g) = &glob {
                        parts.push(format!("glob: {}", g));
                    }
                    if let Some(p) = &path {
                        if p != "." {
                            parts.push(format!("path: {}", basename(p)));
                        }
                    }
                    parts.join(", ")
                }
                (None, Some(r)) => crate::width::truncate_with_ellipsis(r, 100),
                (Some(s), None) => crate::width::truncate_with_ellipsis(s, 100),
                _ => String::new(),
            }
        }
        "parallel_edit_files" => {
            // Show the list of target file basenames so the user can see
            // WHAT will be edited at a glance in the approval prompt —
            // mirroring how Bash(rm -rf /path) tells the user exactly
            // which command needs approval. Without this, the approval
            // prompt just shows "ParallelEditFiles:" with no detail,
            // leaving the user blind to the scope of the dispatch.
            if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
                let names: Vec<String> = files
                    .iter()
                    .filter_map(|entry| {
                        entry
                            .get("path")
                            .and_then(|p| p.as_str())
                            .map(|s| basename(s))
                    })
                    .collect();
                let detail = names.join(", ");
                crate::width::truncate_with_ellipsis(&detail, 200)
            } else {
                String::new()
            }
        }
        "todo" => {
            // Show the task description (add) or id+status (update) so the
            // user can see WHAT the agent is tracking without expanding the row.
            let action = get_str("action").unwrap_or_default();
            match action.as_str() {
                "add" => get_str("content")
                    .map(|c| crate::width::truncate_with_ellipsis(&c, 100))
                    .unwrap_or_default(),
                "update" => {
                    let id = v.get("id").and_then(|x| x.as_u64());
                    let status = get_str("status").unwrap_or_default();
                    match (id, status.as_str()) {
                        (Some(i), s) if !s.is_empty() => format!("#{} → {}", i, s),
                        (Some(i), _) => format!("#{}", i),
                        (None, s) if !s.is_empty() => s.to_string(),
                        _ => String::new(),
                    }
                }
                "list" => "list all".to_string(),
                _ => String::new(),
            }
        }
        "use_skill" => get_str("name").unwrap_or_default(),
        _ => {
            // For MCP tools (name starts with mcp__), render the
            // arguments as key=value pairs so users can see what
            // parameters are being passed to the external server.
            if name.starts_with("mcp__") {
                if let Some(obj) = v.as_object() {
                    let pairs: Vec<String> = obj
                        .iter()
                        .filter_map(|(k, val)| {
                            let s = match val {
                                serde_json::Value::String(s) => s.clone(),
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::Bool(b) => b.to_string(),
                                serde_json::Value::Array(a) => {
                                    serde_json::to_string(a).unwrap_or_default()
                                }
                                serde_json::Value::Object(o) => {
                                    serde_json::to_string(o).unwrap_or_default()
                                }
                                _ => return None,
                            };
                            if s.is_empty() {
                                return None;
                            }
                            Some(format!("{}: \"{}\"", k, s.replace('"', "\\\"")))
                        })
                        .collect();
                    if !pairs.is_empty() {
                        return crate::width::truncate_with_ellipsis(&pairs.join(", "), 450);
                    }
                }
            }
            // Fallback: try common single-key args that make sense as detail.
            for key in [
                "file_path",
                "path",
                "file",
                "pattern",
                "query",
                "url",
                "name",
                "symbol",
                "command",
            ] {
                if let Some(s) = get_str(key) {
                    return crate::width::truncate_with_ellipsis(&s, 100);
                }
            }
            String::new()
        }
    }
}

/// Disambiguate parallel-batch child details when multiple calls produce
/// the same short display (e.g. 3 × `Read(SKILL.md)` from different dirs).
///
/// For each child whose `raw_detail` duplicates another, walks up the
/// path from basename toward the root until all duplicates are unique.
/// Non-duplicate entries are left unchanged.
///
/// Example:
///   paths: [skills/a/SKILL.md, skills/b/SKILL.md, skills/c/SKILL.md]
///   raw_details: [SKILL.md, SKILL.md, SKILL.md]
///   → [a/SKILL.md, b/SKILL.md, c/SKILL.md]
///
/// If paths can't be extracted from arguments (non-file tools), falls
/// back to appending `#2`, `#3` suffixes.
fn disambiguate_batch_details(
    names: &[&str],
    args_jsons: &[&str],
    raw_details: &[String],
) -> Vec<String> {
    // Fast path: no duplicates → return as-is.
    let mut seen = std::collections::HashMap::<&str, usize>::new();
    let mut has_dups = false;
    for d in raw_details {
        let count = seen.entry(d.as_str()).or_insert(0);
        *count += 1;
        if *count > 1 {
            has_dups = true;
        }
    }
    if !has_dups {
        return raw_details.to_vec();
    }

    // Extract full paths from args where possible.
    let extract_path = |name: &str, args_json: &str| -> Option<String> {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
            return None;
        };
        let get_str = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        match name {
            "read_file" | "edit_file" | "write_file" | "create_file" | "list_symbols"
            | "blast_radius" | "file_dependencies" => {
                get_str("file_path").or_else(|| get_str("file"))
            }
            "search_replace" => get_str("file_path").or_else(|| get_str("file")),
            "read_symbol" => get_str("file_path"),
            _ => None,
        }
    };

    let full_paths: Vec<Option<String>> = names
        .iter()
        .zip(args_jsons.iter())
        .map(|(n, a)| extract_path(n, a))
        .collect();

    // For each group of duplicates, progressively add parent path
    // components until unique within that group.
    let mut result = raw_details.to_vec();

    // Collect groups of indices that share the same raw_detail.
    let mut groups: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, d) in raw_details.iter().enumerate() {
        groups.entry(d.as_str()).or_default().push(i);
    }

    for (_detail, indices) in groups {
        if indices.len() < 2 {
            continue; // unique, no disambiguation needed
        }

        // Check if we have full paths for all in this group.
        let all_have_paths = indices.iter().all(|&i| full_paths[i].is_some());

        if all_have_paths {
            // Strategy: progressively add parent path components until
            // all entries are unique. Start with 1 parent component
            // (e.g. `a/SKILL.md`), then 2 (`b/a/SKILL.md`), etc.
            let paths: Vec<&str> = indices
                .iter()
                .map(|&i| full_paths[i].as_deref().unwrap())
                .collect();
            let mut depth = 1usize;
            let max_depth = paths
                .iter()
                .map(|p| p.matches('/').count())
                .max()
                .unwrap_or(0);

            loop {
                let candidates: Vec<String> = paths.iter().map(|p| tail_path(p, depth)).collect();

                let all_unique = {
                    let mut s = std::collections::HashSet::new();
                    candidates.iter().all(|c| s.insert(c.as_str()))
                };

                if all_unique || depth >= max_depth {
                    for (i, &idx) in indices.iter().enumerate() {
                        result[idx] = crate::width::truncate_with_ellipsis(&candidates[i], 100);
                    }
                    break;
                }
                depth += 1;
            }
        } else {
            // Fallback: append #2, #3, … suffixes to disambiguate.
            for (seq, &idx) in indices.iter().enumerate() {
                if seq > 0 {
                    let suffixed = format!("{} #{}", raw_details[idx], seq + 1);
                    result[idx] = crate::width::truncate_with_ellipsis(&suffixed, 100);
                }
            }
        }
    }

    result
}

/// Return the last `depth + 1` path components of `path`.
/// E.g. tail_path("a/b/c/SKILL.md", 1) → "c/SKILL.md"
///      tail_path("a/b/c/SKILL.md", 2) → "b/c/SKILL.md"
///      tail_path("a/b/c/SKILL.md", 3) → "a/b/c/SKILL.md"
fn tail_path(path: &str, depth: usize) -> String {
    if depth == 0 {
        return path.rsplit('/').next().unwrap_or(path).to_string();
    }
    // Walk backwards counting separators. To keep `depth + 1` components,
    // we need to find the separator that is `depth + 1`-th from the end,
    // then return everything after it.
    // For depth=1 in "a/b/c/SKILL.md": we need "c/SKILL.md",
    // so find the '/' between "b" and "c" (2nd from end), return after it.
    let mut seen = 0;
    for (i, ch) in path.char_indices().rev() {
        if ch == '/' {
            seen += 1;
            if seen == depth + 1 {
                return path[(i + ch.len_utf8())..].to_string();
            }
        }
    }
    // Fewer than `depth + 1` separators — return the whole path.
    path.to_string()
}

/// Render an `elapsed_ms` value as `XmYs` (over 60 s) or `Ts` (under).
/// Tens of milliseconds aren't useful for the UI; whole seconds match
/// what users see on a wall clock and align column-wise across rows.
pub(crate) fn fmt_elapsed(ms: u64) -> String {
    let total_secs = ms / 1000;
    if total_secs >= 60 {
        format!("{}m{}s", total_secs / 60, total_secs % 60)
    } else {
        format!("{}s", total_secs)
    }
}

/// Build the one-line preview shown under a tool call (`└ …`): the
/// output's first line, plus a ` (N lines)` suffix when it spans more.
///
/// No display-width budget here on purpose. The retained renderer wraps
/// this to the LIVE terminal width (`wrap_line_to_width(_, screen.width()
/// − …)` in the `UiLine::ToolResult` arm), so the preview fills whatever
/// width the screen has and re-fits on resize — and the ` (N lines)`
/// suffix is never lost because wrapping carries it to a continuation row.
/// We used to hard-cap at 80 cols (success) / 200 (failure), which baked a
/// `…` into the string and wasted the right half of wide screens. The
/// 512-col cap that remains is a pure safety bound: it only trips for a
/// pathological multi-KB single line (e.g. a minified file) so it can't
/// wrap into dozens of rows. Real first lines are far shorter.
/// Extract the unique result source domains from a `web_search` tool result.
///
/// Scans the output text for `http(s)://` URLs, keeps each host, strips a
/// leading `www.`, and dedupes preserving first-seen order. Works for the
/// DuckDuckGo backend (explicit `url` lines) and the Exa backend (URLs are
/// embedded in the LLM-ready text blob). Returns `[]` when the output carries
/// no URLs (an Exa blob without links, or an error message) so the caller can
/// fall back to the generic line count.
pub(crate) fn web_search_source_domains(output: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut rest = output;
    while let Some(pos) = rest.find("http") {
        let after = &rest[pos..];
        let scheme_len = if after.starts_with("https://") {
            8
        } else if after.starts_with("http://") {
            7
        } else {
            // "http" not part of a URL scheme — step past it (ASCII, so the
            // byte offset is a valid char boundary) and keep scanning.
            rest = &rest[pos + 4..];
            continue;
        };
        let host_part = &after[scheme_len..];
        // Host ends at the first path / query / port / delimiter byte. All
        // candidates are ASCII, so the split stays on a char boundary even if
        // the host itself contains non-ASCII (IDN) bytes.
        let end = host_part
            .find(|c: char| {
                matches!(c, '/' | '?' | '#' | ':' | ')' | ']' | '"' | '\'' | '>' | '<' | ',')
                    || c.is_whitespace()
            })
            .unwrap_or(host_part.len());
        let host = host_part[..end].trim_end_matches('.');
        let host = host.strip_prefix("www.").unwrap_or(host);
        if host.contains('.') && seen.insert(host.to_string()) {
            out.push(host.to_string());
        }
        rest = &host_part[end..];
    }
    out
}

/// Build the ` → src1, src2 +N` summary suffix for a completed `web_search`
/// call. Shows up to two source domains; any remainder collapses to `+N`.
/// Returns `None` when no domains could be extracted, so the caller keeps the
/// generic line-count suffix.
pub(crate) fn web_search_result_suffix(output: &str) -> Option<String> {
    const SHOWN: usize = 2;
    let domains = web_search_source_domains(output);
    if domains.is_empty() {
        return None;
    }
    let head = domains.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    let extra = domains.len().saturating_sub(SHOWN);
    Some(if extra > 0 {
        format!("{head} +{extra}")
    } else {
        head
    })
}

pub(crate) fn summarise(output: &str) -> String {
    let first = output.lines().next().unwrap_or("(no output)");
    let n = output.lines().count();
    // `truncate_with_ellipsis` (not bare `truncate_to_width`) so that if
    // the safety bound ever does bite, the cut is visibly marked.
    let trimmed = crate::width::truncate_with_ellipsis(first, 512);
    if n > 1 {
        format!("{} ({} lines)", trimmed, n)
    } else {
        trimmed
    }
}

/// A plan-mode interception surfaces as a failed tool result whose body is
/// `blocked: plan mode …` — the kernel prefixes every middleware block with `blocked: `,
/// and `PlanModeGate`'s reason starts with `plan mode is active`. Returns the human reason
/// (sans the `blocked: ` prefix) when the result is such a block, so the UI can render a
/// calm `○` hint instead of a ✗ error — plan-mode enforcement is EXPECTED, not a failure.
/// The model still receives the full `blocked: …` ToolResult via the conversation. The link
/// to the gate's wording is by string; if it ever drifts this returns `None` and the normal
/// ✗ result render is used (a harmless fallback, never a panic).
pub(crate) fn plan_mode_block_reason(output: &str, success: bool) -> Option<&str> {
    if success {
        return None;
    }
    let reason = output.strip_prefix("blocked: ")?;
    reason.starts_with("plan mode").then_some(reason)
}

// SessionPicker tests moved alongside the struct in
// `crate::modals::session_picker::tests`.

/// Sync ctx.reasoning_effort from the current default provider's config.
fn sync_reasoning_effort_from_provider(ctx: &mut LoopCtx) {
    let applicable = reasoning_effort_applicable_on_provider(ctx);
    ctx.reasoning_effort = if applicable {
        ctx.config
            .providers
            .get(&ctx.config.default_provider)
            .and_then(|p| p.reasoning_effort.clone())
    } else {
        None
    };
}

/// Persist the current reasoning_effort to config.toml
fn persist_reasoning_effort(ctx: &mut LoopCtx) {
    let path = Config::default_path();
    let default_provider = ctx.config.default_provider.clone();
    if let Some(p) = ctx.config.providers.get_mut(&default_provider) {
        p.reasoning_effort = ctx.reasoning_effort.clone();
    }
    if let Err(e) = ctx.config.save(&path) {
        eprintln!("[reasoning_effort] failed to save config: {e}");
    }
}

pub(crate) fn reasoning_effort_applicable_on_provider(ctx: &LoopCtx) -> bool {
    let ptype = ctx
        .config
        .providers
        .get(&ctx.config.default_provider)
        .map(|p| p.provider_type.as_str())
        .unwrap_or("");
    // Model-name check delegates to the provider so the UI "applicable" hint
    // and the actual request-body gate (OpenAiProvider) can never diverge.
    (ptype == "deepseek" || ptype == "openai")
        && atomcode_core::provider::openai::OpenAiProvider::reason_effort_applicable(
            &ctx.model_name,
        )
}

/// Install a [`crate::modals::password::PasswordModal`] as the active modal on
/// `app`. Called from the `askpass_rx` select! arm and from the test below.
#[cfg(unix)]
pub(crate) fn install_password_modal(
    app: &mut App,
    prompt: String,
    reply: tokio::sync::oneshot::Sender<Option<String>>,
) {
    app.active_modal = Some(Box::new(
        crate::modals::password::PasswordModal::new(prompt, reply),
    ));
}

#[cfg(test)]
mod install_password_modal_tests {
    use super::*;

    fn caps_for_test() -> crate::terminal::TerminalCaps {
        crate::terminal::TerminalCaps {
            tty: false,
            colors: false,
            spinner: false,
            bracketed_paste: false,
            raw_mode: false,
            scroll_region: false,
            unicode_symbols: false,
            legacy_conhost: false,
            jediterm: false,
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_password_modal_sets_active_modal() {
        let mut app = App::new(&caps_for_test());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        install_password_modal(&mut app, "[sudo] password:".into(), tx);
        assert!(app.active_modal.is_some(), "modal installed");
    }
}
