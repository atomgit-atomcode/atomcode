//! Keyword ranking, shared by every store that wants to be searched.
//!
//! Pure text in, ranking out — no record type, no clock, no filesystem, and no
//! dependency beyond `std`. That is deliberate: the two session stores in this
//! workspace hold different record shapes (L1's `TurnRecord`, the harness's
//! `LoggedEvent` stream) and there is no honest way to make one of them adopt
//! the other's. What they *can* share is the part that actually encodes the
//! search behaviour — CJK bigram expansion, connector stripping, coverage-first
//! scoring — because a second copy of that is a second set of results for the
//! same query, and the copy is what drifts.
//!
//! Callers keep their own recency tiebreak: only they know what a timestamp is.

use std::cmp::Ordering;

/// How well one document matched: how many DISTINCT query terms hit, how many
/// times in total, and how long the text was (the density tiebreak).
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Score {
    pub matched_terms: usize,
    pub occurrences: usize,
    pub hay_len: usize,
}

impl Score {
    /// Whether this document matched at all. Coverage, not occurrences: a
    /// document that repeats one term is not a better answer than one that
    /// covers two.
    pub fn hit(&self) -> bool {
        self.matched_terms > 0
    }

    pub fn density(&self) -> f64 {
        self.occurrences as f64 / self.hay_len.max(1) as f64
    }
}

/// Best first: coverage desc → occurrences desc → density desc.
///
/// Recency is deliberately absent — the caller appends it, because only the
/// caller knows which of its fields is a time.
pub fn best_first(a: &Score, b: &Score) -> Ordering {
    b.matched_terms
        .cmp(&a.matched_terms)
        .then(b.occurrences.cmp(&a.occurrences))
        .then(
            b.density()
                .partial_cmp(&a.density())
                .unwrap_or(Ordering::Equal),
        )
}

/// Score already-lowercased text against already-tokenized terms.
///
/// Both preconditions are the caller's because both are cheap to get wrong
/// twice: a caller that assembles a haystack from five fields should lowercase
/// once, not five times, and terms are tokenized once per query, not per
/// document.
pub fn score(hay_lowercased: &str, terms: &[String]) -> Score {
    let mut matched_terms = 0;
    let mut occurrences = 0;
    for term in terms {
        let n = hay_lowercased.matches(term.as_str()).count();
        if n > 0 {
            matched_terms += 1;
        }
        occurrences += n;
    }
    Score {
        matched_terms,
        occurrences,
        hay_len: hay_lowercased.chars().count(),
    }
}

/// CJK-run expansion: 1 char → the char itself; n≥2 → all consecutive char bigrams.
fn expand_cjk_run(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    match chars.len() {
        0 => Vec::new(),
        1 => vec![chars[0].to_string()],
        n => (0..n - 1)
            .map(|i| format!("{}{}", chars[i], chars[i + 1]))
            .collect(),
    }
}

fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0x20000..=0x2EBEF).contains(&cp)
        || (0x2F800..=0x2FA1F).contains(&cp)
        || (0x3040..=0x30FF).contains(&cp)
        || (0x31F0..=0x31FF).contains(&cp)
        || (0x1100..=0x11FF).contains(&cp)
        || (0xAC00..=0xD7AF).contains(&cp)
}

/// Minimal connector 字 (char, not word) — stripable only at run edges.
fn is_connector_char(c: char) -> bool {
    matches!(c, '的' | '了' | '与' | '和' | '及' | '或')
}

/// 2-char connector words, stripped only at run edges.
const CJK_CONNECTOR_WORDS: &[&str] = &["关于", "以及"];

/// Edge-only connector strip + guard. Returns the core (or the original run when
/// stripping would leave <2 chars); `None` when the whole run is connectors.
fn strip_edge_connectors(run: &str) -> Option<String> {
    let original: Vec<char> = run.chars().collect();
    if original.len() == 1 {
        return if is_connector_char(original[0]) {
            None
        } else {
            Some(original[0].to_string())
        };
    }
    let mut cur = run.to_string();
    loop {
        let before = cur.clone();
        for &w in CJK_CONNECTOR_WORDS {
            if let Some(rest) = cur.strip_prefix(w) {
                cur = rest.to_string();
                break;
            }
        }
        for &w in CJK_CONNECTOR_WORDS {
            if let Some(rest) = cur.strip_suffix(w) {
                cur = rest.to_string();
                break;
            }
        }
        if let Some(first) = cur.chars().next() {
            if is_connector_char(first) {
                if let Some(rest) = cur.strip_prefix(first) {
                    cur = rest.to_string();
                }
            }
        }
        if let Some(last) = cur.chars().next_back() {
            if is_connector_char(last) {
                if let Some(rest) = cur.strip_suffix(last) {
                    cur = rest.to_string();
                }
            }
        }
        if cur == before {
            break;
        }
    }
    match cur.chars().count() {
        0 => None,
        1 => Some(original.iter().collect()), // guard: don't shave a ≥2 run to 1 char
        _ => Some(cur),
    }
}

