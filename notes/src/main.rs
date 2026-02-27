// Timestamped note-taking TUI for live calls.
// Each line gets a timestamp when created (on Enter).
// Exports to markdown matching the interleaved transcript spec.
//
// Usage: notes [output.md]
// Ctrl+S to save, Ctrl+C to save and quit.

use chrono::{DateTime, Local};
use clap::Parser;
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
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use std::io;

const GUTTER_WIDTH: u16 = 9;

#[derive(Parser)]
#[command(about = "Timestamped note-taking for live calls")]
struct Args {
    /// Output markdown file
    #[arg(default_value = "notes.md")]
    output: String,

    /// Start time offset in seconds (sync with recorder)
    #[arg(long, default_value = "0")]
    offset: i64,
}

struct App {
    lines: Vec<String>,
    created_at: Vec<DateTime<Local>>,
    edited_at: Vec<Option<DateTime<Local>>>,
    /// A line is "settled" once the cursor has left it.
    /// Only settled lines get edited_at timestamps on modification.
    settled: Vec<bool>,
    cursor_line: usize,
    cursor_col: usize,
    scroll: usize,
    start_time: DateTime<Local>,
    output_path: String,
    message: Option<String>,
    editor_area: Rect,
}

impl App {
    fn new(output_path: String, offset_secs: i64) -> Self {
        let now = Local::now();
        let start = now - chrono::Duration::seconds(offset_secs);
        Self {
            lines: vec![String::new()],
            created_at: vec![now],
            edited_at: vec![None],
            settled: vec![false],
            cursor_line: 0,
            cursor_col: 0,
            scroll: 0,
            start_time: start,
            output_path,
            message: None,
            editor_area: Rect::default(),
        }
    }

    fn mark_edited(&mut self, line: usize) {
        // Only stamp edited_at if the cursor previously left this line.
        // Then un-settle so subsequent keystrokes in the same visit don't keep updating.
        if line < self.settled.len() && self.settled[line] {
            self.edited_at[line] = Some(Local::now());
            self.settled[line] = false;
        }
    }

    /// Mark the old line as settled when cursor moves to a different line.
    fn settle_on_move(&mut self, old_line: usize) {
        if old_line < self.settled.len() {
            self.settled[old_line] = true;
        }
    }

    fn elapsed_secs(&self) -> i64 {
        (Local::now() - self.start_time).num_seconds()
    }

    fn format_time(&self, ts: &DateTime<Local>) -> String {
        let elapsed = (*ts - self.start_time).num_seconds().max(0);
        let h = elapsed / 3600;
        let m = (elapsed % 3600) / 60;
        let s = elapsed % 60;
        if h > 0 {
            format!("{:02}:{:02}:{:02}", h, m, s)
        } else {
            format!("{:02}:{:02}", m, s)
        }
    }

    fn display_ts(&self, i: usize) -> (&DateTime<Local>, bool) {
        match self.edited_at[i] {
            Some(ref et) => (et, true),
            None => (&self.created_at[i], false),
        }
    }

    fn gutter_label(&self, i: usize) -> (String, bool) {
        // Don't show timestamps for empty lines (reduces visual noise)
        if self.lines[i].trim().is_empty() && i != self.cursor_line {
            return (" ".repeat(GUTTER_WIDTH as usize), false);
        }

        let (ts, edited) = self.display_ts(i);

        // Collapse if same second + same edit status as previous line
        if i > 0 {
            let (prev_ts, prev_edited) = self.display_ts(i - 1);
            if (*ts - *prev_ts).num_seconds().abs() == 0 && edited == prev_edited {
                return (" ".repeat(GUTTER_WIDTH as usize), edited);
            }
        }

        let time_str = self.format_time(ts);
        let prefix = if edited { "~" } else { " " };
        (format!("{}{:<w$}", prefix, time_str, w = GUTTER_WIDTH as usize - 1), edited)
    }

    // --- Editing ---

    fn insert_char(&mut self, c: char) {
        self.lines[self.cursor_line].insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
        self.mark_edited(self.cursor_line);
    }

