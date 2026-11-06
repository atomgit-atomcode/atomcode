//! Conservative exact-cycle detection. No model/provider or language heuristics.
use serde::{Deserialize, Serialize};

pub const REASONING_LOOP_CODE: &str = "reasoning_loop_detected";
pub const RETRY_NOTICE: &str = "检测到推理复读，正在重新生成（1/1）...";
pub const STOP_NOTICE: &str = "再次检测到推理复读，已停止生成。请调整提示或模型后重试。";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningLoopConfig {
    pub enabled: bool,
    pub min_period: usize,
    pub max_period: usize,
    pub min_repeats: usize,
    pub min_repeated_chars: usize,
}
impl Default for ReasoningLoopConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_period: 32,
            max_period: 1024,
            min_repeats: 4,
            min_repeated_chars: 1024,
        }
    }
}

pub struct ReasoningLoopDetector {
    config: ReasoningLoopConfig,
    tail: std::collections::VecDeque<char>,
    since_scan: usize,
}
impl ReasoningLoopDetector {
    pub fn new(mut config: ReasoningLoopConfig) -> Self {
        // Config cannot remove the safety floor or make work/memory unbounded.
        config.min_period = config.min_period.clamp(16, 1024);
        config.max_period = config.max_period.clamp(config.min_period, 1024);
        config.min_repeats = config.min_repeats.clamp(3, 16);
        config.min_repeated_chars = config.min_repeated_chars.clamp(512, 4096);
        Self {
            config,
            tail: Default::default(),
            since_scan: 0,
        }
    }
    /// Arm only for unsigned reasoning before any visible answer/tool fragment.
    /// Both engine paths use this boundary policy, including a final suffix check.
    pub fn observe(&mut self, event: &crate::stream::StreamEvent) -> bool {
        use crate::stream::StreamEvent;
        match event {
            StreamEvent::TextDelta(text) if !text.is_empty() => {
                self.config.enabled = false;
                self.tail.clear();
                false
            }
            StreamEvent::ToolCall(_)
            | StreamEvent::ToolCallDelta { .. }
            | StreamEvent::ReasoningSignature { .. } => {
                self.config.enabled = false;
                self.tail.clear();
                false
            }
            StreamEvent::Reasoning(text) => self.push(text),
            StreamEvent::Done { .. } => self.repeated(),
            _ => false,
        }
    }
    pub fn push(&mut self, delta: &str) -> bool {
        if !self.config.enabled {
            return false;
        }
        // Scan at fixed scalar boundaries even for one huge provider chunk.
        for c in delta.chars() {
            self.tail.push_back(c);
            if self.tail.len() > 4096 {
                self.tail.pop_front();
            }
            self.since_scan += 1;
            if self.since_scan == 128 {
                self.since_scan = 0;
                if self.repeated() {
                    return true;
                }
            }
        }
        false
    }
    pub fn repeated(&self) -> bool {
        if !self.config.enabled || self.tail.len() < self.config.min_repeated_chars {
            return false;
        }
        let tail: Vec<char> = self.tail.iter().rev().copied().collect();
        // Z algorithm: one linear scan of the bounded reversed suffix.
        let n = tail.len();
        let mut z = vec![0usize; n];
        let (mut left, mut right) = (0, 0);
        for i in 1..n {
            if i <= right {
                z[i] = (right - i + 1).min(z[i - left]);
            }
            while i + z[i] < n && tail[z[i]] == tail[i + z[i]] {
                z[i] += 1;
            }
            if i + z[i] > right + 1 {
                left = i;
                right = i + z[i] - 1;
            }
        }
        for period in
            self.config.min_period..=self.config.max_period.min(n / self.config.min_repeats)
        {
            let count = 1 + z[period] / period;
            if count >= self.config.min_repeats
                && count * period >= self.config.min_repeated_chars
                && tail[..period].iter().any(|c| c.is_alphabetic())
            {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_cjk_cycles_independently_of_chunk_boundaries() {
        let text = "让我重新考虑这个问题，但还是回到同一份没有任何进展的分析中。".repeat(100);
        for chunk in [1, 13, 200, 10000] {
            let chars: Vec<_> = text.chars().collect();
            let mut detector = ReasoningLoopDetector::new(Default::default());
            assert!(chars
                .chunks(chunk)
                .any(|part| detector.push(&part.iter().collect::<String>())));
            assert!(detector.tail.len() <= 4096);
        }
    }
    #[test]
    fn spares_short_reconsiderations_numbers_and_new_facts() {
        for text in [
            "让我再想想。".repeat(5),
            "0123456789 ".repeat(1000),
            (0..200)
                .map(|i| format!("检查文件 {i}，得到不同的验证结果与新的事实。\n"))
                .collect(),
        ] {
            let mut detector = ReasoningLoopDetector::new(Default::default());
            assert!(!detector.push(&text));
            assert!(!detector.repeated());
        }
    }
    #[test]
    fn disabled_guard_never_retains_content() {
        let mut detector = ReasoningLoopDetector::new(ReasoningLoopConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(!detector.push(&"重复内容".repeat(10000)));
        assert!(detector.tail.is_empty());
    }
}
