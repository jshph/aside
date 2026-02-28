use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct SessionMeta {
    pub name: String,
    pub start_time: String,
    pub notes_path: String,
    pub created_at: String,
    pub segments: Vec<SegmentMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_note_path: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SegmentMeta {
    pub segment_index: i64,
    pub wav_path: String,
    pub offset_ms: i64,
    pub duration_secs: Option<f64>,
    pub started_at: String,
}

fn meta_path(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{}.meta.json", name))
}

fn read_meta(dir: &Path, name: &str) -> Result<SessionMeta> {
    let path = meta_path(dir, name);
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read session file {:?}", path))?;
    serde_json::from_str(&data).context("failed to parse session JSON")
}

fn write_meta(dir: &Path, meta: &SessionMeta) -> Result<()> {
    let path = meta_path(dir, &meta.name);
    let data = serde_json::to_string_pretty(meta).context("failed to serialize session JSON")?;
    std::fs::write(&path, data).with_context(|| format!("failed to write {:?}", path))
}

pub fn create_session(
    dir: &Path,
    name: &str,
    start_time: &DateTime<Local>,
    notes_path: &str,
) -> Result<()> {
    let now = Local::now().to_rfc3339();
    let meta = SessionMeta {
        name: name.to_string(),
        start_time: start_time.to_rfc3339(),
        notes_path: notes_path.to_string(),
        created_at: now,
        segments: Vec::new(),
        vault_note_path: None,
    };
    write_meta(dir, &meta)
}

pub fn session_exists(dir: &Path, name: &str) -> Result<bool> {
    Ok(meta_path(dir, name).exists())
}

pub fn get_session_start_time(dir: &Path, name: &str) -> Result<DateTime<Local>> {
    let meta = read_meta(dir, name)?;
    let dt = DateTime::parse_from_rfc3339(&meta.start_time)
        .context("invalid start_time in session file")?
        .with_timezone(&Local);
    Ok(dt)
}

pub fn next_segment_index(dir: &Path, session_name: &str) -> Result<i64> {
    let meta = read_meta(dir, session_name)?;
    Ok(meta.segments.len() as i64)
}

pub fn add_segment(
    dir: &Path,
    session_name: &str,
    segment_index: i64,
    wav_path: &str,
    offset_ms: i64,
    duration_secs: Option<f64>,
) -> Result<()> {
    let mut meta = read_meta(dir, session_name)?;
    meta.segments.push(SegmentMeta {
        segment_index,
        wav_path: wav_path.to_string(),
        offset_ms,
        duration_secs,
        started_at: Local::now().to_rfc3339(),
    });
    write_meta(dir, &meta)
}

pub fn update_segment_duration(
    dir: &Path,
    session_name: &str,
    segment_index: i64,
    duration_secs: f64,
) -> Result<()> {
    let mut meta = read_meta(dir, session_name)?;
    if let Some(seg) = meta
        .segments
        .iter_mut()
        .find(|s| s.segment_index == segment_index)
    {
        seg.duration_secs = Some(duration_secs);
    }
    write_meta(dir, &meta)
}

pub struct SessionInfo {
    pub name: String,
    pub start_time: String,
    pub notes_path: String,
    pub segment_count: i64,
}

pub fn list_sessions(dir: &Path) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();

    let entries = std::fs::read_dir(dir).context("failed to read .aside/ directory")?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".meta.json") {
                    let data = std::fs::read_to_string(&path)?;
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&data) {
                        sessions.push(SessionInfo {
                            name: meta.name,
                            start_time: meta.start_time,
                            notes_path: meta.notes_path,
                            segment_count: meta.segments.len() as i64,
                        });
                    }
                }
            }
        }
    }

    sessions.sort_by(|a, b| b.start_time.cmp(&a.start_time));
    Ok(sessions)
}

pub fn get_session_meta(dir: &Path, name: &str) -> Result<SessionMeta> {
    read_meta(dir, name)
}

pub fn set_vault_note_path(dir: &Path, name: &str, vault_path: &str) -> Result<()> {
    let mut meta = read_meta(dir, name)?;
    meta.vault_note_path = Some(vault_path.to_string());
    write_meta(dir, &meta)
}
