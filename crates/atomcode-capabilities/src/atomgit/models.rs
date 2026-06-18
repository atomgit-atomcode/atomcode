//! JSON shapes returned by AtomGit's repo / pull-request / issue endpoints. Unknown
//! keys are ignored by serde. `number`/`id` fields can arrive as JSON strings OR ints
//! (AtomGit stringifies some numerics) — [`de_u64_flex`] accepts both.

use serde::{Deserialize, Deserializer};

/// Deserialize a `u64` from either a JSON number or a numeric string.
pub(crate) fn de_u64_flex<'de, D>(deserializer: D) -> Result<u64, D::Error>
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
            .map_err(|_| serde::de::Error::custom(format!("not a u64: {s:?}"))),
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct User {
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Repo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub owner: User,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Branch {
    #[serde(default, rename = "ref")]
    pub ref_: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PullRequest {
    #[serde(deserialize_with = "de_u64_flex")]
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub head: Branch,
    #[serde(default)]
    pub base: Branch,
    #[serde(default)]
    pub merged: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Issue {
    #[serde(deserialize_with = "de_u64_flex")]
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub user: User,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Comment {
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub id: u64,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub html_url: String,
}

/// `POST .../tags` (create-tag) response. AtomGit's shape is loose, so every field
/// is optional; `name` is accepted as an alias for `tag_name`. The tool falls back to
/// the requested name when the response omits it.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct Tag {
    #[serde(default, alias = "name")]
    pub tag_name: String,
    #[serde(default, alias = "tag_message")]
    pub message: String,
}

/// `POST .../comments` returns an `id` that AtomGit sends as a string.
#[derive(Debug, Deserialize, Clone)]
pub struct CreatedComment {
    #[serde(default, deserialize_with = "de_u64_flex")]
    pub id: u64,
    #[serde(default)]
    pub html_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_number_as_string_or_int() {
        let a: PullRequest = serde_json::from_str(r#"{"number":"7","title":"t","state":"open"}"#).unwrap();
        assert_eq!(a.number, 7);
        let b: PullRequest = serde_json::from_str(r#"{"number":42,"title":"t","state":"open"}"#).unwrap();
        assert_eq!(b.number, 42);
    }

    #[test]
    fn repo_owner_and_branch_ref_parse() {
        let r: Repo = serde_json::from_str(
            r#"{"name":"x","full_name":"o/x","html_url":"u","private":true,"owner":{"login":"o"}}"#,
        )
        .unwrap();
        assert_eq!(r.owner.login, "o");
        assert!(r.private);

        let pr: PullRequest = serde_json::from_str(
            r#"{"number":1,"head":{"ref":"feat"},"base":{"ref":"main"}}"#,
        )
        .unwrap();
        assert_eq!(pr.head.ref_, "feat");
        assert_eq!(pr.base.ref_, "main");
    }

    #[test]
    fn missing_optional_fields_default() {
        let i: Issue = serde_json::from_str(r#"{"number":3}"#).unwrap();
        assert_eq!(i.number, 3);
        assert_eq!(i.title, "");
        assert_eq!(i.user.login, "");

        let c: Comment = serde_json::from_str(r#"{"body":"hi"}"#).unwrap();
        assert_eq!(c.body, "hi");
        assert_eq!(c.id, 0);
    }
}
