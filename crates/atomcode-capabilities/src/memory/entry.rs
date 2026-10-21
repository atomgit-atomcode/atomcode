//! One Markdown entry, with optional inline metadata. No sidecar authority.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;

pub const MARKER: &str = "<!-- atomcode-memory:";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    Active,
    Outdated,
    Deprecated,
}
impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Outdated => "outdated",
            Self::Deprecated => "deprecated",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryMetadata {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub status: MemoryStatus,
    #[serde(default)]
    pub pinned: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub content: String,
    #[serde(flatten)]
    pub metadata: MemoryMetadata,
}
#[derive(Clone, Debug, Default)]
pub struct MemoryPatch {
    pub content: Option<String>,
    pub topic: Option<String>,
    pub source: Option<String>,
    pub status: Option<MemoryStatus>,
    pub pinned: Option<bool>,
    pub expires_at: Option<Option<String>>,
}

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
pub(crate) fn content(value: &str) -> io::Result<String> {
    let value = value.trim();
    if value.is_empty() || value.contains(['\r', '\n']) || value.contains(MARKER) {
        return Err(invalid(
            "memory content must be one non-empty line without the reserved metadata marker",
        ));
    }
    Ok(value.into())
}
fn label(value: &str, max: usize) -> io::Result<Option<String>> {
    let value = value.trim();
    if value.contains(['\r', '\n']) || value.chars().count() > max {
        return Err(invalid(
            "memory topic/source is too long or contains a newline",
        ));
    }
    Ok((!value.is_empty()).then(|| value.into()))
}
impl MemoryEntry {
    pub fn expired_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.metadata
            .expires_at
            .as_deref()
            .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
            .is_some_and(|expiry| expiry <= now)
    }
    pub fn eligible_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.metadata.status == MemoryStatus::Active && !self.expired_at(now)
    }
    pub fn matches_expiry(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        expired: bool,
        within: Option<u32>,
    ) -> bool {
        if expired {
            return self.expired_at(now);
        }
        within.is_none_or(|days| {
            validate_days(days).is_ok()
                && self.eligible_at(now)
                && self
                    .metadata
                    .expires_at
                    .as_deref()
                    .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
                    .is_some_and(|expiry| {
                        now.checked_add_signed(chrono::Duration::days(i64::from(days)))
                            .is_some_and(|end| expiry <= end)
                    })
        })
    }
    pub fn matches(&self, topic: Option<&str>, status: Option<MemoryStatus>) -> bool {
        topic.is_none_or(|t| {
            self.metadata
                .topic
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(t))
        }) && status.is_none_or(|s| self.metadata.status == s)
    }
    pub fn describe(&self) -> String {
        format!("- {}\n  ID: {} · status={} · pinned={} · topic={} · source={} · created_at={} · updated_at={} · expires_at={}\n",
            self.content, self.metadata.id, self.metadata.status.as_str(), self.metadata.pinned,
            self.metadata.topic.as_deref().unwrap_or("—"), self.metadata.source.as_deref().unwrap_or("—"),
            self.metadata.created_at.as_deref().unwrap_or("unknown"), self.metadata.updated_at.as_deref().unwrap_or("unknown"), self.metadata.expires_at.as_deref().unwrap_or("none"))
    }
    pub fn legacy(text: &str) -> Self {
        let text = text.trim();
        Self {
            content: text.into(),
            metadata: MemoryMetadata {
                id: format!("legacy-{:x}", Sha256::digest(text.as_bytes())),
                created_at: None,
                updated_at: None,
                expires_at: None,
                topic: None,
                source: None,
                status: MemoryStatus::Active,
                pinned: false,
            },
        }
    }
    pub fn new(text: &str, patch: &MemoryPatch) -> io::Result<Self> {
        let mut entry = Self::legacy(&content(text)?);
        entry.metadata.id = format!("m-{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        entry.metadata.created_at = Some(now.clone());
        entry.metadata.updated_at = Some(now);
        entry.apply(patch)?;
        Ok(entry)
    }
    pub fn apply(&mut self, patch: &MemoryPatch) -> io::Result<()> {
        let mut next = self.clone();
        if let Some(value) = &patch.content {
            next.content = content(value)?;
        }
        if let Some(value) = &patch.topic {
            next.metadata.topic = label(value, 128)?;
        }
        if let Some(value) = &patch.source {
            next.metadata.source = label(value, 512)?;
        }
        if let Some(value) = patch.status {
            next.metadata.status = value;
        }
        if let Some(value) = patch.pinned {
            next.metadata.pinned = value;
        }
        if let Some(value) = &patch.expires_at {
            if let Some(time) = value {
                chrono::DateTime::parse_from_rfc3339(time)
                    .map_err(|_| invalid("invalid memory expiry timestamp"))?;
            }
            next.metadata.expires_at = value.clone();
        }
        if &next != self {
            let now = chrono::Utc::now();
            let previous = next
                .metadata
                .updated_at
                .as_ref()
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                .map(|v| v.with_timezone(&chrono::Utc));
            next.metadata.updated_at = Some(
                previous
                    .map_or(now, |old| old.max(now))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            );
        }
        *self = next;
        Ok(())
    }
    pub fn render(&self) -> io::Result<String> {
        let text = content(&self.content)?;
        let prefix = if self.metadata.status == MemoryStatus::Active {
            "- "
        } else {
            "> - "
        };
        let json = serde_json::to_string(&self.metadata).map_err(invalid_json)?;
        // Keep comments well-formed and prevent a source/topic containing our
        // reserved marker from being mistaken for the entry's actual suffix.
        let json = json.replace('<', "\\u003c").replace('>', "\\u003e");
        let line = format!("{prefix}{text} {MARKER}v1 {json} -->");
        // Reserve either LF or CRLF so replacing an existing Windows line
        // cannot create a record larger than the bounded reader accepts.
        if line.len() > 64 * 1024 - 2 {
            return Err(invalid(
                "memory entry exceeds the 64 KiB line limit; store long knowledge in a linked file",
            ));
        }
        Ok(line)
    }
}
fn invalid_json(error: serde_json::Error) -> io::Error {
    invalid(format!("invalid memory metadata: {error}"))
}

