//! `read` and `more`, ported from `agent_read_range_from()`.
//!
//! The interesting part is not the header, it is the pair of numbers in the
//! truncation note.  A model that gets `[Read truncated. continue_offset=412;
//! continue_byte_offset=18344. Call more to continue.]` can ask for exactly the
//! next chunk, and `more` can resume from a byte offset instead of re-reading and
//! re-discarding 400 lines.  A resumed chunk may start in the middle of a line, and
//! saying so ("(continued)") is what stops the model from reading the first line of
//! its answer as if it were a whole line.
//!
//! Bytes are streamed, not slurped: the agent reads a 2 GB log file's first 120
//! lines every day, and a sandbox that buffered the file to find line 120 would be
//! the first thing the outer sandbox killed.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use crate::budget::{MAX_TOOL_BYTES, NOTE_MARGIN};
use crate::files;

/// Room the body may use before the read stops.  The header and the resume note
/// are written afterwards, which is why the body stops short of the real limit.
const BODY_LIMIT: usize = MAX_TOOL_BYTES - NOTE_MARGIN;

/// Where the next `more` resumes.  The C agent keeps this on its worker; the
/// sandbox owns it here, which is the only reason `more` can be served remotely.
#[derive(Debug, Clone)]
pub struct MoreState {
    pub path: String,
    pub next_line: usize,
    pub byte_offset: usize,
    pub mid_line: bool,
    pub bare: bool,
}

/// A byte cursor with one byte of look-ahead and a running position, which is what
/// the resume offset has to be measured in.
struct Cursor {
    source: BufReader<File>,
    pos: usize,
    held: Option<u8>,
}

impl Cursor {
    fn open(path: &str, offset: usize) -> std::io::Result<Cursor> {
        let mut file = files::open_regular(path)?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset as u64))?;
        }
        Ok(Cursor {
            source: BufReader::with_capacity(64 * 1024, file),
            pos: offset,
            held: None,
        })
    }

    fn byte(&mut self) -> std::io::Result<Option<u8>> {
        if let Some(byte) = self.held.take() {
            self.pos += 1;
            return Ok(Some(byte));
        }
        let mut one = [0u8; 1];
        // Reading one byte yields a byte or the end of the file; there is nothing
        // else for the caller to distinguish, so nothing to loop over.
        match self.source.read(&mut one)? {
            0 => Ok(None),
            _ => {
                self.pos += 1;
                Ok(Some(one[0]))
            }
        }
    }

    fn peek(&mut self) -> std::io::Result<Option<u8>> {
        if self.held.is_none() {
            let mut one = [0u8; 1];
            self.held = match self.source.read(&mut one)? {
                0 => None,
                _ => Some(one[0]),
            };
        }
        Ok(self.held)
    }
}

