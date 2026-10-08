// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `edit` replaces one uniquely-anchored span of a file.
//!
//! Three properties, all inherited from the C agent: `old` must match exactly once;
//! the write goes through the same atomic replace as `write`; and the file is checked
//! against the bytes that were searched, so an edit computed from a stale read is
//! refused rather than applied over someone else's change.
//!
//! The result text lets the model keep using line numbers it already has.  It says
//! which lines were touched, and how far the lines after them shifted.

use std::fmt::Write;

use crate::budget::Budget;
use crate::files;
use crate::protocol::Request;

const CONTEXT_BEFORE: usize = 5;
const CONTEXT_AFTER: usize = 8;
const EDITED_CONTEXT_HEAD: usize = 18;
const EDITED_CONTEXT_TAIL: usize = 18;
const UPTO_MARKER: &[u8] = b"[upto]";

/// Replaces `old` with `new` in `path`.
pub async fn edit(request: &Request, allow_upto: bool) -> Result<String, String> {
    let Some(path) = request.arg("path").filter(|p| !p.is_empty()) else {
        return Err("edit requires path".into());
    };
    let Some(old) = request.arg("old") else {
        return Err("edit requires non-empty old text".into());
    };
    if old.is_empty() {
        return Err("edit requires non-empty old text".into());
    }
    // An empty `new` is a deletion, which is a real edit.  A missing one is the model
    // leaving out a parameter.
    let Some(new) = request.arg("new") else {
        return Err("edit requires new text".into());
    };

    let data = files::read_bytes(path).await?;
    let (offset, removed, anchored) = find_old_span(&data, old.as_bytes(), allow_upto)?;

    let mut replacement = Vec::with_capacity(data.len() + new.len());
    replacement.extend_from_slice(&data[..offset]);
    replacement.extend_from_slice(new.as_bytes());
    replacement.extend_from_slice(&data[offset + removed..]);

    // The report describes the file as it will be, so it is assembled before the write.
    let old_spans = files::line_spans(&data);
    let new_spans = files::line_spans(&replacement);
    let kind = if anchored {
        "anchored old/new replacement"
    } else {
        "old/new replacement"
    };
    let mut out = Budget::new(crate::budget::MAX_TOOL_BYTES);
    let _ = writeln!(out, "Edited {path} using {kind}");
    if !old_spans.is_empty() {
        let last = std::cmp::min(
            offset + removed.saturating_sub(1),
            data.len().saturating_sub(1),
        );
        let start_line = files::line_for_offset(&old_spans, offset);
        let end_line = files::line_for_offset(&old_spans, last);
        let delta = new_spans.len() as i64 - old_spans.len() as i64;
        let _ = writeln!(
            out,
            "Touched old lines {start_line}-{end_line}; current post-edit context follows."
        );
        if delta != 0 {
            let _ = writeln!(
                out,
                "Line shift: old lines after {end_line} moved by {:+} (old line {} is now line {}). Re-read before relying on old line numbers there.",
                delta,
                end_line + 1,
                end_line as i64 + 1 + delta
            );
        }
        let mut anchor_end = end_line as i64 + delta;
        if anchor_end < start_line as i64 {
            anchor_end = start_line as i64;
        }
        append_context(
            &mut out,
            path,
            &replacement,
            start_line,
            anchor_end as usize,
        );
    }

    // The guard is the bytes the anchor was searched in.  If the file moved, this
    // offset points somewhere else.
    files::replace(path, replacement, Some(data)).await?;
    Ok(out.into_string())
}

/// Where `old` selects its span, and how long that span is.
fn find_old_span(
    data: &[u8],
    old: &[u8],
    allow_upto: bool,
) -> Result<(usize, usize, bool), String> {
    let upto = find(old, UPTO_MARKER);
    if !allow_upto || upto.is_none() {
        let offset = find_unique(data, old, "old text")?;
        return Ok((offset, old.len(), false));
    }
    let head_len = upto.expect("checked");
    if find(&old[head_len + UPTO_MARKER.len()..], UPTO_MARKER).is_some() {
        return Err("old text contains more than one [upto] marker".into());
    }
    let mut tail = &old[head_len + UPTO_MARKER.len()..];
    // The head already ends with the file's newline, so one written after the marker
    // is not part of the file.
    while first_is(tail, b'\n') || first_is(tail, b'\r') {
        tail = &tail[1..];
    }
    if !tail.iter().any(|b| !b.is_ascii_whitespace()) {
        return Err("old text after [upto] must include a unique tail anchor".into());
    }
    let head_pos = find_unique(data, &old[..head_len], "old head")?;
    let start = head_pos + head_len;
    let tail_pos = find_unique_after(data, start, tail, "old tail")?;
    Ok((head_pos, tail_pos + tail.len() - head_pos, true))
}

