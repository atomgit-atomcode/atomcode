//! Writing provider accounts and model profiles into the configuration
//! *document*, key by key.
//!
//! Not `Config` → TOML. A person's configuration file has comments, an order
//! they put things in, and keys this program does not model; serialising a typed
//! `Config` over it throws all three away. So a change here is a patch: the keys
//! this panel owns are set or removed and everything else is left byte for byte
//! — the rule [`crate::settings::SettingSpec::patch`] already follows for flat
//! settings, applied to the two tables `/provider` edits.
//!
//! Deciding *what* to write stays typed and stays with the caller: which account
//! a model hangs off, whether an id is taken, whether deleting one leaves the
//! default dangling are all questions about the configuration as a value. This
//! module only knows how to put an answer into the file.

use anyhow::Result;
use toml_edit::{value, DocumentMut, Item, Table, Value};

/// Set `key` without disturbing what a person wrote around it.
///
/// `Table::insert` replaces the whole key-value pair, and in `toml_edit` a
/// comment above a key is that **key's** decoration — so an insert over an
/// existing key silently eats the comment above it. Assigning to the item
/// leaves the key, its comment and its spacing where they are. The first real
/// run of the providers panel lost the first line of a config file this way.
fn set_key(table: &mut Table, key: &str, item: Item) {
    match table.get_mut(key) {
        Some(existing) => *existing = item,
        None => {
            table.insert(key, item);
        }
    }
}

/// The keys an account row owns. Anything else already in the table — a
/// `user_agent`, a `skip_tls_verify`, something a later version added — is left
/// alone.
#[derive(Clone, Debug, Default)]
pub struct AccountPatch<'a> {
    /// The preset or protocol id this account speaks.
    pub provider: &'a str,
    /// `None` takes the key out of the file, so the preset's own default stands.
    pub base_url: Option<&'a str>,
    pub api_key: KeyWrite<'a>,
    /// `None` leaves the name alone.
    pub display_name: Option<&'a str>,
}

/// What a write does to the stored credential.
///
/// Three states rather than an `Option<&str>`, because there are three things
/// to say and an option carries the wrong two: an empty field means *keep* what
/// is stored, and taking a credential away has to **remove the key** rather
/// than write an empty one. `api_key = ""` is not "no credential" — it is a
/// credential whose value is the empty string, which stops the env-var fallback
/// and sends an empty header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyWrite<'a> {
    /// Leave whatever is in the file alone.
    #[default]
    Keep,
    /// Store this one.
    Set(&'a str),
    /// Take it out of the file.
    Clear,
}

/// The keys a model row owns.
#[derive(Clone, Debug, Default)]
pub struct ModelPatch<'a> {
    pub account: &'a str,
    pub model: &'a str,
    pub context_window: usize,
    /// `None` is "decide for me", and is written by taking the key out.
    pub supports_vision: Option<bool>,
    pub reasoning_effort: Option<&'a str>,
    /// `None` is unrestricted, and is written by taking the key out.
    pub reasoning_effort_levels: Option<&'a [String]>,
}

/// Write one account under `[provider_accounts.<id>]`, creating it if it is new.
pub fn put_account(document: &mut DocumentMut, id: &str, patch: &AccountPatch<'_>) -> Result<()> {
    let table = sub_table(document, "provider_accounts", id);
    set_key(table, "provider", value(patch.provider));
    match patch.base_url {
        Some(url) => {
            set_key(table, "base_url", value(url));
        }
        None => {
            table.remove("base_url");
        }
    }
    match patch.api_key {
        KeyWrite::Keep => {}
        KeyWrite::Set(key) => set_key(table, "api_key", value(key)),
        KeyWrite::Clear => {
            table.remove("api_key");
        }
    }
    if let Some(name) = patch.display_name {
        set_key(table, "display_name", value(name));
    }
    Ok(())
}

