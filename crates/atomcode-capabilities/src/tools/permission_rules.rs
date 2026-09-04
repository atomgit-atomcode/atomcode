//! `PermissionRuleGate` — USER-DECLARED pre-authorization, the missing middle between
//! "approve every call" and `--dangerously-skip-permissions` (approve nothing, ever).
//!
//! ## Why
//!
//! Every existing gate decides from the CALL (is this command destructive? does the target
//! escape the workspace?). None of them lets the user say up front "`git *` is fine in this
//! repo, stop asking". Worse, the one grant users CAN express — the approval panel's
//! "Always" — is scoped PER COMMAND for `bash` (see [`BashTool::always_grant_scope`]), so a
//! command that differs by one argument re-prompts. The only escape hatch was the global
//! `--dangerously-skip-permissions`, which turns off every gate at once.
//!
//! This gate reads a user-declared allow/deny list and answers BEFORE the convenience gates
//! and the generic approval prompt, so a matched `allow` rule runs the call with no prompt
//! and a matched `deny` rule blocks it outright.
//!
//! ## Rule syntax (Claude Code compatible)
//!
//! | rule | matches |
//! |---|---|
//! | `Bash` | every `bash` call |
//! | `Bash(git status)` | exactly that command (whitespace-normalized) |
//! | `Bash(git *)` | any command starting `git ` |
//! | `Bash(npm run test:*)` | CC's trailing `:*` prefix form — same as `npm run test*` |
//! | `Read(~/.zshrc)` | `read_file` on that path (`~` expanded, relative joined to cwd) |
//! | `Edit(src/**)` | `edit_file` under `src/` |
//! | `mcp__server__tool` | that MCP tool |
//! | `*` | every tool (only meaningful in `deny`) |
//!
//! Tool names are matched case-insensitively and `_`-insensitively, and CC's short names are
//! aliased to our wire names (`Read`→`read_file`, `Edit`→`edit_file`, `Write`→`write_file`,
//! `LS`→`list_directory`, `MultiEdit`→`parallel_edit_files`), so a `permissions` block copied
//! from a Claude Code `settings.json` works unchanged.
//!
//! In a pattern, `*` matches any run of characters INCLUDING `/` (`**` is accepted and means
//! the same). This is deliberate: a shell command is not a path, and `Bash(python3 *)` must
//! match `python3 /abs/path/x.py`.
//!
//! ## Precedence and the hard floor
//!
//! `deny` is checked first and wins. An `allow` NEVER applies to a call whose arguments
//! reference a sensitive path ([`references_sensitive_path`]) — a broad `Bash(rm *)` must not
//! silently delete `~/.ssh/id_rsa`. Such a call falls through to the normal gates and still
//! prompts. `deny` is unconditional.
//!
//! ## Ordering
//!
//! Register AFTER the hard boundaries (turn policy, plan mode, `CredentialBashGate`,
//! `SensitivePathGate`, CC `PreToolUse` hooks) and BEFORE the convenience gates
//! ([`OpenFileWorkspaceGate`](super::open_file::OpenFileWorkspaceGate),
//! [`WriteApprovalGate`](super::write_approval::WriteApprovalGate),
//! [`BashWorkspaceGate`](super::bash_workspace_gate::BashWorkspaceGate)) and
//! [`ApprovalMiddleware`](super::approval::ApprovalMiddleware) — a user rule may skip a
//! PROMPT, never a security boundary.
//!
//! [`BashTool::always_grant_scope`]: super::bash::BashTool

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};

use super::bash::normalize_command_for_grant;
use super::resolve_path;
use super::sensitive_path::references_sensitive_path;

/// What the rule set says about one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleDecision {
    /// A `deny` rule matched — block the call.
    Deny,
    /// An `allow` rule matched — run it without prompting.
    Allow,
    /// No rule matched — defer to the normal approval flow.
    NoMatch,
}

