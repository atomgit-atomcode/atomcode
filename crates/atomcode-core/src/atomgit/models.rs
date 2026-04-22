//! JSON shapes returned by AtomGit's issue endpoints. Fields we don't use
//! are omitted — serde silently ignores unknown keys.

use serde::Deserialize;

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
