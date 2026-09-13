//! Content-addressed raw artifacts — the complete payload behind a reduced
//! projection.
//!
//! When a tool response is reduced to fit a token budget, the full payload is
//! stored here and referenced by an `obs_<hash>` id. The model sees the bounded
//! projection plus a handle; `artifact/read` returns the exact original bytes.
//!
//! Properties:
//! - **Writes are asynchronous.** `put` enqueues to a writer thread that owns
//!   its own SQLite connection (WAL), so a slow disk never blocks the browser
//!   loop. `get` checks an in-memory pending map first, so a read immediately
//!   after a write still sees the artifact (read-your-writes).
//! - **Bounded.** Per-payload cap, total-size cap with LRU eviction, and a TTL
//!   purge (on open, hourly from the writer thread, and on demand).
//! - **Recoverable-or-nothing.** `put` reports `false` when a payload exceeds
//!   the per-payload cap; callers must then keep the output unreduced instead
//!   of handing the model a lossy answer with no handle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::Serialize;

use lightbrowse_core::error::{Error, Result};

/// Artifact retention and size policy.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactConfig {
    /// How long an artifact stays readable.
    pub ttl_secs: i64,
    /// Hard cap for the whole `raw_artifacts` table (LRU-evicted by `created_at`).
    pub max_total_bytes: i64,
    /// Largest single payload that will be stored at all.
    pub max_payload_bytes: usize,
}

impl Default for ArtifactConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 24 * 60 * 60,
            max_total_bytes: 512 * 1024 * 1024,
            max_payload_bytes: 10 * 1024 * 1024,
        }
    }
}

/// One stored payload.
#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    pub id: String,
    pub tool: String,
    pub kind: String,
    pub url: Option<String>,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub tokens: usize,
    pub created_at: i64,
    pub expires_at: i64,
}

impl ArtifactRecord {
    /// Build a record: the id is content-addressed over the payload, so the
    /// same bytes always map to the same handle.
    pub fn new(
        tool: impl Into<String>,
        kind: impl Into<String>,
        content_type: impl Into<String>,
        url: Option<String>,
        bytes: Vec<u8>,
        ttl_secs: i64,
    ) -> Self {
        let now = unix_now();
        let id = format!("obs_{}", lightbrowse_core::reduce::fingerprint(&bytes));
        let tokens = lightbrowse_core::reduce::estimate_tokens(&String::from_utf8_lossy(&bytes));
        Self {
            id,
            tool: tool.into(),
            kind: kind.into(),
            url,
            content_type: content_type.into(),
            bytes,
            tokens,
            created_at: now,
            expires_at: now + ttl_secs.max(0),
        }
    }

    /// The stored payload as UTF-8 text (lossy — artifacts are text).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).to_string()
    }
}

/// Metadata for `artifact/list` (no payload).
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactMeta {
    pub id: String,
    pub tool: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub content_type: String,
    pub bytes_len: i64,
    pub tokens: i64,
    pub created_at: i64,
    pub expires_at: i64,
}

enum WriterMsg {
    Put(Box<ArtifactRecord>),
    Sync(Sender<()>),
}

/// Asynchronous, content-addressed artifact store.
pub struct ArtifactStore {
    tx: Sender<WriterMsg>,
    pending: Arc<Mutex<HashMap<String, ArtifactRecord>>>,
    reader: Mutex<Connection>,
    cfg: ArtifactConfig,
}

impl ArtifactStore {
    /// Open (or create) the store at `path`.
    ///
    /// Uses its own writer connection (background thread) plus a reader
    /// connection, both in WAL mode with a 5s busy timeout so they coexist with
    /// the page-cache writer in the same SQLite file.
    pub fn open(path: &Path, cfg: ArtifactConfig) -> Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let reader = open_connection(path)?;
        // Best-effort housekeeping on open.
        let _ = purge_expired(&reader, unix_now());
        let _ = enforce_total_cap(&reader, cfg.max_total_bytes);

        let (tx, rx) = mpsc::channel::<WriterMsg>();
        let pending: Arc<Mutex<HashMap<String, ArtifactRecord>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_writer = Arc::clone(&pending);
        let writer_path = path.to_path_buf();
        thread::Builder::new()
            .name("lightbrowse-artifacts".to_string())
            .spawn(move || writer_loop(writer_path, rx, pending_writer, cfg))
            .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;

