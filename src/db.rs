use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::key;
use crate::proxy::usage::Usage;

pub struct KeyAuth {
    pub id: i64,
    pub device_id: i64,
    pub monthly_token_limit: Option<i64>,
    pub allow_all_models: bool,
    pub capture_bodies: bool,
    pub revoked: bool,
}

pub struct NewRequest<'a> {
    pub started_at: i64,
    pub device_id: i64,
    pub api_key_id: i64,
    pub client_ip: &'a str,
    pub endpoint: &'a str,
    pub model: &'a str,
    pub stream: bool,
    pub prompt_hash: &'a [u8; 32],
    pub message_count: i64,
    pub prompt_chars: i64,
    pub has_tools: bool,
    pub has_image: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
}

pub struct KeyListRow {
    pub id: i64,
    pub device: String,
    pub prefix: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub monthly_token_limit: Option<i64>,
    pub allow_all_models: bool,
    pub capture_until: Option<i64>,
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
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        conn.execute_batch(MIGRATION_V1)?;
        version = 1;
    }
    if version < 2 {
        conn.execute_batch(MIGRATION_V2)?;
    }
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

pub struct NewApiKey<'a> {
    pub device_id: i64,
    pub hash: &'a [u8; 32],
    pub prefix: &'a str,
    pub created_at: i64,
    pub monthly_token_limit: Option<i64>,
    pub allow_all_models: bool,
    pub capture_until: Option<i64>,
}

pub fn insert_api_key(conn: &Connection, key: NewApiKey<'_>) -> Result<i64> {
    let NewApiKey {
        device_id,
        hash,
        prefix,
        created_at,
        monthly_token_limit,
        allow_all_models,
        capture_until,
    } = key;
    let capture_bodies = if capture_until.is_some() { 1 } else { 0 };
    conn.execute(
        "INSERT INTO api_key
         (device_id, key_hash, prefix, created_at, monthly_token_limit, allow_all_models,
          capture_bodies, capture_until)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            device_id,
            hash.as_slice(),
            prefix,
            created_at,
            monthly_token_limit,
            allow_all_models as i64,
            capture_bodies,
            capture_until
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_keys(conn: &Connection) -> Result<Vec<KeyListRow>> {
    let mut stmt = conn.prepare(
        "SELECT k.id, d.name, k.prefix, k.created_at, k.revoked_at, k.monthly_token_limit,
                k.allow_all_models, k.capture_until
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
            allow_all_models: row.get::<_, i64>(6)? != 0,
            capture_until: row.get(7)?,
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
    let mut stmt = conn.prepare(
        "SELECT id, device_id, key_hash, revoked_at, monthly_token_limit, capture_bodies,
                allow_all_models
         FROM api_key",
    )?;
    let mut rows = stmt.query([])?;
    let mut found = None;
    while let Some(row) = rows.next()? {
        let stored: Vec<u8> = row.get(2)?;
        if key::constant_time_eq(&stored, hash) {
            let revoked_at: Option<i64> = row.get(3)?;
            found = Some(KeyAuth {
                id: row.get(0)?,
                device_id: row.get(1)?,
                monthly_token_limit: row.get(4)?,
                capture_bodies: row.get::<_, i64>(5)? != 0,
                allow_all_models: row.get::<_, i64>(6)? != 0,
                revoked: revoked_at.is_some(),
            });
        }
    }
    Ok(found)
}

pub fn insert_request(conn: &Connection, request: &NewRequest<'_>) -> Result<i64> {
    conn.execute(
        "INSERT INTO request
         (started_at, device_id, api_key_id, client_ip, endpoint, model, stream,
          usage_source, prompt_hash, message_count, prompt_chars, has_tools, has_image,
          temperature, max_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'unknown', ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            request.started_at,
            request.device_id,
            request.api_key_id,
            request.client_ip,
            request.endpoint,
            request.model,
            request.stream as i64,
            request.prompt_hash.as_slice(),
            request.message_count,
            request.prompt_chars,
            request.has_tools as i64,
            request.has_image as i64,
            request.temperature,
            request.max_tokens,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn set_capture(conn: &Connection, prefix: &str, until: Option<i64>) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE api_key SET capture_bodies = ?1, capture_until = ?2
         WHERE substr(prefix, 1, length(?3)) = ?3",
        params![if until.is_some() { 1 } else { 0 }, until, prefix],
    )?;
    Ok(changed)
}

/// `limit`: `Some(None)` clears the cap, `Some(Some(n))` sets it, `None` leaves it alone.
pub fn set_key_settings(
    conn: &Connection,
    prefix: &str,
    limit: Option<Option<i64>>,
    allow_all_models: Option<bool>,
) -> Result<usize> {
    if let Some(limit) = limit {
        conn.execute(
            "UPDATE api_key SET monthly_token_limit = ?1
             WHERE substr(prefix, 1, length(?2)) = ?2",
            params![limit, prefix],
        )?;
    }
    if let Some(allow) = allow_all_models {
        conn.execute(
            "UPDATE api_key SET allow_all_models = ?1
             WHERE substr(prefix, 1, length(?2)) = ?2",
            params![allow as i64, prefix],
        )?;
    }
    let changed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM api_key WHERE substr(prefix, 1, length(?1)) = ?1",
        [prefix],
        |row| row.get(0),
    )?;
    Ok(changed as usize)
}

pub fn insert_capture(
    conn: &Connection,
    request_id: i64,
    api_key_id: i64,
    expires_at: i64,
    truncated: bool,
    prompt_body: Option<&str>,
    response_body: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO debug_capture
         (request_id, api_key_id, expires_at, truncated, prompt_body, response_body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            request_id,
            api_key_id,
            expires_at,
            if truncated { 1 } else { 0 },
            prompt_body,
            response_body
        ],
    )?;
    Ok(())
}

pub fn prune_requests(conn: &Connection, older_than: i64) -> Result<usize> {
    let captures = conn.execute(
        "DELETE FROM debug_capture WHERE request_id IN
         (SELECT id FROM request WHERE started_at < ?1)",
        params![older_than],
    )?;
    let requests = conn.execute("DELETE FROM request WHERE started_at < ?1", params![older_than])?;
    Ok(requests + captures)
}

pub fn prune_captures(conn: &Connection, now: i64) -> Result<usize> {
    let expired = conn.execute(
        "DELETE FROM debug_capture WHERE expires_at < ?1",
        params![now],
    )?;
    let cleared = conn.execute(
        "UPDATE api_key SET capture_bodies = 0, capture_until = NULL
         WHERE capture_bodies = 1 AND capture_until IS NOT NULL AND capture_until < ?1",
        params![now],
    )?;
    Ok(expired + cleared)
}

pub fn rollup_day(conn: &Connection, day: &str) -> Result<usize> {
    let inserted = conn.execute(
        "INSERT OR REPLACE INTO daily_rollup
         (day, device_id, model, requests, input_tokens, cached_input_tokens,
          cache_write_tokens, output_tokens, reasoning_tokens)
         SELECT ?1, device_id, model, COUNT(*),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(cached_input_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(reasoning_tokens), 0)
         FROM request
         WHERE date(started_at / 1000, 'unixepoch') = ?1
         GROUP BY device_id, model",
        params![day],
    )?;
    Ok(inserted)
}

pub fn rollup_range(conn: &Connection, since: i64) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT date(started_at / 1000, 'unixepoch') FROM request
         WHERE started_at >= ?1 ORDER BY 1",
    )?;
    let days: Vec<String> = stmt
        .query_map(params![since], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut total = 0;
    for day in days {
        total += rollup_day(conn, &day)?;
    }
    Ok(total)
}

pub fn month_tokens_used(conn: &Connection, api_key_id: i64, since: i64) -> Result<i64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)), 0)
         FROM request WHERE api_key_id = ?1 AND started_at >= ?2",
        params![api_key_id, since],
        |row| row.get(0),
    )?;
    Ok(total)
}

