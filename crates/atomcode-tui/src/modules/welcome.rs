//! The opening block of a new session, as a stream producer.
//!
//! One block, one row: `tui-panel-welcome`. Replacing the whole thing is
//! `[[patch]] id = "tui-panel-welcome"` with another `name`, so a third party that
//! wants a completely different opening does not edit this crate — see
//! `docs/plans/2026-09-15-session-welcome-block-design.md` §"插件化".
//!
//! **What it says comes in from the row.** The command names below are a list of
//! *names*; the one line each is described by is asked of [`WelcomeWords`], which
//! the mounting launcher fills from the product's own localisation
//! (`crate::content::WelcomeWords`). A build that mounts this row without one
//! gets [`ShippedWords`] — the same sentences, in Chinese — so the block still
//! opens with something to read.

use std::sync::Arc;

use crate::block::{Content, Coord, StreamWriter};
use crate::command::Command;
use crate::content::{WelcomeBlock, WelcomeWords};
use crate::module::{Opening, Producer};

/// How many randomly-chosen tips to show, beside the pinned one.
const MAX_TIPS: usize = 3;

/// The one command worth pinning first, when the screen has it.
///
/// Tuix pins `/login` here, and it is the same command on both screens — the
/// first thing a person needs on a machine that has not signed in yet.
const PINNED: &str = "login";

/// The commands worth suggesting, in the order a person would want them.
///
/// Tuix's own pool, name for name. It is a list of **names only**: the one line
/// each is described by comes from [`WelcomeWords`] and the command's own
/// `about` (see [`choose_tips`]), and anything not on the screen is filtered out
/// — so the list may be generous. Adding a name that is not mounted does
/// nothing, rather than recommending a command that does not exist.
///
/// Kept identical to `atomcode-tuix`'s `render::welcome_tips::POOL` on purpose:
/// the two front ends are two renderings of one product, and a person moving
/// between them should be told the same things. This crate does not depend on
/// tuix, so the list is restated here rather than imported — the cost of a
/// divergence is a tip that names a command the other screen pins, which
/// `the_pool_matches_what_the_other_front_end_offers` guards against.
const CANDIDATES: &[&str] = &[
    "login", "provider", "model", "resume", "setup", "skills", "plugin", "webui", "mcp", "plan",
    "session", "loop", "goal", "init", "language", "usage",
];

/// The words this build ships when no launcher provides any.
///
/// The same sentences `atomcode-config`'s Chinese table carries, so a screen
/// mounted without a localisation (a test, `--audit`, a screen-only build) reads
/// the same as the product does. A product with an i18n table provides its own
/// and this one is not consulted — that override is the seam
/// (`crate::plugin::WelcomeWordsSvc`).
pub struct ShippedWords;

impl WelcomeWords for ShippedWords {
    fn heading(&self) -> String {
        "上手提示".to_string()
    }
    fn about(&self, command: &str) -> Option<String> {
        let text = match command {
            "login" => "领取免费额度",
            "provider" => "添加自定义模型",
            "model" => "设置默认模型",
            "resume" => "恢复上次会话",
            "setup" => "一键推荐配置",
            "skills" => "浏览可用技能",
            "plugin" => "安装技能/命令插件",
            "webui" => "在浏览器打开同步会话",
            "mcp" => "接入 MCP 工具",
            "plan" => "只读规划模式",
            "session" => "管理与切换会话",
            "loop" => "循环执行提示词",
            "goal" => "为本次会话设定目标",
            "init" => "扫描代码库生成 AGENTS.md",
            "language" => "切换界面语言",
            "usage" => "查看用量与额度",
            _ => return None,
        };
        Some(text.to_string())
    }
}

