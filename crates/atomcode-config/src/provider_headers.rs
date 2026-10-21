//! Account HTTP headers. Values are credentials: serialization is for storage,
//! while Debug exposes names only. Header names are canonicalized at the boundary.
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ProviderHeaders(BTreeMap<String, String>);

fn name(raw: &str) -> Result<String, &'static str> {
    if raw.is_empty()
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    {
        return Err("Invalid HTTP header name");
    }
    let name = raw.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
    ) {
        return Err("Transport and proxy headers cannot be overridden");
    }
    Ok(name)
}
fn value(raw: &str) -> Result<(), &'static str> {
    if raw.bytes().any(|b| b < 32 && b != b'\t' || b == 127) {
        return Err("Invalid HTTP header value");
    }
    Ok(())
}
impl ProviderHeaders {
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn names(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }
    pub fn redact(&self, text: &str) -> String {
        if self.is_empty() {
            return text.to_owned();
        }
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(text) {
            self.redact_json(&mut json);
            return json.to_string();
        }
        self.redact_plain(text)
    }
    fn redact_json(&self, json: &mut serde_json::Value) {
        match json {
            serde_json::Value::String(s) => *s = self.redact_plain(s),
            serde_json::Value::Array(values) => {
                for v in values {
                    self.redact_json(v);
                }
            }
            serde_json::Value::Object(values) => {
                for v in values.values_mut() {
                    self.redact_json(v);
                }
            }
            _ => {}
        }
    }
    fn redact_plain(&self, text: &str) -> String {
        let mut out = text.to_owned();
        let mut values: Vec<_> = self.0.values().filter(|v| !v.is_empty()).collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        for value in values {
            out = out.replace(value.as_str(), "<redacted>");
        }
        out
    }
}
impl std::fmt::Debug for ProviderHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ProviderHeaders")
            .field(&self.names())
            .finish()
    }
}
impl<'de> Deserialize<'de> for ProviderHeaders {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, String>::deserialize(d)?;
        let mut out = BTreeMap::new();
        for (k, v) in raw {
            let k = name(&k).map_err(serde::de::Error::custom)?;
            value(&v).map_err(serde::de::Error::custom)?;
            if out.insert(k, v).is_some() {
                return Err(serde::de::Error::custom("Duplicate HTTP header name"));
            }
        }
        Ok(Self(out))
    }
}

/// A replacement set: null preserves that name's stored value; omitted names
/// are removed. Applied by the document writer under its exclusive file lock.
#[derive(Clone, Default, Serialize)]
#[serde(transparent)]
pub struct HeaderChanges(pub BTreeMap<String, Option<String>>);
impl HeaderChanges {
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut names = std::collections::BTreeSet::new();
        for (k, v) in &self.0 {
            if !names.insert(name(k)?) {
                return Err("Duplicate HTTP header name");
            }
            if let Some(v) = v {
                value(v)?;
            }
        }
        Ok(())
    }
}
impl std::fmt::Debug for HeaderChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("HeaderChanges")
            .field(&self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}
impl<'de> Deserialize<'de> for HeaderChanges {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, Option<String>>::deserialize(d)?;
        let mut out = BTreeMap::new();
        for (k, v) in raw {
            let k = name(&k).map_err(serde::de::Error::custom)?;
            if let Some(v) = &v {
                value(v).map_err(serde::de::Error::custom)?;
            }
            if out.insert(k, v).is_some() {
                return Err(serde::de::Error::custom("Duplicate HTTP header name"));
            }
        }
        Ok(Self(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disk_validation_errors_never_echo_the_secret_source_line() {
        let raw = r#"[provider_accounts.gateway]
provider = "openai-compatible"
headers = { "x-token" = "do-not-log-this\r\ninjected" }
"#;
        let error = crate::Config::parse_disk_content(raw, std::path::Path::new("config.toml"))
            .unwrap_err();
        assert!(!format!("{error:#}").contains("do-not-log-this"));
    }

    #[test]
    fn values_are_validated_and_never_debugged() {
        let h: ProviderHeaders =
            serde_json::from_str(r#"{"X-Token":"super-secret","Version":"v1"}"#).unwrap();
        assert_eq!(h.names(), vec!["version", "x-token"]);
        assert!(!format!("{h:?}").contains("super-secret"));
        assert_eq!(
            h.redact("upstream echoed super-secret"),
            "upstream echoed <redacted>"
        );
        let quoted: ProviderHeaders =
            serde_json::from_value(serde_json::json!({"x-auth":"a\"b"})).unwrap();
        let echoed = serde_json::json!({"error":{"message":"rejected a\"b"}}).to_string();
        let sanitized: serde_json::Value = serde_json::from_str(&quoted.redact(&echoed)).unwrap();
        assert_eq!(sanitized["error"]["message"], "rejected <redacted>");
        for raw in [
            r#"{"Bad Name":"secret"}"#,
            r#"{"X-Token":"secret\r\ninjected"}"#,
            r#"{"HOST":"other"}"#,
            r#"{"X-Token":"a","x-token":"b"}"#,
        ] {
            assert!(serde_json::from_str::<ProviderHeaders>(raw).is_err());
        }
    }
}
