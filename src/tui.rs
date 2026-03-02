use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Terminal,
};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::app::{App, AppMode, GUTTER_WIDTH};

pub enum TuiAction {
    Quit,
    SwitchDevice(usize),
}

/// Run the TUI event loop. Returns `TuiAction` indicating quit or device switch.
/// Sets `stop_flag` to signal the recorder thread to stop.
pub fn run_tui(app: &mut App, stop_flag: Arc<AtomicBool>) -> Result<TuiAction, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, app, &stop_flag);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;

    // Auto-save on exit
    if app.lines.iter().any(|l| !l.trim().is_empty()) {
        app.save()?;
        let count = app.export().lines().count();
        eprintln!("Saved {} lines to {}", count, app.output_path);
    }

    // Signal recorder to stop
    stop_flag.store(true, Ordering::SeqCst);

    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    stop_flag: &AtomicBool,
) -> Result<TuiAction, Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|f| render(f, app))?;

        if event::poll(std::time::Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => {
                    app.message = None;

                    if let Some(action) = handle_key(app, key) {
                        return Ok(action);
                    }
                }
                Event::Mouse(mouse) => {
                    if app.mode == AppMode::Normal {
                        handle_mouse(app, mouse);
                    }
                }
                _ => {}
            }
        }

        // Also check if stop was requested externally
        if stop_flag.load(Ordering::SeqCst) {
            return Ok(TuiAction::Quit);
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Option<TuiAction> {
    match app.mode {
        AppMode::DeviceSelect => handle_key_device_select(app, key),
        AppMode::Normal => handle_key_normal(app, key),
    }
}

fn handle_key_device_select(app: &mut App, key: KeyEvent) -> Option<TuiAction> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Up => {
            if app.selected_device > 0 {
                app.selected_device -= 1;
            }
        }
        KeyCode::Down => {
            if app.selected_device + 1 < app.devices.len() {
                app.selected_device += 1;
            }
        }
        KeyCode::Enter => {
            let idx = app.selected_device;
            app.mode = AppMode::Normal;
            return Some(TuiAction::SwitchDevice(idx));
        }
        _ => {}
    }
    None
}

fn handle_key_normal(app: &mut App, key: KeyEvent) -> Option<TuiAction> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let sup = key.modifiers.contains(KeyModifiers::SUPER);
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;

    match key.code {
        // Quit
        KeyCode::Char('c') if ctrl => return Some(TuiAction::Quit),

        // Save
        KeyCode::Char('s') if ctrl => {
            let _ = app.save();
        }

        // Device select
        KeyCode::Char('d') if ctrl => {
            let devices = crate::recorder::list_input_devices();
            app.devices = devices.iter().map(|(name, _)| name.clone()).collect();
            // Pre-select the current mic
            app.selected_device = app
                .devices
                .iter()
                .position(|n| *n == app.current_mic_name)
                .unwrap_or(0);
            app.mode = AppMode::DeviceSelect;
        }

        // Delete to start of line: Cmd+Backspace or Ctrl+U
        KeyCode::Backspace if sup => app.delete_to_line_start(),
        KeyCode::Char('u') if ctrl => app.delete_to_line_start(),

        // Delete word back: Alt/Option+Backspace
        KeyCode::Backspace if alt => app.delete_word_back(),

        // Delete word forward: Alt/Option+Delete
        KeyCode::Delete if alt => app.delete_word_forward(),

        // Word navigation: Alt/Option+Arrow
        KeyCode::Left if alt => app.move_word_left(),
        KeyCode::Right if alt => app.move_word_right(),

        // Home/End with Cmd or standalone
        KeyCode::Left if sup => app.home(),
        KeyCode::Right if sup => app.end(),
        KeyCode::Home => app.home(),
        KeyCode::End => app.end(),

        // Basic navigation
        KeyCode::Left if plain => app.move_left(),
        KeyCode::Right if plain => app.move_right(),
        KeyCode::Up if plain => app.move_up(),
        KeyCode::Down if plain => app.move_down(),

        // Editing
        KeyCode::Enter => app.enter(),
        KeyCode::Backspace if plain => app.backspace(),
        KeyCode::Delete if plain => app.delete(),
        KeyCode::Tab => {
            app.insert_char(' ');
            app.insert_char(' ');
        }

        // Character input (no ctrl modifier)
        KeyCode::Char(c) if !ctrl => app.insert_char(c),

        _ => {}
    }

    None
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.handle_click(mouse.column, mouse.row);
        }
        MouseEventKind::ScrollUp => {
            if app.scroll > 0 {
                app.scroll -= 1;
            }
        }
        MouseEventKind::ScrollDown => {
            app.scroll += 1;
        }
        _ => {}
    }
}

