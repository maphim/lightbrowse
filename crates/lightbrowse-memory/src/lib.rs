//! `lightbrowse-memory` — browsing memory for agents.
//!
//! A tiny SQLite store (bundled, no system deps) that remembers what the
//! browser has read:
//!
//! - **page cache** with TTL — a URL fetched recently is served from memory,
//!   skipping the network entirely
//! - **block-level FTS5 index** — search what we read, ranked with BM25
//! - **recent history** — what did we look at?
//!
//! Design decision: this is *session* memory, not a knowledge graph. Semantic
//! retrieval and long-term facts are delegated to the host's memory system
//! (e.g. MemPalace via MCP) — the browser stays a browser.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lightbrowse_core::error::{Error, Result};
use lightbrowse_core::extract;
use lightbrowse_core::page::Page;
use rusqlite::{params, Connection};
use serde::Serialize;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS pages (
    id          INTEGER PRIMARY KEY,
    url         TEXT NOT NULL UNIQUE,
    final_url   TEXT,
    title       TEXT,
    status      INTEGER,
    word_count  INTEGER,
    fetched_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS blocks (
    id          INTEGER PRIMARY KEY,
    page_id     INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    level       INTEGER NOT NULL DEFAULT 0,
    text        TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS blocks_fts USING fts5(
    text,
    content='blocks',
    content_rowid='id'
);
CREATE TABLE IF NOT EXISTS cache (
    url         TEXT PRIMARY KEY,
    page_id     INTEGER NOT NULL,
    fetched_at  INTEGER NOT NULL,
    ttl_secs    INTEGER NOT NULL DEFAULT 300
);
CREATE TABLE IF NOT EXISTS html_cache (
    page_id     INTEGER PRIMARY KEY REFERENCES pages(id) ON DELETE CASCADE,
    html        TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS runbooks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL UNIQUE,
    url             TEXT NOT NULL,
    steps           TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    last_used_at    INTEGER NOT NULL,
    success_count   INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER IF NOT EXISTS blocks_ai AFTER INSERT ON blocks BEGIN
    INSERT INTO blocks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS blocks_ad AFTER DELETE ON blocks BEGIN
    INSERT INTO blocks_fts(blocks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
"#;

/// Maximum blocks stored per page (protects the index from huge pages).
const MAX_BLOCKS_PER_PAGE: usize = 300;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
pub struct PageMeta {
    pub url: String,
    pub final_url: Option<String>,
    pub title: Option<String>,
    pub status: Option<i64>,
    pub word_count: Option<i64>,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub url: String,
    pub title: Option<String>,
    pub text: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CachedPage {
    pub meta: PageMeta,
    pub html: String,
}

/// The browsing memory store. Cheap, synchronous, thread-safe (one writer).
pub struct MemoryStore {
    conn: Mutex<Connection>,
}

impl MemoryStore {
    /// Open (or create) a store at `path`. `None` → in-memory.
    pub fn open(path: Option<&str>) -> Result<Self> {
        let conn = match path {
            Some(p) => Connection::open(p),
            None => Connection::open_in_memory(),
        }
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| Error::Parse(format!("memory schema: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Persist a fetched page + its text blocks into the store.
    /// Upserts by URL. Returns the page's row id.
    pub fn store_page(&self, page: &Page) -> Result<i64> {
        let blocks = extract::extract_text(&page.html).blocks;
        let conn = self.conn.lock().unwrap();

        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pages WHERE url = ?1)",
                params![page.url],
                |r| r.get(0),
            )
            .map_err(|e| Error::Parse(e.to_string()))?;

        let page_id: i64 = if exists {
            conn.execute(
                "UPDATE pages SET final_url=?1, title=?2, status=?3, word_count=?4, fetched_at=?5 WHERE url=?6",
                params![page.url, page.title, page.status as i64, page_html_words(page), now_secs(), page.url],
            )
            .map_err(|e| Error::Parse(e.to_string()))?;
            conn.query_row(
                "SELECT id FROM pages WHERE url=?1",
                params![page.url],
                |r| r.get(0),
            )
            .map_err(|e| Error::Parse(e.to_string()))?
        } else {
            conn.execute(
                "INSERT INTO pages (url, final_url, title, status, word_count, fetched_at) VALUES (?1,?2,?3,?4,?5,?6)",
                params![page.url, page.url, page.title, page.status as i64, page_html_words(page), now_secs()],
            )
            .map_err(|e| Error::Parse(e.to_string()))?;
            conn.last_insert_rowid()
        };

        // Replace blocks (delete + insert keeps FTS trigger simple).
        conn.execute("DELETE FROM blocks WHERE page_id=?1", params![page_id])
            .map_err(|e| Error::Parse(e.to_string()))?;
        for (i, b) in blocks.iter().take(MAX_BLOCKS_PER_PAGE).enumerate() {
            conn.execute(
                "INSERT INTO blocks (page_id, position, level, text) VALUES (?1,?2,?3,?4)",
                params![page_id, i as i64, b.level as i64, b.text],
            )
            .map_err(|e| Error::Parse(e.to_string()))?;
        }
        Ok(page_id)
    }

    /// Cache a page fetch under `url` with a TTL.
    pub fn set_cache(&self, url: &str, page_id: Option<i64>, ttl_secs: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let pid = match page_id {
            Some(id) => id,
            None => conn
                .query_row("SELECT id FROM pages WHERE url=?1", params![url], |r| {
                    r.get(0)
                })
                .map_err(|_| Error::NotInitialized("page not stored yet".into()))?,
        };
        conn.execute(
            "INSERT INTO cache (url, page_id, fetched_at, ttl_secs) VALUES (?1,?2,?3,?4)
             ON CONFLICT(url) DO UPDATE SET page_id=excluded.page_id, fetched_at=excluded.fetched_at, ttl_secs=excluded.ttl_secs",
            params![url, pid, now_secs(), ttl_secs],
        )
        .map_err(|e| Error::Parse(e.to_string()))?;
        Ok(())
    }

    /// Look up a still-fresh cached page. Returns `None` when missing/expired.
    pub fn find_cached(&self, url: &str, max_age: Option<Duration>) -> Result<Option<CachedPage>> {
        let conn = self.conn.lock().unwrap();
        let max_age = max_age.unwrap_or(Duration::from_secs(300)).as_secs() as i64;
        let row = conn.query_row(
            "SELECT p.url, p.final_url, p.title, p.status, p.word_count, p.fetched_at
             FROM cache c JOIN pages p ON p.id = c.page_id
             WHERE c.url = ?1 AND (?2 - c.fetched_at) <= c.ttl_secs AND (?2 - c.fetched_at) <= ?3",
            params![url, now_secs(), max_age],
            |r| {
                Ok(PageMeta {
                    url: r.get(0)?,
                    final_url: r.get(1)?,
                    title: r.get(2)?,
                    status: r.get(3)?,
                    word_count: r.get(4)?,
                    fetched_at: r.get(5)?,
                })
            },
        );
        let meta = match row {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(Error::Parse(e.to_string())),
        };
        drop(conn);
        // Reopen connection read for the HTML (blocks alone don't hold it).
        let conn = self.conn.lock().unwrap();
        let html: String = conn
            .query_row(
                "SELECT group_concat(text, char(10)) FROM blocks WHERE page_id = (SELECT id FROM pages WHERE url=?1) ORDER BY position",
                params![meta.url],
                |r| r.get(0),
            )
            .map_err(|e| Error::Parse(e.to_string()))?;
        Ok(Some(CachedPage { meta, html }))
    }

    /// BM25 search over everything we've read. Falls back to a token
    /// substring scan when FTS5 finds nothing (e.g. stopword-heavy queries).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let conn = self.conn.lock().unwrap();

        // 1) FTS5 BM25 (all tokens must match).
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT p.url, p.title, b.text, bm25(blocks_fts)
             FROM blocks_fts
             JOIN blocks b ON b.id = blocks_fts.rowid
             JOIN pages p ON p.id = b.page_id
             WHERE blocks_fts MATCH ?1
             ORDER BY bm25(blocks_fts)
             LIMIT ?2",
        ) {
            if let Ok(rows) = stmt.query_map(params![query, limit as i64], |r| {
                Ok(SearchHit {
                    url: r.get(0)?,
                    title: r.get(1)?,
                    text: r.get(2)?,
                    score: r.get(3)?,
                })
            }) {
                for r in rows.flatten() {
                    out.push(r);
                }
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }

        // 2) Fallback: substring scan per token (any-token match, ranked by
        //    matched-token count). Handles stopword-heavy natural queries.
        let tokens: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 2)
            .map(|t| t.to_string())
            .collect();
        if tokens.is_empty() {
            return Ok(out);
        }
        let mut stmt = conn
            .prepare("SELECT p.url, p.title, b.text FROM blocks b JOIN pages p ON p.id = b.page_id")
            .map_err(|e| Error::Parse(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| Error::Parse(e.to_string()))?;
        let mut scored: Vec<(f64, SearchHit)> = Vec::new();
        for r in rows {
            let (url, title, text) = r.map_err(|e| Error::Parse(e.to_string()))?;
            let lower = text.to_lowercase();
            let matched = tokens.iter().filter(|t| lower.contains(t.as_str())).count();
            if matched > 0 {
                let score = matched as f64 * 10.0 + (text.len() as f64).min(200.0) / 200.0;
                scored.push((
                    -score, // lower = better, keep consistent with bm25 sign
                    SearchHit {
                        url,
                        title,
                        text,
                        score: -score,
                    },
                ));
            }
        }
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        out = scored.into_iter().take(limit).map(|(_, h)| h).collect();
        Ok(out)
    }

    /// Most recently fetched pages.
    pub fn recent(&self, limit: usize) -> Result<Vec<PageMeta>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT url, final_url, title, status, word_count, fetched_at FROM pages ORDER BY fetched_at DESC, id DESC LIMIT ?1")
            .map_err(|e| Error::Parse(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(PageMeta {
                    url: r.get(0)?,
                    final_url: r.get(1)?,
                    title: r.get(2)?,
                    status: r.get(3)?,
                    word_count: r.get(4)?,
                    fetched_at: r.get(5)?,
                })
            })
            .map_err(|e| Error::Parse(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| Error::Parse(e.to_string()))?);
        }
        Ok(out)
    }

    /// Upsert a runbook (named action recipe) with its JSON steps.
    pub fn save_runbook(&self, name: &str, url: &str, steps_json: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO runbooks (name, url, steps, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name) DO UPDATE SET url=excluded.url, steps=excluded.steps, last_used_at=excluded.last_used_at",
            params![name, url, steps_json, now_secs()],
        )
        .map_err(|e| Error::Parse(e.to_string()))?;
        Ok(())
    }

    /// All runbooks: (name, url, steps_json, success_count).
    pub fn list_runbooks(&self) -> Result<Vec<(String, String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, url, steps, success_count FROM runbooks ORDER BY last_used_at DESC",
            )
            .map_err(|e| Error::Parse(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|e| Error::Parse(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| Error::Parse(e.to_string()))?);
        }
        Ok(out)
    }

    /// Fetch one runbook by name.
    pub fn get_runbook(&self, name: &str) -> Result<Option<(String, String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT name, url, steps, success_count FROM runbooks WHERE name = ?1",
            params![name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Parse(e.to_string())),
        }
    }

    /// Mark a runbook as used successfully.
    pub fn runbook_success(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE runbooks SET success_count = success_count + 1, last_used_at = ?1 WHERE name = ?2",
            params![now_secs(), name],
        )
        .map_err(|e| Error::Parse(e.to_string()))?;
        Ok(())
    }

    /// Count stored pages (for stats/health).
    pub fn page_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))
            .map_err(|e| Error::Parse(e.to_string()))
    }
}

