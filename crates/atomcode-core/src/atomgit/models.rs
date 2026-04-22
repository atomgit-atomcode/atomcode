//! JSON shapes returned by AtomGit's issue endpoints. Fields we don't use
//! are omitted — serde silently ignores unknown keys.

use serde::{Deserialize, Deserializer};

/// AtomGit returns numeric IDs as JSON strings (e.g. `"number": "140"`).
/// Accept both shapes so we don't brittle-fail on a server-side type change.
pub(crate) fn deserialize_u64_from_string_or_int<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Int(u64),
        Str(String),
    }
    match StringOrInt::deserialize(deserializer)? {
        StringOrInt::Int(n) => Ok(n),
        StringOrInt::Str(s) => s
            .parse::<u64>()
            .map_err(|_| serde::de::Error::custom(format!("not a u64: {:?}", s))),
    }
}

/// Shape returned by `GET /repos/{owner}/{repo}/labels` — the repo's
/// label definitions. We only keep `id` (needed to attach to an issue)
/// and `name` (for matching by user-visible name).
#[derive(Debug, Deserialize, Clone)]
pub struct RepoLabel {
    #[serde(deserialize_with = "deserialize_u64_from_string_or_int")]
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Issue {
    #[serde(deserialize_with = "deserialize_u64_from_string_or_int")]
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
    /// Primary assignee. Can be null if nobody is assigned.
    #[serde(default)]
    pub assignee: Option<User>,
    /// Multi-assignee field (Gitea compat). Some deployments return just
    /// `assignee`; others populate both — we check both.
    #[serde(default)]
    pub assignees: Vec<User>,
    #[serde(default)]
    pub labels: Vec<Label>,
}

impl Issue {
    /// True if `username` is in either `assignee` or `assignees[]`.
    pub fn is_assigned_to(&self, username: &str) -> bool {
        if let Some(a) = &self.assignee {
            if a.login == username {
                return true;
            }
        }
        self.assignees.iter().any(|a| a.login == username)
    }

    /// Human-readable list of all assignees. "(unassigned)" when empty.
    pub fn assignee_list(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        if let Some(a) = &self.assignee {
            names.push(&a.login);
        }
        for a in &self.assignees {
            if !names.iter().any(|n| *n == a.login) {
                names.push(&a.login);
            }
        }
        if names.is_empty() {
            "(unassigned)".to_string()
        } else {
            names.join(", ")
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub body: Option<String>,
}

/// Slimmed response shape for `POST /repos/{owner}/{repo}/issues`. We only
/// use `number` (for logging) and `html_url` (to show the user where the
/// new issue lives); everything else the API returns is discarded.
#[derive(Debug, Deserialize)]
pub struct CreatedIssue {
    #[serde(deserialize_with = "deserialize_u64_from_string_or_int")]
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub html_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_number_as_string() {
        // AtomGit's current behavior.
        let raw = r#"{"number":"140","title":"t","state":"open"}"#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(issue.number, 140);
    }

    #[test]
    fn parses_number_as_int() {
        // Defensive: works even if AtomGit switches to numeric.
        let raw = r#"{"number":42,"title":"t","state":"open"}"#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert_eq!(issue.number, 42);
    }

    #[test]
    fn is_assigned_to_checks_both_fields() {
        let raw = r#"{"number":1,"title":"t","state":"open","assignee":{"login":"alice"},"assignees":[{"login":"bob"}]}"#;
        let issue: Issue = serde_json::from_str(raw).unwrap();
        assert!(issue.is_assigned_to("alice"));
        assert!(issue.is_assigned_to("bob"));
        assert!(!issue.is_assigned_to("carol"));
    }
}
