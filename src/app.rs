use chrono::{DateTime, Local};
use ratatui::layout::Rect;
use std::io;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use crate::parser::ParsedLine;
use crate::text_helpers::*;

pub const GUTTER_WIDTH: u16 = 9;

#[derive(PartialEq, Eq)]
pub enum AppMode {
    Normal,
    DeviceSelect,
}

pub struct App {
    pub lines: Vec<String>,
    pub created_at: Vec<DateTime<Local>>,
    pub edited_at: Vec<Option<DateTime<Local>>>,
    /// A line is "settled" once the cursor has left it.
    /// Only settled lines get edited_at timestamps on modification.
    pub settled: Vec<bool>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub scroll: usize,
    pub start_time: DateTime<Local>,
    pub output_path: String,
    pub message: Option<String>,
    pub editor_area: Rect,
    pub mode: AppMode,
    pub devices: Vec<String>,
    pub selected_device: usize,
    pub current_mic_name: String,
    pub mic_level: Arc<AtomicU32>,
    pub spk_level: Arc<AtomicU32>,
}

impl App {
    pub fn new(output_path: String, start_time: DateTime<Local>, mic_name: String) -> Self {
        Self {
            lines: vec![String::new()],
            created_at: vec![Local::now()],
            edited_at: vec![None],
            settled: vec![false],
            cursor_line: 0,
            cursor_col: 0,
            scroll: 0,
            start_time,
            output_path,
            message: None,
            editor_area: Rect::default(),
            mode: AppMode::Normal,
            devices: Vec::new(),
            selected_device: 0,
            current_mic_name: mic_name,
            mic_level: Arc::new(AtomicU32::new(0)),
            spk_level: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Reconstruct App state from parsed markdown lines (for --resume).
    pub fn from_parsed(
        parsed: Vec<ParsedLine>,
        output_path: String,
        start_time: DateTime<Local>,
        mic_name: String,
    ) -> Self {
        let mut lines = Vec::new();
        let mut created_at = Vec::new();
        let mut edited_at = Vec::new();
        let mut settled = Vec::new();

        for p in parsed {
            lines.push(p.text);
            created_at.push(p.created_at);
            edited_at.push(p.edited_at);
            settled.push(true); // All resumed lines are settled
        }

        // Append an empty line for the user to continue typing
        lines.push(String::new());
        created_at.push(Local::now());
        edited_at.push(None);
        settled.push(false);

        let cursor_line = lines.len() - 1;

        Self {
            lines,
            created_at,
            edited_at,
            settled,
            cursor_line,
            cursor_col: 0,
            scroll: cursor_line.saturating_sub(10),
            start_time,
            output_path,
            message: None,
            editor_area: Rect::default(),
            mode: AppMode::Normal,
            devices: Vec::new(),
            selected_device: 0,
            current_mic_name: mic_name,
            mic_level: Arc::new(AtomicU32::new(0)),
            spk_level: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn mark_edited(&mut self, line: usize) {
        if line < self.settled.len() && self.settled[line] {
            self.edited_at[line] = Some(Local::now());
            self.settled[line] = false;
        }
    }

    /// Mark the old line as settled when cursor moves to a different line.
    pub fn settle_on_move(&mut self, old_line: usize) {
        if old_line < self.settled.len() {
            self.settled[old_line] = true;
        }
    }

    pub fn elapsed_secs(&self) -> i64 {
        (Local::now() - self.start_time).num_seconds()
    }

    pub fn format_time(&self, ts: &DateTime<Local>) -> String {
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

    pub fn display_ts(&self, i: usize) -> (&DateTime<Local>, bool) {
        match self.edited_at[i] {
            Some(ref et) => (et, true),
            None => (&self.created_at[i], false),
        }
    }

    pub fn gutter_label(&self, i: usize) -> (String, bool) {
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
        (
            format!("{}{:<w$}", prefix, time_str, w = GUTTER_WIDTH as usize - 1),
            edited,
        )
    }

    // --- Editing ---

    pub fn insert_char(&mut self, c: char) {
        self.lines[self.cursor_line].insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
        self.mark_edited(self.cursor_line);
    }

    pub fn enter(&mut self) {
        let old_line = self.cursor_line;
        let rest = self.lines[self.cursor_line].split_off(self.cursor_col);
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

    pub fn backspace(&mut self) {
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

    pub fn delete(&mut self) {
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

    pub fn delete_word_back(&mut self) {
        if self.cursor_col == 0 {
            self.backspace();
            return;
        }
        let boundary = prev_word_boundary(&self.lines[self.cursor_line], self.cursor_col);
        self.lines[self.cursor_line].replace_range(boundary..self.cursor_col, "");
        self.cursor_col = boundary;
        self.mark_edited(self.cursor_line);
    }

    pub fn delete_to_line_start(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        self.lines[self.cursor_line].replace_range(..self.cursor_col, "");
        self.cursor_col = 0;
        self.mark_edited(self.cursor_line);
    }

    pub fn delete_word_forward(&mut self) {
        if self.cursor_col >= self.lines[self.cursor_line].len() {
            self.delete();
            return;
        }
        let boundary = next_word_boundary(&self.lines[self.cursor_line], self.cursor_col);
        self.lines[self.cursor_line].replace_range(self.cursor_col..boundary, "");
        self.mark_edited(self.cursor_line);
    }

    // --- Navigation ---

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col = prev_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
        } else if self.cursor_line > 0 {
            let old = self.cursor_line;
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.settle_on_move(old);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor_col < self.lines[self.cursor_line].len() {
            self.cursor_col = next_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
        } else if self.cursor_line + 1 < self.lines.len() {
            let old = self.cursor_line;
            self.cursor_line += 1;
            self.cursor_col = 0;
            self.settle_on_move(old);
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor_line > 0 {
            let old = self.cursor_line;
            self.cursor_line -= 1;
            self.snap_cursor_to_line();
            self.settle_on_move(old);
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            let old = self.cursor_line;
            self.cursor_line += 1;
            self.snap_cursor_to_line();
            self.settle_on_move(old);
        }
    }

    pub fn move_word_left(&mut self) {
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

    pub fn move_word_right(&mut self) {
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

    pub fn home(&mut self) {
        self.cursor_col = 0;
    }

    pub fn end(&mut self) {
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    pub fn snap_cursor_to_line(&mut self) {
        let len = self.lines[self.cursor_line].len();
        if self.cursor_col > len {
            self.cursor_col = len;
        }
        // Snap to char boundary
        while self.cursor_col > 0
            && !self.lines[self.cursor_line].is_char_boundary(self.cursor_col)
        {
            self.cursor_col -= 1;
        }
    }

    pub fn ensure_cursor_visible(&mut self, visible_lines: usize) {
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

    pub fn handle_click(&mut self, col: u16, row: u16) {
        let area = self.editor_area;
        let border: u16 = 1;

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

    pub fn export(&self) -> String {
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

    pub fn save(&mut self) -> io::Result<()> {
        let content = self.export();
        std::fs::write(&self.output_path, &content)?;
        let count = content.lines().count();
        self.message = Some(format!("Saved {} lines to {}", count, self.output_path));
        Ok(())
    }
}
