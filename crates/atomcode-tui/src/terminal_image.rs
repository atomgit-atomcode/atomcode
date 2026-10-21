//! Paint-only terminal graphics. Image bytes never travel through text spans.

use crate::{
    caps::Graphics,
    frame::{Frame, Rect},
};
use base64::{engine::general_purpose::STANDARD, Engine};

pub const MASCOT_WIDTH: u16 = 18;
pub const MASCOT_HEIGHT: u16 = 8;
pub const MASCOT_PNG: &[u8] = include_bytes!("../assets/mascot.png");
pub const MASCOT_WAIT_PNG: &[u8] = include_bytes!("../assets/mascot-wait.png");
pub const MASCOT_GO_PNG: &[u8] = include_bytes!("../assets/mascot-go.png");
const FRAME_MS: u64 = 1500;

/// Time until the next boundary, from the host-injected clock. No repeating
/// timer is needed once the original frame returns.
pub(crate) fn next_change(
    started: Option<crate::moment::Timestamp>,
    now: crate::moment::Timestamp,
) -> Option<std::time::Duration> {
    let elapsed = now.0.saturating_sub(started?.0);
    let boundary = if elapsed < FRAME_MS {
        FRAME_MS
    } else if elapsed < FRAME_MS * 2 {
        FRAME_MS * 2
    } else {
        return None;
    };
    Some(std::time::Duration::from_millis(boundary - elapsed))
}

fn mascot_png(
    started: Option<crate::moment::Timestamp>,
    now: crate::moment::Timestamp,
) -> &'static [u8] {
    match started.map(|at| now.0.saturating_sub(at.0)) {
        Some(elapsed) if elapsed < FRAME_MS => MASCOT_WAIT_PNG,
        Some(elapsed) if elapsed < FRAME_MS * 2 => MASCOT_GO_PNG,
        _ => MASCOT_PNG,
    }
}

/// Change paint metadata after stream composition so settled content/cache keys
/// never depend on animation time. Placement stays identical for every frame.
pub(crate) fn animate(
    frame: &mut Frame,
    started: Option<crate::moment::Timestamp>,
    now: crate::moment::Timestamp,
) {
    let png = mascot_png(started, now);
    for part in &mut frame.parts {
        for line in &mut part.lines {
            if let Some(image) = line.image.as_mut() {
                if image.png == MASCOT_PNG {
                    image.png = png;
                }
            }
        }
    }
}

pub(crate) fn mascot_at(frame: &Frame, graphics: Graphics, x: u16, y: u16) -> bool {
    supported(graphics)
        && visible(frame).is_some_and(|image| {
            image.rect.contains(x, y)
                && [MASCOT_PNG, MASCOT_WAIT_PNG, MASCOT_GO_PNG].contains(&image.png)
        })
}

// A single decorative image slot, owned by this application, not a global delete.
const IMAGE_ID: u32 = 0x41544f4d;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Image {
    pub rect: Rect,
    pub png: &'static [u8],
}

