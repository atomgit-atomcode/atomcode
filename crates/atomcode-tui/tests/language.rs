//! The screen answers in the language that was chosen.
//!
//! The rest of this crate's tests assert in Chinese (see
//! `_tests_assert_in_chinese`), which says what the screen reads like but not
//! that it *follows* the setting. This is the other half: the same surfaces,
//! drawn twice, asserted to have changed — and asserted to be in the right
//! script, so a table that answered Chinese to both would still be caught.
//!
//! Its own test binary rather than arms in the ones above, because it moves the
//! process-wide locale: under `cargo nextest` each test is its own process, and
//! `test_lock()` restores the previous locale for anyone running it otherwise.

use atomcode_tui::command::CommandSet;
use atomcode_tui::content::WelcomeWords;
use atomcode_tui::i18n::{set_locale, test_lock, Locale};

fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// Draw `what` once in each language and hand back the two readings.
fn both<T>(what: impl Fn() -> T) -> (T, T) {
    set_locale(Locale::ZhCn);
    let zh = what();
    set_locale(Locale::En);
    let en = what();
    (zh, en)
}

/// The commands' own help, which is what `/help` prints and what the slash menu
/// shows under each name.
#[test]
fn every_command_this_screen_offers_is_described_in_both_languages() {
    let _guard = test_lock();
    let catalogue = || {
        let mut out: Vec<(String, String)> = Vec::new();
        for set in [
            Box::new(atomcode_tui::commands::ScreenCommands) as Box<dyn CommandSet>,
            Box::new(atomcode_tui::commands::SessionCommands),
            Box::new(atomcode_tui::commands::TakeAwayCommands),
            Box::new(atomcode_tui::commands::ToolCommands),
        ] {
            for command in set.commands().into_iter().chain(set.hidden()) {
                out.push((command.name.to_string(), command.about.to_string()));
            }
        }
        out.sort();
        out
    };
    let (zh, en) = both(catalogue);

    assert!(zh.len() > 30, "the catalogue is thin: {}", zh.len());
    assert_eq!(
        zh.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        en.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "the two languages offer different commands"
    );
    for ((name, said_zh), (_, said_en)) in zh.iter().zip(&en) {
        assert!(
            !said_zh.is_empty() && !said_en.is_empty(),
            "/{name} says nothing"
        );
        assert_ne!(said_zh, said_en, "/{name} reads the same in both languages");
        assert!(
            has_cjk(said_zh),
            "/{name}'s Chinese is not Chinese: {said_zh}"
        );
        assert!(
            !has_cjk(said_en),
            "/{name}'s English is not English: {said_en}"
        );
    }
}

/// The small closed sets a panel draws its own vocabulary from.
///
/// One per panel that has one, because each is a separate `match` and a
/// forgotten arm shows up in exactly one of them.
#[test]
fn each_panel_s_own_words_follow_the_language() {
    let _guard = test_lock();

    let (zh, en) = both(|| {
        vec![
            atomcode_tui::settings::Applies::Restart.say(),
            atomcode_tui::providers::Tab::Accounts.label(),
            atomcode_tui::plugins::Scope::User.label(),
            atomcode_tui::plugins::Tab::Installed.label(),
            atomcode_tui::plugins::PluginAction::Uninstall.label(),
            atomcode_tui::plugins::MarketAction::Remove.label(),
            atomcode_tui::tools::State::On.about(),
            atomcode_tui::rewind::Scope::Conversation.about(),
            // 这一句真跑起来是英文的:它本是宿主写死的一句英文,屏幕原样
            // 传了出去。现在它是屏幕自己的话,所以跟着语言走。
            atomcode_tui::rewind::CodeOff::NotEnabled.say(),
            atomcode_tui::rewind::CodeOff::NoSession.say(),
            atomcode_tui::text::spoken_duration(4 * 60 + 12),
            atomcode_tui::text::when(0),
        ]
    });

    for (zh, en) in zh.iter().zip(&en) {
        assert_ne!(zh, en, "this word did not change: {zh}");
        assert!(has_cjk(zh), "not Chinese: {zh}");
        assert!(!has_cjk(en), "not English: {en}");
    }
}

/// A host's refusal, which the person reads and acts on.
#[test]
fn a_refusal_is_said_in_the_language_in_force() {
    let _guard = test_lock();
    let (zh, en) = both(|| {
        atomcode_tui::i18n::t(atomcode_tui::i18n::Msg::HostBusy {
            reason: "PRESERVED",
        })
        .into_owned()
    });
    assert_ne!(zh, en);
    assert!(has_cjk(&zh) && !has_cjk(&en), "{zh} / {en}");
    // The host's own words ride through untranslated — the screen frames the
    // refusal, it does not know what the host is busy with.
    assert!(zh.contains("PRESERVED") && en.contains("PRESERVED"));
}

/// The opening block, when the launcher provided no words of its own.
///
/// These used to be a second copy in Chinese inside this crate, which is why
/// the property is written down: they are the product's own sentences now, and
/// they follow the product's language.
#[test]
fn the_welcome_block_this_build_ships_follows_the_language() {
    let _guard = test_lock();
    let words = atomcode_tui::modules::welcome::ShippedWords;
    let (zh, en) = both(|| (words.heading(), words.about("init")));

    assert_ne!(zh.0, en.0, "the heading did not change");
    assert_ne!(zh.1, en.1, "the tip did not change");
    assert!(has_cjk(&zh.0) && !has_cjk(&en.0), "{:?} / {:?}", zh.0, en.0);
    assert_eq!(
        words.about("mouse"),
        None,
        "a command the table has no line for is nothing, not a blank"
    );
}

/// What the end of a turn says, which is drawn from a `StopReason` the screen
/// does not choose — the arm most likely to be added in one language only.
#[test]
fn how_a_turn_ended_is_said_in_both_languages() {
    let _guard = test_lock();
    let (zh, en) =
        both(|| atomcode_tui::i18n::t(atomcode_tui::i18n::Msg::StopRunawayFuse).into_owned());
    assert_ne!(zh, en);
    assert!(has_cjk(&zh) && !has_cjk(&en), "{zh} / {en}");
}
