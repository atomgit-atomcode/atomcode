//! `session/list` handler: session history discovery over the live table and
//! the native session catalog.
//!
//! The protocol's "session history" is the merged view of live ACP sessions
//! plus persisted (closed) sessions from the native [`SessionManager`] catalog
//! shared with the CLI/TUI. This module owns that merge, the keyset pagination
//! (cursor tokens are opaque to clients but self-describing here), and the
//! `cwd` filter.

use agent_client_protocol::schema::v1::{ListSessionsRequest, ListSessionsResponse, SessionInfo};
use agent_client_protocol::Error as AcpError;
use atomcode_capabilities::session::{CatalogPresence, CatalogScan, SessionManager, SessionMeta};

use crate::acp::sessions::{wire_session_id, Sessions};

// ── session/list handler ─────────────────────────────────────────────────────

/// Maximum number of sessions returned per `session/list` page. The protocol
/// asks agents to enforce a reasonable internal page size (SHOULD); the full
/// table is paginated instead of returned in one response.
pub const SESSION_LIST_PAGE_SIZE: usize = 50;

/// Cursor token prefix. Cursors are opaque to clients, but they are
/// self-describing here so an undecodable token can be rejected (the protocol
/// asks agents to error on invalid cursors) instead of silently returning an
/// empty page.
const SESSION_LIST_CURSOR_PREFIX: &str = "atomcode-v1:";

/// Encode the last session id of a page into the opaque `nextCursor` token.
fn encode_list_cursor(last_session_id: &str) -> String {
    format!("{SESSION_LIST_CURSOR_PREFIX}{last_session_id}")
}

/// Decode a request cursor back into the last session id of the previous page.
fn decode_list_cursor(cursor: &str) -> Result<String, AcpError> {
    cursor
        .strip_prefix(SESSION_LIST_CURSOR_PREFIX)
        .map(str::to_string)
        .ok_or_else(|| {
            AcpError::invalid_params().data(format!("invalid session/list cursor `{cursor}`"))
        })
}