    fn enter(&mut self) {
        let old_line = self.cursor_line;
        let rest = self.lines[self.cursor_line].split_off(self.cursor_col);
        // Mark as edited if content was split (not just Enter at end of line)
        if !rest.is_empty() && self.cursor_col > 0 {
            self.mark_edited(self.cursor_line);
        }
        self.settle_on_move(old_line);
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.lines.insert(self.cursor_line, rest);
        self.created_at.insert(self.cursor_line, Local::now());
        self.edited_at.insert(self.cursor_line, None);
        self.settled.insert(self.cursor_line, false);
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let prev = prev_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
            self.lines[self.cursor_line].replace_range(prev..self.cursor_col, "");
            self.cursor_col = prev;
            self.mark_edited(self.cursor_line);
        } else if self.cursor_line > 0 {
            let old_line = self.cursor_line;
            let current = self.lines.remove(self.cursor_line);
            self.created_at.remove(self.cursor_line);
            self.edited_at.remove(self.cursor_line);
            self.settled.remove(self.cursor_line);
            self.settle_on_move(old_line.min(self.lines.len().saturating_sub(1)));
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.mark_edited(self.cursor_line);
        }
    }

    fn delete(&mut self) {
        if self.cursor_col < self.lines[self.cursor_line].len() {
            let next = next_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
            self.lines[self.cursor_line].replace_range(self.cursor_col..next, "");
            self.mark_edited(self.cursor_line);
        } else if self.cursor_line + 1 < self.lines.len() {
            let next_line = self.lines.remove(self.cursor_line + 1);
            self.created_at.remove(self.cursor_line + 1);
            self.edited_at.remove(self.cursor_line + 1);
            self.settled.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next_line);
            self.mark_edited(self.cursor_line);
        }
    }

    fn delete_word_back(&mut self) {
        if self.cursor_col == 0 {
            self.backspace();
            return;
        }
        let boundary = prev_word_boundary(&self.lines[self.cursor_line], self.cursor_col);
        self.lines[self.cursor_line].replace_range(boundary..self.cursor_col, "");
        self.cursor_col = boundary;
        self.mark_edited(self.cursor_line);
    }

    fn delete_to_line_start(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        self.lines[self.cursor_line].replace_range(..self.cursor_col, "");
        self.cursor_col = 0;
        self.mark_edited(self.cursor_line);
    }

    fn delete_word_forward(&mut self) {
        if self.cursor_col >= self.lines[self.cursor_line].len() {
            self.delete();
            return;
        }
        let boundary = next_word_boundary(&self.lines[self.cursor_line], self.cursor_col);
        self.lines[self.cursor_line].replace_range(self.cursor_col..boundary, "");
        self.mark_edited(self.cursor_line);
    }

    // --- Navigation ---

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col = prev_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
        } else if self.cursor_line > 0 {
            let old = self.cursor_line;
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.settle_on_move(old);
        }
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.lines[self.cursor_line].len() {
            self.cursor_col = next_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
        } else if self.cursor_line + 1 < self.lines.len() {
            let old = self.cursor_line;
            self.cursor_line += 1;
            self.cursor_col = 0;
            self.settle_on_move(old);
        }
    }

    fn move_up(&mut self) {
        if self.cursor_line > 0 {
            let old = self.cursor_line;
            self.cursor_line -= 1;
            self.snap_cursor_to_line();
            self.settle_on_move(old);
        }
    }

    fn move_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            let old = self.cursor_line;
            self.cursor_line += 1;
            self.snap_cursor_to_line();
            self.settle_on_move(old);
        }
    }

    fn move_word_left(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                let old = self.cursor_line;
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].len();
                self.settle_on_move(old);
            }
            return;
        }
        self.cursor_col = prev_word_boundary(&self.lines[self.cursor_line], self.cursor_col);
    }

    fn move_word_right(&mut self) {
        if self.cursor_col >= self.lines[self.cursor_line].len() {
            if self.cursor_line + 1 < self.lines.len() {
                let old = self.cursor_line;
                self.cursor_line += 1;
                self.cursor_col = 0;
                self.settle_on_move(old);
            }
            return;
        }
        self.cursor_col = next_word_end(&self.lines[self.cursor_line], self.cursor_col);
    }

    fn home(&mut self) {
        self.cursor_col = 0;
    }

    fn end(&mut self) {
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    fn snap_cursor_to_line(&mut self) {
        let len = self.lines[self.cursor_line].len();
        if self.cursor_col > len {
            self.cursor_col = len;
        }
        // Snap to char boundary
        while self.cursor_col > 0 && !self.lines[self.cursor_line].is_char_boundary(self.cursor_col)
        {
            self.cursor_col -= 1;
        }
    }

    fn ensure_cursor_visible(&mut self, visible_lines: usize) {
        if visible_lines == 0 {
            return;
        }
        if self.cursor_line < self.scroll {
            self.scroll = self.cursor_line;
        } else if self.cursor_line >= self.scroll + visible_lines {
            self.scroll = self.cursor_line - visible_lines + 1;
        }
    }

    // --- Mouse ---

    fn handle_click(&mut self, col: u16, row: u16) {
        let area = self.editor_area;
        let border: u16 = 1;

        // Check bounds (inside borders, past gutter)
        if col < area.x + border + GUTTER_WIDTH || col >= area.x + area.width - border {
            return;
        }
        if row < area.y + border || row >= area.y + area.height - border {
            return;
        }

        let click_line = (row - area.y - border) as usize + self.scroll;
        let click_col = (col - area.x - border - GUTTER_WIDTH) as usize;

        if click_line < self.lines.len() {
            let old = self.cursor_line;
            self.cursor_line = click_line;
            self.cursor_col = click_col.min(self.lines[self.cursor_line].len());
            self.snap_cursor_to_line();
            if old != self.cursor_line {
                self.settle_on_move(old);
            }
        }
    }

    // --- Export ---

    fn export(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let created = self.format_time(&self.created_at[i]);
            match self.edited_at[i] {
                Some(ref et) => {
                    let edited = self.format_time(et);
                    out.push_str(&format!("[{} ~{}] {}\n", created, edited, line));
                }
                None => {
                    out.push_str(&format!("[{}] {}\n", created, line));
                }
            }
        }
        out
    }

    fn save(&mut self) -> io::Result<()> {
        let content = self.export();
        std::fs::write(&self.output_path, &content)?;
        let count = content.lines().count();
        self.message = Some(format!("Saved {} lines to {}", count, self.output_path));
        Ok(())
    }
}

