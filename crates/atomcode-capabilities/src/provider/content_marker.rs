//! Observations at the original wire field, never a text-to-tool parser.
//!
//! Only a prefix length and a flag survive a chunk: no argument or answer text
//! is retained or logged. The token below appeared in a real gateway response.
const MARKER: &[u8] = b"]<]minimax[>[";
/// Whether text contains a known reserved native chat-template token.
/// This is format evidence, not permission to parse or execute text as tools.
/// Callers decide whether such text is usable in their own domain (a generated
/// session title, for example). Ordinary XML/tag examples are not matched.
pub fn contains_native_chat_template_token(text: &str) -> bool {
    text.as_bytes()
        .windows(MARKER.len())
        .any(|part| part == MARKER)
}

// Prefix table keeps overlapping matches linear without retaining body bytes.
const PREFIX: [usize; MARKER.len()] = {
    let mut table = [0; MARKER.len()];
    let mut i = 1;
    let mut matched = 0;
    while i < MARKER.len() {
        while matched > 0 && MARKER[i] != MARKER[matched] {
            matched = table[matched - 1];
        }
        if MARKER[i] == MARKER[matched] {
            matched += 1;
        }
        table[i] = matched;
        i += 1;
    }
    table
};

#[derive(Default)]
pub(super) struct ContentMarker {
    matched: usize,
    pub(super) seen: bool,
}
impl ContentMarker {
    pub fn observe(&mut self, content: &str) {
        if self.seen {
            return;
        }
        for byte in content.bytes() {
            while self.matched > 0 && byte != MARKER[self.matched] {
                self.matched = PREFIX[self.matched - 1];
            }
            if byte == MARKER[self.matched] {
                self.matched += 1;
            }
            if self.matched == MARKER.len() {
                self.seen = true;
                tracing::warn!(
                    error_origin = "native_tool_marker_in_content",
                    wire_field = "choices[0].delta.content",
                    marker_kind = "minimax_chat_template",
                    "provider content marker observation"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_split_points_and_overlapping_prefixes_are_observed_once() {
        let token = std::str::from_utf8(MARKER).unwrap();
        assert!(contains_native_chat_template_token(&format!(
            "正文 {token} details"
        )));
        assert!(!contains_native_chat_template_token(
            "<tool_call> literal XML"
        ));
        for split in 0..=token.len() {
            let mut marker = ContentMarker::default();
            marker.observe("正文 ]<]<]");
            marker.observe(&token[..split]);
            marker.observe(&token[split..]);
            assert!(marker.seen, "split {split}");
            marker.observe(token);
            assert!(marker.seen);
        }
        let mut next = ContentMarker::default();
        next.observe("<tool_call> is ordinary documentation, not a reserved native token.");
        next.observe("]<]mini");
        assert!(!next.seen, "new requests do not inherit a partial token");
    }
    #[test]
    fn the_once_per_stream_diagnostic_never_logs_body_or_arguments() {
        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                struct Visitor(String);
                impl tracing::field::Visit for Visitor {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        use std::fmt::Write;
                        let _ = write!(self.0, "{}={:?} ", field.name(), value);
                    }
                }
                let mut visitor = Visitor(String::new());
                event.record(&mut visitor);
                self.0.lock().unwrap().push(visitor.0);
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            assert!(contains_native_chat_template_token(
                "PRIVATE_BODY ]<]minimax[>["
            ));
            let mut marker = ContentMarker::default();
            marker.observe("PRIVATE_BODY ]<]mini");
            marker.observe("max[>[<tool_call>PRIVATE_ARGUMENTS");
            marker.observe("]<]minimax[>[PRIVATE_SECOND_MATCH");
            let mut next = ContentMarker::default();
            next.observe("]<]minimax[>[");
        });
        let logs = capture.0.lock().unwrap();
        assert_eq!(logs.len(), 2, "one observation per response");
        for line in logs.iter() {
            for expected in [
                "native_tool_marker_in_content",
                "choices[0].delta.content",
                "minimax_chat_template",
            ] {
                assert!(line.contains(expected), "{line}");
            }
            assert!(!line.contains("PRIVATE_"), "{line}");
        }
    }
}
