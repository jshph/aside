use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::Connection;
use std::path::Path;

pub fn open_db(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).context("failed to open session database")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            name        TEXT PRIMARY KEY,
            start_time  TEXT NOT NULL,
            notes_path  TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS segments (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            session_name    TEXT NOT NULL REFERENCES sessions(name),
            segment_index   INTEGER NOT NULL,
            wav_path        TEXT NOT NULL,
            offset_ms       INTEGER NOT NULL,
            duration_secs   REAL,
            started_at      TEXT NOT NULL,
            UNIQUE(session_name, segment_index)
        );",
    )?;
    Ok(conn)
}

pub fn create_session(
    conn: &Connection,
    name: &str,
    start_time: &DateTime<Local>,
    notes_path: &str,
) -> Result<()> {
    let now = Local::now().to_rfc3339();
    let start = start_time.to_rfc3339();
    conn.execute(
        "INSERT INTO sessions (name, start_time, notes_path, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![name, start, notes_path, now, now],
    )
    .context("failed to create session (name already exists?)")?;
    Ok(())
}

pub fn get_session_start_time(conn: &Connection, name: &str) -> Result<DateTime<Local>> {
    let start_str: String = conn
        .query_row(
            "SELECT start_time FROM sessions WHERE name = ?1",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .context(format!("session '{}' not found in database", name))?;
    let dt = DateTime::parse_from_rfc3339(&start_str)
        .context("invalid start_time in database")?
        .with_timezone(&Local);
    Ok(dt)
}

pub fn session_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE name = ?1",
        rusqlite::params![name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn next_segment_index(conn: &Connection, session_name: &str) -> Result<i64> {
    let max: Option<i64> = conn
        .query_row(
            "SELECT MAX(segment_index) FROM segments WHERE session_name = ?1",
            rusqlite::params![session_name],
            |row| row.get(0),
        )
        .unwrap_or(None);
    Ok(max.map(|m| m + 1).unwrap_or(0))
}

pub fn add_segment(
    conn: &Connection,
    session_name: &str,
    segment_index: i64,
    wav_path: &str,
    offset_ms: i64,
    duration_secs: Option<f64>,
) -> Result<()> {
    let now = Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO segments (session_name, segment_index, wav_path, offset_ms, duration_secs, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![session_name, segment_index, wav_path, offset_ms, duration_secs, now],
    )
    .context("failed to add segment")?;

    // Update session's updated_at
    conn.execute(
        "UPDATE sessions SET updated_at = ?1 WHERE name = ?2",
        rusqlite::params![now, session_name],
    )?;

    Ok(())
}

pub fn update_segment_duration(
    conn: &Connection,
    session_name: &str,
    segment_index: i64,
    duration_secs: f64,
) -> Result<()> {
    conn.execute(
        "UPDATE segments SET duration_secs = ?1 WHERE session_name = ?2 AND segment_index = ?3",
        rusqlite::params![duration_secs, session_name, segment_index],
    )?;
    Ok(())
}

pub struct SessionInfo {
    pub name: String,
    pub start_time: String,
    pub notes_path: String,
    pub segment_count: i64,
    pub updated_at: String,
}

pub fn list_sessions(conn: &Connection) -> Result<Vec<SessionInfo>> {
    let mut stmt = conn.prepare(
        "SELECT s.name, s.start_time, s.notes_path, s.updated_at,
                (SELECT COUNT(*) FROM segments WHERE session_name = s.name)
         FROM sessions s
         ORDER BY s.created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SessionInfo {
            name: row.get(0)?,
            start_time: row.get(1)?,
            notes_path: row.get(2)?,
            updated_at: row.get(3)?,
            segment_count: row.get(4)?,
        })
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        sessions.push(row?);
    }
    Ok(sessions)
}
