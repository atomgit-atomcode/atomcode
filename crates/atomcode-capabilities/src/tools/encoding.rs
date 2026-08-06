//! Shared text-encoding helpers for the file tools.
//!
//! Files are UTF-8 by default, but Chinese Windows editors routinely write
//! `.txt`/`.md`/source files as GBK (a subset of GB18030). `read_file` already
//! decodes those for display; `edit_file` reuses the same detection here so it can
//! edit them in place and write them back in their ORIGINAL encoding rather than
//! silently converting to UTF-8.
//!
//! Safety: a legacy encoding is only ever claimed when a full decode→re-encode
//! reproduces the file's exact bytes ([`decode_for_edit`]'s round-trip guard). That
//! makes an in-place edit lossless for the untouched content and refuses ambiguous
//! files (Latin-1, Big5, truncated UTF-8, binary) instead of corrupting them.

/// Text-ish extensions worth trying a GBK/GB18030 decode for when UTF-8 fails. A
/// binary file with one of these would already have tripped the read tool's binary
/// sniff, so this gate just avoids feeding genuine binary blobs to the decoder.
pub(crate) const GBK_CANDIDATE_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "sql", "ini", "conf", "cfg", "toml", "yaml",
    "yml", "html", "htm", "xml", "json", "js", "ts", "css", "py", "rb", "go", "rs", "c", "h",
    "cpp", "hpp", "java", "kt", "sh", "bat", "ps1",
];

pub(crate) fn has_text_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            GBK_CANDIDATE_EXTENSIONS.iter().any(|t| *t == e)
        })
        .unwrap_or(false)
}

/// Attempt to decode a file that failed UTF-8 validation, for DISPLAY (read_file).
/// Tries GB18030 (superset of GBK/GB2312) only, and only for text-ish extensions —
/// that's ~100% of the real-world miss on Chinese Windows `.txt`. Returns `None` for
/// everything else so the caller emits a recovery hint instead of mojibake.
pub(crate) fn decode_non_utf8_text(path: &std::path::Path, bytes: &[u8]) -> Option<String> {
    if !has_text_extension(path) {
        return None;
    }
    let (decoded, _, had_errors) = encoding_rs::GB18030.decode(bytes);
    if had_errors {
        return None;
    }
    Some(decoded.into_owned())
}

/// The on-disk encoding an [`edit_file`] operation must preserve when writing back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileEncoding {
    Utf8,
    Gb18030,
}

/// A file decoded to UTF-8 text for editing, tagged with its original on-disk encoding.
pub(crate) struct DecodedFile {
    pub text: String,
    pub encoding: FileEncoding,
}

/// Decode a file for EDITING: return its text as UTF-8 plus the encoding to write back.
///
/// - Valid UTF-8 → [`FileEncoding::Utf8`] (unchanged from the historical path).
/// - Otherwise, a text-ish extension that decodes as GB18030 AND round-trips
///   (`encode(decode(bytes)) == bytes`) → [`FileEncoding::Gb18030`]. The round-trip
///   guard proves the decode is lossless for THIS file, so re-encoding untouched
///   content reproduces its exact bytes.
/// - Anything else → `None`: the caller refuses the edit rather than risk corruption.
pub(crate) fn decode_for_edit(path: &std::path::Path, bytes: &[u8]) -> Option<DecodedFile> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(DecodedFile {
            text: text.to_string(),
            encoding: FileEncoding::Utf8,
        }),
        Err(_) => {
            let text = decode_non_utf8_text(path, bytes)?;
            // Round-trip guard: only treat this as GB18030 if re-encoding the decoded
            // text reproduces the original bytes exactly. Rejects mis-detected Latin-1 /
            // Big5 / truncated-UTF-8 files that happen to decode without error.
            let reencoded = encode(&text, FileEncoding::Gb18030)?;
            if reencoded == bytes {
                Some(DecodedFile {
                    text,
                    encoding: FileEncoding::Gb18030,
                })
            } else {
                None
            }
        }
    }
}

/// Encode edited UTF-8 text back to the file's original encoding. Returns `None` if the
/// text cannot be losslessly represented (so the caller refuses to write rather than
/// emit replacement bytes). GB18030 covers all of Unicode, so this effectively only
/// guards against a pathological encoder error.
pub(crate) fn encode(text: &str, encoding: FileEncoding) -> Option<Vec<u8>> {
    match encoding {
        FileEncoding::Utf8 => Some(text.as_bytes().to_vec()),
        FileEncoding::Gb18030 => {
            let (bytes, _, had_errors) = encoding_rs::GB18030.encode(text);
            if had_errors {
                None
            } else {
                Some(bytes.into_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn utf8_file_decodes_as_utf8_and_round_trips() {
        let bytes = "hello 世界\n".as_bytes();
        let d = decode_for_edit(Path::new("a.txt"), bytes).unwrap();
        assert_eq!(d.encoding, FileEncoding::Utf8);
        assert_eq!(d.text, "hello 世界\n");
        assert_eq!(encode(&d.text, d.encoding).unwrap(), bytes);
    }

    #[test]
    fn gbk_text_file_decodes_as_gb18030_and_round_trips() {
        let (gbk, _, err) = encoding_rs::GB18030.encode("第一行\n第二行\n");
        assert!(!err);
        let d = decode_for_edit(Path::new("notes.txt"), &gbk).unwrap();
        assert_eq!(d.encoding, FileEncoding::Gb18030);
        assert_eq!(d.text, "第一行\n第二行\n");
        // Re-encoding the (unmodified) decoded text reproduces the exact bytes.
        assert_eq!(encode(&d.text, d.encoding).unwrap(), gbk.to_vec());
    }

    #[test]
    fn non_utf8_without_text_extension_is_refused() {
        let (gbk, _, _) = encoding_rs::GB18030.encode("第一行\n");
        // `.bin` is not a text extension → not decoded → caller treats as unsupported.
        assert!(decode_for_edit(Path::new("blob.bin"), &gbk).is_none());
    }

    #[test]
    fn non_round_tripping_bytes_are_refused() {
        // A stray 0x80 is invalid UTF-8 and not a valid GB18030 lead byte, so the
        // round-trip guard rejects it rather than corrupt an ambiguous file.
        let mut bytes = b"plain text\n".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(b"\n");
        assert!(std::str::from_utf8(&bytes).is_err());
        assert!(decode_for_edit(Path::new("weird.txt"), &bytes).is_none());
    }
}