/// Legacy lines have derived IDs, never invented timestamps. Unknown metadata
/// versions fail explicitly instead of becoming ordinary active memory text.
pub fn parse_line(line: &str) -> io::Result<Option<MemoryEntry>> {
    let line = line.trim();
    let Some(at) = line.rfind(MARKER) else {
        return Ok(line.strip_prefix("- ").map(MemoryEntry::legacy));
    };
    let text = line[..at].trim_end();
    let text = text
        .strip_prefix("- ")
        .or_else(|| text.strip_prefix("> - "))
        .ok_or_else(|| invalid("memory metadata must be attached to a bullet entry"))?;
    let json = line[at + MARKER.len()..]
        .strip_prefix("v1 ")
        .and_then(|s| s.strip_suffix(" -->"))
        .ok_or_else(|| {
            invalid("unsupported or malformed memory metadata version; use a compatible build")
        })?;
    let metadata: MemoryMetadata = serde_json::from_str(json).map_err(invalid_json)?;
    if metadata.id.is_empty()
        || metadata.id.len() > 96
        || !metadata
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(invalid("invalid memory entry ID"));
    }
    for time in [
        &metadata.created_at,
        &metadata.updated_at,
        &metadata.expires_at,
    ]
    .into_iter()
    .flatten()
    {
        chrono::DateTime::parse_from_rfc3339(time)
            .map_err(|_| invalid("invalid memory timestamp"))?;
    }
    if let Some(topic) = &metadata.topic {
        label(topic, 128)?;
    }
    if let Some(source) = &metadata.source {
        label(source, 512)?;
    }
    Ok(Some(MemoryEntry {
        content: content(text)?,
        metadata,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_identity_has_no_invented_time_and_survives_adoption_and_content_update() {
        let mut entry = parse_line("- previous fact").unwrap().unwrap();
        let id = entry.metadata.id.clone();
        assert!(entry.metadata.created_at.is_none());
        entry
            .apply(&MemoryPatch {
                content: Some("corrected fact".into()),
                ..Default::default()
            })
            .unwrap();
        let parsed = parse_line(&entry.render().unwrap()).unwrap().unwrap();
        assert_eq!(parsed.metadata.id, id);
        assert!(parsed.metadata.created_at.is_none());
        assert!(parsed.metadata.updated_at.is_some());
        assert_eq!(parsed.content, "corrected fact");
    }
    #[test]
    fn metadata_round_trips_and_inactive_entries_are_not_legacy_bullets() {
        let entry = MemoryEntry::new(
            "非显然的项目约束",
            &MemoryPatch {
                topic: Some("runtime".into()),
                source: Some("issue <!-- atomcode-memory:v9 --> 12".into()),
                status: Some(MemoryStatus::Deprecated),
                pinned: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        let line = entry.render().unwrap();
        assert!(line.starts_with("> - "));
        assert_eq!(parse_line(&line).unwrap().unwrap(), entry);
        assert!(entry.metadata.created_at.is_some());
    }
    #[test]
    fn malformed_or_future_metadata_and_multiline_mutations_are_rejected() {
        for line in [
            "- fact <!-- atomcode-memory:v2 {} -->",
            "- fact <!-- atomcode-memory:v1 broken -->",
            "<!-- atomcode-memory:v1 {} -->",
        ] {
            assert!(parse_line(line).is_err(), "{line}");
        }
        assert!(MemoryEntry::new("one\n- another", &Default::default()).is_err());
    }
}

/// Duration bounds keep writes and list windows explicit and overflow-free.
pub fn validate_days(days: u32) -> io::Result<()> {
    if !(1..=36500).contains(&days) {
        return Err(invalid("memory duration must be 1..36500 whole days"));
    }
    Ok(())
}
pub fn expiry_patch(
    days: Option<u32>,
    clear: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> io::Result<Option<Option<String>>> {
    if clear && days.is_some() {
        return Err(invalid("cannot combine ttl_days and clear_expiry"));
    }
    if let Some(days) = days {
        validate_days(days)?;
        return Ok(Some(Some(
            now.checked_add_signed(chrono::Duration::days(i64::from(days)))
                .ok_or_else(|| invalid("memory expiry exceeds timestamp range"))?
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )));
    }
    Ok(clear.then_some(None))
}

#[cfg(test)]
mod expiry_tests {
    use super::*;
    fn instant(value: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }
    #[test]
    fn expiry_boundaries_offsets_and_upcoming_filters_are_clock_explicit() {
        let now = instant("2026-10-21T00:00:00Z");
        let mut entry = MemoryEntry::new("temporary convention", &MemoryPatch::default()).unwrap();
        assert!(entry.eligible_at(now));
        assert!(!entry.matches_expiry(now, false, Some(7)));
        entry
            .apply(&MemoryPatch {
                expires_at: Some(Some("2026-10-21T08:00:00+08:00".into())),
                pinned: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert!(entry.expired_at(now));
        assert!(!entry.eligible_at(now));
        assert!(entry.matches_expiry(now, true, None));
        assert!(!entry.matches_expiry(now, false, Some(7)));
        let before = now - chrono::Duration::milliseconds(1);
        assert!(entry.eligible_at(before));
        assert!(entry.matches_expiry(before, false, Some(1)));
        entry
            .apply(&MemoryPatch {
                expires_at: expiry_patch(Some(7), false, now).unwrap(),
                ..Default::default()
            })
            .unwrap();
        assert!(entry.matches_expiry(now, false, Some(7)));
        assert!(!entry.matches_expiry(now, false, Some(6)));
        entry
            .apply(&MemoryPatch {
                status: Some(MemoryStatus::Deprecated),
                ..Default::default()
            })
            .unwrap();
        assert!(!entry.matches_expiry(now, false, Some(7)));
    }
    #[test]
    fn ordinary_updates_do_not_renew_and_clearing_is_explicit() {
        let now = instant("2026-10-21T00:00:00Z");
        let mut entry = MemoryEntry::new(
            "fact",
            &MemoryPatch {
                expires_at: expiry_patch(Some(7), false, now).unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
        let expiry = entry.metadata.expires_at.clone();
        entry
            .apply(&MemoryPatch {
                content: Some("new fact".into()),
                pinned: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entry.metadata.expires_at, expiry);
        assert_eq!(
            parse_line(&entry.render().unwrap()).unwrap().unwrap(),
            entry
        );
        let unchanged = entry.clone();
        assert!(entry
            .apply(&MemoryPatch {
                expires_at: Some(Some("invalid".into())),
                ..Default::default()
            })
            .is_err());
        assert_eq!(entry, unchanged);
        assert!(expiry_patch(Some(0), false, now).is_err());
        assert!(expiry_patch(Some(36501), false, now).is_err());
        assert!(expiry_patch(Some(1), true, now).is_err());
        assert!(expiry_patch(Some(1), false, chrono::DateTime::<chrono::Utc>::MAX_UTC).is_err());
        assert!(!entry.matches_expiry(now, false, Some(u32::MAX)));
        entry
            .apply(&MemoryPatch {
                expires_at: expiry_patch(None, true, now).unwrap(),
                ..Default::default()
            })
            .unwrap();
        assert!(entry.metadata.expires_at.is_none());
        assert!(entry.eligible_at(now + chrono::Duration::days(50000)));
    }
}
