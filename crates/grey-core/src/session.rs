//! SQLite-backed, lossless conversation persistence.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ChatMessage;

const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: Vec<ChatMessage>,
}

impl Session {
    pub fn new(
        title: impl Into<String>,
        workspace: impl Into<String>,
        messages: Vec<ChatMessage>,
    ) -> Self {
        let now = unix_timestamp();
        Self {
            id: Uuid::new_v4().to_string(),
            title: title.into(),
            workspace: workspace.into(),
            created_at: now,
            updated_at: now,
            messages,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: usize,
}

pub struct SessionStore {
    connection: Mutex<Connection>,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating session directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("opening session database {}", path.display()))?;
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn save(&self, session: &mut Session) -> Result<()> {
        session.updated_at = unix_timestamp();
        let messages = serde_json::to_string(&session.messages)?;
        self.connection.lock().unwrap().execute(
            "INSERT INTO sessions
             (id, title, workspace, created_at, updated_at, messages_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               title = excluded.title,
               workspace = excluded.workspace,
               updated_at = excluded.updated_at,
               messages_json = excluded.messages_json",
            params![
                session.id,
                session.title,
                session.workspace,
                session.created_at,
                session.updated_at,
                messages
            ],
        )?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Option<Session>> {
        self.connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, title, workspace, created_at, updated_at, messages_json
                 FROM sessions WHERE id = ?1",
                [id],
                row_to_session,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT id, title, workspace, created_at, updated_at, messages_json
             FROM sessions ORDER BY updated_at DESC, id ASC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.max(1) as i64], |row| {
            let messages_json: String = row.get(5)?;
            let message_count = serde_json::from_str::<Vec<ChatMessage>>(&messages_json)
                .map(|messages| messages.len())
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        messages_json.len(),
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                workspace: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                message_count,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn latest_for_workspace(&self, workspace: &str) -> Result<Option<Session>> {
        self.connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, title, workspace, created_at, updated_at, messages_json
                 FROM sessions WHERE workspace = ?1
                 ORDER BY updated_at DESC, id ASC LIMIT 1",
                [workspace],
                row_to_session,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn save_usage(&self, session_id: &str, usage_json: &str) -> Result<()> {
        let changed = self.connection.lock().unwrap().execute(
            "UPDATE sessions SET usage_json = ?1 WHERE id = ?2",
            params![usage_json, session_id],
        )?;
        anyhow::ensure!(changed == 1, "session not found: {session_id}");
        Ok(())
    }

    pub fn load_usage(&self, session_id: &str) -> Result<Option<String>> {
        self.connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT usage_json FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|opt| opt.flatten())
            .map_err(Into::into)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connection.lock().unwrap();
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            connection.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS sessions (
                   id TEXT PRIMARY KEY,
                   title TEXT NOT NULL,
                   workspace TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL,
                   messages_json TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS sessions_workspace_updated
                   ON sessions(workspace, updated_at DESC);
                 PRAGMA user_version = 1;
                 COMMIT;",
            )?;
        }
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 2 {
            connection.execute_batch(
                "BEGIN;
                 ALTER TABLE sessions ADD COLUMN usage_json TEXT;
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }
        let current: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            current == SCHEMA_VERSION,
            "unsupported session database schema {current}"
        );
        Ok(())
    }
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let messages_json: String = row.get(5)?;
    let messages = serde_json::from_str(&messages_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            messages_json.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        workspace: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        messages,
    })
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Role, ToolCall};

    #[test]
    fn saves_lists_and_loads_tool_messages_losslessly() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&directory.path().join("sessions.db")).unwrap();
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "README.md"}),
        };
        let messages = vec![
            ChatMessage::new(Role::System, "system"),
            ChatMessage::assistant("", vec![call.clone()]),
            ChatMessage::tool_result(&call, "contents"),
        ];
        let mut session = Session::new("test", "/workspace", messages.clone());
        store.save(&mut session).unwrap();

        let loaded = store.load(&session.id).unwrap().unwrap();
        assert_eq!(loaded.messages, messages);
        let summaries = store.list(10).unwrap();
        assert_eq!(summaries[0].id, session.id);
        assert_eq!(summaries[0].message_count, 3);
        assert_eq!(
            store
                .latest_for_workspace("/workspace")
                .unwrap()
                .unwrap()
                .id,
            session.id
        );
    }

    #[test]
    fn schema_v2_migration_adds_usage_json_column() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("sessions.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                   id TEXT PRIMARY KEY,
                   title TEXT NOT NULL,
                   workspace TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL,
                   messages_json TEXT NOT NULL
                 );
                 CREATE INDEX sessions_workspace_updated
                   ON sessions(workspace, updated_at DESC);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (id, title, workspace, created_at, updated_at, messages_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!["s1", "old", "/ws", 100, 100, "[]"],
            )
            .unwrap();
        }

        let store = SessionStore::open(&db_path).unwrap();
        let loaded = store.load("s1").unwrap().unwrap();
        assert_eq!(loaded.title, "old");
    }

    #[test]
    fn save_and_load_usage_json_roundtrip() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&directory.path().join("sessions.db")).unwrap();

        let mut session = Session::new("test", "/workspace", vec![]);
        store.save(&mut session).unwrap();

        assert_eq!(store.load_usage(&session.id).unwrap(), None);

        store
            .save_usage(&session.id, r#"{"total_input_tokens":42}"#)
            .unwrap();

        let loaded = store.load_usage(&session.id).unwrap().unwrap();
        assert!(loaded.contains("42"));
    }
}
