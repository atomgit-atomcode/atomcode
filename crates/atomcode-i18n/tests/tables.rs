//! What the two tables must hold, checked against the tables themselves.
//!
//! The `match` in each language file is exhaustive, so a *missing* translation
//! is already a compile error. These are the two failures the compiler cannot
//! see: an English arm that is still Chinese (a variant added by copying the
//! line above it), and one sentence written into both tables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// `(variant, rendered literal)` for every arm of one language file.
fn arms(path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(path).expect("the table is there");
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Msg::") else {
            continue;
        };
        let variant = rest
            .split("=>")
            .next()
            .unwrap_or_default()
            .trim()
            .trim_end_matches('{')
            .trim()
            .to_string();
        // The first literal on the arm is the sentence; a `format!` puts it first.
        if let Some(said) = first_literal(line) {
            out.push((variant, said));
        }
    }
    assert!(
        out.len() > 100,
        "{path:?} looks unparsed: only {} arms",
        out.len()
    );
    out
}

fn first_literal(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut out = String::new();
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                if i < chars.len() {
                    out.push(chars[i]);
                }
                i += 1;
            }
            return Some(out);
        }
        i += 1;
    }
    None
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// An English arm that still reads in Chinese.
///
/// The way this happens is copying the line above and forgetting the second
/// half of the edit — which compiles, passes every test that does not read that
/// one row, and only shows up as a Chinese word on an English screen.
#[test]
fn no_english_arm_is_left_in_chinese() {
    // Names, not sentences: a person looking for the Chinese option is looking
    // for the word 中文, whatever language the rest of the screen is in.
    const NAMED_IN_THEIR_OWN_LANGUAGE: &[&str] = &[
        "OnboardLanguageChinese",
        // The other front end's language picker, which spells each option in
        // its own language and glosses it: `简体中文 (Simplified Chinese)`.
        "OnboardingLanguageOptionZhCn",
    ];
    let mut left = Vec::new();
    for table in ["product", "screen"] {
        for (variant, said) in arms(&src().join(table).join("en.rs")) {
            if has_cjk(&said) && !NAMED_IN_THEIR_OWN_LANGUAGE.contains(&variant.as_str()) {
                left.push(format!("{table}/en.rs  Msg::{variant} => {said:?}"));
            }
        }
    }
    assert!(
        left.is_empty(),
        "these English arms are still Chinese:\n  {}",
        left.join("\n  ")
    );
}

/// One sentence, one place.
///
/// The screen may read the product's table (`screen::product`), and where the
/// product already has a line that is what it must do. A second copy is how the
/// two front ends come to say one thing two ways — which is the whole reason
/// the tables live in one crate. `gates/tui-i18n.sh` runs the same check with a
/// ratchet; this one runs with the tests.
#[test]
fn the_two_tables_say_nothing_twice() {
    let said = |table: &str| -> Vec<((String, String), String)> {
        let en: HashMap<String, String> =
            arms(&src().join(table).join("en.rs")).into_iter().collect();
        arms(&src().join(table).join("zh_cn.rs"))
            .into_iter()
            // One character is a joiner or a mark, not a sentence.
            .filter(|(_, text)| has_cjk(text) && text.chars().count() > 1)
            .filter_map(|(variant, text)| {
                en.get(&variant)
                    .map(|e| ((text, e.to_lowercase()), variant.clone()))
            })
            .collect()
    };
    let product: HashMap<_, _> = said("product").into_iter().collect();
    let twice: Vec<String> = said("screen")
        .into_iter()
        .filter_map(|(key, variant)| {
            product
                .get(&key)
                .map(|theirs| format!("{:?}: product::{theirs} / screen::{variant}", key.0))
        })
        .collect();
    assert!(
        twice.is_empty(),
        "these sentences are written into both tables — the screen should read \
         the product's entry instead:\n  {}",
        twice.join("\n  ")
    );
}

/// Switching the locale switches both tables at once.
///
/// The property the one-crate shape exists for: before it, the screen had a
/// table the product's `/language` could not reach, so a switch moved the
/// welcome block and left the status bar behind.
#[test]
fn one_switch_moves_both_tables() {
    let _guard = atomcode_i18n::test_lock();

    atomcode_i18n::set_locale(atomcode_i18n::Locale::ZhCn);
    let zh = (
        atomcode_i18n::product::t(atomcode_i18n::product::Msg::ApprovalDeny).into_owned(),
        atomcode_i18n::screen::t(atomcode_i18n::screen::Msg::StatusStopping).into_owned(),
    );

    atomcode_i18n::set_locale(atomcode_i18n::Locale::En);
    let en = (
        atomcode_i18n::product::t(atomcode_i18n::product::Msg::ApprovalDeny).into_owned(),
        atomcode_i18n::screen::t(atomcode_i18n::screen::Msg::StatusStopping).into_owned(),
    );

    assert_ne!(zh.0, en.0, "the product's table did not move");
    assert_ne!(zh.1, en.1, "the screen's table did not move");
    assert!(has_cjk(&zh.1) && !has_cjk(&en.1), "{zh:?} → {en:?}");
}
