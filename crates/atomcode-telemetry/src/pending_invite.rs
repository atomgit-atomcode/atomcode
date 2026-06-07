use std::path::Path;

/// Parsed state from the `pending_invite` file written by install.sh / install.ps1.
#[derive(Debug, Clone)]
pub struct PendingInvite {
    pub invite_code: String,
    pub install_uuid: uuid::Uuid,
    pub attempted_at: i64,
}

/// Read `pending_invite` from `<atomcode_dir>/pending_invite`.
///
/// The file is NOT deleted here — it is still needed when the user logs in.
/// Returns `None` if the file is missing, malformed, contains an invalid invite
/// code / UUID, or has exceeded the 30-day expiry window.
pub fn load(atomcode_dir: &Path) -> Option<PendingInvite> {
    let path = atomcode_dir.join("pending_invite");
    let content = std::fs::read_to_string(&path).ok()?;

    let invite = parse_key_value(&content)?;

    // 30-day expiry check (local clock — server re-checks on its side)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    if now.saturating_sub(invite.attempted_at) > 30 * 24 * 3600 {
        return None;
    }

    // Format validation: invite code must be exactly 8 alphanumeric chars
    if invite.invite_code.len() != 8
        || !invite
            .invite_code
            .chars()
            .all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }

    Some(invite)
}

/// Parse key=value lines from the install script's output format.
fn parse_key_value(content: &str) -> Option<PendingInvite> {
    let mut invite_code = None;
    let mut install_uuid = None;
    let mut attempted_at = None;

    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "invite_code" => invite_code = Some(v.trim().to_string()),
                "install_uuid" => install_uuid = v.trim().parse::<uuid::Uuid>().ok(),
                "attempted_at" => attempted_at = v.trim().parse::<i64>().ok(),
                _ => {}
            }
        }
    }

    Some(PendingInvite {
        invite_code: invite_code?,
        install_uuid: install_uuid?,
        attempted_at: attempted_at?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("atomcode-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("pending_invite");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        dir
    }

    #[test]
    fn parses_valid_file() {
        let dir = write_temp(
            "invite_code=ABC12345\ninstall_uuid=550e8400-e29b-41d4-a716-446655440000\nattempted_at=1800000000\n",
        );
        let invite = load(&dir).unwrap();
        assert_eq!(invite.invite_code, "ABC12345");
        assert_eq!(
            invite.install_uuid,
            uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()
        );
        assert_eq!(invite.attempted_at, 1800000000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_malformed_invite_code() {
        let dir = write_temp(
            "invite_code=bad!\ninstall_uuid=550e8400-e29b-41d4-a716-446655440000\nattempted_at=1800000000\n",
        );
        assert!(load(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_invalid_uuid() {
        let dir =
            write_temp("invite_code=ABC12345\ninstall_uuid=not-a-uuid\nattempted_at=1800000000\n");
        assert!(load(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_expired() {
        let dir = write_temp(
            "invite_code=ABC12345\ninstall_uuid=550e8400-e29b-41d4-a716-446655440000\nattempted_at=1000000\n",
        );
        assert!(load(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn returns_none_for_missing_file() {
        let dir = std::path::PathBuf::from("/nonexistent/path");
        assert!(load(&dir).is_none());
    }
}
