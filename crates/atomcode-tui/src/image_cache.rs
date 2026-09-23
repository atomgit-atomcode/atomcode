//! Content-addressable disk cache for attached images.
//!
//! An image the person attached is kept here, keyed by a stable content hash, so
//! a marker recalled with arrow-up (and, in a later step, a resumed session) can
//! have its **bytes** re-attached and re-sent — rather than reaching the model as
//! a bare `[Image #N]` placeholder it cannot see. The bytes written are the
//! already-normalised ones (longest edge ≤ 1568px, ~1.5 MB budget — see
//! `image_normalize`), so nothing here re-inflates the context.
//!
//! Everything is best-effort: the in-memory `attach::Attachments` gallery is the
//! source of truth for the current session, and this is only durability, so every
//! failure is swallowed.

use atomcode_kernel::message::ImageContent;

/// `$ATOMCODE_HOME/image-cache`, or `~/.atomcode/image-cache`. `None` when there
/// is no home to write under — the caller just skips caching.
///
/// Resolved here rather than through `atomcode-config` on purpose: the screen is
/// an App apart and does not depend on that crate (see this crate's Cargo.toml).
/// It agrees with it on the one thing that matters — the `ATOMCODE_HOME` override
/// and the `.atomcode` directory name.
pub fn cache_dir() -> Option<std::path::PathBuf> {
    let home = match std::env::var_os("ATOMCODE_HOME") {
        Some(value) if !value.is_empty() => std::path::PathBuf::from(value),
        _ => crate::text::home_dir()?.join(".atomcode"),
    };
    Some(home.join("image-cache"))
}

/// A stable content hash of an image, for its cache filename and the dedup the
/// cache rests on. FNV-1a over the media type and base64 bytes — stable across
/// runs (unlike `DefaultHasher`), which a filename needs.
pub fn hash(img: &ImageContent) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [img.media_type.as_bytes(), img.data.as_bytes()] {
        for b in part {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The extension for a media type, so a human poking at the cache sees real image
/// files. Unknown types fall back to `bin`.
fn ext_for(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// The cache path for an image, given its content hash and media type.
fn path_for(dir: &std::path::Path, hash: u64, media_type: &str) -> std::path::PathBuf {
    dir.join(format!("{hash:016x}.{}", ext_for(media_type)))
}

/// Best-effort write of an image's (already-normalised) bytes, keyed by content
/// hash. Idempotent — a content-addressed file that exists is left alone. Every
/// failure is swallowed.
pub fn write(img: &ImageContent) {
    use base64::Engine as _;
    let Some(dir) = cache_dir() else { return };
    let path = path_for(&dir, hash(img), &img.media_type);
    if path.exists() {
        return;
    }
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(img.data.as_bytes()) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&path, &raw);
    }
}

/// Read an image's bytes back from the cache by content hash, for re-attaching a
/// recalled or resumed image. `None` when the file is gone.
pub fn read(hash: u64, media_type: &str) -> Option<Vec<u8>> {
    let dir = cache_dir()?;
    std::fs::read(path_for(&dir, hash, media_type)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(tag: &str) -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: tag.into(),
        }
    }

    #[test]
    fn hash_is_stable_and_content_addressed() {
        // Same content → same hash (dedup); different content → different.
        assert_eq!(hash(&img("abc")), hash(&img("abc")));
        assert_ne!(hash(&img("abc")), hash(&img("abd")));
        // The media type is part of the identity.
        let mut jpeg = img("abc");
        jpeg.media_type = "image/jpeg".into();
        assert_ne!(hash(&img("abc")), hash(&jpeg));
    }

    #[test]
    fn ext_follows_the_media_type() {
        assert_eq!(ext_for("image/png"), "png");
        assert_eq!(ext_for("image/jpeg"), "jpg");
        assert_eq!(ext_for("image/webp"), "webp");
        assert_eq!(ext_for("application/octet-stream"), "bin");
    }

    #[test]
    fn write_then_read_round_trips_the_real_bytes() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        // Point the cache at a scratch home for this test only.
        std::env::set_var("ATOMCODE_HOME", dir.path());
        let raw = b"\x89PNG\r\n\x1a\nsome-bytes";
        let content = ImageContent {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(raw),
        };
        write(&content);
        let got = read(hash(&content), &content.media_type).expect("the bytes were cached");
        assert_eq!(got, raw, "the raw (decoded) bytes round-trip");
        assert_eq!(
            read(0xdead_beef, "image/png"),
            None,
            "an unknown hash is a miss, not a panic"
        );
        std::env::remove_var("ATOMCODE_HOME");
    }
}
