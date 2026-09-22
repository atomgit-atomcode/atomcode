//! The commands the product's own rows owe a person: `/skills`, one command per
//! skill they may invoke, and `/review`.
//!
//! The harness's `skills` and `tool-code-review` rows register these. The product
//! swaps the first for `skills-host` and disables the second, mounting its own
//! reviewer through `host-tools` — and both replacements mounted the tools and
//! forgot the commands. The skills half was fixed in `f8b085bb1`, which this
//! asserts from the product's side as well (a skill that is a command, and
//! `/skills` itself); `/review` is the same omission in the other replacement,
//! and had no command on the product at all
//! (`docs/plans/2026-09-19-remaining-gaps.md`, 2026-09-22).
//!
//! The assertions are on the product's assembly, not on the harness rows,
//! because the harness rows were never the ones missing them.

mod support;

use std::sync::Arc;

use atomcode_coding::{CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_harness::seams::{AgentsSvc, CommandsSvc};
use atomcode_kernel::provider::LlmProvider;
use support::{allow, mount, quiet_options, turn};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

struct CannedProvider;

#[async_trait::async_trait]
impl LlmProvider for CannedProvider {
    fn model_name(&self) -> &str {
        "canned"
    }
    async fn chat_stream(
        &self,
        _: &[atomcode_kernel::message::Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        use atomcode_kernel::stream::StreamEvent;
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
    let skill = dir.join(name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nDo the thing.\n"),
    )
    .unwrap();
}

#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn the_product_offers_the_commands_its_skill_and_review_rows_own() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    write_skill(skills.path(), "tidy-imports", "sorts and prunes imports");
    let cfg = CodingAgentConfig::new("k", "http://localhost", "canned", project.path());

    let opts = PrepareOptions {
        session: SessionMode::Fresh,
        skill_dirs: Some(vec![skills.path().to_path_buf()]),
        review: true,
        ..quiet_options()
    };
    let mut mounted = mount(&cfg, opts, Arc::new(CannedProvider)).await;
    // A turn creates the conversation's own agent, which is what a command is
    // offered to.
    turn(&mut mounted.handle, "hello", allow()).await;

    let ctx = mounted.context();
    let agent = ctx
        .service::<AgentsSvc>()
        .expect("agents")
        .list()
        .into_iter()
        .find(|a| a.parent().is_none())
        .expect("the conversation's own agent");
    let commands = ctx.service::<CommandsSvc>().expect("the command catalog");
    let offered: Vec<String> = commands
        .offered_for(&agent)
        .into_iter()
        .map(|c| c.name)
        .collect();
    for name in ["skills", "tidy-imports", "review"] {
        assert!(
            offered.contains(&name.to_string()),
            "the product offers `/{name}`: {offered:?}"
        );
    }

    let listed = commands
        .find("skills", &agent)
        .expect("offered")
        .run(agent.clone(), "")
        .await
        .expect("running it");
    assert!(
        listed.contains("tidy-imports") && listed.contains("sorts and prunes imports"),
        "`/skills` lists what is installed: {listed}"
    );

    mounted.shutdown().await;
}
