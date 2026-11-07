//! Frontend-only download sampling. Callers supply monotonic elapsed time.
use atomcode_tui::update::ProgressFrame;
use std::collections::VecDeque;
use std::time::Duration;

pub const REFRESH: Duration = Duration::from_millis(150);

pub struct DownloadMeter {
    bytes: u64,
    total: u64,
    samples: VecDeque<(Duration, u64)>,
}
impl Default for DownloadMeter {
    fn default() -> Self {
        Self {
            bytes: 0,
            total: 0,
            samples: VecDeque::from([(Duration::ZERO, 0)]),
        }
    }
}
impl DownloadMeter {
    pub fn update(&mut self, bytes: u64, total: u64) {
        self.bytes = bytes;
        self.total = total;
    }
    pub fn frame(&mut self, now: Duration) -> ProgressFrame {
        if self.samples.back().is_none_or(|(at, _)| *at < now) {
            self.samples.push_back((now, self.bytes));
        }
        let cutoff = now.saturating_sub(Duration::from_secs(3));
        while self.samples.len() > 2 && self.samples[1].0 <= cutoff {
            self.samples.pop_front();
        }
        let (at, bytes) = self.samples.front().copied().unwrap_or_default();
        let seconds = now.saturating_sub(at).as_secs_f64();
        let speed = (seconds >= 0.15).then(|| self.bytes.saturating_sub(bytes) as f64 / seconds);
        let remaining_seconds = speed
            .filter(|speed| *speed > 0.0 && self.total > 0)
            .map(|speed| (self.total.saturating_sub(self.bytes) as f64 / speed).ceil() as u64);
        ProgressFrame {
            title: atomcode_tui::i18n::t(atomcode_tui::i18n::Msg::UpgradeProgressDownloading)
                .into_owned(),
            downloading: true,
            pulse: (now.as_millis() / 150) as u64,
            bytes: self.bytes,
            total: self.total,
            bytes_per_second: speed,
            remaining_seconds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measures_rate_eta_and_stalls_with_bounded_samples() {
        let mut meter = DownloadMeter::default();
        meter.update(1_048_576, 4_194_304);
        let frame = meter.frame(Duration::from_secs(1));
        assert_eq!(frame.bytes_per_second, Some(1_048_576.0));
        assert_eq!(frame.remaining_seconds, Some(3));
        for n in 2..1000 {
            meter.frame(Duration::from_secs(n));
        }
        let stalled = meter.frame(Duration::from_secs(1000));
        assert_eq!(stalled.bytes_per_second, Some(0.0));
        assert_eq!(stalled.remaining_seconds, None);
        assert!(meter.samples.len() <= 5);
    }
    #[test]
    fn unknown_totals_have_no_eta_and_progress_never_overflows() {
        let mut meter = DownloadMeter::default();
        meter.update(u64::MAX, 0);
        let frame = meter.frame(Duration::from_secs(1));
        assert_eq!(frame.remaining_seconds, None);
        assert!(frame.download_line(100).contains('?'));
        meter.update(u64::MAX, u64::MAX);
        assert!(meter
            .frame(Duration::from_secs(2))
            .download_line(100)
            .contains("100%"));
    }
}
