use chrono::{DateTime, Local};
use regex::Regex;

pub struct ParsedLine {
    pub text: String,
    pub created_at: DateTime<Local>,
    pub edited_at: Option<DateTime<Local>>,
}

/// Parse a time string like "MM:SS" or "HH:MM:SS" into total seconds.
fn parse_time_str(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        2 => {
            let m: i64 = parts[0].parse().ok()?;
            let s: i64 = parts[1].parse().ok()?;
            Some(m * 60 + s)
        }
        3 => {
            let h: i64 = parts[0].parse().ok()?;
            let m: i64 = parts[1].parse().ok()?;
            let s: i64 = parts[2].parse().ok()?;
            Some(h * 3600 + m * 60 + s)
        }
        _ => None,
    }
}

/// Parse markdown lines back into structured data for resume.
///
/// Recognizes two formats:
///   `[MM:SS] text`           → created_at only
///   `[MM:SS ~MM:SS] text`    → created_at + edited_at
///
/// Unrecognized lines are appended to the previous parsed line.
pub fn parse_markdown(content: &str, start_time: &DateTime<Local>) -> Vec<ParsedLine> {
    let re_edited = Regex::new(r"^\[(\d+:\d{2}(?::\d{2})?) ~(\d+:\d{2}(?::\d{2})?)\] (.*)$").unwrap();
    let re_simple = Regex::new(r"^\[(\d+:\d{2}(?::\d{2})?)\] (.*)$").unwrap();

    let mut lines: Vec<ParsedLine> = Vec::new();

    for raw_line in content.lines() {
        if let Some(caps) = re_edited.captures(raw_line) {
            let created_secs = parse_time_str(caps.get(1).unwrap().as_str());
            let edited_secs = parse_time_str(caps.get(2).unwrap().as_str());
            let text = caps.get(3).unwrap().as_str().to_string();

            if let (Some(c), Some(e)) = (created_secs, edited_secs) {
                lines.push(ParsedLine {
                    text,
                    created_at: *start_time + chrono::Duration::seconds(c),
                    edited_at: Some(*start_time + chrono::Duration::seconds(e)),
                });
                continue;
            }
        }

        if let Some(caps) = re_simple.captures(raw_line) {
            let created_secs = parse_time_str(caps.get(1).unwrap().as_str());
            let text = caps.get(2).unwrap().as_str().to_string();

            if let Some(c) = created_secs {
                lines.push(ParsedLine {
                    text,
                    created_at: *start_time + chrono::Duration::seconds(c),
                    edited_at: None,
                });
                continue;
            }
        }

        // Unrecognized line: append to previous, or create new entry
        if let Some(last) = lines.last_mut() {
            last.text.push('\n');
            last.text.push_str(raw_line);
        } else {
            // Orphan line at the top with no timestamp — use start_time
            lines.push(ParsedLine {
                text: raw_line.to_string(),
                created_at: *start_time,
                edited_at: None,
            });
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_start() -> DateTime<Local> {
        Local.with_ymd_and_hms(2025, 1, 1, 10, 0, 0).unwrap()
    }

    #[test]
    fn test_simple_line() {
        let start = make_start();
        let lines = parse_markdown("[01:30] hello world", &start);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "hello world");
        assert_eq!(
            (lines[0].created_at - start).num_seconds(),
            90
        );
        assert!(lines[0].edited_at.is_none());
    }

    #[test]
    fn test_edited_line() {
        let start = make_start();
        let lines = parse_markdown("[01:00 ~02:30] edited text", &start);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "edited text");
        assert_eq!(
            (lines[0].created_at - start).num_seconds(),
            60
        );
        let et = lines[0].edited_at.unwrap();
        assert_eq!((et - start).num_seconds(), 150);
    }

    #[test]
    fn test_unrecognized_appends() {
        let start = make_start();
        let input = "[00:10] first line\ncontinuation text";
        let lines = parse_markdown(input, &start);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "first line\ncontinuation text");
    }

    #[test]
    fn test_multiple_lines() {
        let start = make_start();
        let input = "[00:05] line one\n[00:10] line two\n[01:00 ~01:30] line three";
        let lines = parse_markdown(input, &start);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "line one");
        assert_eq!(lines[1].text, "line two");
        assert_eq!(lines[2].text, "line three");
    }

    #[test]
    fn test_round_trip() {
        // Export format → parse → same content
        let start = make_start();
        let input = "[00:00] first note\n[01:30] second note\n[05:00 ~06:00] edited note";
        let lines = parse_markdown(input, &start);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "first note");
        assert_eq!(lines[1].text, "second note");
        assert_eq!(lines[2].text, "edited note");
    }

    #[test]
    fn test_hhmmss_format() {
        let start = make_start();
        let lines = parse_markdown("[01:30:00] long session note", &start);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            (lines[0].created_at - start).num_seconds(),
            5400
        );
    }
}
