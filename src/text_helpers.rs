pub fn prev_char_boundary(s: &str, col: usize) -> usize {
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

pub fn next_char_boundary(s: &str, col: usize) -> usize {
    let mut i = col + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

pub fn prev_word_boundary(line: &str, col: usize) -> usize {
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

pub fn next_word_boundary(line: &str, col: usize) -> usize {
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

pub fn next_word_end(line: &str, col: usize) -> usize {
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