/// An `[upto]` edit needs its tail anchor to occur once after the head, which is what
/// makes it as safe as a whole-span match.
fn find_unique(data: &[u8], needle: &[u8], label: &str) -> Result<usize, String> {
    if needle.is_empty() {
        return Err(format!("{label} anchor is empty"));
    }
    let first = find(data, needle).ok_or_else(|| format!("{label} anchor not found"))?;
    if find_at(data, first + 1, needle).is_some() {
        return Err(format!("{label} anchor is not unique"));
    }
    Ok(first)
}

fn find_unique_after(
    data: &[u8],
    start: usize,
    needle: &[u8],
    label: &str,
) -> Result<usize, String> {
    if needle.is_empty() {
        return Err(format!("{label} anchor is empty"));
    }
    if start > data.len() {
        return Err(format!("{label} search starts outside file"));
    }
    let first = find_at(data, start, needle)
        .ok_or_else(|| format!("{label} anchor not found after old head"))?;
    if find_at(data, first + 1, needle).is_some() {
        return Err(format!("{label} anchor is not unique after old head"));
    }
    Ok(first)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    find_at(haystack, 0, needle)
}

fn find_at(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let tail = haystack.get(from..)?;
    tail.windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn first_is(haystack: &[u8], byte: u8) -> bool {
    haystack.first() == Some(&byte)
}

/// Post-edit lines around the edit, with the edited span shortened when it is longer
/// than the model can use.
fn append_context(
    out: &mut Budget,
    path: &str,
    data: &[u8],
    anchor_start: usize,
    anchor_end: usize,
) {
    let spans = files::line_spans(data);
    if spans.is_empty() {
        return;
    }
    let anchor_start = anchor_start.clamp(1, spans.len());
    let anchor_end = anchor_end.clamp(anchor_start, spans.len());
    let ctx_start = anchor_start.saturating_sub(CONTEXT_BEFORE).max(1);
    let ctx_end = (anchor_end + CONTEXT_AFTER).min(spans.len());

    let _ = writeln!(
        out,
        "Current file around edit: {path} lines {ctx_start}-{ctx_end} of {}",
        spans.len()
    );

    let edited = anchor_end - anchor_start + 1;
    if edited <= EDITED_CONTEXT_HEAD + EDITED_CONTEXT_TAIL {
        for line in ctx_start..=ctx_end {
            append_line(out, data, spans[line - 1], line);
        }
    } else {
        let head_end = anchor_start + EDITED_CONTEXT_HEAD - 1;
        let tail_start = anchor_end.saturating_sub(EDITED_CONTEXT_TAIL) + 1;
        for line in ctx_start..=head_end {
            append_line(out, data, spans[line - 1], line);
        }
        let _ = writeln!(
            out,
            "... {} edited lines omitted ...",
            tail_start.saturating_sub(head_end + 1)
        );
        for line in tail_start..=ctx_end {
            append_line(out, data, spans[line - 1], line);
        }
    }
}

fn append_line(out: &mut Budget, data: &[u8], span: (usize, usize), line: usize) {
    let content = trim_line_end(&data[span.0..span.1]);
    let _ = write!(out, "{line} ");
    // The bytes are the file's.  A file that is not valid UTF-8 still has to be
    // reportable.
    out.write_str(&String::from_utf8_lossy(content)).ok();
    let _ = writeln!(out);
}

fn trim_line_end(line: &[u8]) -> &[u8] {
    let trimmed = match line {
        [rest @ .., b'\n'] => rest,
        _ => line,
    };
    match trimmed {
        [rest @ .., b'\r'] => rest,
        _ => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_request;

    fn request(args: &str) -> crate::protocol::Request {
        parse_request(format!(r#"{{"id":1,"tool":"edit","args":{{{args}}}}}"#).as_bytes()).unwrap()
    }

    /// A file holding `body`, in a directory that deletes itself with the test.  The
    /// caller keeps the directory by holding the second half of the pair.
    fn temp(tag: &str, body: &str) -> (String, tempfile::TempDir) {
        let dir = tempfile::TempDir::with_prefix(format!("ds4-helper-edit-{tag}-")).unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, body).unwrap();
        (path.to_str().unwrap().to_string(), dir)
    }

    fn escape(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }

    #[tokio::test]
    async fn a_unique_edit_reports_the_touched_lines_and_the_context() {
        let body = (1..=12).map(|n| format!("line {n}\n")).collect::<String>();
        let (path, _dir) = temp("unique", &body);
        let text = edit(
            &request(&format!(
                r#""path":"{path}","old":"line 5","new":"CHANGED""#
            )),
            false,
        )
        .await
        .unwrap();
        assert!(
            text.starts_with(&format!("Edited {path} using old/new replacement\n")),
            "{text}"
        );
        assert!(text.contains("Touched old lines 5-5"), "{text}");
        assert!(!text.contains("Line shift"), "{text}");
        assert!(text.contains("Current file around edit: "), "{text}");
        assert!(text.contains("5 CHANGED\n"), "{text}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("CHANGED\n")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_ambiguous_selector_is_refused_and_nothing_is_written() {
        let (path, _dir) = temp("ambiguous", "aa\nbb\naa\n");
        let err = edit(
            &request(&format!(r#""path":"{path}","old":"aa","new":"cc""#)),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "old text anchor is not unique");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aa\nbb\naa\n");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_missing_anchor_and_a_stale_read_are_both_refused() {
        let (path, _dir) = temp("missing", "one\n");
        let err = edit(
            &request(&format!(r#""path":"{path}","old":"nope","new":"x""#)),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "old text anchor not found");

        // Somebody else rewrote the file between the search and the write.
        let stale = {
            let data = files::read_bytes(&path).await.unwrap();
            std::fs::write(&path, "someone else\n").unwrap();
            let mut replacement = Vec::new();
            replacement.extend_from_slice(&data[..0]);
            replacement.extend_from_slice(b"mine");
            files::replace(&path, replacement, Some(data))
                .await
                .unwrap_err()
        };
        assert!(
            stale.contains("file changed while editing; read it again"),
            "{stale}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else\n");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn adding_lines_reports_the_shift_so_old_numbers_are_not_trusted() {
        // Words rather than bare numbers.  "5" also occurs inside "15" and "25", and
        // an anchor that is not unique is refused before anything is written.
        let body = (1..=30).map(|n| format!("row {n}\n")).collect::<String>();
        let (path, _dir) = temp("shift", &body);
        let text = edit(
            &request(&format!(
                r#""path":"{path}","old":"row 5","new":"row 5\nrow 5a\nrow 5b""#
            )),
            false,
        )
        .await
        .unwrap();
        assert!(text.contains("Touched old lines 5-5"), "{text}");
        assert!(text.contains("moved by +2"), "{text}");
        assert!(text.contains("old line 6 is now line 8"), "{text}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_upto_span_edits_between_two_anchors_when_enabled() {
        let (path, _dir) = temp("upto", "head\nmiddle junk that is long\ntail\n");
        let old = escape("head\n[upto]\ntail\n");
        let text = edit(
            &request(&format!(
                r#""path":"{path}","old":"{old}","new":"replaced\n""#
            )),
            true,
        )
        .await
        .unwrap();
        assert!(
            text.contains("using anchored old/new replacement"),
            "{text}"
        );
        // The span runs from the head through the tail, so both anchors go with it.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced\n");

        // The same request without the flag treats the marker as literal text.
        let (path, _dir) = temp("upto-off", "head\nmiddle junk that is long\ntail\n");
        let err = edit(
            &request(&format!(
                r#""path":"{path}","old":"{old}","new":"replaced\n""#
            )),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "old text anchor not found");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_upto_marker_needs_a_real_tail_anchor() {
        let (path, _dir) = temp("upto-tail", "head\nbody\n");
        let old = escape("head\n[upto]\n");
        let err = edit(
            &request(&format!(r#""path":"{path}","old":"{old}","new":"x""#)),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "old text after [upto] must include a unique tail anchor"
        );

        let old = escape("head\n[upto]\nbody\n[upto]\n");
        let err = edit(
            &request(&format!(r#""path":"{path}","old":"{old}","new":"x""#)),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "old text contains more than one [upto] marker");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_long_edited_span_is_summarised_rather_than_dumped() {
        let body = (1..=200).map(|n| format!("{n}\n")).collect::<String>();
        let (path, _dir) = temp("long", &body);
        let text = edit(
            &request(&format!(
                r#""path":"{path}","old":"{}","new":"{}""#,
                escape("50\n51\n52\n"),
                escape(
                    // More lines than the edited-context limits, so the middle is left out.
                    &(60..130).map(|n| format!("new {n}\n")).collect::<String>()
                )
            )),
            false,
        )
        .await
        .unwrap();
        assert!(text.contains("edited lines omitted"), "{text}");
        assert!(text.len() < 4096, "{} bytes", text.len());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn argument_errors_come_before_any_disk_access() {
        assert_eq!(
            edit(&request(r#""old":"a","new":"b""#), false)
                .await
                .unwrap_err(),
            "edit requires path"
        );
        assert_eq!(
            edit(&request(r#""path":"/tmp/x","new":"b""#), false)
                .await
                .unwrap_err(),
            "edit requires non-empty old text"
        );
        assert_eq!(
            edit(&request(r#""path":"/tmp/x","old":"""#), false)
                .await
                .unwrap_err(),
            "edit requires non-empty old text"
        );
        assert_eq!(
            edit(&request(r#""path":"/tmp/x","old":"a""#), false)
                .await
                .unwrap_err(),
            "edit requires new text"
        );
    }

    #[tokio::test]
    async fn an_empty_new_text_deletes_the_span() {
        let (path, _dir) = temp("delete", "keep\ndrop\nkeep2\n");
        edit(
            &request(&format!(r#""path":"{path}","old":"drop\n","new":"""#)),
            false,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\nkeep2\n");
        std::fs::remove_file(path).unwrap();
    }
}