// --- Word/char boundary helpers ---

fn prev_char_boundary(s: &str, col: usize) -> usize {
    let mut i = col.min(s.len());
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, col: usize) -> usize {
    let mut i = col + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

fn prev_word_boundary(line: &str, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    let chars: Vec<(usize, char)> = line[..col].char_indices().collect();
    if chars.is_empty() {
        return 0;
    }
    let mut i = chars.len();
    // Skip whitespace backward
    while i > 0 && chars[i - 1].1.is_whitespace() {
        i -= 1;
    }
    // Skip word chars backward
    while i > 0 && !chars[i - 1].1.is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        0
    } else {
        chars[i].0
    }
}

fn next_word_boundary(line: &str, col: usize) -> usize {
    if col >= line.len() {
        return line.len();
    }
    let chars: Vec<(usize, char)> = line[col..].char_indices().collect();
    let mut i = 0;
    // Skip word chars forward
    while i < chars.len() && !chars[i].1.is_whitespace() {
        i += 1;
    }
    // Skip whitespace forward
    while i < chars.len() && chars[i].1.is_whitespace() {
        i += 1;
    }
    if i >= chars.len() {
        line.len()
    } else {
        col + chars[i].0
    }
}

fn next_word_end(line: &str, col: usize) -> usize {
    if col >= line.len() {
        return line.len();
    }
    let chars: Vec<(usize, char)> = line[col..].char_indices().collect();
    let mut i = 0;
    // Skip whitespace forward
    while i < chars.len() && chars[i].1.is_whitespace() {
        i += 1;
    }
    // Skip word chars forward
    while i < chars.len() && !chars[i].1.is_whitespace() {
        i += 1;
    }
    if i >= chars.len() {
        line.len()
    } else {
        col + chars[i].0
    }
}

// --- Main ---

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut app = App::new(args.output, args.offset);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut app);

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

    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|f| render(f, app))?;

        if event::poll(std::time::Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => {
                    app.message = None;

                    if handle_key(app, key) {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => {
                    handle_mouse(app, mouse);
                }
                _ => {}
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let sup = key.modifiers.contains(KeyModifiers::SUPER);
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;

    match key.code {
        // Quit
        KeyCode::Char('c') if ctrl => return true,

        // Save
        KeyCode::Char('s') if ctrl => {
            let _ = app.save();
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

    false
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
            Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM)
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

    let block = Block::default().borders(Borders::ALL).title(" notes ");
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
        format!("{:02}:{:02}:{:02}", elapsed / 3600, (elapsed % 3600) / 60, elapsed % 60)
    } else {
        format!("{:02}:{:02}", elapsed / 60, elapsed % 60)
    };

    let status_text = if let Some(ref msg) = app.message {
        format!(" {} | {}", time, msg)
    } else {
        format!(
            " {} | {} lines | ^S save  ^C quit  opt+\u{232b} word del",
            time,
            app.lines.len()
        )
    };

    let status = Paragraph::new(status_text).style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(status, status_area);
}