/// Reads `max_lines` lines starting at `start_line`, from `offset` bytes into the
/// file.  `mid_line` says the offset is not at a line start, which is true only for
/// a resumed read.
#[allow(clippy::too_many_arguments)]
pub fn read_range(
    path: &str,
    start_line: i64,
    max_lines: i64,
    whole_file: bool,
    bare: bool,
    offset: usize,
    mid_line: bool,
    more: &mut Option<MoreState>,
    set_more: bool,
) -> Result<String, String> {
    if path.is_empty() {
        // The C agent returns before it has touched anything, resume state
        // included, so the previous read is still the one `more` would continue.
        return Err("read requires path".into());
    }
    let result = read_inner(
        path, start_line, max_lines, whole_file, bare, offset, mid_line, more, set_more,
    );
    if result.is_err() && set_more {
        // A failed read leaves nothing to continue.  Keeping a stale offset would
        // silently repeat or skip a chunk of the last successful read.
        *more = None;
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn read_inner(
    path: &str,
    start_line: i64,
    max_lines: i64,
    whole_file: bool,
    bare: bool,
    offset: usize,
    mid_line: bool,
    more: &mut Option<MoreState>,
    set_more: bool,
) -> Result<String, String> {
    let mut cur = Cursor::open(path, offset)
        .map_err(|err| format!("read failed: {}", files::err_message(&err)))?;

    let start_line = std::cmp::max(start_line, 1) as usize;
    let max_lines = std::cmp::max(max_lines, 1) as usize;
    let mut line = if offset > 0 { start_line } else { 1 };

    // Walk past the lines we were not asked for.  This reads them, but only one
    // byte at a time through a buffer, and never holds them.
    while line < start_line {
        let Some(byte) = cur.byte().map_err(io_failure)? else {
            break;
        };
        if byte == b'\r' {
            // The newline of a CRLF belongs to this same line break.
            if cur.peek().map_err(io_failure)? == Some(b'\n') {
                cur.byte().map_err(io_failure)?;
            }
        }
        if byte == b'\n' || byte == b'\r' {
            line += 1;
        }
    }

    let mut body: Vec<u8> = Vec::new();
    let mut beginning = !mid_line;
    let mut prefix = true;
    let mut last_line = 0usize;
    let mut lines = 0usize;
    let mut stopped: Option<&'static str> = None;

    while whole_file || lines < max_lines {
        let Some(byte) = cur.peek().map_err(io_failure)? else {
            break;
        };
        if body.len() >= BODY_LIMIT && (byte & 0xc0 != 0x80 || body.len() >= BODY_LIMIT + 3) {
            // Out of room.  A continuation byte is allowed three more bytes so a
            // multi-byte character finishes instead of arriving as half a glyph.
            break;
        }
        if byte == 0 {
            // Half a binary file in the model's context is not a document, and the
            // bytes are useless for anything the model would do with them.
            stopped = Some("read encountered binary data");
            break;
        }
        cur.byte().map_err(io_failure)?;
        // Recorded before the line's bytes are written: a file that ends with a
        // newline has already counted the empty line after it, and reporting that
        // would put a line in the header that the body does not contain.
        last_line = line.max(last_line);
        if prefix && !bare {
            let mut label = String::new();
            let _ = write!(
                label,
                "{line}{} ",
                if beginning { "" } else { " (continued)" }
            );
            body.extend_from_slice(label.as_bytes());
        }
        prefix = false;
        beginning = false;

        if byte == b'\r' {
            if bare {
                body.push(b'\r');
            }
            let crlf = cur.peek().map_err(io_failure)? == Some(b'\n');
            if crlf {
                cur.byte().map_err(io_failure)?;
            }
            if !bare || crlf {
                // Normalised: the model should see one line ending, not a stray
                // control character that its own tools will not recognise.  Bare
                // mode keeps the bytes it was given and adds nothing.
                body.push(b'\n');
            }
            line += 1;
            lines += 1;
            beginning = true;
            prefix = true;
        } else {
            body.push(byte);
            if byte == b'\n' {
                line += 1;
                lines += 1;
                beginning = true;
                prefix = true;
            }
        }
    }

    let next_offset = cur.pos;
    let more_to_come = cur.peek().map_err(io_failure)?.is_some();

    if let Some(message) = stopped {
        return Err(message.into());
    }
    if whole_file && more_to_come {
        return Err(
            "whole read exceeds the 128 KiB output limit; use read and more for chunks".into(),
        );
    }

    let mut out = String::new();
    if !bare {
        let kind = if more_to_come {
            " (partial read)"
        } else {
            " (end of file)"
        };
        let _ = writeln!(
            out,
            "{path}: lines {}-{last_line}{kind}",
            if last_line > 0 { start_line } else { 0 }
        );
    }
    // The file's bytes are the answer; a file that is not valid UTF-8 gets
    // replacements here rather than a JSON encoder that cannot encode it at all.
    out.push_str(&String::from_utf8_lossy(&body));
    if more_to_come {
        let _ = write!(
            out,
            "\n[Read truncated. continue_offset={line}; continue_byte_offset={next_offset}{}. Call more to continue.]\n",
            if beginning { "" } else { " (within line)" }
        );
    }

    if set_more {
        *more = more_to_come.then(|| MoreState {
            path: path.to_string(),
            next_line: line,
            byte_offset: next_offset,
            mid_line: !beginning,
            bare,
        });
    }
    Ok(out)
}

/// Continues the previous read.
pub fn more(state: &mut Option<MoreState>, count: i64) -> Result<String, String> {
    let Some(saved) = state.clone() else {
        return Err("no previous output to continue".into());
    };
    read_range(
        &saved.path,
        saved.next_line as i64,
        count,
        false,
        saved.bare,
        saved.byte_offset,
        saved.mid_line,
        state,
        true,
    )
}

fn io_failure(err: std::io::Error) -> String {
    format!("read failed: {}", files::err_message(&err))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ds4-helper-read-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        path
    }

    fn write_file(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = temp_path(tag);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[allow(clippy::type_complexity)]
    fn read(path: &std::path::Path, start: i64, count: i64) -> Result<String, String> {
        read_range(
            path.to_str().unwrap(),
            start,
            count,
            false,
            false,
            0,
            false,
            &mut None,
            false,
        )
    }

    #[test]
    fn a_short_file_reads_whole_with_a_header() {
        let path = write_file("short", b"alpha\nbravo\ncharlie\n");
        let text = read(&path, 1, 500).unwrap();
        assert!(
            text.starts_with(&format!("{}: lines 1-3 (end of file)\n", path.display())),
            "{text}"
        );
        assert!(text.contains("1 alpha\n"), "{text}");
        assert!(text.contains("3 charlie\n"), "{text}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_partial_read_names_where_to_resume_and_remembers_it() {
        let body = (1..=50).map(|n| format!("line {n}\n")).collect::<String>();
        let path = write_file("partial", body.as_bytes());
        let mut state = None;
        let text = read_range(
            path.to_str().unwrap(),
            1,
            10,
            false,
            false,
            0,
            false,
            &mut state,
            true,
        )
        .unwrap();
        assert!(text.contains("lines 1-10 (partial read)"), "{text}");
        // "line 1\n" through "line 9\n" are 7 bytes each, "line 10\n" is 8.
        assert!(
            text.contains("[Read truncated. continue_offset=11; continue_byte_offset=71."),
            "{text}"
        );
        let saved = state.clone().unwrap();
        assert_eq!((saved.next_line, saved.byte_offset), (11, 71));
        assert!(!saved.mid_line);

        let next = more(&mut state, 5).unwrap();
        assert!(next.contains("lines 11-15 (partial read)"), "{next}");
        assert!(next.contains("11 line 11\n"), "{next}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_resume_inside_a_line_says_continued() {
        let path = write_file("midline", b"first\n0123456789abcdefghijklmnop\nlast\n");
        let mut state = Some(MoreState {
            path: path.to_str().unwrap().to_string(),
            next_line: 2,
            byte_offset: 16, // ten bytes into line 2, which starts at 6
            mid_line: true,
            bare: false,
        });
        let text = more(&mut state, 5).unwrap();
        assert!(text.contains("2 (continued) abcdefghijklmnop\n"), "{text}");
        assert!(text.contains("3 last\n"), "{text}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn carriage_returns_become_line_ends_unless_the_read_is_bare() {
        let path = write_file("crlf", b"one\r\ntwo\rthree\n");
        let text = read(&path, 1, 10).unwrap();
        assert!(text.contains("lines 1-3 (end of file)"), "{text}");
        assert!(!text.contains('\r'), "{text:?}");

        let bare = read_range(
            path.to_str().unwrap(),
            1,
            10,
            false,
            true,
            0,
            false,
            &mut None,
            false,
        )
        .unwrap();
        assert!(bare.contains("one\r\n"), "{bare:?}");
        assert!(bare.contains("two\r"), "{bare:?}");
        assert!(bare.starts_with("one"), "bare mode has no header: {bare:?}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn binary_content_is_refused() {
        let path = write_file("binary", b"text\x00more");
        assert_eq!(
            read(&path, 1, 10).unwrap_err(),
            "read encountered binary data"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn whole_is_an_error_rather_than_a_silent_cut() {
        let body = "x\n".repeat(600 * 1024);
        let path = write_file("whole", body.as_bytes());
        let err = read_range(
            path.to_str().unwrap(),
            1,
            500,
            true,
            false,
            0,
            false,
            &mut None,
            false,
        )
        .unwrap_err();
        assert!(err.contains("whole read exceeds"), "{err}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_read_stops_at_the_limit_without_splitting_a_character() {
        let body = "中文".repeat(80 * 1024);
        let path = write_file("wide", body.as_bytes());
        let mut state = None;
        let text = read_range(
            path.to_str().unwrap(),
            1,
            1_000_000,
            false,
            false,
            0,
            false,
            &mut state,
            true,
        )
        .unwrap();
        assert!(text.len() <= MAX_TOOL_BYTES, "{}", text.len());
        assert!(text.contains("[Read truncated."), "{}", tail(&text, 200));
        let offset = state.clone().unwrap().byte_offset;
        assert!(
            (body.as_bytes()[offset] & 0xc0) != 0x80,
            "offset {offset} is inside a character"
        );
        let next = more(&mut state, 1).unwrap();
        assert!(next.contains('中'), "{next}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn an_absent_more_and_an_absent_path_are_their_own_errors() {
        assert_eq!(
            more(&mut None, 24).unwrap_err(),
            "no previous output to continue"
        );
        assert_eq!(
            read_range("", 1, 10, false, false, 0, false, &mut None, false).unwrap_err(),
            "read requires path"
        );
    }

    #[test]
    fn a_missing_file_reports_the_errno() {
        let err = read(Path::new("/definitely/not/here"), 1, 10).unwrap_err();
        assert!(err.starts_with("read failed: "), "{err}");
        assert!(err.contains("No such file"), "{err}");
    }

    #[test]
    fn a_failed_read_clears_the_resume_state() {
        let path = write_file("clears", &"line\n".repeat(200).into_bytes());
        let mut state = None;
        read_range(
            path.to_str().unwrap(),
            1,
            5,
            false,
            false,
            0,
            false,
            &mut state,
            true,
        )
        .unwrap();
        assert!(state.is_some());
        std::fs::remove_file(&path).unwrap();
        let _ = more(&mut state, 5).unwrap_err();
        assert!(
            state.is_none(),
            "a read that failed must not leave an offset behind"
        );
    }

    #[test]
    fn a_line_range_past_the_end_reads_nothing() {
        let path = write_file("past", b"one\ntwo\n");
        let text = read(&path, 99, 10).unwrap();
        assert!(text.contains("lines 0-0 (end of file)"), "{text}");
        std::fs::remove_file(path).unwrap();
    }

    fn tail(text: &str, bytes: usize) -> String {
        text.chars()
            .skip(text.chars().count().saturating_sub(bytes))
            .collect()
    }

    use std::path::Path;
}