fn page_html_words(page: &Page) -> i64 {
    extract::extract_text(&page.html).word_count as i64
}

/// Navigate with the memory cache: return a cached page when fresh, else
/// fetch (honoring `engine`), store, and cache it. Returns `(page, cached)`.
pub async fn navigate_cached(
    store: &MemoryStore,
    fetch: &dyn lightbrowse_core::backend::BrowserBackend,
    cdp: Option<&dyn lightbrowse_core::backend::BrowserBackend>,
    session: &lightbrowse_core::session::Session,
    url: &str,
    engine: lightbrowse_core::config::Engine,
    ttl_secs: i64,
) -> Result<(Page, bool)> {
    if let Some(c) = store.find_cached(url, Some(Duration::from_secs(ttl_secs.max(0) as u64)))? {
        tracing::debug!("memory: cache hit for {url}");
        return Ok((c.html_to_page(), true));
    }
    let page = lightbrowse_core::service::navigate(fetch, cdp, session, url, engine).await?;
    let page_id = store.store_page(&page)?;
    // Cache under BOTH the requested URL and the final (post-redirect) URL,
    // so repeated fetches of either hit the cache.
    store.set_cache(url, Some(page_id), ttl_secs)?;
    if page.url != url {
        store.set_cache(&page.url, Some(page_id), ttl_secs)?;
    }
    Ok((page, false))
}