/// The tips to show: the pinned one when it exists, then up to [`MAX_TIPS`] more.
///
/// **Filtered against the screen's own commands.** Tuix's pool pins `/provider`,
/// `/webui` and `/plan` among others, none of which this screen need have;
/// offering one anyway would recommend a command nobody can type.
///
/// `seed` comes from the working directory, and is read **once** by
/// [`Welcome::opening`] rather than taken on each call: `Content::lines` runs every
/// frame, and rolling there would change the block under the reader and move its
/// `content_hash` every frame. Tuix had to persist a `welcome_tip_indices` for
/// exactly that reason; deciding once is the same property for less machinery.
pub(crate) fn choose_tips(
    commands: &[Command],
    words: &dyn WelcomeWords,
    seed: &str,
) -> Vec<(String, String)> {
    let find = |name: &str| commands.iter().find(|command| command.name == name);

    // What one command is described by: the localisation when it has a line for
    // it, and the command's own `about` otherwise. The fallback is what keeps a
    // tip from being a command name with nothing beside it on a screen the
    // launcher's table does not know about.
    let describe = |command: &Command| -> String {
        words
            .about(&command.name)
            .unwrap_or_else(|| command.about.to_string())
    };

    let mut tips: Vec<(String, String)> = Vec::new();
    if let Some(pinned) = find(PINNED) {
        tips.push((format!("/{}", pinned.name), describe(pinned)));
    }

    let mut rest: Vec<&Command> = CANDIDATES
        .iter()
        .filter(|name| **name != PINNED)
        .filter_map(|name| find(name))
        .collect();

    // A stable order for a stable seed: one directory picks the same tips every
    // time, which is what lets the block be a pure function of its inputs.
    use rand::seq::SliceRandom;
    use rand::SeedableRng as _;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed_of(seed));
    rest.shuffle(&mut rng);

    for command in rest.into_iter().take(MAX_TIPS) {
        tips.push((format!("/{}", command.name), describe(command)));
    }
    tips
}