pub fn supported(protocol: Graphics) -> bool {
    matches!(protocol, Graphics::ITerm2 | Graphics::Kitty)
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// Only complete, unobscured placements can be emitted. In particular a menu
/// must never end up behind a terminal image drawn after the text pass.
pub(crate) fn visible(frame: &Frame) -> Option<Image> {
    for (index, part) in frame.parts.iter().enumerate() {
        for (row, line) in part.lines.iter().take(part.rect.h as usize).enumerate() {
            let Some(mut image) = line.image else {
                continue;
            };
            let local = image.rect;
            if local.is_empty()
                || u32::from(local.x) + u32::from(local.w) > u32::from(part.rect.w)
                || row + local.y as usize + local.h as usize > part.rect.h as usize
            {
                continue;
            }
            image.rect.x = part.rect.x.saturating_add(local.x);
            image.rect.y = part
                .rect
                .y
                .saturating_add(row as u16)
                .saturating_add(local.y);
            // Leave a spare bottom row: inline protocols may advance the
            // cursor after drawing. Never let that scroll the terminal.
            if u32::from(image.rect.x) + u32::from(image.rect.w) > u32::from(frame.size.w)
                || u32::from(image.rect.y) + u32::from(image.rect.h) >= u32::from(frame.size.h)
                || frame.parts[index + 1..]
                    .iter()
                    .any(|part| overlaps(image.rect, part.rect))
            {
                continue;
            }
            return Some(image);
        }
    }
    None
}

pub(crate) fn delete(protocol: Graphics) -> String {
    if protocol == Graphics::Kitty {
        format!("\x1b_Ga=d,d=I,i={IMAGE_ID},q=2;\x1b\\")
    } else {
        String::new()
    }
}

pub(crate) fn draw(image: Image, protocol: Graphics) -> String {
    let r = image.rect;
    let data = STANDARD.encode(image.png);
    let mut out = format!("\x1b7\x1b[{};{}H", r.y + 1, r.x + 1);
    match protocol {
        Graphics::ITerm2 => {
            out.push_str(&format!(
                "\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=1:{}\x07",
                image.png.len(),
                r.w,
                r.h,
                data
            ));
        }
        Graphics::Kitty => {
            // Kitty limits each base64 chunk to 4096 bytes. Suppress replies
            // and cursor movement; no protocol response can enter the editor.
            let chunks = data.as_bytes().chunks(4096);
            let count = chunks.len();
            for (index, chunk) in chunks.enumerate() {
                let more = usize::from(index + 1 < count);
                if index == 0 {
                    out.push_str(&format!(
                        "\x1b_Ga=T,f=100,t=d,i={IMAGE_ID},c={},r={},C=1,q=2,m={more};",
                        r.w, r.h
                    ));
                } else {
                    out.push_str(&format!("\x1b_Gq=2,m={more};"));
                }
                out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
                out.push_str("\x1b\\");
            }
        }
        _ => return String::new(),
    }
    out.push_str("\x1b8");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ansi::Lines, caps::Caps, frame::Line};

    fn frame() -> Frame {
        let mut frame = Frame::new(80, 24);
        let mut rows = vec![Line::raw(" ".repeat(20)); 8];
        rows[0].image = Some(Image {
            rect: Rect::new(2, 0, 18, 8),
            png: MASCOT_PNG,
        });
        frame.place("welcome", Rect::new(0, 2, 80, 8), rows);
        frame.place("status", Rect::new(0, 23, 80, 1), vec![Line::raw("idle")]);
        frame
    }

    #[test]
    fn a_click_sequence_has_fixed_geometry_and_a_finite_deadline() {
        use crate::moment::Timestamp;
        let started = Some(Timestamp::millis(1000));
        let rect = visible(&frame()).unwrap().rect;
        for (at, expected, next_ms) in [
            (1000, MASCOT_WAIT_PNG, Some(1500)),
            (2499, MASCOT_WAIT_PNG, Some(1)),
            (2500, MASCOT_GO_PNG, Some(1500)),
            (3999, MASCOT_GO_PNG, Some(1)),
            (4000, MASCOT_PNG, None),
            (5000, MASCOT_PNG, None),
        ] {
            let now = Timestamp::millis(at);
            let mut scene = frame();
            animate(&mut scene, started, now);
            let image = visible(&scene).unwrap();
            assert_eq!(image.png, expected);
            assert_eq!(image.rect, rect, "animation must not change layout");
            assert_eq!(next_change(started, now).map(|d| d.as_millis()), next_ms);
            assert!(mascot_at(&scene, Graphics::ITerm2, rect.x, rect.y));
            assert!(!mascot_at(&scene, Graphics::None, rect.x, rect.y));
        }
        assert!(next_change(None, Timestamp::millis(1000)).is_none());
        for png in [MASCOT_WAIT_PNG, MASCOT_GO_PNG] {
            let reader = png::Decoder::new(std::io::Cursor::new(png))
                .read_info()
                .unwrap();
            assert_eq!((reader.info().width, reader.info().height), (512, 512));
            assert_eq!(reader.info().color_type, png::ColorType::Rgba);
        }
        let mut covered = frame();
        covered.place("menu", rect, vec![Line::raw("menu")]);
        assert!(!mascot_at(&covered, Graphics::ITerm2, rect.x, rect.y));
    }

    #[test]
    fn the_embedded_asset_is_a_transparent_png() {
        let decoder = png::Decoder::new(std::io::Cursor::new(MASCOT_PNG));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (256, 214));
        assert_eq!(reader.info().color_type, png::ColorType::Rgba);
    }

    #[test]
    fn protocol_payloads_are_the_original_png_and_preserve_the_cursor() {
        let image = visible(&frame()).unwrap();
        let iterm = draw(image, Graphics::ITerm2);
        assert!(iterm.contains(";width=18;height=8;preserveAspectRatio=1:"));
        let encoded = iterm
            .split_once(':')
            .unwrap()
            .1
            .split('\x07')
            .next()
            .unwrap();
        assert_eq!(STANDARD.decode(encoded).unwrap(), MASCOT_PNG);
        let kitty = draw(image, Graphics::Kitty);
        assert!(kitty.contains("a=T,f=100,t=d,"));
        let payloads: Vec<_> = kitty
            .split("\x1b_G")
            .skip(1)
            .map(|chunk| {
                chunk
                    .split_once(';')
                    .unwrap()
                    .1
                    .split("\x1b\\")
                    .next()
                    .unwrap()
            })
            .collect();
        assert!(payloads.iter().all(|payload| payload.len() <= 4096));
        assert_eq!(STANDARD.decode(payloads.concat()).unwrap(), MASCOT_PNG);
        for bytes in [iterm, kitty] {
            assert!(bytes.starts_with("\x1b7\x1b[3;3H"));
            assert!(bytes.ends_with("\x1b8"));
        }
        assert!(draw(image, Graphics::None).is_empty());
    }

    #[test]
    fn clipping_and_overlays_suppress_the_image() {
        let mut frame = frame();
        assert!(visible(&frame).is_some());
        frame.parts[0].rect.h = 7;
        assert!(visible(&frame).is_none());
        frame.parts[0].rect.h = 8;
        frame.place("menu", Rect::new(3, 4, 8, 2), vec![Line::raw("menu")]);
        assert!(visible(&frame).is_none());
    }

    #[test]
    fn moved_and_overlaid_images_are_cleaned_up_and_restored() {
        for graphics in [Graphics::ITerm2, Graphics::Kitty] {
            let caps = Caps {
                graphics,
                ..Caps::default()
            };
            let mut frame = frame();
            let old = Lines::of(&frame, caps, None);
            frame.parts[0].rect.y += 1;
            let moved = Lines::of(&frame, caps, Some(&old));
            let patch = moved.patch_from(Some(&old));
            assert!(patch.contains("\x1b[3;1H"), "erase old image top");
            assert!(patch.contains("\x1b7\x1b[4;3H"), "draw at the new position");
            frame.place("menu", Rect::new(3, 4, 8, 2), vec![Line::raw("menu")]);
            let covered = Lines::of(&frame, caps, Some(&moved));
            let patch = covered.patch_from(Some(&moved));
            assert!(!patch.contains("File=inline=1") && !patch.contains("a=T,f=100"));
            assert!(patch.contains("menu"));
            frame.parts.pop();
            let restored = Lines::of(&frame, caps, Some(&covered));
            assert!(restored
                .patch_from(Some(&covered))
                .contains("\x1b7\x1b[4;3H"));
            frame.size.h = 11;
            assert!(
                visible(&frame).is_none(),
                "bottom edge must not cause scrolling"
            );
        }
    }

    #[test]
    fn repaint_tracks_graphics_without_retransmitting_on_spinner_updates() {
        for graphics in [Graphics::ITerm2, Graphics::Kitty] {
            let caps = Caps {
                graphics,
                ..Caps::default()
            };
            let mut frame = frame();
            let first = Lines::of(&frame, caps, None);
            let protocol = if graphics == Graphics::ITerm2 {
                "1337;File"
            } else {
                "a=T,f=100"
            };
            assert!(first.full().contains(protocol));
            let same = Lines::of(&frame, caps, Some(&first));
            assert!(same.patch_from(Some(&first)).is_empty());
            frame.parts[1].lines[0] = Line::raw("busy");
            let spinner = Lines::of(&frame, caps, Some(&same));
            assert!(!spinner.patch_from(Some(&same)).contains(protocol));
            frame.parts[0].lines[0].background = Some(crate::frame::Color::Ground);
            let repainted = Lines::of(&frame, caps, Some(&spinner));
            assert!(repainted.patch_from(Some(&spinner)).contains(protocol));
            frame.parts[0].lines[0].image = None;
            let removed = Lines::of(&frame, caps, Some(&repainted));
            let erase = removed.patch_from(Some(&repainted));
            assert!(erase.contains("\x1b[10;1H"), "erase the image's bottom row");
            assert!(!erase.contains(protocol));
            if graphics == Graphics::Kitty {
                assert!(erase.contains("a=d,d=I"));
            }
        }
        assert!(!crate::ansi::encode(&frame()).contains("1337;File"));
        assert!(!frame().rows().join("\n").contains('\x1b'));
    }
}