fn render(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let editor_area = chunks[0];
    let status_area = chunks[1];

    // Store for mouse click handling
    app.editor_area = editor_area;

    let visible_lines = editor_area.height.saturating_sub(2) as usize;
    app.ensure_cursor_visible(visible_lines);

    // Build visible lines
    let mut display_lines: Vec<Line> = Vec::new();
    let end = (app.scroll + visible_lines).min(app.lines.len());

    for i in app.scroll..end {
        let (gutter, edited) = app.gutter_label(i);

        let gutter_style = if edited {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let text_style = if i == app.cursor_line {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::Gray)
        };

        display_lines.push(Line::from(vec![
            Span::styled(gutter, gutter_style),
            Span::styled(&app.lines[i], text_style),
        ]));
    }

    let block = Block::default().borders(Borders::ALL).title(" aside ");
    let paragraph = Paragraph::new(display_lines).block(block);
    f.render_widget(paragraph, editor_area);

    // Cursor
    let cursor_x = editor_area.x + 1 + GUTTER_WIDTH + app.cursor_col as u16;
    let cursor_y = editor_area.y + 1 + (app.cursor_line - app.scroll) as u16;
    if cursor_x < editor_area.x + editor_area.width - 1
        && cursor_y < editor_area.y + editor_area.height - 1
    {
        f.set_cursor_position((cursor_x, cursor_y));
    }

    // Status bar
    let elapsed = app.elapsed_secs();
    let time = if elapsed >= 3600 {
        format!(
            "{:02}:{:02}:{:02}",
            elapsed / 3600,
            (elapsed % 3600) / 60,
            elapsed % 60
        )
    } else {
        format!("{:02}:{:02}", elapsed / 60, elapsed % 60)
    };

    let mic_peak = app.mic_level.swap(0, Ordering::Relaxed);
    let spk_peak = app.spk_level.swap(0, Ordering::Relaxed);

    let status_text = if let Some(ref msg) = app.message {
        format!(" {} | {}", time, msg)
    } else {
        format!(
            " {} | {} lines | mic {} spk {} | ^D device  ^S save  ^C quit",
            time,
            app.lines.len(),
            level_meter(mic_peak, 6),
            level_meter(spk_peak, 6),
        )
    };

    let status = Paragraph::new(status_text).style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(status, status_area);

    // Device select overlay
    if app.mode == AppMode::DeviceSelect {
        render_device_overlay(f, app);
    }
}

fn render_device_overlay(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    let max_name_len = app
        .devices
        .iter()
        .map(|n| n.len())
        .max()
        .unwrap_or(20)
        .max(20);
    let width = (max_name_len as u16 + 6).min(area.width.saturating_sub(4));
    let height = (app.devices.len() as u16 + 2).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, name) in app.devices.iter().enumerate() {
        let is_current = *name == app.current_mic_name;
        let marker = if is_current { "*" } else { " " };
        let prefix = if i == app.selected_device {
            ">"
        } else {
            " "
        };
        let label = format!("{}{} {}", prefix, marker, name);

        let style = if i == app.selected_device {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(label, style)));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Select mic (↑↓ Enter Esc) ");
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, overlay_area);
}

fn level_meter(peak_bits: u32, width: usize) -> String {
    let peak = f32::from_bits(peak_bits);
    if peak <= 0.0 {
        return "░".repeat(width);
    }
    let db = 20.0 * peak.log10();
    // Map -48 dB .. 0 dB onto 0 .. width bars
    let normalized = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
    let filled = (normalized * width as f32).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}
