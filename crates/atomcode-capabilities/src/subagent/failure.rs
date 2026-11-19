use chrono::{DateTime, Days, NaiveTime, Utc};

use super::SubagentError;

/// Recognize CLI diagnostics, never a successful agent's task response.
pub(super) fn permission_denied(diagnostic: &str) -> Option<SubagentError> {
    diagnostic.lines().map(str::trim).find_map(|line| {
        let message = line.strip_prefix("Error: ").unwrap_or(line);
        ["Permission denied", "Operation not permitted"]
            .iter()
            .any(|prefix| {
                message == *prefix
                    || message
                        .strip_prefix(prefix)
                        .is_some_and(|suffix| suffix.starts_with(" (os error "))
            })
            .then(|| SubagentError::PermissionDenied(message.to_owned()))
    })
}

pub(super) fn claude_quota(diagnostic: &str, now: DateTime<Utc>) -> Option<SubagentError> {
    const LIMIT: &str = "You've hit your session limit";
    let line = diagnostic.lines().map(str::trim).find_map(|line| {
        let line = line.strip_prefix("Error: ").unwrap_or(line);
        let suffix = line.strip_prefix(LIMIT)?;
        (suffix.is_empty() || suffix.starts_with(" · resets ")).then_some(line)
    })?;
    let retry_at = line
        .strip_prefix(LIMIT)
        .and_then(|suffix| suffix.strip_prefix(" · resets "))
        .and_then(|reset| parse_claude_reset(reset, now));
    Some(SubagentError::RateLimited {
        message: line.to_owned(),
        retry_at,
    })
}

fn parse_claude_reset(reset: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(reset) {
        return Some(timestamp.to_utc());
    }
    let (time, zone) = reset.strip_suffix(')')?.rsplit_once(" (")?;
    let zone = zone.parse::<chrono_tz::Tz>().ok()?;
    let time = NaiveTime::parse_from_str(time, "%I:%M%P")
        .ok()
        .or_else(|| {
            let (hour, meridiem) = time.split_at_checked(time.len().checked_sub(2)?)?;
            NaiveTime::parse_from_str(&format!("{hour}:00{meridiem}"), "%I:%M%P").ok()
        })?;
    let today = now.with_timezone(&zone).date_naive();
    for date in [today, today.checked_add_days(Days::new(1))?] {
        let deadline = date
            .and_time(time)
            .and_local_timezone(zone)
            .latest()?
            .to_utc();
        if deadline > now {
            return Some(deadline);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_quota_resolves_the_next_local_reset_across_midnight() {
        let now = "2026-11-18T15:58:00Z".parse().unwrap();
        let error = claude_quota(
            "You've hit your session limit · resets 1:20am (Asia/Shanghai)",
            now,
        );
        assert!(matches!(
            error,
            Some(SubagentError::RateLimited { retry_at: Some(at), .. })
                if at.to_rfc3339() == "2026-11-18T17:20:00+00:00"
        ));
        assert!(matches!(
            claude_quota("You've hit your session limit · resets later", now),
            Some(SubagentError::RateLimited { retry_at: None, .. })
        ));
        assert_eq!(
            parse_claude_reset("1am (Asia/Shanghai)", now)
                .unwrap()
                .to_rfc3339(),
            "2026-11-18T17:00:00+00:00"
        );
    }
}
