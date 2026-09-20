//! Narrowing the tool catalog: drop one tool out of a row, keep the rest.
//!
//! The switch a config tree has is the **row**, and a row usually mounts
//! several tools (`tool-fs-world` brings five). "Drop `write_file`, keep
//! `read_file`" had no expression at all, and neither did "let this MCP server
//! offer two of its forty tools" — an MCP server's tools are published at
//! runtime, by a row whose name list nobody wrote.
//!
//! So the policy sits on the catalog, and is enforced where a tool is
//! registered rather than where the schema is rendered. That is what makes the
//! third case — replacing one tool out of a row — work: the incumbent never
//! takes the name, so a second row can register under it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::seams::{Aspect, OperationsSvc, ToolsSvc};
use atomcode_harness::{bundle, plugins};
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use serde_json::{json, Value};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("tool-policy-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

// ---- a tool a product outside this workspace writes ----------------------

/// Answers with a fixed word, under whatever name it was built with — so a
/// replacement is distinguishable from the tool it replaced.
struct Stub {
    name: &'static str,
    says: &'static str,
}

#[async_trait]
impl Tool for Stub {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.says
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: self.says.to_string(),
            is_error: false,
            images: vec![],
        }
    }
}

/// A row that mounts one [`Stub`] through the door every tool row uses.
struct StubRow {
    row: &'static str,
    tool: &'static str,
    says: &'static str,
}

#[async_trait]
impl Plugin for StubRow {
    fn name(&self) -> &'static str {
        self.row
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "a tool written outside the harness"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        plugins::tools::mount(
            ctx,
            vec![Arc::new(Stub {
                name: self.tool,
                says: self.says,
            })],
        )
    }
}

fn tree(dir: &Path, rows: &str) -> ConfigTree {
    let root = bundle::toml_string(&dir.to_string_lossy());
    let base = format!(
        "[[insert]]\nname = \"tool-fs-world\"\n\n\
         [[insert]]\nname = \"tool-search-world\"\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [{{ text = \"ok\" }}] }}\n\n\
         {rows}"
    );
    ConfigTree::from_layers([
        bundle::infra().expect("infra"),
        Layer::from_toml(&base).expect("rows"),
    ])
    .expect("tree")
}

async fn start(tree: ConfigTree, rows: Vec<Arc<dyn Plugin>>) -> App {
    let mut catalog = plugins::catalog();
    for row in rows {
        catalog.register(row);
    }
    let mut app = App::new(catalog, tree);
    app.start().await.expect("must mount");
    app
}

fn names(app: &App) -> Vec<String> {
    app.context().service::<ToolsSvc>().unwrap().names()
}

// ---- the criteria ---------------------------------------------------------

/// One tool out of a five-tool row, and the row itself stays.
#[tokio::test]
async fn an_excluded_tool_never_enters_the_catalog() {
    let dir = scratch("exclude");
    let app = start(
        tree(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"write_file\"] }\n",
        ),
        vec![],
    )
    .await;

    let names = names(&app);
    assert!(
        !names.contains(&"write_file".to_string()),
        "excluded, yet in the catalog: {names:?}"
    );
    assert!(
        names.contains(&"read_file".to_string()) && names.contains(&"edit_file".to_string()),
        "the rest of the row must still be there: {names:?}"
    );

    let tools = app.context().service::<ToolsSvc>().unwrap();
    assert!(
        tools.get("write_file").is_none(),
        "a call must not resolve either — the schema and the lookup are one table"
    );
    assert!(
        !tools.defs().iter().any(|d| d.name == "write_file"),
        "and the model must not be shown it"
    );
    assert_eq!(
        tools.turned_away(),
        vec!["tool-fs-world:write_file".to_string()],
        "and it can say which row offered what it dropped"
    );
}

/// Replacing one tool out of a row: the incumbent is excluded, which frees the
/// name, and a row from outside registers its own under it.
#[tokio::test]
async fn excluding_a_name_frees_it_for_a_replacement() {
    let dir = scratch("replace");
    let app = start(
        tree(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"tool-fs-world:read_file\"] }\n\n\
             [[insert]]\nname = \"my-read-file\"\n",
        ),
        vec![Arc::new(StubRow {
            row: "my-read-file",
            tool: "read_file",
            says: "the product's own read_file",
        })],
    )
    .await;

    let tools = app.context().service::<ToolsSvc>().unwrap();
    let read = tools.get("read_file").expect("the replacement is mounted");
    assert_eq!(
        read.description(),
        "the product's own read_file",
        "the name went to the replacement, not to the row that was excluded"
    );
    assert!(
        names(&app).contains(&"write_file".to_string()),
        "and the rest of the incumbent's row is untouched"
    );
}

/// `include` is a whitelist; a name in both lists is out.
#[tokio::test]
async fn include_is_a_whitelist_and_exclude_beats_it() {
    let dir = scratch("include");
    let app = start(
        tree(
            &dir,
            "[[patch]]\nid = \"tools\"\n\
             config = { include = [\"read_file\", \"write_file\"], exclude = [\"write_file\"] }\n",
        ),
        vec![],
    )
    .await;

    let names = names(&app);
    assert_eq!(
        names,
        vec!["read_file".to_string()],
        "only the included name that is not also excluded: {names:?}"
    );
}