/// One whitespace-token containing non-ASCII chars: drop punctuation → split into
/// CJK/literal runs → edge-strip connectors on CJK runs → bigram-expand CJK runs,
/// keep literal runs whole. Pure-ASCII tokens never enter here.
fn tokenize_cjk_token(token: &str) -> Vec<String> {
    let cleaned: String = token.chars().filter(|c| c.is_alphanumeric()).collect();
    let chars: Vec<char> = cleaned.chars().collect();
    let mut terms = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let cjk_run = is_cjk(chars[i]);
        let mut j = i;
        while j < chars.len() && is_cjk(chars[j]) == cjk_run {
            j += 1;
        }
        let run: String = chars[i..j].iter().collect();
        if cjk_run {
            if let Some(core) = strip_edge_connectors(&run) {
                terms.extend(expand_cjk_run(&core));
            }
        } else {
            terms.push(run); // literal run kept whole (already lowercased)
        }
        i = j;
    }
    terms
}

/// Lowercase → split_whitespace → pure-ASCII tokens verbatim / CJK tokens via
/// [`tokenize_cjk_token`] → dedup, first-occurrence order.
pub fn tokenize(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let mut terms: Vec<String> = Vec::new();
    for raw in lower.split_whitespace() {
        let candidates: Vec<String> = if raw.is_ascii() {
            vec![raw.to_string()]
        } else {
            tokenize_cjk_token(raw)
        };
        for t in candidates {
            if !t.is_empty() && !terms.contains(&t) {
                terms.push(t);
            }
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_query_preserves_ascii_behavior() {
        assert_eq!(
            tokenize("OAuth Refresh  TOKEN"),
            vec!["oauth", "refresh", "token"]
        );
        assert_eq!(tokenize("oauth.refresh"), vec!["oauth.refresh"]);
        assert!(tokenize("").is_empty());
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn tokenize_query_expands_cjk_to_bigrams() {
        assert_eq!(tokenize("工作任务"), vec!["工作", "作任", "任务"]);
        assert_eq!(tokenize("工"), vec!["工"]);
        assert_eq!(tokenize("工作任务 工作"), vec!["工作", "作任", "任务"]);
    }

    #[test]
    fn tokenize_query_cleans_punctuation_and_connectors() {
        assert_eq!(tokenize("工作,任务"), vec!["工作", "作任", "任务"]);
        assert_eq!(tokenize("工作，任务"), vec!["工作", "作任", "任务"]);
        assert_eq!(tokenize("关于工作 任务"), vec!["工作", "任务"]);
        assert_eq!(tokenize("工作的"), vec!["工作"]);
        assert_eq!(tokenize("目的"), vec!["目的"]);
        assert!(tokenize("的 了 和").is_empty());
    }

    #[test]
    fn tokenize_query_handles_mixed_ascii_cjk() {
        assert_eq!(tokenize("OAuth的token"), vec!["oauth", "token"]);
        let kana = tokenize("日本語のセッションid");
        assert!(!kana.is_empty());
        assert!(kana.iter().any(|t| t == "id"));
    }

    #[test]
    fn coverage_beats_repetition() {
        // Two distinct terms once each must outrank one term five times: the
        // question "did this turn talk about both things" is the one a person
        // is actually asking.
        let terms = vec!["oauth".to_string(), "token".to_string()];
        let broad = score("oauth token", &terms);
        let deep = score("oauth oauth oauth oauth oauth", &terms);
        assert_eq!(
            best_first(&broad, &deep),
            Ordering::Less,
            "broad ranks first"
        );
    }

    #[test]
    fn a_miss_is_reported_as_a_miss() {
        assert!(!score("nothing here", &["oauth".to_string()]).hit());
        assert!(score("an oauth thing", &["oauth".to_string()]).hit());
    }

    #[test]
    fn density_breaks_a_tie_towards_the_shorter_text() {
        let terms = vec!["x".to_string()];
        let tight = score("x", &terms);
        let padded = score(&format!("x{}", " ".repeat(200)), &terms);
        assert_eq!(best_first(&tight, &padded), Ordering::Less);
    }
}
