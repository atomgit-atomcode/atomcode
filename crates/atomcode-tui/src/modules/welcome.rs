//! The opening block of a new session, as a stream producer.
//!
//! One block, one row: `tui-panel-welcome`. Replacing the whole thing is
//! `[[patch]] id = "tui-panel-welcome"` with another `name`, so a third party that
//! wants a completely different opening does not edit this crate — see
//! `docs/plans/2026-09-15-session-welcome-block-design.md` §"插件化".

use std::sync::Arc;

use crate::block::{Content, Coord, StreamWriter};
use crate::command::Command;
use crate::content::WelcomeBlock;
use crate::module::{Opening, Producer};

/// How many randomly-chosen tips to show, beside the pinned one.
const MAX_TIPS: usize = 3;

/// The one command worth pinning first, when the screen has it.
///
/// Tuix pins `/login` here. This screen's command set does not have it, and a
/// pinned slot whose command is absent **yields** rather than being filled with a
/// command that is not there — the whole point of filtering.
const PINNED: &str = "login";

/// The commands worth suggesting, in the order a person would want them.
///
/// A list of **names only**. Descriptions come from each `Command`'s own `about`
/// (see `choose_tips`), and anything not on the screen is filtered out — so this
/// list may be generous: adding a name that is not mounted does nothing, rather
/// than recommending a command that does not exist.
const CANDIDATES: &[&str] = &[
    "login",
    "resume",
    "model",
    "skills",
    "mcp",
    "rows",
    "help",
    "layout",
    "audit",
    "compact",
    "context",
    "transcript",
    "reasoning",
    "tools",
    "mouse",
];

/// The tips to show: the pinned one when it exists, then up to [`MAX_TIPS`] more.
///
/// **Filtered against the screen's own commands.** Tuix's pool pins `/provider`,
/// `/webui` and `/plan` among others, none of which this screen need have; copying
/// that pool would recommend commands nobody can type.
///
/// `seed` comes from the working directory, and is read **once** by
/// [`Welcome::opening`] rather than taken on each call: `Content::lines` runs every
/// frame, and rolling there would change the block under the reader and move its
/// `content_hash` every frame. Tuix had to persist a `welcome_tip_indices` for
/// exactly that reason; deciding once is the same property for less machinery.
pub(crate) fn choose_tips(commands: &[Command], seed: &str) -> Vec<(String, String)> {
    let find = |name: &str| commands.iter().find(|command| command.name == name);

    let mut tips: Vec<(String, String)> = Vec::new();
    if let Some(pinned) = find(PINNED) {
        tips.push((format!("/{}", pinned.name), pinned.about.to_string()));
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
        tips.push((format!("/{}", command.name), command.about.to_string()));
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
/// A unit struct: it holds no state. Everything it says is decided in `opening`
/// from what it is handed.
pub struct Welcome;

impl Welcome {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
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
        // pure, so the tips are settled with the block.
        let tips = choose_tips(&open.commands, &open.cwd);
        Some(Arc::new(WelcomeBlock {
            cwd: open.cwd.clone(),
            model: open.model.clone(),
            version: open.version,
            tips,
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
            "resume",
            "model",
            "skills",
            "mcp",
            "rows",
            "help",
            "layout",
            "audit",
            "compact",
            "context",
            "transcript",
            "reasoning",
            "tools",
            "mouse",
        ])
    }

    #[test]
    fn tips_only_name_commands_the_screen_actually_has() {
        // The reason this module exists: tuix's pool pins `/provider`, `/webui`,
        // `/plan` and others this screen need not have, and copying it would
        // recommend commands nobody can type.
        let tips = choose_tips(&commands(&["resume", "help"]), "~/proj");
        for (command, _) in &tips {
            assert!(
                ["/resume", "/help"].contains(&command.as_str()),
                "recommended a command the screen does not have: {command}"
            );
        }
    }

    #[test]
    fn the_pinned_slot_yields_when_the_screen_has_no_such_command() {
        let without = choose_tips(&commands(&["resume", "help"]), "~/proj");
        assert!(
            !without.iter().any(|(c, _)| c == "/login"),
            "the screen has no /login, so it must not be offered: {without:?}"
        );
        let with = choose_tips(&commands(&["login", "resume", "help"]), "~/proj");
        assert_eq!(with[0].0, "/login", "and it leads when it does exist");
    }

    #[test]
    fn one_directory_picks_the_same_tips_every_time() {
        // The property the whole design leans on: the block is a pure function of
        // its inputs, so two renders in one directory agree.
        let commands = all_offered();
        assert_eq!(
            choose_tips(&commands, "~/proj"),
            choose_tips(&commands, "~/proj")
        );
    }

    #[test]
    fn two_directories_do_not_have_to_agree() {
        // Not a guarantee that they differ — a shuffle may land the same way — but
        // the seed does reach the shuffle. Four draws from a pool of fourteen make
        // an accidental match unlikely enough to catch a hardcoded answer.
        let commands = all_offered();
        let picks: Vec<Vec<String>> = ["~/a", "~/b", "~/c", "~/d", "~/e"]
            .iter()
            .map(|seed| {
                choose_tips(&commands, seed)
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
        let tips = choose_tips(&all_offered(), "~/proj");
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
        assert!(choose_tips(&[], "~/proj").is_empty());
    }

    #[test]
    fn the_description_is_the_commands_own() {
        // Taken from `Command::about`, never written again here: a second copy is
        // how a command's help and the welcome screen come to disagree.
        let tips = choose_tips(&commands(&["resume"]), "~/proj");
        assert_eq!(tips[0].1, "说明");
    }

    #[test]
    fn opening_builds_the_block_from_what_it_was_handed() {
        let welcome = Welcome::new();
        let open = Opening {
            cwd: "~/proj".into(),
            model: Some("a-model".into()),
            version: "9.9.9",
            commands: commands(&["resume", "help"]),
        };
        let block = welcome.opening(Coord::default(), &open).expect("a block");
        assert_eq!(block.kind(), "welcome");
        assert!(block.always_open(), "it is not a thing to fold away");
    }
}