/// Handle a `session/list` request over the live table AND the native
/// session catalog (the single persistence owner).
///
/// The merged list is exactly the protocol's "session history": live sessions
/// plus persisted (closed) sessions from the native catalog shared with the
/// CLI/TUI. Entries are deduplicated by native id (the live entry wins for
/// cwd), titled from the native `SessionMeta.name` (fallback names are
/// omitted), sorted by wire id, and paginated cursor-style. Legacy-only core
/// records are excluded — they cannot be resumed by the native pipeline.
pub async fn handle_list_sessions(
    sessions: &Sessions,
    req: &ListSessionsRequest,
    scan: &CatalogScan,
) -> Result<ListSessionsResponse, AcpError> {
    if let Some(cwd) = &req.cwd {
        if !cwd.is_absolute() {
            return Err(AcpError::invalid_params().data(format!(
                "session/list cwd filter must be an absolute path (got `{}`)",
                cwd.display()
            )));
        }
    }
    let after: Option<String> = req.cursor.as_deref().map(decode_list_cursor).transpose()?;

    let map = sessions.lock().await;
    // Native id → (cwd, title, additional_directories): live entries win over
    // catalog entries. The live entry reports the roots requested at setup;
    // persisted (closed) entries have none stored (the protocol requires
    // clients to re-send the full list on load/resume — it is not restored).
    let mut merged: std::collections::BTreeMap<
        String,
        (std::path::PathBuf, Option<String>, Vec<std::path::PathBuf>),
    > = std::collections::BTreeMap::new();
    for state in map.values() {
        merged.insert(
            state.native_id.clone(),
            (
                state.cwd.clone(),
                None,
                state.additional_directories.clone(),
            ),
        );
    }
    let mut catalog = scan.entries.clone();
    SessionManager::collapse_fork_lineages(&mut catalog);
    for entry in catalog {
        if entry.presence == CatalogPresence::LegacyOnly {
            // Historical core JSON only — the native resume pipeline cannot
            // restore it, so it must not be advertised as resumable history.
            continue;
        }
        let title = (!entry.name.is_empty()
            && !SessionMeta::name_needs_fallback(&entry.name, &entry.id))
        .then_some(entry.name);
        merged
            .entry(entry.id)
            .and_modify(|(cwd, live_title, _additional)| {
                let _ = cwd;
                if live_title.is_none() {
                    *live_title = title.clone();
                }
            })
            .or_insert((entry.working_dir, title, Vec::new()));
    }

    let mut infos: Vec<SessionInfo> = merged
        .into_iter()
        .filter(|(_, (cwd, _, _))| req.cwd.as_ref().map(|filter| cwd == filter).unwrap_or(true))
        .map(|(native_id, (cwd, title, additional))| {
            let mut info = SessionInfo::new(wire_session_id(&native_id), cwd);
            if !additional.is_empty() {
                info = info.additional_directories(additional);
            }
            if let Some(title) = title {
                info = info.title(title);
            }
            info
        })
        .collect();
    infos.sort_by(|a, b| a.session_id.0.cmp(&b.session_id.0));

    // Keyset pagination: the cursor is the last id of the previous page; this
    // page starts strictly after it. A cursor naming a session that has since
    // been deleted naturally degrades to "everything after that id".
    let start = match &after {
        Some(after) => infos
            .iter()
            .position(|info| info.session_id.0.as_ref() > after.as_str())
            .unwrap_or(infos.len()),
        None => 0,
    };
    let total = infos.len();
    let end = std::cmp::min(start + SESSION_LIST_PAGE_SIZE, total);
    let page: Vec<SessionInfo> = infos[start..end].to_vec();
    let next_cursor = (end < total)
        .then(|| {
            page.last()
                .map(|info| encode_list_cursor(info.session_id.0.as_ref()))
        })
        .flatten();

    Ok(ListSessionsResponse::new(page).next_cursor(next_cursor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ListSessionsRequest, SessionId};
    use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

    use crate::acp::sessions::test_support::{catalog_entry, empty_scan, sessions_with};
    use crate::acp::sessions::Sessions;

    #[tokio::test]
    async fn list_returns_all_sessions_with_cwd() {
        let sessions = sessions_with(vec![("acp-2", "/b"), ("acp-1", "/a")]);

        let resp = handle_list_sessions(&sessions, &ListSessionsRequest::new(), &empty_scan())
            .await
            .unwrap();
        let ids: Vec<String> = resp
            .sessions
            .iter()
            .map(|s| s.session_id.0.to_string())
            .collect();
        let cwds: Vec<std::path::PathBuf> = resp.sessions.iter().map(|s| s.cwd.clone()).collect();
        assert_eq!(ids, vec!["acp-1", "acp-2"], "sorted by session id");
        assert_eq!(
            cwds,
            vec![
                std::path::PathBuf::from("/a"),
                std::path::PathBuf::from("/b")
            ]
        );
        assert!(resp.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_respects_cwd_filter() {
        let sessions = sessions_with(vec![("acp-1", "/work-a"), ("acp-2", "/work-b")]);

        let req = ListSessionsRequest::new().cwd("/work-b");
        let resp = handle_list_sessions(&sessions, &req, &empty_scan())
            .await
            .unwrap();
        let ids: Vec<String> = resp
            .sessions
            .iter()
            .map(|s| s.session_id.0.to_string())
            .collect();
        assert_eq!(ids, vec!["acp-2"]);
    }

    #[tokio::test]
    async fn list_reports_live_additional_directories() {
        let sessions = sessions_with(vec![("acp-1", "/work")]);
        {
            let infos = handle_list_sessions(&sessions, &ListSessionsRequest::new(), &empty_scan())
                .await
                .unwrap()
                .sessions;
            assert_eq!(
                infos[0].additional_directories.len(),
                0,
                "no additional roots on a plain session"
            );
        }
        // Give the live session additional roots and list again: they must be
        // reported back on `SessionInfo.additionalDirectories`.
        sessions
            .lock()
            .await
            .get_mut("acp-1")
            .unwrap()
            .additional_directories = vec![
            std::path::PathBuf::from("/shared"),
            std::path::PathBuf::from("/docs"),
        ];

        let infos = handle_list_sessions(&sessions, &ListSessionsRequest::new(), &empty_scan())
            .await
            .unwrap()
            .sessions;
        assert_eq!(
            infos[0].additional_directories,
            vec![
                std::path::PathBuf::from("/shared"),
                std::path::PathBuf::from("/docs")
            ]
        );
    }

    #[tokio::test]
    async fn list_empty_table_returns_empty() {
        let sessions: Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let resp = handle_list_sessions(&sessions, &ListSessionsRequest::new(), &empty_scan())
            .await
            .unwrap();
        assert!(resp.sessions.is_empty());
    }

    #[tokio::test]
    async fn list_paginates_with_cursor_and_rejects_invalid_cursor() {
        // 3 sessions > page size? No — the page size is 50. Build a table larger
        // than one page by inserting SESSION_LIST_PAGE_SIZE + 2 sessions.
        let entries: Vec<(String, &str)> = (0..SESSION_LIST_PAGE_SIZE + 2)
            .map(|n| (format!("acp-{n:03}"), "/work"))
            .collect();
        let refs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(id, cwd)| (id.as_str(), *cwd))
            .collect();
        let sessions = sessions_with(refs);

        // First page: page-size entries, sorted by id, with a next_cursor.
        let page1 = handle_list_sessions(&sessions, &ListSessionsRequest::new(), &empty_scan())
            .await
            .unwrap();
        assert_eq!(page1.sessions.len(), SESSION_LIST_PAGE_SIZE);
        assert_eq!(page1.sessions[0].session_id.0.as_ref(), "acp-000");
        let cursor = page1.next_cursor.clone().expect("more pages follow");

        // Second page: the remaining 2 entries, no next_cursor.
        let req = ListSessionsRequest::new().cursor(cursor.clone());
        let page2 = handle_list_sessions(&sessions, &req, &empty_scan())
            .await
            .unwrap();
        assert_eq!(page2.sessions.len(), 2);
        assert_eq!(
            page2.sessions[0].session_id.0.as_ref(),
            format!("acp-{:03}", SESSION_LIST_PAGE_SIZE)
        );
        assert!(page2.next_cursor.is_none());

        // An undecodable cursor is an invalid-params error, not an empty page.
        let req = ListSessionsRequest::new().cursor("garbage");
        let err = handle_list_sessions(&sessions, &req, &empty_scan())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("invalid"), "cursor error: {text}");
    }

    #[tokio::test]
    async fn list_rejects_relative_cwd_filter() {
        let sessions = sessions_with(vec![("acp-1", "/work")]);
        let req = ListSessionsRequest::new().cwd("relative/dir");
        let err = handle_list_sessions(&sessions, &req, &empty_scan())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[tokio::test]
    async fn list_merges_persisted_catalog_and_dedupes_live() {
        // Live session `acp-1` (native "1") ALSO has a native record with a
        // title; the catalog adds closed session "2" and a legacy-only record
        // that must NOT be advertised (it cannot be resumed natively).
        let sessions = sessions_with(vec![("acp-1", "/a")]);
        let scan = CatalogScan {
            entries: vec![
                catalog_entry("1", "Live titled", "/a"),
                catalog_entry("2", "Closed session", "/b"),
                CatalogEntry {
                    presence: CatalogPresence::LegacyOnly,
                    ..catalog_entry("3", "legacy", "/c")
                },
            ],
            diagnostics: Vec::new(),
        };

        let resp = handle_list_sessions(&sessions, &ListSessionsRequest::new(), &scan)
            .await
            .unwrap();
        let ids: Vec<String> = resp
            .sessions
            .iter()
            .map(|s| s.session_id.0.to_string())
            .collect();
        assert_eq!(ids, vec!["acp-1", "acp-2"], "merged + legacy excluded");
        // The live entry picks up the catalog title.
        assert_eq!(resp.sessions[0].title.as_deref(), Some("Live titled"));
        assert_eq!(resp.sessions[1].title.as_deref(), Some("Closed session"));
    }

    #[tokio::test]
    async fn list_omits_fallback_titles() {
        // A meta whose name is the untouched placeholder must not leak a
        // synthetic title to the client.
        let sessions: Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let scan = CatalogScan {
            entries: vec![catalog_entry("n1", "default", "/a")],
            diagnostics: Vec::new(),
        };
        let resp = handle_list_sessions(&sessions, &ListSessionsRequest::new(), &scan)
            .await
            .unwrap();
        assert_eq!(resp.sessions.len(), 1);
        assert_eq!(resp.sessions[0].title, None, "fallback name is not a title");
    }
}