/// The case row-level switches cannot reach: tools that arrive at runtime, by
/// the `mcp__{server}__{tool}` name MCP publishes under.
#[tokio::test]
async fn a_pattern_keeps_one_servers_tools_out_and_leaves_another_in() {
    let dir = scratch("mcp");
    let app = start(
        tree(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"mcp__github__*\"] }\n\n\
             [[insert]]\nname = \"fake-github\"\n\n\
             [[insert]]\nname = \"fake-jira\"\n",
        ),
        vec![
            Arc::new(StubRow {
                row: "fake-github",
                tool: "mcp__github__create_issue",
                says: "github",
            }),
            Arc::new(StubRow {
                row: "fake-jira",
                tool: "mcp__jira__create_issue",
                says: "jira",
            }),
        ],
    )
    .await;

    let names = names(&app);
    assert!(
        !names.contains(&"mcp__github__create_issue".to_string()),
        "the pattern must keep that server's tools out: {names:?}"
    );
    assert!(
        names.contains(&"mcp__jira__create_issue".to_string()),
        "and leave the other server's alone: {names:?}"
    );
}

/// A narrowed catalog says so, from the live catalog — otherwise the model
/// reports a tool it no longer has, which is the failure `describe_self` exists
/// to prevent (`docs/adr/0009`).
#[tokio::test]
async fn the_tree_says_what_it_kept_out() {
    let dir = scratch("says");
    let app = start(
        tree(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"write_file\"] }\n",
        ),
        vec![],
    )
    .await;

    let ctx = app.context();
    let said = ctx
        .service::<OperationsSvc>()
        .unwrap()
        .render(Aspect::Operations, &ctx)
        .join("\n");
    assert!(
        said.contains("write_file"),
        "the tree kept a tool out and cannot say which:\n{said}"
    );
}

/// And an ordinary tree says nothing about a policy it does not have.
#[tokio::test]
async fn an_open_catalog_says_nothing_about_a_policy() {
    let dir = scratch("open");
    let app = start(tree(&dir, ""), vec![]).await;
    let ctx = app.context();
    let said = ctx
        .service::<OperationsSvc>()
        .unwrap()
        .render(Aspect::Operations, &ctx)
        .join("\n");
    assert!(
        !said.contains("TOOL CATALOG"),
        "nothing was narrowed, so there is nothing to explain:\n{said}"
    );
}

/// The bare form is the other half of the pair: it excludes that name from
/// everyone, so a host cannot accidentally leave a second `write_file` behind
/// by adding a row.
#[tokio::test]
async fn a_bare_name_excludes_every_rows_copy_of_it() {
    let dir = scratch("bare");
    let app = start(
        tree(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"read_file\"] }\n\n\
             [[insert]]\nname = \"my-read-file\"\n",
        ),
        vec![Arc::new(StubRow {
            row: "my-read-file",
            tool: "read_file",
            says: "the product's own read_file",
        })],
    )
    .await;

    let names = names(&app);
    assert!(
        !names.contains(&"read_file".to_string()),
        "the unqualified pattern names the tool, not one row's copy: {names:?}"
    );
}

// ---- the switch a person flips mid-session --------------------------------

/// Off and back on, without touching the row that owns the tool.
#[tokio::test]
async fn a_tool_turned_off_leaves_the_catalog_and_comes_back_unchanged() {
    let dir = scratch("switch");
    let app = start(tree(&dir, ""), vec![]).await;
    let tools = app.context().service::<ToolsSvc>().unwrap();

    assert_eq!(tools.turn_off("write_file"), vec!["write_file".to_string()]);
    assert!(!names(&app).contains(&"write_file".to_string()));
    assert!(tools.get("write_file").is_none(), "nor may a call resolve");
    assert!(!tools.defs().iter().any(|d| d.name == "write_file"));
    assert_eq!(tools.held_back(), vec!["write_file".to_string()]);
    assert!(
        names(&app).contains(&"read_file".to_string()),
        "the rest of the row is untouched"
    );

    assert_eq!(tools.turn_on("write_file"), vec!["write_file".to_string()]);
    assert!(names(&app).contains(&"write_file".to_string()));
    assert!(tools.held_back().is_empty());
}

/// A whole server off, one of its tools back — the shape of the request.
#[tokio::test]
async fn a_server_goes_off_by_pattern_and_one_tool_comes_back_by_name() {
    let dir = scratch("server");
    let app = start(
        tree(
            &dir,
            "[[insert]]\nname = \"fake-github-a\"\n\n[[insert]]\nname = \"fake-github-b\"\n",
        ),
        vec![
            Arc::new(StubRow {
                row: "fake-github-a",
                tool: "mcp__github__create_issue",
                says: "a",
            }),
            Arc::new(StubRow {
                row: "fake-github-b",
                tool: "mcp__github__delete_repo",
                says: "b",
            }),
        ],
    )
    .await;
    let tools = app.context().service::<ToolsSvc>().unwrap();

    let off = tools.turn_off("mcp__github__*");
    assert_eq!(off.len(), 2, "both of that server's tools: {off:?}");
    assert!(!names(&app).iter().any(|n| n.starts_with("mcp__github__")));

    let back = tools.turn_on("mcp__github__create_issue");
    assert_eq!(back, vec!["mcp__github__create_issue".to_string()]);
    let live = names(&app);
    assert!(
        live.contains(&"mcp__github__create_issue".to_string()),
        "the one asked back is back: {live:?}"
    );
    assert!(
        !live.contains(&"mcp__github__delete_repo".to_string()),
        "and the rest of the server stays off: {live:?}"
    );
}