pub struct FinishRequest<'a> {
    pub id: i64,
    pub finished_at: i64,
    pub status_code: Option<i64>,
    pub latency_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub usage: Option<&'a Usage>,
    pub error_kind: Option<&'a str>,
    pub aborted: bool,
    pub quota_exceeded: bool,
}

pub fn finish_request(conn: &Connection, request: FinishRequest<'_>) -> Result<()> {
    let FinishRequest {
        id,
        finished_at,
        status_code,
        latency_ms,
        ttft_ms,
        usage,
        error_kind,
        aborted,
        quota_exceeded,
    } = request;
    let (source, input, cached, cache_write, output, reasoning) = match usage {
        Some(usage) => (
            "upstream",
            Some(usage.input_tokens),
            Some(usage.cached_input_tokens),
            Some(usage.cache_write_tokens),
            Some(usage.output_tokens),
            Some(usage.reasoning_tokens),
        ),
        None => ("unknown", None, None, None, None, None),
    };
    conn.execute(
        "UPDATE request SET
           finished_at = ?2, status_code = ?3, latency_ms = ?4, ttft_ms = ?5,
           input_tokens = ?6, cached_input_tokens = ?7, cache_write_tokens = ?8,
           output_tokens = ?9, reasoning_tokens = ?10, usage_source = ?11,
           error_kind = ?12, aborted = ?13, quota_exceeded = ?14
         WHERE id = ?1 AND finished_at IS NULL",
        params![
            id,
            finished_at,
            status_code,
            latency_ms,
            ttft_ms,
            input,
            cached,
            cache_write,
            output,
            reasoning,
            source,
            error_kind,
            aborted as i64,
            quota_exceeded as i64,
        ],
    )?;
    Ok(())
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

/// Version 2 adds a per-key switch that lifts the model allowlist.
const MIGRATION_V2: &str = "
ALTER TABLE api_key ADD COLUMN allow_all_models INTEGER NOT NULL DEFAULT 0;
PRAGMA user_version = 2;
";
