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

/// Coerce a JSON value into a label list, tolerating every shape AtomGit/GitCode
/// might send (the wire shape is unconfirmed — E2E is blocked): an array of
/// strings, an array of `{ "name": "..." }` objects, `null`, or anything else.
/// A non-array (or absent, via serde `default`) collapses to an empty list.
///
/// Presence of the *field* is a separate question from its contents — callers
/// that must not clobber labels (see [`AtomgitClient::repo_labels`]) inspect the
/// raw key themselves rather than relying on this lossy conversion.
pub(crate) fn project_labels_from_json(v: &serde_json::Value) -> Vec<String> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| match e {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(o) => {
                o.get("name").and_then(|n| n.as_str()).map(str::to_string)
            }
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Tolerant deserializer for `project_labels`. Delegates to
/// [`project_labels_from_json`] so the model path and the read-back path share
/// one coercion. Absent (via serde `default`) → empty.
fn de_project_labels<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(project_labels_from_json(&v))
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
    #[serde(default, deserialize_with = "de_project_labels")]
    pub project_labels: Vec<String>,
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
mod label_shape_tests {
    use super::Repo;
    #[test]
    fn project_labels_tolerates_all_wire_shapes() {
        let strs: Repo =
            serde_json::from_value(serde_json::json!({"name":"w","project_labels":["a","b"]}))
                .unwrap();
        assert_eq!(strs.project_labels, vec!["a".to_string(), "b".to_string()]);
        let objs: Repo = serde_json::from_value(
            serde_json::json!({"name":"w","project_labels":[{"name":"a"},{"name":"b"}]}),
        )
        .unwrap();
        assert_eq!(objs.project_labels, vec!["a".to_string(), "b".to_string()]);
        let nul: Repo =
            serde_json::from_value(serde_json::json!({"name":"w","project_labels":null})).unwrap();
        assert!(nul.project_labels.is_empty());
        let absent: Repo = serde_json::from_value(serde_json::json!({"name":"w"})).unwrap();
        assert!(absent.project_labels.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_number_as_string_or_int() {
        let a: PullRequest =
            serde_json::from_str(r#"{"number":"7","title":"t","state":"open"}"#).unwrap();
        assert_eq!(a.number, 7);
        let b: PullRequest =
            serde_json::from_str(r#"{"number":42,"title":"t","state":"open"}"#).unwrap();
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

        let pr: PullRequest =
            serde_json::from_str(r#"{"number":1,"head":{"ref":"feat"},"base":{"ref":"main"}}"#)
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
