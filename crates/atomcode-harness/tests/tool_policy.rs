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