/// The case a switch has to survive: the tool is published after the person
/// said to hide it, which is every MCP server that is still connecting.
#[tokio::test]
async fn a_tool_that_arrives_after_the_switch_is_born_hidden() {
    let dir = scratch("late");
    let app = start(tree(&dir, ""), vec![]).await;
    let tools = app.context().service::<ToolsSvc>().unwrap();

    assert!(tools.turn_off("mcp__github__*").is_empty(), "nothing yet");

    // What `publish_mcp` does when the server finally answers.
    tools
        .register_from(
            "mcp-host",
            Arc::new(Stub {
                name: "mcp__github__create_issue",
                says: "late",
            }),
        )
        .expect("publication must not fail");

    assert!(
        !names(&app).contains(&"mcp__github__create_issue".to_string()),
        "it must not slip in behind the person"
    );
    assert_eq!(
        tools.turn_on("mcp__github__*"),
        vec!["mcp__github__create_issue".to_string()],
        "and it is there to be asked back"
    );
}

/// The config's answer is not a suggestion: a command cannot widen it.
#[tokio::test]
async fn a_switch_cannot_put_back_what_the_config_excluded() {
    let dir = scratch("cannot");
    let app = start(
        tree(
            &dir,
            "[[patch]]\nid = \"tools\"\nconfig = { exclude = [\"write_file\"] }\n",
        ),
        vec![],
    )
    .await;
    let tools = app.context().service::<ToolsSvc>().unwrap();

    assert!(
        tools.turn_on("write_file").is_empty(),
        "there is nothing held back to restore — it never entered"
    );
    assert!(!names(&app).contains(&"write_file".to_string()));
}

/// The person's entry: `/tools`, `/tools off …`, `/tools on …`. Registered by
/// the row that owns the catalog, and deliberately not a tool — an agent that
/// can put its own tools back has not been restricted.
#[tokio::test]
async fn a_person_works_the_switch_through_the_command() {
    let dir = scratch("command");
    let app = start(tree(&dir, ""), vec![]).await;
    let ctx = app.context();
    let catalog = ctx
        .service::<atomcode_harness::seams::CommandsSvc>()
        .expect("the catalog is a core row");
    let agent = atomcode_harness::create_agent(&app)
        .await
        .expect("an agent to run commands against");
    let run = |args: &'static str| {
        let catalog = catalog.clone();
        let agent = agent.clone();
        async move {
            catalog
                .find("tools", &agent)
                .expect("`/tools` is on offer")
                .run(agent.clone(), args)
                .await
        }
    };

    let listed = run("").await.expect("listing");
    assert!(listed.contains("write_file"), "{listed}");

    let off = run("off write_file").await.expect("off");
    assert!(off.contains("write_file"), "{off}");
    assert!(!names(&app).contains(&"write_file".to_string()));
    let listed = run("").await.expect("listing");
    assert!(
        listed.contains("本次会话关掉的"),
        "the listing separates what the person turned off: {listed}"
    );

    let on = run("on write_file").await.expect("on");
    assert!(on.contains("write_file"), "{on}");
    assert!(names(&app).contains(&"write_file".to_string()));

    assert!(
        run("off").await.is_err(),
        "`off` with nothing to name is a refusal, not a no-op"
    );
    assert!(run("sideways nope").await.is_err(), "and so is a typo");

    assert!(
        !ctx.service::<ToolsSvc>()
            .unwrap()
            .names()
            .iter()
            .any(|n| n == "tools"),
        "the switch is a command, not a tool the model can call"
    );
}

/// And a tool the person turned off says so too — otherwise the model keeps
/// telling them it can do a thing it no longer can.
#[tokio::test]
async fn the_tree_says_what_the_person_turned_off() {
    let dir = scratch("said-off");
    let app = start(tree(&dir, ""), vec![]).await;
    let ctx = app.context();
    let said = |ctx: &Context| {
        ctx.service::<OperationsSvc>()
            .unwrap()
            .render(Aspect::Operations, ctx)
            .join("\n")
    };
    assert!(
        !said(&ctx).contains("TOOL CATALOG"),
        "nothing is off, so there is nothing to say"
    );

    ctx.service::<ToolsSvc>().unwrap().turn_off("write_file");
    let now = said(&ctx);
    assert!(
        now.contains("write_file") && now.contains("/tools on"),
        "it must name the tool and how to put it back:\n{now}"
    );
}
