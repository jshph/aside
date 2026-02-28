mod app;
mod parser;
mod recorder;
mod session;
mod text_helpers;
mod tui;

use anyhow::{bail, Context, Result};
use chrono::Local;
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "aside", about = "Record and take timestamped notes in one session")]
struct Args {
    /// Session name (creates <name>.md and recordings/<name>_seg0.wav)
    name: Option<String>,

    /// Resume an existing session
    #[arg(long)]
    resume: Option<String>,

    /// List all sessions
    #[arg(long)]
    list: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list {
        return cmd_list();
    }

    if let Some(name) = args.resume {
        return cmd_resume(&name);
    }

    if let Some(name) = args.name {
        return cmd_new(&name);
    }

    // No args — print help
    eprintln!("Usage: aside <name>            Start a new session");
    eprintln!("       aside --resume <name>   Resume an existing session");
    eprintln!("       aside --list            List all sessions");
    std::process::exit(1);
}

fn recordings_dir() -> PathBuf {
    PathBuf::from("recordings")
}

fn db_path() -> PathBuf {
    recordings_dir().join(".aside.db")
}

fn ensure_recordings_dir() -> Result<()> {
    std::fs::create_dir_all(recordings_dir()).context("failed to create recordings/ directory")?;
    Ok(())
}

fn cmd_new(name: &str) -> Result<()> {
    ensure_recordings_dir()?;

    let conn = session::open_db(&db_path())?;

    if session::session_exists(&conn, name)? {
        bail!(
            "Session '{}' already exists. Use --resume {} to continue it.",
            name,
            name
        );
    }

    let notes_path = format!("{}.md", name);

    // Check if markdown file already exists (orphan from outside aside)
    if std::path::Path::new(&notes_path).exists() {
        bail!(
            "File '{}' already exists. Choose a different session name or delete the file.",
            notes_path
        );
    }

    let start_time = Local::now();

    session::create_session(&conn, name, &start_time, &notes_path)?;

    let segment_index = 0i64;
    let wav_path = format!("recordings/{}_seg{}.wav", name, segment_index);
    let offset_ms = 0i64;

    session::add_segment(&conn, name, segment_index, &wav_path, offset_ms, None)?;

    eprintln!("New session: {}", name);
    eprintln!("  Notes: {}", notes_path);
    eprintln!("  Audio: {}", wav_path);

    run_session(name, &notes_path, &wav_path, start_time, segment_index, &conn)
}

fn cmd_resume(name: &str) -> Result<()> {
    ensure_recordings_dir()?;

    let conn = session::open_db(&db_path())?;

    if !session::session_exists(&conn, name)? {
        bail!(
            "Session '{}' not found in database. Use `aside {}` to start a new session.",
            name,
            name
        );
    }

    let start_time = session::get_session_start_time(&conn, name)?;
    let notes_path = format!("{}.md", name);

    if !std::path::Path::new(&notes_path).exists() {
        bail!(
            "Notes file '{}' not found. Cannot resume without the markdown file.",
            notes_path
        );
    }

    let content = std::fs::read_to_string(&notes_path).context("failed to read notes file")?;
    let parsed = parser::parse_markdown(&content, &start_time);

    let segment_index = session::next_segment_index(&conn, name)?;
    let wav_path = format!("recordings/{}_seg{}.wav", name, segment_index);

    let offset_ms = (Local::now() - start_time).num_milliseconds();
    session::add_segment(&conn, name, segment_index, &wav_path, offset_ms, None)?;

    eprintln!("Resuming session: {}", name);
    eprintln!("  Notes: {} ({} lines loaded)", notes_path, parsed.len());
    eprintln!("  Audio: {}", wav_path);
    eprintln!(
        "  Offset: {}ms from original start",
        offset_ms
    );

    let mic_name = recorder::default_input_device_name().unwrap_or_else(|| "Unknown".into());
    let mut app = app::App::from_parsed(parsed, notes_path, start_time, mic_name);

    run_with_recorder(&mut app, &wav_path, name, segment_index, None, &conn)
}

fn cmd_list() -> Result<()> {
    let db = db_path();
    if !db.exists() {
        eprintln!("No sessions found (no database at {:?}).", db);
        return Ok(());
    }

    let conn = session::open_db(&db)?;
    let sessions = session::list_sessions(&conn)?;

    if sessions.is_empty() {
        eprintln!("No sessions found.");
        return Ok(());
    }

    eprintln!(
        "{:<20} {:<24} {:<8} {}",
        "NAME", "STARTED", "SEGS", "NOTES"
    );
    eprintln!("{}", "-".repeat(72));
    for s in &sessions {
        eprintln!(
            "{:<20} {:<24} {:<8} {}",
            s.name, s.start_time, s.segment_count, s.notes_path
        );
    }

    Ok(())
}

fn run_session(
    name: &str,
    notes_path: &str,
    wav_path: &str,
    start_time: chrono::DateTime<Local>,
    segment_index: i64,
    conn: &rusqlite::Connection,
) -> Result<()> {
    let mic_name = recorder::default_input_device_name().unwrap_or_else(|| "Unknown".into());
    let mut app = app::App::new(notes_path.to_string(), start_time, mic_name);
    run_with_recorder(&mut app, wav_path, name, segment_index, None, conn)
}

fn run_with_recorder(
    app: &mut app::App,
    wav_path: &str,
    session_name: &str,
    mut segment_index: i64,
    initial_device: Option<cpal::Device>,
    conn: &rusqlite::Connection,
) -> Result<()> {
    let mut current_wav = wav_path.to_string();
    let mut current_device = initial_device;

    loop {
        let stop_flag = Arc::new(AtomicBool::new(false));

        let recorder =
            recorder::RecorderHandle::start(stop_flag.clone(), current_device.as_ref())?;

        let tui_result = tui::run_tui(app, stop_flag.clone());

        // TUI has exited, stop_flag is set. Now finalize recording.
        stop_flag.store(true, Ordering::SeqCst);
        let duration = recorder.stop_and_write(&current_wav)?;
        session::update_segment_duration(conn, session_name, segment_index, duration)?;

        match tui_result {
            Ok(tui::TuiAction::Quit) => return Ok(()),
            Ok(tui::TuiAction::SwitchDevice(idx)) => {
                // Re-enumerate devices to get the actual cpal::Device
                let mut devices = recorder::list_input_devices();
                if idx >= devices.len() {
                    app.message = Some("Device no longer available".into());
                    current_device = None;
                    app.current_mic_name =
                        recorder::default_input_device_name().unwrap_or_else(|| "Unknown".into());
                } else {
                    let (name, dev) = devices.remove(idx);
                    app.current_mic_name = name;
                    current_device = Some(dev);
                }

                // Create a new segment
                segment_index = session::next_segment_index(conn, session_name)?;
                current_wav =
                    format!("recordings/{}_seg{}.wav", session_name, segment_index);
                let offset_ms = (Local::now() - app.start_time).num_milliseconds();
                session::add_segment(
                    conn,
                    session_name,
                    segment_index,
                    &current_wav,
                    offset_ms,
                    None,
                )?;
            }
            Err(e) => return Err(anyhow::anyhow!("{}", e)),
        }
    }
}
