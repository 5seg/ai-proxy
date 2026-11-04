use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::key;

pub struct KeyAuth {
    pub revoked: bool,
}

pub struct KeyListRow {
    pub id: i64,
    pub device: String,
    pub prefix: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub monthly_token_limit: Option<i64>,
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }

    let conn = Connection::open(path)
        .with_context(|| format!("failed to open database {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;",
    )?;

    let empty: bool =
        conn.query_row("SELECT COUNT(*) = 0 FROM sqlite_master", [], |row| row.get(0))?;
    if empty {
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
    }

    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= 1 {
        return Ok(());
    }
    conn.execute_batch(MIGRATION_V1)?;
    Ok(())
}

pub fn ensure_device(
    conn: &Connection,
    name: &str,
    note: Option<&str>,
    now: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO device (name, note, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO NOTHING",
        params![name, note, now],
    )?;
    let id = conn.query_row("SELECT id FROM device WHERE name = ?1", [name], |row| {
        row.get(0)
    })?;
    Ok(id)
}

pub fn insert_api_key(
    conn: &Connection,
    device_id: i64,
    hash: &[u8; 32],
    prefix: &str,
    now: i64,
    limit: Option<i64>,
    capture: bool,
) -> Result<i64> {
    let capture_bodies = if capture { 1 } else { 0 };
    let capture_until = if capture {
        Some(now + 24 * 60 * 60 * 1000)
    } else {
        None
    };
    conn.execute(
        "INSERT INTO api_key
         (device_id, key_hash, prefix, created_at, monthly_token_limit, capture_bodies, capture_until)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            device_id,
            hash.as_slice(),
            prefix,
            now,
            limit,
            capture_bodies,
            capture_until
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_keys(conn: &Connection) -> Result<Vec<KeyListRow>> {
    let mut stmt = conn.prepare(
        "SELECT k.id, d.name, k.prefix, k.created_at, k.revoked_at, k.monthly_token_limit
         FROM api_key k
         JOIN device d ON d.id = k.device_id
         ORDER BY k.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(KeyListRow {
            id: row.get(0)?,
            device: row.get(1)?,
            prefix: row.get(2)?,
            created_at: row.get(3)?,
            revoked_at: row.get(4)?,
            monthly_token_limit: row.get(5)?,
        })
    })?;

    let mut keys = Vec::new();
    for row in rows {
        keys.push(row?);
    }
    Ok(keys)
}

pub fn revoke_keys(conn: &Connection, prefix: &str, now: i64) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE api_key SET revoked_at = ?1
         WHERE revoked_at IS NULL AND substr(prefix, 1, length(?2)) = ?2",
        params![now, prefix],
    )?;
    Ok(changed)
}

pub fn find_key(conn: &Connection, hash: &[u8; 32]) -> Result<Option<KeyAuth>> {
    let mut stmt = conn.prepare("SELECT key_hash, revoked_at FROM api_key")?;
    let mut rows = stmt.query([])?;
    let mut found = None;
    while let Some(row) = rows.next()? {
        let stored: Vec<u8> = row.get(0)?;
        let revoked_at: Option<i64> = row.get(1)?;
        if key::constant_time_eq(&stored, hash) {
            found = Some(revoked_at.is_some());
        }
    }
    Ok(found.map(|revoked| KeyAuth { revoked }))
}

const MIGRATION_V1: &str = r#"
BEGIN;

CREATE TABLE device (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  note        TEXT,
  created_at  INTEGER NOT NULL
);

CREATE TABLE api_key (
  id                 INTEGER PRIMARY KEY,
  device_id          INTEGER NOT NULL REFERENCES device(id),
  key_hash           BLOB NOT NULL UNIQUE,
  prefix             TEXT NOT NULL,
  created_at         INTEGER NOT NULL,
  revoked_at         INTEGER,
  monthly_token_limit INTEGER,
  quota_weight_input  REAL NOT NULL DEFAULT 1.0,
  quota_weight_cached REAL NOT NULL DEFAULT 1.0,
  quota_weight_output REAL NOT NULL DEFAULT 1.0,
  capture_bodies     INTEGER NOT NULL DEFAULT 0,
  capture_until      INTEGER
);

CREATE TABLE request (
  id                 INTEGER PRIMARY KEY,
  started_at         INTEGER NOT NULL,
  finished_at        INTEGER,
  device_id          INTEGER NOT NULL REFERENCES device(id),
  api_key_id         INTEGER NOT NULL REFERENCES api_key(id),
  client_ip          TEXT NOT NULL,
  endpoint           TEXT NOT NULL,
  model              TEXT NOT NULL,
  stream             INTEGER NOT NULL,
  status_code        INTEGER,
  error_kind         TEXT,
  latency_ms         INTEGER,
  ttft_ms            INTEGER,
  input_tokens       INTEGER,
  cached_input_tokens INTEGER,
  cache_write_tokens  INTEGER,
  output_tokens      INTEGER,
  reasoning_tokens   INTEGER,
  usage_source       TEXT NOT NULL,
  prompt_hash        BLOB,
  message_count      INTEGER,
  prompt_chars       INTEGER,
  has_tools          INTEGER,
  has_image          INTEGER,
  temperature        REAL,
  max_tokens         INTEGER,
  aborted            INTEGER NOT NULL DEFAULT 0,
  quota_exceeded     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_request_started ON request(started_at);
CREATE INDEX idx_request_device  ON request(device_id, started_at);
CREATE INDEX idx_request_key     ON request(api_key_id, started_at);
CREATE INDEX idx_request_hash    ON request(prompt_hash);

CREATE TABLE price (
  model                   TEXT NOT NULL,
  effective_from          TEXT NOT NULL,
  input_per_mtok          REAL NOT NULL,
  cached_per_mtok         REAL NOT NULL,
  cache_write_per_mtok    REAL NOT NULL,
  output_per_mtok         REAL NOT NULL,
  PRIMARY KEY (model, effective_from)
);

CREATE TABLE daily_rollup (
  day                TEXT NOT NULL,
  device_id          INTEGER NOT NULL,
  model              TEXT NOT NULL,
  requests           INTEGER NOT NULL,
  input_tokens       INTEGER NOT NULL,
  cached_input_tokens INTEGER NOT NULL,
  cache_write_tokens  INTEGER NOT NULL,
  output_tokens      INTEGER NOT NULL,
  reasoning_tokens   INTEGER NOT NULL,
  PRIMARY KEY (day, device_id, model)
);

CREATE TABLE debug_capture (
  request_id   INTEGER PRIMARY KEY REFERENCES request(id),
  api_key_id   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  truncated    INTEGER NOT NULL,
  prompt_body  TEXT,
  response_body TEXT
);

INSERT INTO price VALUES ('deepseek/deepseek-v4.1-flash', '2026-11-01', 0.15, 0.003, 0.0, 0.60);

PRAGMA user_version = 1;

COMMIT;
"#;
