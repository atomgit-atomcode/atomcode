// crates/atomcode-core/src/commands/mod.rs
//
// Custom slash-command registry. Users define commands as `.md` files with
// YAML-style frontmatter in two locations:
//
//   1. `$ATOMCODE_HOME/commands/`          — global (apply to every project)
//   2. `<project>/.atomcode/commands/`  — project-level (override global
//                                          when names collide)
//
// Each file has the shape:
//
// ```markdown
// ---
// name: review
// description: 对当前 diff 进行代码审查
// args: optional
// ---
// 请对当前 git diff 中的所有改动进行代码审查。
// 如有指定文件则只审查: $ARGUMENTS
// ```
//
// The `args` field controls the command's argument expectation:
//
//   `none`     (default) — No argument required. Selecting from the slash menu
//                          executes immediately, same as a built-in command.
//   `optional`           — Argument may be supplied. Menu selection completes
//                          to `/name ` so the user can type an argument before
//                          pressing Enter again to execute.
//   `required`           — Argument is mandatory. Menu selection completes to
//                          `/name `, and submitting with an empty argument is
//                          rejected with an error message.
//
// Template variables:
//
//   `$ARGUMENTS`  /  `${ARGUMENTS}`   — Replaced with the user-provided text
//                                        after the command name (if any).
//
// The registry is loaded once at startup (or on `/mcp reload`-style events)
// and queried by the TUI dispatch loop. Custom commands are NOT LLM-invocable
// — they are user-invocable via `/command_name` in the input, sending the
// rendered template as a user message to the agent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single custom command parsed from a `.md` template file.
#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub args_requirement: ArgsRequirement,
    pub template: String,
    pub source: PathBuf,
    pub namespace: Option<String>,
}

impl CustomCommand {
    /// The lookup key used in the registry — `"plugin:name"` when namespaced,
    /// otherwise just `"name"`.
    pub fn key(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}:{}", ns, self.name),
            None => self.name.clone(),
        }
    }

    /// Render the template, replacing `$ARGUMENTS` / `${ARGUMENTS}` with `args`.
    ///
    /// Normalizes `${ARGUMENTS}` → `$ARGUMENTS` first so the chained
    /// `.replace()` never re-scans the interpolated `args` for the other
    /// placeholder — otherwise user input containing a literal
    /// `${ARGUMENTS}` or `$ARGUMENTS` would cause recursive replacement
    /// and corrupt the output.
    pub fn render(&self, args: &str) -> String {
        self.template
            .replace("${ARGUMENTS}", "$ARGUMENTS")
            .replace("$ARGUMENTS", args)
    }
}

/// Whether a custom command expects arguments from the user, controlling
/// both the slash-menu Enter behaviour and dispatch validation:
///
/// | Value      | Menu Enter          | Empty-arg submit |
/// |------------|---------------------|------------------|
/// | `None`     | Execute immediately | Accepted         |
/// | `Optional` | Complete to `/name `| Accepted         |
/// | `Required` | Complete to `/name `| Rejected (error) |
#[derive(Debug, Clone, PartialEq)]
pub enum ArgsRequirement {
    Required,
    Optional,
    None,
}

/// In-memory registry of all custom commands discovered from global +
/// project-level directories. Project commands override global ones when
/// names collide (loaded second).
pub struct CustomCommandRegistry {
    commands: HashMap<String, CustomCommand>,
}

impl CustomCommandRegistry {
    /// Scan both global (`$ATOMCODE_HOME/commands/`) and project-level
    /// (`<project_root>/.atomcode/commands/`) directories, merging results.
    /// Project entries win on name collision.
    pub fn load(project_root: &Path) -> Self {
        let config_dir = atomcode_config::config::Config::config_dir();
        let mut commands = HashMap::new();
        // Global first — project overrides on second pass.
        Self::load_from_dir(&config_dir.join("commands"), None, &mut commands);
        Self::load_from_dir(
            &project_root.join(".atomcode/commands"),
            None,
            &mut commands,
        );
        // Plugin layer
        for assets in
            atomcode_capabilities::plugin::loader::iter_installed_plugin_assets_for(project_root)
        {
            Self::load_from_dir(&assets.commands_dir(), Some(&assets.plugin), &mut commands);
        }
        Self { commands }
    }