/// A string to a seed. FNV-1a, the same scheme as `block::hash_of`.
fn seed_of(seed: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The opening block's producer.
///
/// It holds the brand and nothing else: what this build calls itself is decided
/// once, by the row that mounts this (`crate::rows::WelcomePanel` reading
/// `BrandSvc`), rather than read from a constant in the middle of the layout
/// code. Everything else it says is decided in `opening` from what it is handed.
pub struct Welcome {
    brand: Arc<crate::content::Brand>,
}

impl Welcome {
    pub fn new(brand: Arc<crate::content::Brand>) -> Arc<Self> {
        Arc::new(Self { brand })
    }
}

impl Producer for Welcome {
    fn id(&self) -> &'static str {
        "welcome"
    }

    /// A welcome block folds no facts: everything it says is decided once, in
    /// `opening`.
    fn absorb(
        &self,
        _logged: &atomcode_harness::session::LoggedEvent,
        _out: &mut StreamWriter<'_>,
    ) {
    }

    fn opening(&self, _at: Coord, open: &Opening) -> Option<Arc<dyn Content>> {
        // Rolled here and only here. `lines` is called every frame and must be
        // pure, so the tips — and the heading, which is settled for the same
        // reason — are decided with the block.
        //
        // The words arrive on the opening rather than off a service read when
        // this producer mounted: the launcher's own rows mount *after* the
        // screen's, so a mount-time lookup would miss precisely the product that
        // has a language table to offer (`Opening::words`).
        let words: &dyn WelcomeWords = open.words.as_deref().unwrap_or(&ShippedWords);
        let tips = choose_tips(&open.commands, words, &open.cwd);
        Some(Arc::new(WelcomeBlock {
            cwd: open.cwd.clone(),
            model: open.model.clone(),
            version: open.version,
            heading: words.heading(),
            tips,
            brand: self.brand.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(names: &[&'static str]) -> Vec<Command> {
        names
            .iter()
            .map(|name| Command::new(name, "说明"))
            .collect()
    }

    fn all_offered() -> Vec<Command> {
        commands(&[
            "provider", "model", "resume", "setup", "skills", "plugin", "webui", "mcp", "plan",
            "session", "loop", "goal", "init", "language", "usage",
        ])
    }

    /// The words this crate ships, which is what a test with no launcher gets.
    fn words() -> ShippedWords {
        ShippedWords
    }

    #[test]
    fn tips_only_name_commands_the_screen_actually_has() {
        // The reason this module exists: the pool pins `/provider`, `/webui`,
        // `/plan` and others a screen need not have, and offering one anyway
        // would recommend a command nobody can type.
        let tips = choose_tips(&commands(&["resume", "help"]), &words(), "~/proj");
        for (command, _) in &tips {
            assert!(
                ["/resume", "/help"].contains(&command.as_str()),
                "recommended a command the screen does not have: {command}"
            );
        }
    }

    #[test]
    fn the_pinned_slot_yields_when_the_screen_has_no_such_command() {
        let without = choose_tips(&commands(&["resume", "help"]), &words(), "~/proj");
        assert!(
            !without.iter().any(|(c, _)| c == "/login"),
            "the screen has no /login, so it must not be offered: {without:?}"
        );
        let with = choose_tips(&commands(&["login", "resume", "help"]), &words(), "~/proj");
        assert_eq!(with[0].0, "/login", "and it leads when it does exist");
    }

    #[test]
    fn one_directory_picks_the_same_tips_every_time() {
        // The property the whole design leans on: the block is a pure function of
        // its inputs, so two renders in one directory agree.
        let commands = all_offered();
        assert_eq!(
            choose_tips(&commands, &words(), "~/proj"),
            choose_tips(&commands, &words(), "~/proj")
        );
    }

    #[test]
    fn two_directories_do_not_have_to_agree() {
        // Not a guarantee that they differ — a shuffle may land the same way — but
        // the seed does reach the shuffle. Four draws from a pool of fifteen make
        // an accidental match unlikely enough to catch a hardcoded answer.
        let commands = all_offered();
        let picks: Vec<Vec<String>> = ["~/a", "~/b", "~/c", "~/d", "~/e"]
            .iter()
            .map(|seed| {
                choose_tips(&commands, &words(), seed)
                    .into_iter()
                    .map(|(c, _)| c)
                    .collect()
            })
            .collect();
        let distinct = {
            let mut seen = picks.clone();
            seen.sort();
            seen.dedup();
            seen.len()
        };
        assert!(
            distinct > 1,
            "the seed never reached the shuffle: {picks:?}"
        );
    }

    #[test]
    fn tips_are_distinct_and_capped() {
        let tips = choose_tips(&all_offered(), &words(), "~/proj");
        assert!(tips.len() <= MAX_TIPS + 1, "at most four: {tips:?}");
        let mut names: Vec<&str> = tips.iter().map(|(c, _)| c.as_str()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "one command twice: {tips:?}");
    }

    #[test]
    fn an_empty_command_table_produces_no_tips() {
        // A tree with no command sets mounted: no tips at all, rather than a
        // heading over nothing.
        assert!(choose_tips(&[], &words(), "~/proj").is_empty());
    }

    /// The pool is the other front end's, name for name — a divergence here is
    /// how one screen comes to pin a command the other one does not.
    ///
    /// The list is restated rather than imported (this crate does not depend on
    /// tuix), so it is the one thing that can silently drift. Written out here
    /// as the literal, so an edit to `CANDIDATES` that is not made in both
    /// places fails here rather than on somebody's screen.
    #[test]
    fn the_pool_matches_what_the_other_front_end_offers() {
        // `atomcode-tuix/src/render/welcome_tips.rs`'s `POOL`, plus the pinned
        // `/login` this one lists too (there it is `PINNED`, here it is entry 0
        // of the same list).
        let tuix = [
            "login", "provider", "model", "resume", "setup", "skills", "plugin", "webui", "mcp",
            "plan", "session", "loop", "goal", "init", "language", "usage",
        ];
        assert_eq!(
            CANDIDATES, tuix,
            "the welcome tip pool drifted from the other front end's"
        );
    }

    #[test]
    fn a_command_with_no_localised_line_falls_back_to_its_own_description() {
        // The seam may be a partial table — a launcher that knows only some of
        // these commands. A tip with nothing beside it would be worse than the
        // command's own words, so the fallback is the command's `about`.
        struct Partial;
        impl WelcomeWords for Partial {
            fn heading(&self) -> String {
                "Tips".into()
            }
            fn about(&self, command: &str) -> Option<String> {
                (command == "resume").then(|| "pick up where you left off".to_string())
            }
        }
        let tips = choose_tips(&commands(&["resume", "init"]), &Partial, "~/proj");
        let by_name = |name: &str| {
            tips.iter()
                .find(|(c, _)| c == name)
                .map(|(_, about)| about.clone())
                .unwrap_or_else(|| panic!("no tip for {name}: {tips:?}"))
        };
        assert_eq!(by_name("/resume"), "pick up where you left off");
        assert_eq!(by_name("/init"), "说明", "the command's own words");
    }

    #[test]
    fn the_description_is_the_localisations_when_it_has_one() {
        // The welcome screen and the command's own help read the same sentence
        // because both come from the product's table — not because this file
        // keeps a second copy of it.
        let tips = choose_tips(&commands(&["resume"]), &words(), "~/proj");
        assert_eq!(tips[0].1, "恢复上次会话");
    }

    #[test]
    fn opening_builds_the_block_from_what_it_was_handed() {
        let welcome = Welcome::new(Arc::new(crate::content::Brand::default()));
        let open = Opening {
            cwd: "~/proj".into(),
            model: Some("a-model".into()),
            version: "9.9.9",
            commands: commands(&["resume", "help"]),
            ..Default::default()
        };
        let block = welcome.opening(Coord::default(), &open).expect("a block");
        assert_eq!(block.kind(), "welcome");
        assert!(block.always_open(), "it is not a thing to fold away");
    }

    #[test]
    fn the_heading_is_whatever_the_words_on_the_opening_say() {
        // Settled with the block, from the words the opening carries — a screen
        // whose launcher follows `/language` reads its own heading here.
        //
        // **On the opening, not on the producer**: the launcher's rows mount
        // after the screen's, so words resolved at producer-mount time would be
        // missing for exactly the launcher that has some. This criterion pins
        // the property that a *late* answer still reaches the block.
        struct Other;
        impl WelcomeWords for Other {
            fn heading(&self) -> String {
                "Tips for getting started".into()
            }
            fn about(&self, _command: &str) -> Option<String> {
                None
            }
        }
        let welcome = Welcome::new(Arc::new(crate::content::Brand::default()));
        let open = Opening {
            cwd: "~/proj".into(),
            commands: commands(&["resume"]),
            words: Some(Arc::new(Other)),
            ..Default::default()
        };
        let with_words = welcome.opening(Coord::default(), &open).expect("a block");
        // The heading is what a person reads, so assert on it rather than only on
        // the hash: a hash inequality would also hold for a block that lost the
        // heading entirely.
        let drawn = with_words
            .lines(&crate::block::RenderCtx::bare(80))
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            drawn.contains("Tips for getting started"),
            "the injected heading is on screen:\n{drawn}"
        );
        assert!(
            !drawn.contains("上手提示"),
            "and the shipped one is not:\n{drawn}"
        );
    }

    #[test]
    fn an_opening_with_no_words_gets_the_sentences_this_crate_ships() {
        // A screen mounted without a launcher — a test, `--audit` — still opens
        // with something to read, rather than a heading over nothing.
        let welcome = Welcome::new(Arc::new(crate::content::Brand::default()));
        let open = Opening {
            cwd: "~/proj".into(),
            commands: commands(&["resume"]),
            ..Default::default()
        };
        let block = welcome.opening(Coord::default(), &open).expect("a block");
        let drawn = block
            .lines(&crate::block::RenderCtx::bare(80))
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(drawn.contains("上手提示"), "{drawn}");
        assert!(drawn.contains("恢复上次会话"), "{drawn}");
    }
}
