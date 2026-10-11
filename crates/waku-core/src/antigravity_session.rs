//! Discovery and (degraded) history import for Antigravity (agy) sessions.
//!
//! agy persists every conversation under `~/.gemini/antigravity-cli/`:
//! one SQLite database per conversation in `conversations/<uuid>.db`, plus a
//! global `conversation_summaries.db` index. The summary index carries plain
//! text columns — title, preview, workspace URIs, timestamps, status — and is
//! the discovery surface here. The per-conversation `steps` table stores
//! protobuf blobs with no published schema, so turn-level history import is
//! intentionally unavailable: `provider_session_history` returns an empty
//! history, and Waku falls back to its own transcript for every task it
//! started itself.
//!
//! Like `grok_session`, this module discovers real sessions but only resumes
//! them through the driver; the summaries index is read-only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::DateTime;
use rusqlite::Connection;
use serde_json::Value;
use uuid::Uuid;

use waku_protocol::model::{
    ProviderKind, ProviderResumeCursor, ProviderSessionHistory, ProviderSessionSummary,
};

const AGY_HOME: &str = ".gemini/antigravity-cli";
const SUMMARIES_DB: &str = "conversation_summaries.db";

fn agy_cli_home() -> Option<PathBuf> {
    dirs::home_dir()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|home| home.join(AGY_HOME))
}

/// Sessions this Waku install started are tracked in its own store; native
/// discovery exists so a user can adopt conversations agy started itself.
pub fn list_provider_sessions(limit: usize) -> anyhow::Result<Vec<ProviderSessionSummary>> {
    let Some(home) = agy_cli_home() else {
        return Ok(Vec::new());
    };
    let db_path = home.join(SUMMARIES_DB);
    if !db_path.is_file() {
        return Ok(Vec::new());
    }
    // Open read-only: the CLI owns this database and a locked write handle
    // must never turn discovery into a corrupting writer.
    let connection =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("failed to open {}", db_path.display()))?;

    let mut sessions = Vec::new();
    let mut statement = connection.prepare(
        "SELECT conversation_id, title, preview, workspace_uris, status,
                last_modified_time, last_user_input_time, killed
         FROM conversation_summaries",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let conversation_id: String = row.get("conversation_id")?;
        if Uuid::parse_str(&conversation_id).is_err() {
            continue;
        }
        if row.get::<_, i64>("killed").unwrap_or(0) != 0 {
            continue;
        }
        let status: String = row.get("status")?;
        // Nested/battle conversations are managed by their parents.
        if status.is_empty() {
            continue;
        }
        let title = [
            row.get::<_, String>("title")?,
            row.get::<_, String>("preview")?,
        ]
        .into_iter()
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| format!("agy session {}", &conversation_id[..8]));
        let workspace_uris: String = row.get("workspace_uris")?;
        let Some(cwd) = first_workspace_path(&workspace_uris) else {
            continue;
        };
        let updated_at = row
            .get::<_, String>("last_modified_time")?
            .parse::<DateTime<chrono::Utc>>()
            .ok()
            .map(|time| time.timestamp().max(0) as u64)
            .unwrap_or(0);
        let created_at = row
            .get::<_, String>("last_user_input_time")?
            .parse::<DateTime<chrono::Utc>>()
            .ok()
            .map(|time| time.timestamp().max(0) as u64)
            .unwrap_or(updated_at)
            .min(updated_at.max(1));
        sessions.push(ProviderSessionSummary {
            cursor: ProviderResumeCursor::Antigravity { conversation_id },
            title,
            cwd,
            created_at,
            updated_at,
        });
    }
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    sessions.truncate(limit);
    Ok(sessions)
}

/// `workspace_uris` is a JSON array of `file://` URIs; the first absolute
/// directory is the canonical workspace.
fn first_workspace_path(workspace_uris: &str) -> Option<PathBuf> {
    let value = serde_json::from_str::<Value>(workspace_uris).ok()?;
    let uris = value.as_array()?;
    uris.iter().filter_map(Value::as_str).find_map(|uri| {
        let path = uri.strip_prefix("file://")?;
        let path = Path::new(path);
        path.is_absolute().then(|| path.to_path_buf())
    })
}

/// Turn-level import is unavailable — the steps table is protobuf without a
/// published schema — so adoption starts from an empty transcript and the
/// resumed conversation's own memory.
pub fn provider_session_history(
    _conversation_id: &str,
    _visible_turn_limit: usize,
) -> anyhow::Result<ProviderSessionHistory> {
    Ok(ProviderSessionHistory::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_workspace_path_reads_file_uris() {
        assert_eq!(
            first_workspace_path(r#"["file:///data/experiments","file:///tmp"]"#),
            Some(PathBuf::from("/data/experiments"))
        );
        assert_eq!(first_workspace_path("[]"), None);
        assert_eq!(first_workspace_path("not json"), None);
    }

    #[test]
    fn empty_summaries_db_lists_nothing() {
        let root = std::env::temp_dir().join(format!("agy-session-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        // No summaries db at all: discovery degrades to an empty list.
        let db_path = root.join(SUMMARIES_DB);
        let connection = Connection::open(&db_path).unwrap();
        connection
            .execute(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT, title TEXT, preview TEXT,
                    workspace_uris TEXT, status TEXT, last_modified_time TEXT,
                    last_user_input_time TEXT, killed NUMERIC)",
                [],
            )
            .unwrap();
        drop(connection);
        // Point the module at the temp home through the documented override.
        // SAFETY: single-threaded test process; no other test reads HOME.
        unsafe {
            std::env::set_var("HOME", &root);
        }
        let sessions = list_provider_sessions(10).unwrap();
        // SAFETY: same single-threaded scope as above.
        unsafe {
            std::env::remove_var("HOME");
        }
        std::fs::remove_dir_all(&root).unwrap();
        assert!(sessions.is_empty());
    }
}
