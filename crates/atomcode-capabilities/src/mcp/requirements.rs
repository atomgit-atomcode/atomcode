//! Required server declarations are a connection contract, never tool authority.
use std::collections::BTreeMap;

use anyhow::{bail, Result};

use super::types::ServerCapabilities;

pub(crate) fn validate(requirements: &BTreeMap<String, u64>) -> Result<()> {
    if requirements.len() > 64 {
        bail!("too many required MCP experimental capabilities (maximum 64)");
    }
    for (name, version) in requirements {
        if name.is_empty()
            || name.len() > 256
            || name.chars().any(char::is_control)
            || *version == 0
        {
            bail!("invalid required MCP experimental capability {name:?}: use a bounded nonempty name and positive integer version");
        }
    }
    Ok(())
}

pub(crate) fn check(
    requirements: &BTreeMap<String, u64>,
    capabilities: &ServerCapabilities,
) -> Result<()> {
    validate(requirements)?;
    for (name, expected) in requirements {
        let actual = capabilities
            .experimental
            .get(name)
            .and_then(|declaration| declaration.get("version"))
            .and_then(serde_json::Value::as_u64);
        if actual != Some(*expected) {
            bail!("MCP server does not satisfy required experimental capability {name:?} version {expected}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capability_versions_are_exact_typed_positive_integers() {
        let required = BTreeMap::from([("test/strict".into(), 1)]);
        for declaration in [
            json!(null),
            json!({}),
            json!({"version":true}),
            json!({"version":"1"}),
            json!({"version":1.0}),
            json!({"version":0}),
            json!({"version":2}),
        ] {
            let caps = ServerCapabilities {
                experimental: json!({"test/strict":declaration}),
                ..Default::default()
            };
            assert!(check(&required, &caps).is_err());
        }
        let caps = ServerCapabilities {
            experimental: json!({"test/strict":{"version":1,"other":"ignored"}}),
            ..Default::default()
        };
        check(&required, &caps).unwrap();
        let both = BTreeMap::from([("test/strict".into(), 1), ("test/second".into(), 2)]);
        assert!(check(&both, &caps).is_err());
        let caps = ServerCapabilities {
            experimental: json!({"test/strict":{"version":1},"test/second":{"version":2}}),
            ..Default::default()
        };
        check(&both, &caps).unwrap();
        check(&BTreeMap::new(), &ServerCapabilities::default()).unwrap();
        for required in [
            BTreeMap::from([(String::new(), 1)]),
            BTreeMap::from([("bad\nname".into(), 1)]),
            BTreeMap::from([("test/strict".into(), 0)]),
            BTreeMap::from([("a".repeat(257), 1)]),
        ] {
            assert!(validate(&required).is_err());
        }
        assert!(validate(&(0..65).map(|i| (format!("test/{i}"), 1)).collect()).is_err());
    }
}
