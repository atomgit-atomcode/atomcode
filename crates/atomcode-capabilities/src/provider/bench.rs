//! Benchmark-only entry points. Compiled only with the `bench` feature.
//!
//! `benches/core.rs` is a separate crate, so it can use public tools and
//! session APIs directly but cannot name private provider decoders. These
//! wrappers expose the measurements without widening the public API.

/// Decode `lines` synthetic chat-completion SSE events, fed in chunks of
/// `per_chunk` lines. Returns the number of decoded events.
pub fn sse_feed(lines: usize, per_chunk: usize) -> usize {
    super::openai_compat::bench_sse_feed(lines, per_chunk)
}
