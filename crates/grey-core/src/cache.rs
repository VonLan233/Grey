//! SQLite-backed request prefix cache.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ChatMessage, ToolCall, Usage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub cached_at: i64,
}

#[derive(Debug, Clone)]
pub struct RequestCacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    pub ttl_hours: u64,
}

impl Default for RequestCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 1000,
            ttl_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
}

pub struct RequestCache {
    connection: Mutex<Connection>,
    config: RequestCacheConfig,
    hits: Mutex<u64>,
    misses: Mutex<u64>,
}

impl RequestCache {
    pub fn open(path: &Path, config: RequestCacheConfig) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cache directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("opening cache database {}", path.display()))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS cache_entries (
                key TEXT PRIMARY KEY,
                model TEXT NOT NULL,
                response_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS cache_entries_last_accessed
                ON cache_entries(last_accessed ASC);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            config,
            hits: Mutex::new(0),
            misses: Mutex::new(0),
        })
    }

    pub fn get(&self, model: &str, messages: &[ChatMessage]) -> Option<CachedResponse> {
        self.get_for_provider("", model, messages)
    }

    pub fn get_for_provider(
        &self,
        provider: &str,
        model: &str,
        messages: &[ChatMessage],
    ) -> Option<CachedResponse> {
        if !self.config.enabled {
            return None;
        }
        let key = cache_key(provider, model, messages);
        let now = unix_timestamp();
        let ttl_secs = self.config.ttl_hours * 3600;

        let result = self
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT response_json, created_at FROM cache_entries WHERE key = ?1",
                [&key],
                |row| {
                    let json: String = row.get(0)?;
                    let created: i64 = row.get(1)?;
                    Ok((json, created))
                },
            )
            .optional()
            .ok()
            .flatten();

        let (json, created) = match result {
            Some(v) => v,
            None => {
                *self.misses.lock().unwrap() += 1;
                return None;
            }
        };

        if now - created >= ttl_secs as i64 {
            *self.misses.lock().unwrap() += 1;
            let _ = self
                .connection
                .lock()
                .unwrap()
                .execute("DELETE FROM cache_entries WHERE key = ?1", [&key]);
            return None;
        }

        let _ = self.connection.lock().unwrap().execute(
            "UPDATE cache_entries SET last_accessed = ?1 WHERE key = ?2",
            params![now, key],
        );

        let mut cached: CachedResponse = serde_json::from_str(&json).ok()?;
        cached.cached_at = created;
        *self.hits.lock().unwrap() += 1;
        Some(cached)
    }

    pub fn put(
        &self,
        model: &str,
        messages: &[ChatMessage],
        response: &CachedResponse,
    ) -> Result<()> {
        self.put_for_provider("", model, messages, response)
    }

    pub fn put_for_provider(
        &self,
        provider: &str,
        model: &str,
        messages: &[ChatMessage],
        response: &CachedResponse,
    ) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let key = cache_key(provider, model, messages);
        let now = unix_timestamp();
        let json = serde_json::to_string(response)?;

        self.connection.lock().unwrap().execute(
            "INSERT OR REPLACE INTO cache_entries (key, model, response_json, created_at, last_accessed)\n             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![key, model, json, now],
        )?;

        self.evict_lru()?;
        Ok(())
    }

    fn evict_lru(&self) -> Result<()> {
        let conn = self.connection.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM cache_entries", [], |row| row.get(0))?;
        let excess = count.saturating_sub(self.config.max_entries as i64);
        if excess > 0 {
            conn.execute(
                "DELETE FROM cache_entries WHERE key IN (
                    SELECT key FROM cache_entries
                    ORDER BY last_accessed ASC LIMIT ?1\n                )",
                params![excess],
            )?;
        }
        Ok(())
    }

    pub fn evict_expired(&self) -> Result<usize> {
        let now = unix_timestamp();
        let ttl_secs = self.config.ttl_hours * 3600;
        let cutoff = now - ttl_secs as i64;
        let conn = self.connection.lock().unwrap();
        let count = conn.execute(
            "DELETE FROM cache_entries WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(count)
    }

    pub fn clear(&self) -> Result<()> {
        self.connection
            .lock()
            .unwrap()
            .execute("DELETE FROM cache_entries", [])?;
        Ok(())
    }

    pub fn stats(&self) -> CacheStats {
        let hits = *self.hits.lock().unwrap();
        let misses = *self.misses.lock().unwrap();
        let entries = self
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM cache_entries", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0) as usize;
        CacheStats {
            hits,
            misses,
            entries,
        }
    }
}

