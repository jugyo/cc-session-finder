//! Multi-byte safe query editor: stores chars, supports cursor moves and
//! backspace at grapheme boundaries.

use unicode_segmentation::UnicodeSegmentation;

pub struct QueryEditor {
    /// Stored as a vector of grapheme clusters for safe cursor handling.
    graphemes: Vec<String>,
    /// Cursor in [0, graphemes.len()].
    cursor: usize,
    /// Cached string view.
    cache: String,
    dirty: bool,
}

impl QueryEditor {
    pub fn with_initial(s: String) -> Self {
        let graphemes: Vec<String> = s.graphemes(true).map(|g| g.to_string()).collect();
        let cursor = graphemes.len();
        let mut e = Self {
            graphemes,
            cursor,
            cache: String::new(),
            dirty: true,
        };
        e.refresh_cache();
        e
    }

    pub fn query(&self) -> &str {
        &self.cache
    }

    pub fn set_query(&mut self, s: String) {
        self.graphemes = s.graphemes(true).map(|g| g.to_string()).collect();
        self.cursor = self.graphemes.len();
        self.dirty = true;
        self.refresh_cache();
    }

    pub fn insert(&mut self, c: char) {
        self.graphemes.insert(self.cursor, c.to_string());
        self.cursor += 1;
        self.dirty = true;
        self.refresh_cache();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.graphemes.remove(self.cursor);
            self.dirty = true;
            self.refresh_cache();
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }
    pub fn move_right(&mut self) {
        if self.cursor < self.graphemes.len() {
            self.cursor += 1;
        }
    }
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }
    pub fn move_end(&mut self) {
        self.cursor = self.graphemes.len();
    }

    /// Delete the grapheme under the cursor (Emacs C-d).
    pub fn delete_forward(&mut self) {
        if self.cursor < self.graphemes.len() {
            self.graphemes.remove(self.cursor);
            self.dirty = true;
            self.refresh_cache();
        }
    }

    /// Kill from cursor to end of line (Emacs C-k).
    pub fn kill_to_end(&mut self) {
        if self.cursor < self.graphemes.len() {
            self.graphemes.truncate(self.cursor);
            self.dirty = true;
            self.refresh_cache();
        }
    }

    /// Kill from beginning of line to cursor (Emacs C-u in readline mode).
    pub fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.graphemes.drain(0..self.cursor);
            self.cursor = 0;
            self.dirty = true;
            self.refresh_cache();
        }
    }

    /// Kill the previous whitespace-delimited word (Emacs/readline C-w).
    pub fn kill_word_backward(&mut self) {
        let mut i = self.cursor;
        while i > 0 && is_whitespace_grapheme(&self.graphemes[i - 1]) {
            i -= 1;
        }
        while i > 0 && !is_whitespace_grapheme(&self.graphemes[i - 1]) {
            i -= 1;
        }
        if i < self.cursor {
            self.graphemes.drain(i..self.cursor);
            self.cursor = i;
            self.dirty = true;
            self.refresh_cache();
        }
    }

    /// Cursor position measured in display columns from the start of the
    /// editor. Used by the view layer for caret placement.
    pub fn cursor_col(&self) -> u16 {
        use unicode_width::UnicodeWidthStr;
        let mut w = 0u16;
        for g in &self.graphemes[..self.cursor] {
            w = w.saturating_add(UnicodeWidthStr::width(g.as_str()) as u16);
        }
        w
    }

    fn refresh_cache(&mut self) {
        if self.dirty {
            self.cache.clear();
            for g in &self.graphemes {
                self.cache.push_str(g);
            }
            self.dirty = false;
        }
    }
}

fn is_whitespace_grapheme(g: &str) -> bool {
    g.chars().all(char::is_whitespace)
}