        Ok(Self {
            tx,
            pending,
            reader: Mutex::new(reader),
            cfg,
        })
    }

    /// Enqueue a payload. Returns `false` when the payload exceeds
    /// `max_payload_bytes` and was therefore not stored — the caller must then
    /// leave the output unreduced instead of returning a projection with no
    /// handle.
    pub fn put(&self, record: ArtifactRecord) -> Result<bool> {
        if record.bytes.len() > self.cfg.max_payload_bytes {
            tracing::warn!(
                "artifact {} not stored: {} bytes exceeds the {} byte cap",
                record.id,
                record.bytes.len(),
                self.cfg.max_payload_bytes
            );
            return Ok(false);
        }
        self.pending
            .lock()
            .unwrap()
            .insert(record.id.clone(), record.clone());
        self.tx
            .send(WriterMsg::Put(Box::new(record)))
            .map_err(|_| Error::Io(std::io::Error::other("artifact writer stopped")))?;
        Ok(true)
    }

    /// Read a payload by id (pending writes included).
    pub fn get(&self, id: &str) -> Result<Option<ArtifactRecord>> {
        if let Some(record) = self.pending.lock().unwrap().get(id) {
            return Ok(Some(record.clone()));
        }
        let conn = self.reader.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, tool, kind, url, content_type, bytes, tokens, created_at, expires_at
                 FROM raw_artifacts WHERE id = ?1 AND expires_at > ?2",
            )
            .map_err(sql_err)?;
        let mut rows = stmt.query(params![id, unix_now()]).map_err(sql_err)?;
        match rows.next().map_err(sql_err)? {
            Some(row) => Ok(Some(row_to_record(row)?)),
            None => Ok(None),
        }
    }

    /// Most recent artifacts first.
    pub fn list(&self, limit: usize, tool: Option<&str>) -> Result<Vec<ArtifactMeta>> {
        let limit = limit.clamp(1, 200) as i64;
        let conn = self.reader.lock().unwrap();
        let now = unix_now();
        let mut out = Vec::new();
        match tool {
            Some(tool) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, tool, kind, url, content_type, bytes_len, tokens, created_at, expires_at
                         FROM raw_artifacts WHERE expires_at > ?1 AND tool = ?2
                         ORDER BY created_at DESC, id DESC LIMIT ?3",
                    )
                    .map_err(sql_err)?;
                let rows = stmt
                    .query_map(params![now, tool, limit], row_to_meta)
                    .map_err(sql_err)?;
                for row in rows {
                    out.push(row.map_err(sql_err)?);
                }
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, tool, kind, url, content_type, bytes_len, tokens, created_at, expires_at
                         FROM raw_artifacts WHERE expires_at > ?1
                         ORDER BY created_at DESC, id DESC LIMIT ?2",
                    )
                    .map_err(sql_err)?;
                let rows = stmt
                    .query_map(params![now, limit], row_to_meta)
                    .map_err(sql_err)?;
                for row in rows {
                    out.push(row.map_err(sql_err)?);
                }
            }
        }
        Ok(out)
    }

    /// `(artifact count, total payload bytes)`.
    pub fn stats(&self) -> Result<(i64, i64)> {
        let conn = self.reader.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(bytes_len), 0) FROM raw_artifacts WHERE expires_at > ?1",
                params![unix_now()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)?;
        Ok(row)
    }

    /// Delete expired artifacts now.
    pub fn purge_expired(&self) -> Result<usize> {
        let conn = self.reader.lock().unwrap();
        purge_expired(&conn, unix_now())
    }

    /// Block until everything enqueued so far has been written (tests, shutdown).
    pub fn flush(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WriterMsg::Sync(tx))
            .map_err(|_| Error::Io(std::io::Error::other("artifact writer stopped")))?;
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| Error::Io(std::io::Error::other("artifact writer timed out")))
    }

    /// The retention/size policy this store was opened with.
    pub fn config(&self) -> ArtifactConfig {
        self.cfg
    }

    /// Identifier a store uses for a payload, so callers and the store agree.
    pub fn id_for(payload: &[u8]) -> String {
        format!("obs_{}", lightbrowse_core::reduce::fingerprint(payload))
    }
}

fn sql_err(e: rusqlite::Error) -> Error {
    Error::Parse(format!("artifact store: {e}"))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    crate::ensure_schema(&conn)?;
    Ok(conn)
}

fn row_to_record(row: &rusqlite::Row<'_>) -> Result<ArtifactRecord> {
    Ok(ArtifactRecord {
        id: row.get(0).map_err(sql_err)?,
        tool: row.get(1).map_err(sql_err)?,
        kind: row.get(2).map_err(sql_err)?,
        url: row.get(3).map_err(sql_err)?,
        content_type: row.get(4).map_err(sql_err)?,
        bytes: row.get(5).map_err(sql_err)?,
        tokens: row.get::<_, i64>(6).map_err(sql_err)?.max(0) as usize,
        created_at: row.get(7).map_err(sql_err)?,
        expires_at: row.get(8).map_err(sql_err)?,
    })
}