/// Canonicalize a tool name for comparison: lowercase, `_` removed, then CC's short names
/// folded onto our wire names. So `Read` / `read_file` / `ReadFile` all compare equal.
fn canonical_tool(name: &str) -> String {
    let squashed: String = name
        .chars()
        .filter(|c| *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect();
    match squashed.as_str() {
        "read" => "readfile".to_string(),
        "write" => "writefile".to_string(),
        "edit" => "editfile".to_string(),
        "ls" | "list" => "listdirectory".to_string(),
        "multiedit" | "paralleledit" => "paralleleditfiles".to_string(),
        "shell" | "terminal" => "bash".to_string(),
        "todo" => "todowrite".to_string(),
        _ => squashed,
    }
}

/// Glob-ish match where `*` spans ANY characters (`/` included) and `?` is exactly one.
/// `**` is accepted and behaves as `*`. Everything else is literal.
///
/// Iterative backtracking (no recursion): a pathological pattern from a config file must not
/// blow the stack in the tool path.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Last `*` seen and the text position to resume from when a branch fails.
    let (mut star, mut resume) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            // Collapse a `**` (or longer) run into one wildcard.
            while pi < p.len() && p[pi] == '*' {
                pi += 1;
            }
            star = pi;
            resume = ti;
            if pi == p.len() {
                return true; // trailing `*` swallows the rest
            }
        } else if star != usize::MAX {
            pi = star;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// One parsed rule: a tool plus an optional argument pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRule {
    /// Canonicalized tool name, or `*` for every tool.
    tool: String,
    /// `None` ⇒ the bare `Tool` form: matches any arguments.
    pattern: Option<String>,
}

impl PermissionRule {
    /// Parse one `Tool` / `Tool(pattern)` rule. Returns `None` for a malformed rule
    /// (empty, or an unbalanced paren) so the caller can report it instead of silently
    /// applying something the user did not write.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let Some(open) = raw.find('(') else {
            return Some(Self {
                tool: canonical_tool(raw),
                pattern: None,
            });
        };
        if !raw.ends_with(')') {
            return None;
        }
        let tool = raw[..open].trim();
        if tool.is_empty() {
            return None;
        }
        let pattern = raw[open + 1..raw.len() - 1].trim();
        if pattern.is_empty() {
            return None;
        }
        Some(Self {
            tool: canonical_tool(tool),
            pattern: Some(pattern.to_string()),
        })
    }

    fn matches(&self, tool: &str, targets: &[String]) -> bool {
        if self.tool != "*" && self.tool != canonical_tool(tool) {
            return false;
        }
        let Some(pattern) = &self.pattern else {
            return true; // bare `Tool` — any arguments
        };
        // CC spells a bash prefix rule `npm run test:*`; the `:` is its separator, not a
        // literal character of the command. Accept both spellings.
        let pattern = match pattern.strip_suffix(":*") {
            Some(prefix) => format!("{prefix}*"),
            None => pattern.clone(),
        };
        targets.iter().any(|t| wildcard_match(&pattern, t))
    }
}

/// A parsed `allow` / `deny` rule set. Cheap to consult; built once per assembly.
#[derive(Clone, Debug, Default)]
pub struct PermissionRules {
    allow: Vec<PermissionRule>,
    deny: Vec<PermissionRule>,
}

impl PermissionRules {
    /// Parse both lists. Malformed rules are SKIPPED and returned as the second element so
    /// the caller can warn — a typo must never silently widen or narrow the policy.
    pub fn parse(allow: &[String], deny: &[String]) -> (Self, Vec<String>) {
        let mut invalid = Vec::new();
        let mut parse_list = |list: &[String]| -> Vec<PermissionRule> {
            list.iter()
                .filter_map(|raw| match PermissionRule::parse(raw) {
                    Some(rule) => Some(rule),
                    None => {
                        invalid.push(raw.clone());
                        None
                    }
                })
                .collect()
        };
        let deny = parse_list(deny);
        let allow = parse_list(allow);
        (Self { allow, deny }, invalid)
    }

