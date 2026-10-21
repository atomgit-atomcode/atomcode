//! Paint-only terminal graphics. Image bytes never travel through text spans.

use crate::{
    caps::Graphics,
    frame::{Frame, Rect},
};
use base64::{engine::general_purpose::STANDARD, Engine};

pub const MASCOT_WIDTH: u16 = 24;
pub const MASCOT_HEIGHT: u16 = 10;
pub const MASCOT_PNG: &[u8] = include_bytes!("../assets/mascot.png");
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
        let mut rows = vec![Line::raw(" ".repeat(26)); 10];
        rows[0].image = Some(Image {
            rect: Rect::new(2, 0, 24, 10),
            png: MASCOT_PNG,
        });
        frame.place("welcome", Rect::new(0, 2, 80, 10), rows);
        frame.place("status", Rect::new(0, 23, 80, 1), vec![Line::raw("idle")]);
        frame
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
        assert!(iterm.contains(";width=24;height=10;preserveAspectRatio=1:"));
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
        frame.parts[0].rect.h = 9;
        assert!(visible(&frame).is_none());
        frame.parts[0].rect.h = 10;
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
            frame.size.h = 13;
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
            assert!(erase.contains("\x1b[12;1H"), "erase the image's bottom row");
            assert!(!erase.contains(protocol));
            if graphics == Graphics::Kitty {
                assert!(erase.contains("a=d,d=I"));
            }
        }
        assert!(!crate::ansi::encode(&frame()).contains("1337;File"));
        assert!(!frame().rows().join("\n").contains('\x1b'));
    }
}