    /// An empty registry — useful for tests or when custom commands are
    /// disabled.
    pub fn empty() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Register a single command into the registry. Used by tests and
    /// plugin installers that build commands programmatically rather than
    /// reading `.md` files from disk.
    pub fn register(&mut self, cmd: CustomCommand) {
        let key = cmd.key();
        self.commands.insert(key, cmd);
    }

    fn load_from_dir(
        dir: &Path,
        namespace: Option<&str>,
        commands: &mut HashMap<String, CustomCommand>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(mut cmd) = Self::parse_command_file(&path) {
                if let Some(ns) = namespace {
                    cmd.namespace = Some(ns.to_string());
                }
                let key = cmd.key();
                commands.insert(key, cmd);
            }
        }
    }

    fn parse_command_file(path: &Path) -> Option<CustomCommand> {
        // Skip oversized command files to avoid high memory usage at startup.
        const MAX_COMMAND_FILE_SIZE: u64 = 1 << 20; // 1 MiB
        if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_COMMAND_FILE_SIZE {
            return None;
        }
        let content = std::fs::read_to_string(path).ok()?;
        let (frontmatter, template) = Self::split_frontmatter(&content)?;
        let name = Self::extract_field(&frontmatter, "name")?;
        let description = Self::extract_field(&frontmatter, "description")
            .unwrap_or_else(|| format!("Custom command: {}", name));
        let args_str = Self::extract_field(&frontmatter, "args").unwrap_or_else(|| "none".into());
        let args_requirement = match args_str.as_str() {
            "required" => ArgsRequirement::Required,
            "optional" => ArgsRequirement::Optional,
            _ => ArgsRequirement::None,
        };
        Some(CustomCommand {
            name,
            description,
            args_requirement,
            template: template.trim().to_string(),
            source: path.to_path_buf(),
            namespace: None,
        })
    }

    /// Split `---\n..frontmatter..\n---\nbody` into (frontmatter, body).
    /// Returns `None` when the content doesn't start with `---`.
    fn split_frontmatter(content: &str) -> Option<(String, String)> {
        let content = content.trim();
        if !content.starts_with("---") {
            return None;
        }
        let rest = &content[3..];
        let end = rest.find("---")?;
        Some((rest[..end].trim().to_string(), rest[end + 3..].to_string()))
    }

    /// Extract a `key: value` field from frontmatter text. Handles leading
    /// whitespace but not quoted values — keeps parsing minimal.
    fn extract_field(frontmatter: &str, key: &str) -> Option<String> {
        for line in frontmatter.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(key) {
                if let Some(value) = rest.trim_start().strip_prefix(':') {
                    return Some(value.trim().to_string());
                }
            }
        }
        None
    }

    /// Look up a command by name.
    pub fn get(&self, name: &str) -> Option<&CustomCommand> {
        self.commands.get(name)
    }

    /// Resolve `name` to a command: by exact key first (`name` or
    /// `plugin:name`), then falling back to a **bare-name** match. Plugin
    /// commands are keyed `plugin:name` but listed/typed as just `name` — so
    /// `/wechat` resolves to `weixin:wechat` when that bare name is unique.
    /// Ambiguous bare names (same command name in two plugins) require the
    /// full `plugin:name`.
    ///
    /// This is the single source of truth for "is `name` a known custom
    /// command?" — both the TUI submit gate (which decides whether to treat
    /// `/name` as a slash command vs. a chat message) and [`render`] go
    /// through it, so they can never disagree. Using exact-key [`get`] in the
    /// gate while rendering via the bare-name path here was the bug that sent
    /// `/wechat` to the agent as plain text.
    pub fn resolve(&self, name: &str) -> Option<&CustomCommand> {
        self.commands.get(name).or_else(|| {
            let mut matches = self.commands.values().filter(|c| c.name == name);
            let first = matches.next();
            match matches.next() {
                None => first,   // unique bare-name match
                Some(_) => None, // ambiguous → require plugin:name
            }
        })
    }

    /// Render the template for `name`, replacing `$ARGUMENTS` /
    /// `${ARGUMENTS}` with the provided args string. Name resolution follows
    /// [`resolve`].
    ///
    /// Note: this method does NOT validate `args_requirement` — it always
    /// performs the substitution. Empty-arg validation for `Required` happens
    /// in the dispatch layer (`execute_slash_command_impl`).
    pub fn render(&self, name: &str, args: &str) -> Option<String> {
        let cmd = self.resolve(name)?;
        Some(cmd.render(args))
    }

    /// All commands, sorted by name.
    pub fn list(&self) -> Vec<&CustomCommand> {
        let mut cmds: Vec<_> = self.commands.values().collect();
        cmds.sort_by_key(|c| &c.name);
        cmds
    }

    /// `(name, description)` pairs for every registered custom command,
    /// sorted by name. Convenient for feeding into completion / menu builders.
    pub fn command_names_and_descriptions(&self) -> Vec<(String, String)> {
        self.list()
            .iter()
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_works() {
        let content = "---\nname: review\ndescription: Code review\n---\nTemplate body here";
        let (fm, body) = CustomCommandRegistry::split_frontmatter(content).unwrap();
        assert!(fm.contains("name: review"));
        assert!(fm.contains("description: Code review"));
        assert_eq!(body.trim(), "Template body here");
    }

    #[test]
    fn split_frontmatter_returns_none_without_delimiters() {
        assert!(CustomCommandRegistry::split_frontmatter("no frontmatter here").is_none());
        assert!(CustomCommandRegistry::split_frontmatter("--- only opening").is_none());
    }

    #[test]
    fn extract_field_works() {
        let fm = "name: review\ndescription: Code review\nargs: optional";
        assert_eq!(
            CustomCommandRegistry::extract_field(fm, "name"),
            Some("review".into())
        );
        assert_eq!(
            CustomCommandRegistry::extract_field(fm, "description"),
            Some("Code review".into())
        );
        assert_eq!(
            CustomCommandRegistry::extract_field(fm, "args"),
            Some("optional".into())
        );
        assert_eq!(CustomCommandRegistry::extract_field(fm, "missing"), None);
    }

    #[test]
    fn parse_command_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(
            &path,
            "---\nname: review\ndescription: Code review\nargs: optional\n---\nReview: $ARGUMENTS",
        )
        .unwrap();
        let cmd = CustomCommandRegistry::parse_command_file(&path).unwrap();
        assert_eq!(cmd.name, "review");
        assert_eq!(cmd.description, "Code review");
        assert_eq!(cmd.args_requirement, ArgsRequirement::Optional);
        assert_eq!(cmd.template, "Review: $ARGUMENTS");
        assert_eq!(cmd.source, path);
    }

    #[test]
    fn parse_command_file_skips_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.md");
        // Create a file larger than 1 MiB (1_048_576 bytes).
        let padding = "x".repeat(1_048_577);
        let huge_content = format!("---\nname: huge\ndescription: Huge\n---\n{}", padding);
        std::fs::write(&path, huge_content).unwrap();
        assert!(CustomCommandRegistry::parse_command_file(&path).is_none());
    }

    #[test]
    fn render_replaces_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let cmd_dir = dir.path().join(".atomcode/commands");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::write(
            cmd_dir.join("review.md"),
            "---\nname: review\ndescription: Review\nargs: optional\n---\nReview $ARGUMENTS and ${ARGUMENTS} done",
        )
        .unwrap();
        let reg = CustomCommandRegistry::load(dir.path());
        let rendered = reg.render("review", "main.rs").unwrap();
        assert_eq!(rendered, "Review main.rs and main.rs done");
    }

    #[test]
    fn render_empty_args() {
        let dir = tempfile::tempdir().unwrap();
        let cmd_dir = dir.path().join(".atomcode/commands");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::write(
            cmd_dir.join("test.md"),
            "---\nname: test\ndescription: Run tests\n---\nRun all tests. Focus: $ARGUMENTS",
        )
        .unwrap();
        let reg = CustomCommandRegistry::load(dir.path());
        let rendered = reg.render("test", "").unwrap();
        assert_eq!(rendered, "Run all tests. Focus: ");
    }

    #[test]
    fn load_from_dir_skips_non_md() {
        let dir = tempfile::tempdir().unwrap();
        let cmd_dir = dir.path().join("commands");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        // Valid .md file
        std::fs::write(
            cmd_dir.join("valid.md"),
            "---\nname: valid\ndescription: Valid cmd\n---\nTemplate",
        )
        .unwrap();
        // Non-md file — should be skipped
        std::fs::write(
            cmd_dir.join("skip.txt"),
            "---\nname: skip\ndescription: Skip\n---\nNope",
        )
        .unwrap();
        // No extension — should be skipped
        std::fs::write(
            cmd_dir.join("noext"),
            "---\nname: noext\ndescription: No ext\n---\nNope",
        )
        .unwrap();
        let mut commands = HashMap::new();
        CustomCommandRegistry::load_from_dir(&cmd_dir, None, &mut commands);
        assert_eq!(commands.len(), 1);
        assert!(commands.contains_key("valid"));
    }

    #[test]
    fn project_overrides_global_same_name() {
        let root = tempfile::tempdir().unwrap();

        // Simulate global dir
        let global_dir = root.path().join("global_commands");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("review.md"),
            "---\nname: review\ndescription: Global review\n---\nGlobal template",
        )
        .unwrap();

        // Simulate project dir
        let project_dir = root.path().join("project_commands");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("review.md"),
            "---\nname: review\ndescription: Project review\n---\nProject template",
        )
        .unwrap();

        let mut commands = HashMap::new();
        // Load global first, then project — project should override.
        CustomCommandRegistry::load_from_dir(&global_dir, None, &mut commands);
        CustomCommandRegistry::load_from_dir(&project_dir, None, &mut commands);

        let cmd = commands.get("review").unwrap();
        assert_eq!(cmd.description, "Project review");
        assert_eq!(cmd.template, "Project template");
    }

    #[test]
    #[serial_test::serial]
    fn list_returns_sorted_commands() {
        // Isolate $ATOMCODE_HOME so the global commands dir and any installed
        // plugin commands on the dev machine don't leak into the assertion.
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());

        let dir = tempfile::tempdir().unwrap();
        let cmd_dir = dir.path().join(".atomcode/commands");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::write(
            cmd_dir.join("zebra.md"),
            "---\nname: zebra\ndescription: Z\n---\nZ",
        )
        .unwrap();
        std::fs::write(
            cmd_dir.join("alpha.md"),
            "---\nname: alpha\ndescription: A\n---\nA",
        )
        .unwrap();
        let reg = CustomCommandRegistry::load(dir.path());
        let names: Vec<_> = reg.list().iter().map(|c| c.name.as_str()).collect();

        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn empty_registry_has_no_commands() {
        let reg = CustomCommandRegistry::empty();
        assert!(reg.list().is_empty());
        assert!(reg.get("anything").is_none());
        assert!(reg.render("anything", "").is_none());
    }

    #[test]
    fn parse_file_without_frontmatter_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.md");
        std::fs::write(&path, "No frontmatter here, just text.").unwrap();
        assert!(CustomCommandRegistry::parse_command_file(&path).is_none());
    }

    #[test]
    fn parse_file_without_name_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noname.md");
        std::fs::write(&path, "---\ndescription: Missing name field\n---\nTemplate").unwrap();
        assert!(CustomCommandRegistry::parse_command_file(&path).is_none());
    }

    #[test]
    fn default_description_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodesc.md");
        std::fs::write(&path, "---\nname: nodesc\n---\nTemplate").unwrap();
        let cmd = CustomCommandRegistry::parse_command_file(&path).unwrap();
        assert_eq!(cmd.description, "Custom command: nodesc");
    }

    #[test]
    fn default_args_requirement_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noargs.md");
        std::fs::write(&path, "---\nname: noargs\n---\nTemplate").unwrap();
        let cmd = CustomCommandRegistry::parse_command_file(&path).unwrap();
        assert_eq!(cmd.args_requirement, ArgsRequirement::None);
    }

    #[test]
    #[serial_test::serial]
    fn load_plugin_layer_namespaces_commands() {
        // Set up an installed plugin on disk via the plugin module's state.
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());

        let plugin_dir = tmp.path().join("plugins/marketplaces/p");
        let cmd_dir = plugin_dir.join("commands");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::write(
            cmd_dir.join("greet.md"),
            "---\nname: greet\ndescription: hi\n---\nhello $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("plugins/installed_plugins.json"),
            r#"{"version":1,"plugins":{"p@p":{"marketplace":"p","plugin":"p","plugin_dir":"marketplaces/p","installed_at":"x"}}}"#,
        )
        .unwrap();

        let working = tempfile::tempdir().unwrap();
        let reg = CustomCommandRegistry::load(working.path());
        assert!(reg.get("p:greet").is_some());
    }
    #[test]
    fn render_resolves_bare_name_for_namespaced_command() {
        let mut reg = CustomCommandRegistry::empty();
        reg.commands.insert(
            "weixin:wechat".into(),
            CustomCommand {
                name: "wechat".into(),
                description: "d".into(),
                args_requirement: ArgsRequirement::Optional,
                template: "do $ARGUMENTS".into(),
                source: std::path::PathBuf::from("x"),
                namespace: Some("weixin".into()),
            },
        );
        assert_eq!(reg.render("wechat", "login").as_deref(), Some("do login"));
        assert_eq!(reg.render("weixin:wechat", "").as_deref(), Some("do "));
        assert!(reg.render("nope", "").is_none());
    }

    #[test]
    fn resolve_bare_name_matches_render_for_gate() {
        // The TUI submit gate decides "is `/x` a slash command?" via
        // `resolve()`, while dispatch renders via `render()`. They MUST
        // agree, or a plugin command typed as `/wechat` (keyed
        // `weixin:wechat`) passes neither/one and gets sent to the agent
        // as plain text. `get()` is exact-key only and intentionally does
        // NOT do bare-name fallback — so the gate must use `resolve()`.
        let mut reg = CustomCommandRegistry::empty();
        reg.commands.insert(
            "weixin:wechat".into(),
            CustomCommand {
                name: "wechat".into(),
                description: "d".into(),
                args_requirement: ArgsRequirement::Optional,
                template: "do $ARGUMENTS".into(),
                source: std::path::PathBuf::from("x"),
                namespace: Some("weixin".into()),
            },
        );
        // The asymmetry that caused the bug: exact-key lookup misses.
        assert!(reg.get("wechat").is_none());
        // resolve() bridges it via the same bare-name fallback render() uses.
        assert!(reg.resolve("wechat").is_some());
        assert!(reg.resolve("weixin:wechat").is_some());
        assert!(reg.resolve("nope").is_none());
        // resolve() and render() must agree on what is a known command.
        assert_eq!(
            reg.resolve("wechat").is_some(),
            reg.render("wechat", "").is_some()
        );
    }

    #[test]
    fn render_bare_name_ambiguous_returns_none() {
        let mut reg = CustomCommandRegistry::empty();
        for ns in ["a", "b"] {
            reg.commands.insert(
                format!("{ns}:wechat"),
                CustomCommand {
                    name: "wechat".into(),
                    description: "d".into(),
                    args_requirement: ArgsRequirement::None,
                    template: "t".into(),
                    source: std::path::PathBuf::from("x"),
                    namespace: Some(ns.into()),
                },
            );
        }
        assert!(reg.render("wechat", "").is_none());
        assert!(reg.render("a:wechat", "").is_some());
    }
}
