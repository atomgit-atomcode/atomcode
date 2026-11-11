//! Host-local origin policy/setup data. Execution belongs to the external backend.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(super) const ENV: &str = "ATOMCODE_BROWSER_ORIGIN_POLICY";
pub(super) const CAPABILITY: &str = "atomcode/browser-origin-scope";
const CORE_VERSION: &str = "1.66.0-alpha-2026-11-09";

#[derive(Clone, Debug, Default, Args)]
pub struct OriginArgs {
    /// Absolute optional origin-scope backend.cjs path; not installed automatically.
    #[arg(long, conflicts_with = "unrestricted_origins")]
    pub origin_backend: Option<PathBuf>,
    /// Absolute installed @playwright/mcp directory for the scoped backend.
    #[arg(long, conflicts_with = "unrestricted_origins")]
    pub backend_root: Option<PathBuf>,
    /// Allow an exact HTTP(S) origin (repeatable); requires the optional backend.
    #[arg(long = "allow-origin", conflicts_with_all = ["deny_all_origins", "unrestricted_origins"])]
    pub allowed_origins: Vec<String>,
    /// Absolute trusted JSON policy file (v1 or v2); validated contents are saved.
    #[arg(long, conflicts_with_all = ["allowed_origins", "deny_all_origins", "unrestricted_origins"])]
    pub origin_policy_file: Option<PathBuf>,
    /// Explicitly deny all origins.
    #[arg(long, conflicts_with = "unrestricted_origins")]
    pub deny_all_origins: bool,
    /// Explicitly return to the stock backend without a saved origin restriction.
    #[arg(long)]
    pub unrestricted_origins: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Scoped {
    script: PathBuf,
    root: PathBuf,
    policy: Policy,
}

#[derive(Debug)]
pub(super) enum Kind {
    Stock(PathBuf),
    Scoped(Scoped),
}

#[derive(Debug)]
pub(super) struct Prepared {
    pub node: PathBuf,
    pub kind: Kind,
    pub remove_scope: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Permission {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Permissions {
    #[serde(skip_serializing_if = "Option::is_none")]
    access: Option<Permission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    downloads: Option<Permission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uploads: Option<Permission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_cdp_access: Option<Permission>,
}

#[derive(Clone, Debug)]
enum Policy {
    V1(Vec<String>),
    V2(BTreeMap<String, Permissions>),
}

impl Policy {
    fn version(&self) -> u64 {
        match self {
            Self::V1(_) => 1,
            Self::V2(_) => 2,
        }
    }
    fn origins(&self) -> Vec<String> {
        match self {
            Self::V1(origins) => origins.clone(),
            Self::V2(origins) => origins
                .iter()
                .filter(|(_, p)| p.access == Some(Permission::Allow))
                .map(|(origin, _)| origin.clone())
                .collect(),
        }
    }
    fn downloads(&self) -> usize {
        match self {
            Self::V1(_) => 0,
            Self::V2(origins) => origins
                .values()
                .filter(|p| p.downloads == Some(Permission::Allow))
                .count(),
        }
    }
    fn json(&self) -> Result<String> {
        Ok(serde_json::to_string(&match self {
            Self::V1(origins) => json!({"version":1,"origins":origins}),
            Self::V2(origins) => json!({"version":2,"origins":origins}),
        })?)
    }
    fn summary(&self) -> String {
        let origins = self.origins();
        let scope = if origins.is_empty() {
            "deny all origins".into()
        } else {
            origins.join(", ")
        };
        if self.version() == 1 {
            scope
        } else {
            format!("{scope}; controlled unauthenticated HTTP(S) downloads: {} origin(s); uploads/CDP denied", self.downloads())
        }
    }
}

pub(super) fn normalize(origins: Vec<String>) -> Result<Vec<String>> {
    if origins.len() > 256 {
        bail!("at most 256 browser origins are permitted");
    }
    let mut canonical = BTreeSet::new();
    for raw in origins {
        let authority = raw
            .split_once("://")
            .map(|(_, rest)| rest.strip_suffix('/').unwrap_or(rest));
        if raw.len() > 2048
            || raw.chars().any(|c| c.is_control() || c.is_whitespace())
            || raw.contains(['?', '#', '@', '\\', '*'])
            || authority.is_none_or(|part| part.is_empty() || part.contains('/'))
        {
            bail!("use exact HTTP(S) origins without paths, credentials, queries or wildcards");
        }
        let url = reqwest::Url::parse(&raw).context("invalid browser origin")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || url.path() != "/"
            || !url.username().is_empty()
            || url.password().is_some()
        {
            bail!("use exact HTTP(S) origins without paths, credentials, queries or wildcards");
        }
        canonical.insert(url.origin().ascii_serialization());
    }
    let result: Vec<_> = canonical.into_iter().collect();
    if policy_json(&result)?.len() > 65536 {
        bail!("browser origin policy exceeds 64 KiB");
    }
    Ok(result)
}

fn parse_policy(raw: &str) -> Result<Policy> {
    if raw.len() > 65536 {
        bail!("saved browser origin policy exceeds 64 KiB");
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        version: u64,
        origins: Value,
    }
    let input: Input = serde_json::from_str(raw).context("invalid browser origin policy")?;
    match input.version {
        1 => Ok(Policy::V1(normalize(
            serde_json::from_value(input.origins).context("invalid v1 browser origins")?,
        )?)),
        2 => {
            if input.origins.as_object().is_some_and(|origins| {
                origins.values().any(|p| {
                    p.as_object()
                        .is_none_or(|fields| fields.values().any(|v| v != "allow" && v != "deny"))
                })
            }) {
                bail!("browser permission values must be allow or deny");
            }
            let map: BTreeMap<String, Permissions> =
                serde_json::from_value(input.origins).context("invalid v2 browser permissions")?;
            if map.len() > 256 {
                bail!("at most 256 browser origins are permitted");
            }
            let mut canonical = BTreeMap::new();
            for (raw, permissions) in map {
                if permissions.uploads == Some(Permission::Allow)
                    || permissions.full_cdp_access == Some(Permission::Allow)
                    || (permissions.downloads == Some(Permission::Allow)
                        && permissions.access != Some(Permission::Allow))
                {
                    bail!("unsupported or inconsistent browser origin permissions");
                }
                let origin = normalize(vec![raw])?.remove(0);
                if canonical.insert(origin, permissions).is_some() {
                    bail!("duplicate canonical browser origin");
                }
            }
            let policy = Policy::V2(canonical);
            if policy.json()?.len() > 65536 {
                bail!("browser origin policy exceeds 64 KiB");
            }
            Ok(policy)
        }
        _ => bail!("unsupported browser origin policy version"),
    }
}

fn read_policy(path: &Path) -> Result<Policy> {
    let path = file(path, "--origin-policy-file")?;
    let mut raw = String::new();
    std::fs::File::open(path)?
        .take(65537)
        .read_to_string(&mut raw)
        .context("cannot read browser policy file")?;
    parse_policy(&raw)
}

fn policy_json(origins: &[String]) -> Result<String> {
    Ok(serde_json::to_string(
        &json!({"version":1,"origins":origins}),
    )?)
}

fn output(dirs: &atomcode_capabilities::ProductDirs) -> Result<PathBuf> {
    Ok(std::path::absolute(
        dirs.user()
            .join("integrations/browser-use/origin-artifacts"),
    )?)
}

fn file(path: &Path, label: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{label} must be an absolute path");
    }
    let resolved = std::fs::canonicalize(path).with_context(|| format!("{label} was not found"))?;
    if !resolved.is_file() {
        bail!("{label} must be a regular file");
    }
    Ok(resolved)
}

fn node(entry: Option<&Value>, explicit: Option<&Path>) -> Result<PathBuf> {
    let saved = entry
        .and_then(|entry| entry.get("command"))
        .and_then(Value::as_str);
    Ok(std::fs::canonicalize(
        which::which(explicit.unwrap_or(Path::new(saved.unwrap_or("node"))))
            .context("Node is not installed; install Node 20+ explicitly")?,
    )?)
}

pub(super) fn requirements(entry: Option<&Value>) -> Result<BTreeMap<String, u64>> {
    let Some(entry) = entry else {
        return Ok(Default::default());
    };
    if entry.get("requiredExperimentalCapabilities").is_some()
        && entry.get("required_experimental_capabilities").is_some()
    {
        bail!("duplicate required capability fields in browser configuration");
    }
    match entry
        .get("requiredExperimentalCapabilities")
        .or_else(|| entry.get("required_experimental_capabilities"))
    {
        None => Ok(Default::default()),
        Some(value) => Ok(serde_json::from_value(value.clone())
            .context("invalid saved browser capability requirements")?),
    }
}

fn saved(
    entry: Option<&Value>,
    dirs: &atomcode_capabilities::ProductDirs,
) -> Result<Option<Scoped>> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    let policy = entry.get("env").and_then(|env| env.get(ENV));
    if entry.get("originScope").is_none() {
        if policy.is_some() || requirements(Some(entry))?.contains_key(CAPABILITY) {
            bail!("unrecognized saved origin restriction; inspect configuration rather than discarding it");
        }
        return Ok(None);
    }
    let version = entry["originScope"]
        .as_u64()
        .context("invalid managed origin scope version")?;
    if ![1, 2].contains(&version) {
        bail!("unsupported managed origin scope version");
    }
    if requirements(Some(entry))?.get(CAPABILITY) != Some(&version) {
        bail!("saved origin scope requires its exact-version MCP connection contract");
    }
    let args: Vec<String> = serde_json::from_value(entry["args"].clone())
        .context("invalid saved scoped browser arguments")?;
    if args.len() != 5
        || args[1] != "--backend-root"
        || args[3] != "--output-dir"
        || Path::new(&args[4]) != output(dirs)?
    {
        bail!("unrecognized saved scoped browser arguments; inspect configuration");
    }
    if !Path::new(&args[0]).is_absolute() || !Path::new(&args[2]).is_absolute() {
        bail!("saved scoped browser paths must be absolute");
    }
    let policy = parse_policy(
        policy
            .and_then(Value::as_str)
            .context("missing saved browser origin policy")?,
    )?;
    if policy.version() != version {
        bail!("saved policy and managed scope versions differ");
    }
    Ok(Some(Scoped {
        script: args[0].clone().into(),
        root: args[2].clone().into(),
        policy,
    }))
}

pub(super) fn describe(
    entry: Option<&Value>,
    dirs: &atomcode_capabilities::ProductDirs,
) -> Result<String> {
    match saved(entry, dirs)? {
        None => Ok("Origin scope: not enforced by the stock backend.".into()),
        Some(scope) if scope.policy.version() == 1 && scope.policy.origins().is_empty() => {
            Ok("Origin scope: deny all origins (configured only).".into())
        }
        Some(scope) => Ok(format!(
            "Origin scope (configured only): {}",
            scope.policy.summary()
        )),
    }
}

pub(super) fn prepare(
    dirs: &atomcode_capabilities::ProductDirs,
    entry: Option<&Value>,
    server: Option<&Path>,
    explicit_node: Option<&Path>,
    change: &OriginArgs,
) -> Result<Prepared> {
    let modes = usize::from(!change.allowed_origins.is_empty())
        + usize::from(change.deny_all_origins)
        + usize::from(change.unrestricted_origins)
        + usize::from(change.origin_policy_file.is_some());
    if modes > 1
        || (change.unrestricted_origins
            && (change.origin_backend.is_some() || change.backend_root.is_some()))
    {
        bail!("browser origin options are mutually exclusive");
    }
    // Explicit stock path plus unrestriction can repair malformed saved scope.
    let previous = if change.unrestricted_origins && server.is_some() {
        None
    } else {
        saved(entry, dirs)?
    };
    let wants_scope = !change.unrestricted_origins
        && (previous.is_some()
            || change.origin_backend.is_some()
            || change.backend_root.is_some()
            || !change.allowed_origins.is_empty()
            || change.deny_all_origins
            || change.origin_policy_file.is_some());
    let kind = if wants_scope {
        if server.is_some() {
            bail!("--server selects the stock backend; use --origin-backend and --backend-root for scope");
        }
        let script = change
            .origin_backend
            .as_deref()
            .or_else(|| previous.as_ref().map(|s| s.script.as_path()))
            .context("first scoped setup requires --origin-backend <absolute backend.cjs path>")?;
        let root = change.backend_root.as_deref().or_else(|| previous.as_ref().map(|s| s.root.as_path()))
            .context("first scoped setup requires --backend-root <absolute installed @playwright/mcp directory>")?;
        let policy = if let Some(path) = &change.origin_policy_file {
            read_policy(path)?
        } else if change.deny_all_origins {
            Policy::V1(Vec::new())
        } else if !change.allowed_origins.is_empty() {
            Policy::V1(normalize(change.allowed_origins.clone())?)
        } else {
            previous.as_ref().context("first scoped setup needs --allow-origin, --deny-all-origins or --origin-policy-file")?.policy.clone()
        };
        if !root.is_absolute() {
            bail!("--backend-root must be absolute");
        }
        let root = std::fs::canonicalize(root).context("browser backend root was not found")?;
        if !root.is_dir() {
            bail!("browser backend root must be a directory");
        }
        Kind::Scoped(Scoped {
            script: file(script, "--origin-backend")?,
            root,
            policy,
        })
    } else {
        let stock = server.map(Path::to_path_buf).or_else(|| previous.as_ref().map(|scope| scope.root.join("cli.js")))
            .or_else(|| entry.and_then(|v| v["args"].get(0)).and_then(Value::as_str).map(PathBuf::from))
            .context("backend not configured; use --server for stock setup, or --origin-backend/--backend-root and an explicit origin policy")?;
        Kind::Stock(file(&stock, "--server")?)
    };
    Ok(Prepared {
        node: node(entry, explicit_node)?,
        kind,
        remove_scope: change.unrestricted_origins,
    })
}

impl Prepared {
    #[cfg(test)]
    pub(super) fn stock(node: PathBuf, server: PathBuf) -> Self {
        Self {
            node,
            kind: Kind::Stock(server),
            remove_scope: false,
        }
    }
    pub(super) fn scope_version(&self) -> Option<u64> {
        match &self.kind {
            Kind::Scoped(scope) => Some(scope.policy.version()),
            _ => None,
        }
    }
    pub(super) fn downloads_enabled(&self) -> bool {
        matches!(&self.kind, Kind::Scoped(scope) if scope.policy.downloads() > 0)
    }
    pub(super) fn scoped(&self) -> bool {
        self.scope_version().is_some()
    }
    pub(super) fn args(&self, dirs: &atomcode_capabilities::ProductDirs) -> Result<Vec<String>> {
        match &self.kind {
            Kind::Stock(server) => Ok(std::iter::once(
                server
                    .to_str()
                    .context("backend path must be UTF-8")?
                    .to_string(),
            )
            .chain(super::OPTIONS.iter().map(|arg| arg.to_string()))
            .collect()),
            Kind::Scoped(scope) => Ok(vec![
                scope
                    .script
                    .to_str()
                    .context("backend path must be UTF-8")?
                    .into(),
                "--backend-root".into(),
                scope
                    .root
                    .to_str()
                    .context("backend root must be UTF-8")?
                    .into(),
                "--output-dir".into(),
                output(dirs)?
                    .to_str()
                    .context("artifact path must be UTF-8")?
                    .into(),
            ]),
        }
    }
    pub(super) fn env(&self, dirs: &atomcode_capabilities::ProductDirs) -> Result<Value> {
        let mut env = super::managed_env(dirs)?;
        if let Kind::Scoped(scope) = &self.kind {
            env[ENV] = json!(scope.policy.json()?);
        }
        Ok(env)
    }
    pub(super) fn summary(&self) -> String {
        match &self.kind {
            Kind::Stock(_) => "Origin scope: not enforced by the stock backend.".into(),
            Kind::Scoped(scope)
                if scope.policy.version() == 1 && scope.policy.origins().is_empty() =>
            {
                "Origin scope: deny all origins.".into()
            }
            Kind::Scoped(scope) => format!("Origin scope: {}", scope.policy.summary()),
        }
    }
    pub(super) async fn check(&self, dirs: &atomcode_capabilities::ProductDirs) -> Result<()> {
        super::check_node(&self.node).await?;
        match &self.kind {
            Kind::Stock(server) => super::check_stock(&self.node, server).await,
            Kind::Scoped(scope) => {
                let mut args = self.args(dirs)?;
                args.push("--doctor".into());
                let env: BTreeMap<String, String> = serde_json::from_value(self.env(dirs)?)?;
                let output = super::diagnostic(&self.node, &args, &env).await?;
                let report: Value = serde_json::from_str(&output)
                    .context("unsupported scoped browser doctor report")?;
                if report["version"].as_u64() != Some(scope.policy.version())
                    || report["kind"] != "doctor"
                    || report["browser_launched"] != false
                    || report["capability"] != CAPABILITY
                    || report["mcp"] != super::VERSION
                    || report["playwright"] != CORE_VERSION
                    || report["allowed_origin_count"].as_u64()
                        != Some(scope.policy.origins().len() as u64)
                    || report["downloads"]
                        != if scope.policy.downloads() > 0 {
                            "controlled-http-unauthenticated"
                        } else {
                            "deny-artifacts"
                        }
                    || report["uploads"] != "deny"
                    || report["full_cdp_access"] != "deny"
                {
                    bail!(
                        "unsupported scoped browser doctor contract; no configuration was changed"
                    );
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_policy_keeps_denials_and_canonicalizes_grants() {
        let policy = parse_policy(r#"{"version":2,"origins":{"https://EXAMPLE.test:443/":{"access":"allow","downloads":"allow"},"https://denied.test":{"access":"deny","downloads":"deny"}}}"#).unwrap();
        assert_eq!(policy.version(), 2);
        assert_eq!(policy.origins(), ["https://example.test"]);
        assert_eq!(policy.downloads(), 1);
        let saved: Value = serde_json::from_str(&policy.json().unwrap()).unwrap();
        assert_eq!(saved["origins"]["https://denied.test"]["access"], "deny");
        assert_eq!(
            parse_policy(&policy.json().unwrap())
                .unwrap()
                .json()
                .unwrap(),
            policy.json().unwrap()
        );
    }

    #[test]
    fn unsupported_or_ambiguous_v2_permissions_fail_closed() {
        for permissions in [
            json!(null),
            json!([]),
            json!({"access":null}),
            json!({"downloads":"allow"}),
            json!({"access":"allow","uploads":"allow"}),
            json!({"access":"allow","full_cdp_access":"allow"}),
            json!({"access":"allow","unexpected":"deny"}),
        ] {
            assert!(parse_policy(
                &json!({"version":2,"origins":{"https://a.test":permissions}}).to_string()
            )
            .is_err());
        }
        assert!(parse_policy(
            r#"{"version":2,"origins":{"https://A.test:443/":{},"https://a.test":{}}}"#
        )
        .is_err());
        assert!(parse_policy(r#"{"version":2,"origins":{"https://a.test/path":{}}}"#).is_err());
        assert!(parse_policy(r#"{"version":3,"origins":{}}"#).is_err());
    }
}
