//! The byte budget every tool output shares.
//!
//! An over-long answer pushes the conversation out of the model's window, so the
//! sandbox keeps the agent's rule: stop at 128 KiB on a character boundary and say
//! so.

use std::fmt;
use std::fmt::Write;

/// The same limit the agent applies to a sandbox answer.
pub const MAX_TOOL_BYTES: usize = 128 * 1024;

/// Room kept free for the note, so the finished text still fits the limit.
pub const NOTE_MARGIN: usize = 4096;

const NOTE: &str = "\n[Output truncated at the tool byte limit. Narrow the request.]\n";

/// A `String` that stops growing at its limit, through [`fmt::Write`].
pub struct Budget {
    text: String,
    limit: usize,
    truncated: bool,
}

impl Budget {
    pub fn new(limit: usize) -> Self {
        Budget {
            text: String::new(),
            limit,
            truncated: false,
        }
    }

    /// A limit that leaves room for the note itself.
    pub fn for_output() -> Self {
        Self::new(MAX_TOOL_BYTES.saturating_sub(NOTE_MARGIN))
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The text so far, without the note: `read` adds its own with resume coordinates.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Closes the text, adding the note when something was dropped.
    pub fn into_string(mut self) -> String {
        if self.truncated {
            self.truncated = false;
            self.limit = usize::MAX;
            let _ = self.write_str(NOTE);
        }
        self.text
    }
}

impl Write for Budget {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.truncated || s.is_empty() {
            return Ok(());
        }
        let room = self.limit.saturating_sub(self.text.len());
        if s.len() <= room {
            self.text.push_str(s);
            return Ok(());
        }
        // Half a multi-byte character is worse than the text left out.
        let mut cut = room;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.text.push_str(&s[..cut]);
        self.truncated = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_untouched() {
        let mut b = Budget::new(100);
        writeln!(b, "one {}", 2).unwrap();
        assert_eq!(b.into_string(), "one 2\n");
    }

    #[test]
    fn truncation_stops_further_output_and_says_so() {
        let mut b = Budget::new(16);
        write!(b, "aaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(b.truncated());
        write!(b, "more").unwrap();
        let text = b.into_string();
        assert!(text.starts_with("aaaaaaaaaaaaaaaa"), "{text}");
        assert!(text.ends_with(NOTE), "{text}");
    }

    #[test]
    fn a_multi_byte_character_is_never_cut_in_half() {
        let mut b = Budget::new(5);
        write!(b, "a😀bc").unwrap();
        let text = b.into_string();
        // "a" + the 4-byte emoji fit exactly; "bc" does not.
        assert!(text.starts_with("a😀"), "{}", text.escape_debug());
        assert!(!text[NOTE.len()..].contains('b'));
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    #[test]
    fn a_budget_that_never_fits_still_reports_the_note() {
        let mut b = Budget::new(1);
        write!(b, "{}", "x".repeat(200)).unwrap();
        assert!(b.into_string().ends_with(NOTE));
    }

    #[test]
    fn the_output_budget_leaves_room_for_the_note() {
        let mut b = Budget::for_output();
        for _ in 0..4000 {
            writeln!(b, "{}", "x".repeat(64)).unwrap();
        }
        assert!(b.into_string().len() <= MAX_TOOL_BYTES);
    }
}