fn cache_key(provider: &str, model: &str, messages: &[ChatMessage]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(provider.as_bytes());
    hasher.update(b"\0");
    hasher.update(model.as_bytes());
    hasher.update(b"\0");
    let json = serde_json::to_string(messages).unwrap_or_default();
    hasher.update(json.as_bytes());
    hex::encode(hasher.finalize())
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
    use crate::Role;

    fn cached(text: &str) -> CachedResponse {
        CachedResponse {
            text: text.into(),
            tool_calls: vec![],
            usage: Usage::default(),
            cached_at: 0,
        }
    }

    fn messages(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::new(Role::User, text)]
    }

    #[test]
    fn put_and_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(&dir.path().join("cache.db"), RequestCacheConfig::default())
            .unwrap();

        let msgs = messages("hello");
        let resp = cached("world");
        cache.put("model-a", &msgs, &resp).unwrap();

        let got = cache.get("model-a", &msgs).unwrap();
        assert_eq!(got.text, "world");
    }

    #[test]
    fn different_messages_have_different_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(&dir.path().join("cache.db"), RequestCacheConfig::default())
            .unwrap();

        let msgs1 = messages("hello");
        let msgs2 = messages("goodbye");
        cache.put("m", &msgs1, &cached("a")).unwrap();
        cache.put("m", &msgs2, &cached("b")).unwrap();

        assert_eq!(cache.get("m", &msgs1).unwrap().text, "a");
        assert_eq!(cache.get("m", &msgs2).unwrap().text, "b");
    }

    #[test]
    fn different_models_have_different_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(&dir.path().join("cache.db"), RequestCacheConfig::default())
            .unwrap();

        let msgs = messages("hello");
        cache.put("model-a", &msgs, &cached("a")).unwrap();
        cache.put("model-b", &msgs, &cached("b")).unwrap();

        assert_eq!(cache.get("model-a", &msgs).unwrap().text, "a");
        assert_eq!(cache.get("model-b", &msgs).unwrap().text, "b");
    }

    #[test]
    fn ttl_expiry_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(
            &dir.path().join("cache.db"),
            RequestCacheConfig {
                enabled: true,
                max_entries: 100,
                ttl_hours: 0,
            },
        )
        .unwrap();

        let msgs = messages("hello");
        cache.put("m", &msgs, &cached("world")).unwrap();
        assert!(cache.get("m", &msgs).is_none());
    }

    #[test]
    fn lru_eviction_when_over_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(
            &dir.path().join("cache.db"),
            RequestCacheConfig {
                enabled: true,
                max_entries: 2,
                ttl_hours: 24,
            },
        )
        .unwrap();

        cache.put("m", &messages("a"), &cached("a")).unwrap();
        cache.put("m", &messages("b"), &cached("b")).unwrap();
        cache.put("m", &messages("c"), &cached("c")).unwrap();

        let stats = cache.stats();
        assert!(stats.entries <= 2, "entries = {}", stats.entries);
    }

    #[test]
    fn clear_removes_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(&dir.path().join("cache.db"), RequestCacheConfig::default())
            .unwrap();

        cache.put("m", &messages("a"), &cached("a")).unwrap();
        assert!(cache.get("m", &messages("a")).is_some());

        cache.clear().unwrap();
        assert!(cache.get("m", &messages("a")).is_none());
    }

    #[test]
    fn disabled_cache_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(
            &dir.path().join("cache.db"),
            RequestCacheConfig {
                enabled: false,
                ..Default::default()
            },
        )
        .unwrap();

        let msgs = messages("hello");
        cache.put("m", &msgs, &cached("world")).unwrap();
        assert!(cache.get("m", &msgs).is_none());
    }

    #[test]
    fn stats_track_hits_and_misses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RequestCache::open(&dir.path().join("cache.db"), RequestCacheConfig::default())
            .unwrap();

        let msgs = messages("hello");
        cache.put("m", &msgs, &cached("world")).unwrap();
        cache.get("m", &msgs);
        cache.get("m", &messages("miss"));

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }
}