impl CachedPage {
    /// Rebuild a [`Page`] from cached data.
    pub fn html_to_page(self) -> Page {
        Page {
            url: self.meta.url.clone(),
            title: self.meta.title.clone().unwrap_or_default(),
            status: self.meta.status.unwrap_or(0) as u16,
            headers: Default::default(),
            html: self.html,
            truncated: false,
            mime: Some("text/html".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightbrowse_core::page::Page;

    fn test_page(url: &str, html: &str) -> Page {
        Page {
            url: url.into(),
            title: "Test".into(),
            status: 200,
            headers: Default::default(),
            html: html.into(),
            truncated: false,
            mime: Some("text/html".into()),
        }
    }

    #[test]
    fn store_and_search() {
        let m = MemoryStore::open(None).unwrap();
        m.store_page(&test_page(
            "https://a.test/rust",
            "<html><body><h1>Rust async</h1><p>Tokio is an async runtime for Rust.</p></body></html>",
        ))
        .unwrap();
        m.store_page(&test_page(
            "https://a.test/browser",
            "<html><body><h1>Browsers</h1><p>Chromium renders JavaScript for the browser.</p></body></html>",
        ))
        .unwrap();
        let hits = m.search("async runtime", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].url.contains("rust"));
    }

    #[test]
    fn cache_roundtrip() {
        let m = MemoryStore::open(None).unwrap();
        let p = test_page(
            "https://a.test/cache",
            "<html><body><p>cached content here</p></body></html>",
        );
        m.store_page(&p).unwrap();
        m.set_cache("https://a.test/cache", None, 300).unwrap();
        let c = m.find_cached("https://a.test/cache", None).unwrap();
        assert!(c.is_some());
        assert!(c.unwrap().html.contains("cached content"));
    }

    #[test]
    fn runbook_crud() {
        let m = MemoryStore::open(None).unwrap();
        m.save_runbook(
            "login-demo",
            "https://demo.test/login",
            r#"[{"action":"type"}]"#,
        )
        .unwrap();
        let (name, url, steps, cnt) = m.get_runbook("login-demo").unwrap().unwrap();
        assert_eq!(name, "login-demo");
        assert_eq!(url, "https://demo.test/login");
        assert!(steps.contains("type"));
        assert_eq!(cnt, 0);
        // upsert + success bump
        m.save_runbook(
            "login-demo",
            "https://demo.test/login",
            r#"[{"action":"press"}]"#,
        )
        .unwrap();
        m.runbook_success("login-demo").unwrap();
        let (_, _, steps, cnt) = m.get_runbook("login-demo").unwrap().unwrap();
        assert!(steps.contains("press"));
        assert_eq!(cnt, 1);
        assert_eq!(m.list_runbooks().unwrap().len(), 1);
    }

    #[test]
    fn recent_ordered() {
        let m = MemoryStore::open(None).unwrap();
        m.store_page(&test_page(
            "https://a.test/1",
            "<html><body><p>one</p></body></html>",
        ))
        .unwrap();
        m.store_page(&test_page(
            "https://a.test/2",
            "<html><body><p>two</p></body></html>",
        ))
        .unwrap();
        let recent = m.recent(5).unwrap();
        assert_eq!(recent.len(), 2);
        assert!(recent[0].url.ends_with("/2"));
    }
}