/// Write one model profile under `[models.<id>]`.
///
/// Only the keys a person can reach from the panel are touched: a `max_tokens`
/// or a `note` written by hand survives an edit made here, which is the whole
/// reason this is a patch and not a rewrite.
pub fn put_model(document: &mut DocumentMut, id: &str, patch: &ModelPatch<'_>) -> Result<()> {
    let table = sub_table(document, "models", id);
    set_key(table, "account", value(patch.account));
    set_key(table, "model", value(patch.model));
    set_key(table, "context_window", value(patch.context_window as i64));
    match patch.supports_vision {
        Some(can) => {
            set_key(table, "supports_vision", value(can));
        }
        None => {
            table.remove("supports_vision");
        }
    }
    match patch.reasoning_effort {
        Some(effort) => {
            set_key(table, "reasoning_effort", value(effort));
        }
        None => {
            table.remove("reasoning_effort");
        }
    }
    match patch.reasoning_effort_levels {
        Some(levels) => {
            let mut array = toml_edit::Array::new();
            for level in levels {
                array.push(Value::from(level.as_str()));
            }
            set_key(
                table,
                "reasoning_effort_levels",
                Item::Value(Value::Array(array)),
            );
        }
        None => {
            table.remove("reasoning_effort_levels");
        }
    }
    Ok(())
}

/// Change what a legacy `[providers.<id>]` entry points at, in place.
///
/// The flat table is not written *to* any more — everything added is an account
/// plus a model — but an entry that is already there is still a person's, and
/// editing one must not silently migrate it into a shape their other tools have
/// not seen. Only the keys given are touched.
pub fn patch_legacy_provider(
    document: &mut DocumentMut,
    id: &str,
    provider_type: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<()> {
    let Some(table) = existing_sub_table(document, "providers", id) else {
        anyhow::bail!("配置里没有 [providers.{id}]");
    };
    if let Some(wire) = provider_type {
        set_key(table, "type", value(wire));
    }
    if let Some(url) = base_url {
        set_key(table, "base_url", value(url));
    }
    if let Some(key) = api_key {
        set_key(table, "api_key", value(key));
    }
    Ok(())
}

/// Change what a legacy `[providers.<id>]` entry *runs*, in place.
///
/// The flat table is one row on both of the panel's lists — it is an account and
/// a model at once — so editing its model half patches the same table its
/// connection half lives in. Only the keys the model form owns are touched.
pub fn patch_legacy_model(
    document: &mut DocumentMut,
    id: &str,
    patch: &ModelPatch<'_>,
) -> Result<()> {
    let Some(table) = existing_sub_table(document, "providers", id) else {
        anyhow::bail!("配置里没有 [providers.{id}]");
    };
    set_key(table, "model", value(patch.model));
    set_key(table, "context_window", value(patch.context_window as i64));
    match patch.supports_vision {
        Some(can) => {
            set_key(table, "supports_vision", value(can));
        }
        None => {
            table.remove("supports_vision");
        }
    }
    match patch.reasoning_effort {
        Some(effort) => {
            set_key(table, "reasoning_effort", value(effort));
        }
        None => {
            table.remove("reasoning_effort");
        }
    }
    match patch.reasoning_effort_levels {
        Some(levels) => {
            let mut array = toml_edit::Array::new();
            for level in levels {
                array.push(Value::from(level.as_str()));
            }
            set_key(
                table,
                "reasoning_effort_levels",
                Item::Value(Value::Array(array)),
            );
        }
        None => {
            table.remove("reasoning_effort_levels");
        }
    }
    Ok(())
}

/// Take an account out of the file. Silent when it is not there — a delete of
/// something already gone is the state the caller wanted.
pub fn remove_account(document: &mut DocumentMut, id: &str) {
    remove_sub_table(document, "provider_accounts", id);
}

pub fn remove_model(document: &mut DocumentMut, id: &str) {
    remove_sub_table(document, "models", id);
}

pub fn remove_legacy_provider(document: &mut DocumentMut, id: &str) {
    remove_sub_table(document, "providers", id);
}

/// Point `default_model` at a selection, or take the key out.
pub fn set_default_model(document: &mut DocumentMut, selection: Option<&str>) {
    match selection {
        Some(id) => {
            set_key(document.as_table_mut(), "default_model", value(id));
        }
        None => {
            document.as_table_mut().remove("default_model");
        }
    }
}

/// Take the legacy `default_provider` out, for when what it named is gone.
pub fn clear_default_provider(document: &mut DocumentMut) {
    document.as_table_mut().remove("default_provider");
}

/// `[<parent>.<id>]`, made if it is not there.
///
/// Implicit parent: `[provider_accounts]` itself is never written as a header of
/// its own, so a file that had no accounts gains `[provider_accounts.mine]` and
/// not an empty table above it.
fn sub_table<'a>(document: &'a mut DocumentMut, parent: &str, id: &str) -> &'a mut Table {
    let root = document.as_table_mut();
    if !root.contains_key(parent) || !root[parent].is_table() {
        let mut made = Table::new();
        made.set_implicit(true);
        root.insert(parent, Item::Table(made));
    }
    let parent_table = root[parent].as_table_mut().expect("inserted table");
    parent_table.set_implicit(true);
    if !parent_table.contains_key(id) || !parent_table[id].is_table() {
        parent_table.insert(id, Item::Table(Table::new()));
    }
    parent_table[id].as_table_mut().expect("inserted table")
}