    /// No rules at all — the gate can be left unregistered.
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// Decide one call. `deny` wins; `allow` is suppressed for sensitive-path arguments.
    pub fn decide(&self, tool: &str, args: &str, cwd: &Path) -> RuleDecision {
        if self.is_empty() {
            return RuleDecision::NoMatch;
        }
        let targets = match_targets(tool, args, cwd);
        if self.deny.iter().any(|r| r.matches(tool, &targets)) {
            return RuleDecision::Deny;
        }
        if self.allow.iter().any(|r| r.matches(tool, &targets)) {
            // HARD FLOOR: a user allow-rule pre-authorizes convenience, never a secret. A
            // sensitive target falls through so the normal gates still prompt.
            if references_sensitive_path(args) {
                return RuleDecision::NoMatch;
            }
            return RuleDecision::Allow;
        }
        RuleDecision::NoMatch
    }
}

/// The strings a rule pattern is matched against for this call. `bash` matches on its
/// COMMAND (normalized like the approval grant key, so a cosmetic re-emit still matches);
/// every other tool matches on its path-ish argument, both as written and resolved, so
/// `Read(~/.zshrc)` and `Read(/Users/me/.zshrc)` both work.
fn match_targets(tool: &str, args: &str, cwd: &Path) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return Vec::new();
    };
    // The shell-executing tools match on their COMMAND. `bash_start` backgrounds a command
    // that is exactly as dangerous as a foreground one, so a rule must reach it too —
    // `command` is not a path key, so without this arm a `BashStart(rm *)` pattern silently
    // matched NOTHING, and a deny rule that never fires is worse than no rule at all.
    if matches!(canonical_tool(tool).as_str(), "bash" | "bashstart") {
        return value
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| vec![normalize_command_for_grant(c)])
            .unwrap_or_default();
    }
    let mut targets = Vec::new();
    for raw in super::target_arg_values(args) {
        let resolved = resolve_path(&raw, cwd).to_string_lossy().to_string();
        if resolved != raw {
            targets.push(resolved);
        }
        targets.push(raw);
    }
    targets
}

/// Middleware form of [`PermissionRules`]. Clone-cheap (Arc-backed).
pub struct PermissionRuleGate {
    rules: Arc<PermissionRules>,
    /// LIVE working dir — the same handle the other gates read, so a `/cd` moves the
    /// baseline a relative path rule resolves against.
    cwd: Arc<RwLock<PathBuf>>,
}

impl PermissionRuleGate {
    pub fn new(rules: Arc<PermissionRules>, cwd: Arc<RwLock<PathBuf>>) -> Self {
        Self { rules, cwd }
    }

    /// Gate over a FIXED workspace root (tests / assemblies with an immutable working dir).
    pub fn pinned(rules: PermissionRules, root: PathBuf) -> Self {
        Self::new(Arc::new(rules), Arc::new(RwLock::new(root)))
    }
}