fn row_to_meta(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactMeta> {
    Ok(ArtifactMeta {
        id: row.get(0)?,
        tool: row.get(1)?,
        kind: row.get(2)?,
        url: row.get(3)?,
        content_type: row.get(4)?,
        bytes_len: row.get(5)?,
        tokens: row.get(6)?,
        created_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

fn insert_artifact(conn: &Connection, record: &ArtifactRecord) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO raw_artifacts
         (id, tool, kind, url, content_type, bytes, bytes_len, tokens, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            record.id,
            record.tool,
            record.kind,
            record.url,
            record.content_type,
            record.bytes,
            record.bytes.len() as i64,
            record.tokens as i64,
            record.created_at,
            record.expires_at,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn purge_expired(conn: &Connection, now: i64) -> Result<usize> {
    conn.execute(
        "DELETE FROM raw_artifacts WHERE expires_at <= ?1",
        params![now],
    )
    .map_err(sql_err)
}

fn total_artifact_bytes(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(bytes_len), 0) FROM raw_artifacts",
        [],
        |r| r.get(0),
    )
    .map_err(sql_err)
}

/// Evict oldest artifacts until the table is within `max_total_bytes`.
fn enforce_total_cap(conn: &Connection, max_total_bytes: i64) -> Result<usize> {
    let total = total_artifact_bytes(conn)?;
    if total <= max_total_bytes {
        return Ok(0);
    }
    let mut removed = 0usize;
    let mut over = total - max_total_bytes;
    let mut stmt = conn
        .prepare(
            "SELECT id, bytes_len FROM raw_artifacts
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(sql_err)?;
    let mut victims: Vec<String> = Vec::new();
    for row in rows {
        let (id, len) = row.map_err(sql_err)?;
        if over <= 0 {
            break;
        }
        over -= len;
        victims.push(id);
        removed += 1;
    }
    drop(stmt);
    if !victims.is_empty() {
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        for id in &victims {
            tx.execute("DELETE FROM raw_artifacts WHERE id = ?1", params![id])
                .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        tracing::info!("artifact store: evicted {removed} artifact(s) over the size cap");
    }
    Ok(removed)
}

fn writer_loop(
    path: PathBuf,
    rx: mpsc::Receiver<WriterMsg>,
    pending: Arc<Mutex<HashMap<String, ArtifactRecord>>>,
    cfg: ArtifactConfig,
) {
    let conn = match open_connection(&path) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::error!("artifact writer: cannot open {}: {e}", path.display());
            return;
        }
    };
    let mut last_purge = unix_now();
    // Running total, so the size cap is enforced on every write without
    // re-scanning the table each time.
    let mut total_bytes = total_artifact_bytes(&conn).unwrap_or(0);
    while let Ok(msg) = rx.recv() {
        match msg {
            WriterMsg::Put(record) => {
                if let Err(e) = insert_artifact(&conn, &record) {
                    tracing::warn!("artifact writer: insert {} failed: {e}", record.id);
                }
                // Only forget the pending copy once it is durable.
                pending.lock().unwrap().remove(&record.id);
                total_bytes += record.bytes.len() as i64;
                if total_bytes > cfg.max_total_bytes {
                    let _ = enforce_total_cap(&conn, cfg.max_total_bytes);
                    total_bytes = total_artifact_bytes(&conn).unwrap_or(0);
                }
                if unix_now() - last_purge >= 3600 {
                    let _ = purge_expired(&conn, unix_now());
                    total_bytes = total_artifact_bytes(&conn).unwrap_or(0);
                    last_purge = unix_now();
                }
            }
            WriterMsg::Sync(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("lightbrowse-artifacts-{tag}-{unique}.db"))
    }

    fn record(tool: &str, payload: &str, ttl_secs: i64) -> ArtifactRecord {
        ArtifactRecord::new(
            tool,
            "page_text",
            "application/json",
            Some("https://example.test/".to_string()),
            payload.as_bytes().to_vec(),
            ttl_secs,
        )
    }

    fn store(tag: &str, cfg: ArtifactConfig) -> (ArtifactStore, PathBuf) {
        let path = temp_path(tag);
        let store = ArtifactStore::open(&path, cfg).expect("open store");
        (store, path)
    }

    #[test]
    fn id_is_content_addressed() {
        let a = record("navigate", "same", 60);
        let b = record("snapshot", "same", 60);
        assert_eq!(a.id, b.id, "id depends on payload bytes only");
        assert_eq!(a.id, ArtifactStore::id_for(b"same"));
        let c = record("navigate", "different", 60);
        assert_ne!(a.id, c.id);
    }

    #[test]
    fn put_then_read_roundtrips_bytes() {
        let (store, path) = store("roundtrip", ArtifactConfig::default());
        let payload = r#"{"url":"https://example.test/","text":"hello"}"#;
        let rec = record("navigate", payload, 60);
        let id = rec.id.clone();
        assert!(store.put(rec).unwrap());
        assert_eq!(store.get(&id).unwrap().unwrap().bytes, payload.as_bytes());
        store.flush().unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().bytes, payload.as_bytes());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_your_writes_before_the_writer_flushes() {
        let (store, path) = store("pending", ArtifactConfig::default());
        let rec = record("extract", "payload", 60);
        let id = rec.id.clone();
        assert!(store.put(rec).unwrap());
        // No flush(): the pending map must serve the read.
        let got = store.get(&id).unwrap().expect("pending artifact readable");
        assert_eq!(got.text(), "payload");
        store.flush().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn oversized_payload_is_refused_not_truncated() {
        let cfg = ArtifactConfig {
            max_payload_bytes: 16,
            ..ArtifactConfig::default()
        };
        let (store, path) = store("oversize", cfg);
        let rec = record("navigate", &"x".repeat(64), 60);
        let id = rec.id.clone();
        assert!(!store.put(rec).unwrap());
        assert!(store.get(&id).unwrap().is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expired_artifacts_are_purged() {
        let (store, path) = store("ttl", ArtifactConfig::default());
        let rec = record("navigate", "old", -10);
        let id = rec.id.clone();
        assert!(store.put(rec).unwrap());
        store.flush().unwrap();
        assert!(store.get(&id).unwrap().is_none());
        assert_eq!(store.purge_expired().unwrap(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn total_size_cap_evicts_oldest_first() {
        let cfg = ArtifactConfig {
            max_total_bytes: 400,
            ..ArtifactConfig::default()
        };
        let (store, path) = store("cap", cfg);
        let mut ids = Vec::new();
        for index in 0..6 {
            let rec = record("navigate", &format!("{index}").repeat(100), 3600);
            ids.push(rec.id.clone());
            assert!(store.put(rec).unwrap());
        }
        store.flush().unwrap();
        let (count, bytes) = store.stats().unwrap();
        assert!(
            bytes <= 400,
            "cap not enforced: {bytes} bytes in {count} artifacts"
        );
        let listed = store.list(50, None).unwrap();
        assert!(listed.len() < 6, "expected eviction, got {}", listed.len());
        // The newest artifact must survive an LRU eviction.
        assert!(store.get(ids.last().unwrap()).unwrap().is_some());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn list_filters_by_tool_and_orders_newest_first() {
        let (store, path) = store("list", ArtifactConfig::default());
        for (tool, payload) in [("navigate", "a"), ("snapshot", "b"), ("navigate", "c")] {
            assert!(store.put(record(tool, payload, 3600)).unwrap());
        }
        store.flush().unwrap();
        assert_eq!(store.list(10, None).unwrap().len(), 3);
        let nav = store.list(10, Some("navigate")).unwrap();
        assert_eq!(nav.len(), 2);
        assert!(nav.iter().all(|m| m.tool == "navigate"));
        let snap = store.list(10, Some("snapshot")).unwrap();
        assert_eq!(snap.len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn artifacts_survive_reopen() {
        let path = temp_path("reopen");
        let id = {
            let store = ArtifactStore::open(&path, ArtifactConfig::default()).unwrap();
            let rec = record("navigate", "durable", 3600);
            let id = rec.id.clone();
            assert!(store.put(rec).unwrap());
            store.flush().unwrap();
            id
        };
        let store = ArtifactStore::open(&path, ArtifactConfig::default()).unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().text(), "durable");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn page_cache_and_artifacts_share_one_file() {
        let path = temp_path("shared");
        let memory = crate::MemoryStore::open(Some(path.to_str().unwrap())).unwrap();
        let store = ArtifactStore::open(&path, ArtifactConfig::default()).unwrap();
        let rec = record("navigate", "shared", 3600);
        let id = rec.id.clone();
        assert!(store.put(rec).unwrap());
        store.flush().unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().text(), "shared");
        drop(memory);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn schema_version_is_bumped_and_legacy_scrub_still_runs() {
        let path = temp_path("version");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE runbooks (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE,
             url TEXT NOT NULL, steps TEXT NOT NULL, created_at INTEGER NOT NULL,
             last_used_at INTEGER NOT NULL, success_count INTEGER NOT NULL DEFAULT 0);
             INSERT INTO runbooks (name, url, steps, created_at, last_used_at)
             VALUES ('login-legacy', 'https://example.test/', '{}', 1, 1);",
        )
        .unwrap();
        drop(conn);

        let store = ArtifactStore::open(&path, ArtifactConfig::default()).unwrap();
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let legacy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runbooks WHERE name = 'login-legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 0, "legacy login runbook must still be scrubbed");
        drop(conn);
        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