fn existing_sub_table<'a>(
    document: &'a mut DocumentMut,
    parent: &str,
    id: &str,
) -> Option<&'a mut Table> {
    document
        .as_table_mut()
        .get_mut(parent)?
        .as_table_mut()?
        .get_mut(id)?
        .as_table_mut()
}

fn remove_sub_table(document: &mut DocumentMut, parent: &str, id: &str) {
    if let Some(table) = document
        .as_table_mut()
        .get_mut(parent)
        .and_then(Item::as_table_mut)
    {
        table.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"# mine, with a comment
default_model = "deepseek/chat"

[providers.deepseek]
type = "openai"
model = "deepseek-chat"
api_key = "sk-legacy"

[provider_accounts.mine]
provider = "openai-compatible"
base_url = "https://one.example.com/v1"
api_key = "sk-one"
user_agent = "hand-written"

[models."mine/a"]
account = "mine"
model = "a"
context_window = 128000
note = "hand-written"
"#;

    fn doc() -> DocumentMut {
        FILE.parse::<DocumentMut>().expect("the fixture parses")
    }

    /// The property the whole module exists for: a write touches its own keys
    /// and nothing else — not the comment, not the key order, not a key this
    /// build does not model.
    #[test]
    fn a_write_leaves_the_rest_of_the_file_alone() {
        let mut document = doc();
        put_account(
            &mut document,
            "mine",
            &AccountPatch {
                provider: "openai-compatible",
                base_url: Some("https://two.example.com/v1"),
                api_key: KeyWrite::Keep,
                display_name: None,
            },
        )
        .unwrap();
        let out = document.to_string();
        assert!(out.starts_with("# mine, with a comment"), "{out}");
        assert!(out.contains("https://two.example.com/v1"), "{out}");
        assert!(
            out.contains(r#"user_agent = "hand-written""#),
            "a key this panel does not own survives: {out}"
        );
        assert!(
            out.contains(r#"api_key = "sk-one""#),
            "an untouched credential stays: {out}"
        );
        assert!(out.contains(r#"note = "hand-written""#), "{out}");
    }

    #[test]
    fn an_account_with_no_endpoint_loses_the_key_rather_than_writing_an_empty_one() {
        let mut document = doc();
        put_account(
            &mut document,
            "mine",
            &AccountPatch {
                provider: "ollama",
                base_url: None,
                api_key: KeyWrite::Keep,
                display_name: None,
            },
        )
        .unwrap();
        let out = document.to_string();
        assert!(!out.contains("base_url"), "{out}");
        assert!(out.contains(r#"provider = "ollama""#), "{out}");
    }

    #[test]
    fn a_new_account_lands_without_an_empty_parent_header() {
        let mut document = "default_model = \"x\"\n"
            .parse::<DocumentMut>()
            .expect("parses");
        put_account(
            &mut document,
            "fresh",
            &AccountPatch {
                provider: "openai-compatible",
                base_url: Some("https://fresh.example.com/v1"),
                api_key: KeyWrite::Set("sk-fresh"),
                display_name: None,
            },
        )
        .unwrap();
        let out = document.to_string();
        assert!(out.contains("[provider_accounts.fresh]"), "{out}");
        assert!(
            !out.contains("[provider_accounts]\n"),
            "no empty parent header: {out}"
        );
    }

    #[test]
    fn a_model_keeps_the_keys_this_panel_does_not_own() {
        let mut document = doc();
        put_model(
            &mut document,
            "mine/a",
            &ModelPatch {
                account: "mine",
                model: "a-2",
                context_window: 200_000,
                supports_vision: Some(true),
                reasoning_effort: None,
                reasoning_effort_levels: Some(&["low".to_string(), "high".to_string()]),
            },
        )
        .unwrap();
        let out = document.to_string();
        assert!(out.contains(r#"model = "a-2""#), "{out}");
        assert!(out.contains("context_window = 200000"), "{out}");
        assert!(out.contains("supports_vision = true"), "{out}");
        assert!(
            out.contains(r#"reasoning_effort_levels = ["low", "high"]"#),
            "{out}"
        );
        assert!(out.contains(r#"note = "hand-written""#), "{out}");
    }

    #[test]
    fn unsetting_takes_the_key_out_rather_than_writing_a_default_in() {
        let mut document = doc();
        put_model(
            &mut document,
            "mine/a",
            &ModelPatch {
                account: "mine",
                model: "a",
                context_window: 128_000,
                supports_vision: Some(false),
                reasoning_effort: Some("high"),
                reasoning_effort_levels: Some(&["high".to_string()]),
            },
        )
        .unwrap();
        put_model(
            &mut document,
            "mine/a",
            &ModelPatch {
                account: "mine",
                model: "a",
                context_window: 128_000,
                supports_vision: None,
                reasoning_effort: None,
                reasoning_effort_levels: None,
            },
        )
        .unwrap();
        let out = document.to_string();
        assert!(!out.contains("supports_vision"), "{out}");
        assert!(!out.contains("reasoning_effort"), "{out}");
    }

    /// Taking a credential away removes the key. An empty string left behind is
    /// a credential whose value is "", which is not the same as none.
    #[test]
    fn clearing_a_credential_removes_the_key_rather_than_emptying_it() {
        let mut document = doc();
        put_account(
            &mut document,
            "mine",
            &AccountPatch {
                provider: "ollama",
                base_url: None,
                api_key: KeyWrite::Clear,
                display_name: None,
            },
        )
        .unwrap();
        let out = document.to_string();
        assert!(
            !out.contains(r#"api_key = "sk-one""#) && !out.contains(r#"api_key = """#),
            "this account's key is gone, not blanked: {out}"
        );
        assert!(
            out.contains(r#"api_key = "sk-legacy""#),
            "and only this account's: {out}"
        );
    }

    #[test]
    fn deleting_takes_only_the_one_table() {
        let mut document = doc();
        remove_account(&mut document, "mine");
        remove_model(&mut document, "mine/a");
        let out = document.to_string();
        assert!(!out.contains("[provider_accounts.mine]"), "{out}");
        assert!(!out.contains(r#"[models."mine/a"]"#), "{out}");
        assert!(out.contains("[providers.deepseek]"), "{out}");
    }

    #[test]
    fn a_legacy_entry_is_edited_where_it_stands() {
        let mut document = doc();
        patch_legacy_provider(
            &mut document,
            "deepseek",
            Some("anthropic"),
            Some("https://legacy.example.com"),
            None,
        )
        .unwrap();
        let out = document.to_string();
        assert!(out.contains(r#"type = "anthropic""#), "{out}");
        assert!(out.contains("https://legacy.example.com"), "{out}");
        assert!(
            out.contains(r#"api_key = "sk-legacy""#),
            "an untouched credential stays: {out}"
        );
        assert!(
            patch_legacy_provider(&mut document, "nope", Some("openai"), None, None).is_err(),
            "editing one that is not there is refused rather than created"
        );
    }

    /// Overwriting a key must not eat the comment above it: a comment is the
    /// *key's* decoration in `toml_edit`, so an `insert` over an existing key
    /// throws it away — which is how the first real run of the panel lost the
    /// first line of the file.
    #[test]
    fn pointing_the_default_somewhere_else_keeps_the_comment_above_it() {
        let mut document = doc();
        set_default_model(&mut document, Some("mine/b"));
        let out = document.to_string();
        assert!(out.starts_with("# mine, with a comment"), "{out}");
        assert!(out.contains(r#"default_model = "mine/b""#), "{out}");
    }

    #[test]
    fn the_default_can_be_pointed_somewhere_else_or_taken_away() {
        let mut document = doc();
        set_default_model(&mut document, Some("mine/a"));
        assert!(document.to_string().contains(r#"default_model = "mine/a""#));
        set_default_model(&mut document, None);
        assert!(!document.to_string().contains("default_model"));
    }
}
