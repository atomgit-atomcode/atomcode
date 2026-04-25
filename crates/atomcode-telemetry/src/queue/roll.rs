//! Segment rolling decisions. Pure logic (no I/O) — easy to unit-test.
//!
//! Roll when current segment reaches **500 events OR 512 KB**.

pub const MAX_EVENTS_PER_SEGMENT: u32 = 500;
pub const MAX_BYTES_PER_SEGMENT: u64 = 512 * 1024;

pub const MAX_SEGMENT_FILES: usize = 50;
pub const MAX_TOTAL_BYTES: u64 = 5 * 1024 * 1024;

pub fn should_roll(events_in_segment: u32, bytes_in_segment: u64) -> bool {
    events_in_segment >= MAX_EVENTS_PER_SEGMENT || bytes_in_segment >= MAX_BYTES_PER_SEGMENT
}

pub fn over_cap(segment_count: usize, total_bytes: u64) -> bool {
    segment_count > MAX_SEGMENT_FILES || total_bytes > MAX_TOTAL_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rolls_on_count() {
        assert!(should_roll(500, 0));
        assert!(!should_roll(499, 0));
    }
    #[test]
    fn rolls_on_bytes() {
        assert!(should_roll(0, 512 * 1024));
        assert!(!should_roll(0, 512 * 1024 - 1));
    }
    #[test]
    fn cap_detection() {
        assert!(over_cap(51, 0));
        assert!(over_cap(1, 5 * 1024 * 1024 + 1));
        assert!(!over_cap(1, 100));
    }
}