#[async_trait]
impl ToolMiddleware for PermissionRuleGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        // A poisoned cwd lock only costs relative-path resolution; match on the raw
        // argument rather than failing the call.
        let cwd = self
            .cwd
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        match self.rules.decide(tool.name(), &call.arguments, &cwd) {
            RuleDecision::Allow => BeforeOutcome::Allow {
                reason: Some(format!(
                    "pre-authorized by a [permissions] allow rule for '{}'",
                    tool.name()
                )),
            },
            RuleDecision::Deny => BeforeOutcome::deny(format!(
                "blocked by a [permissions] deny rule for '{}'",
                tool.name()
            )),
            RuleDecision::NoMatch => BeforeOutcome::Proceed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        PathBuf::from("/work")
    }

    fn rules(allow: &[&str], deny: &[&str]) -> PermissionRules {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        let deny: Vec<String> = deny.iter().map(|s| s.to_string()).collect();
        let (rules, invalid) = PermissionRules::parse(&allow, &deny);
        assert!(invalid.is_empty(), "unexpected invalid rules: {invalid:?}");
        rules
    }

    fn bash_args(cmd: &str) -> String {
        serde_json::json!({ "command": cmd }).to_string()
    }

    #[test]
    fn wildcard_star_crosses_slashes() {
        // The whole point: a command argument is not a path segment.
        assert!(wildcard_match("python3 *", "python3 /home/u/x.py"));
        assert!(wildcard_match("git *", "git commit -m msg"));
        assert!(!wildcard_match("git *", "npm run git"));
        assert!(wildcard_match("*", "anything at all"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
        assert!(wildcard_match("src/**/*.rs", "src/a/b/c.rs"));
        assert!(wildcard_match("exact", "exact"));
        assert!(!wildcard_match("exact", "exactly"));
    }

    /// A backtracking matcher must not go exponential (or blow the stack) on the classic
    /// adversarial pattern — this comes from a user-editable config file.
    #[test]
    fn wildcard_match_handles_pathological_pattern() {
        let pattern = "a*a*a*a*a*a*a*a*b";
        let text = "a".repeat(64);
        assert!(!wildcard_match(pattern, &text));
    }

    #[test]
    fn bare_tool_rule_matches_any_args() {
        let r = rules(&["Bash"], &[]);
        assert_eq!(
            r.decide("bash", &bash_args("rm -rf build"), &cwd()),
            RuleDecision::Allow
        );
    }

    #[test]
    fn command_pattern_scopes_the_grant() {
        let r = rules(&["Bash(git *)"], &[]);
        assert_eq!(
            r.decide("bash", &bash_args("git status"), &cwd()),
            RuleDecision::Allow
        );
        assert_eq!(
            r.decide("bash", &bash_args("rm -rf /"), &cwd()),
            RuleDecision::NoMatch
        );
    }

    /// The exact gap this feature closes: the approval panel's "Always" is per-command, so
    /// a second, slightly different command re-prompts. One rule covers the whole family.
    #[test]
    fn one_rule_covers_a_command_family() {
        let r = rules(&["Bash(python3 *)"], &[]);
        for cmd in [
            "python3 a.py",
            "python3 -c 'print(1)'",
            "python3 /home/u/下载/b.py",
        ] {
            assert_eq!(
                r.decide("bash", &bash_args(cmd), &cwd()),
                RuleDecision::Allow,
                "{cmd}"
            );
        }
    }

    #[test]
    fn cc_colon_star_prefix_form_is_accepted() {
        let r = rules(&["Bash(npm run test:*)"], &[]);
        assert_eq!(
            r.decide("bash", &bash_args("npm run test -- --watch"), &cwd()),
            RuleDecision::Allow
        );
    }

    #[test]
    fn deny_beats_allow() {
        let r = rules(&["Bash"], &["Bash(rm *)"]);
        assert_eq!(
            r.decide("bash", &bash_args("ls"), &cwd()),
            RuleDecision::Allow
        );
        assert_eq!(
            r.decide("bash", &bash_args("rm -rf build"), &cwd()),
            RuleDecision::Deny
        );
    }

    /// HARD FLOOR: a broad allow rule must not become a silent credential exfiltration path.
    #[test]
    fn allow_never_applies_to_a_sensitive_target() {
        let r = rules(&["Bash", "Read"], &[]);
        assert_eq!(
            r.decide("bash", &bash_args("cat ~/.ssh/id_rsa"), &cwd()),
            RuleDecision::NoMatch
        );
        let args = serde_json::json!({ "file_path": "/home/u/.ssh/id_rsa" }).to_string();
        assert_eq!(r.decide("read_file", &args, &cwd()), RuleDecision::NoMatch);
    }

    /// …but a deny rule still applies to one (deny is unconditional).
    #[test]
    fn deny_still_applies_to_a_sensitive_target() {
        let r = rules(&[], &["Bash"]);
        assert_eq!(
            r.decide("bash", &bash_args("cat ~/.ssh/id_rsa"), &cwd()),
            RuleDecision::Deny
        );
    }

    #[test]
    fn cc_short_tool_names_alias_onto_wire_names() {
        for (rule, wire) in [
            ("Read", "read_file"),
            ("Write", "write_file"),
            ("Edit", "edit_file"),
            ("LS", "list_directory"),
            ("MultiEdit", "parallel_edit_files"),
        ] {
            let r = rules(&[rule], &[]);
            let args = serde_json::json!({ "file_path": "/work/x.rs" }).to_string();
            assert_eq!(r.decide(wire, &args, &cwd()), RuleDecision::Allow, "{rule}");
        }
    }

    /// The TUI hands the approval panel a PascalCase display name; rules must match either
    /// spelling so a rule written as `ReadFile` behaves like `Read`.
    #[test]
    fn tool_name_match_is_case_and_underscore_insensitive() {
        let r = rules(&["ReadFile"], &[]);
        let args = serde_json::json!({ "file_path": "/work/x.rs" }).to_string();
        assert_eq!(r.decide("read_file", &args, &cwd()), RuleDecision::Allow);
        assert_eq!(r.decide("Read_File", &args, &cwd()), RuleDecision::Allow);
    }

    #[test]
    fn path_rule_matches_raw_and_resolved_forms() {
        let r = rules(&["Read(/work/src/**)"], &[]);
        // Relative as written, absolute after resolution against the cwd.
        let args = serde_json::json!({ "file_path": "src/main.rs" }).to_string();
        assert_eq!(r.decide("read_file", &args, &cwd()), RuleDecision::Allow);
        let outside = serde_json::json!({ "file_path": "/etc/hosts" }).to_string();
        assert_eq!(
            r.decide("read_file", &outside, &cwd()),
            RuleDecision::NoMatch
        );
    }

    #[test]
    fn star_tool_matches_everything() {
        let r = rules(&[], &["*"]);
        assert_eq!(
            r.decide("bash", &bash_args("ls"), &cwd()),
            RuleDecision::Deny
        );
        assert_eq!(r.decide("web_fetch", "{}", &cwd()), RuleDecision::Deny);
    }

    #[test]
    fn mcp_tool_names_are_matchable() {
        let r = rules(&["mcp__playwright__navigate"], &[]);
        assert_eq!(
            r.decide("mcp__playwright__navigate", "{}", &cwd()),
            RuleDecision::Allow
        );
        assert_eq!(
            r.decide("mcp__playwright__click", "{}", &cwd()),
            RuleDecision::NoMatch
        );
    }

    /// A rule written for the background shell tool must actually match it. Before this,
    /// `command` was not a path key and only `bash` had a command arm, so every
    /// `BashStart(...)` PATTERN matched nothing — including deny rules.
    #[test]
    fn background_shell_tool_matches_command_rules() {
        let r = rules(&[], &["BashStart(rm *)"]);
        assert_eq!(
            r.decide("bash_start", &bash_args("rm -rf /"), &cwd()),
            RuleDecision::Deny
        );
        assert_eq!(
            r.decide("bash_start", &bash_args("git status"), &cwd()),
            RuleDecision::NoMatch
        );
        // A rule naming one shell tool does not leak onto the other.
        let only_fg = rules(&["Bash(git *)"], &[]);
        assert_eq!(
            only_fg.decide("bash_start", &bash_args("git status"), &cwd()),
            RuleDecision::NoMatch
        );
    }

    #[test]
    fn malformed_rules_are_reported_not_silently_applied() {
        let (rules, invalid) = PermissionRules::parse(
            &[
                "Bash(git *".to_string(),
                "".to_string(),
                "Bash()".to_string(),
            ],
            &[],
        );
        assert!(rules.is_empty());
        assert_eq!(invalid.len(), 3);
    }

    #[test]
    fn empty_rule_set_never_matches() {
        let r = PermissionRules::default();
        assert!(r.is_empty());
        assert_eq!(
            r.decide("bash", &bash_args("rm -rf /"), &cwd()),
            RuleDecision::NoMatch
        );
    }

    /// A normalized command matches the same way the approval grant key does, so a
    /// re-emitted command that differs only by a comment or spacing still matches.
    #[test]
    fn command_match_is_normalized_like_the_grant_key() {
        let r = rules(&["Bash(git status)"], &[]);
        assert_eq!(
            r.decide("bash", &bash_args("git   status  # check"), &cwd()),
            RuleDecision::Allow
        );
    }
}
