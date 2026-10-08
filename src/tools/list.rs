// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `list` prints one directory, in the order the filesystem gives it.
//!
//! Not sorted.  The agent prints `readdir()` order, and the model reads that order.

use std::fmt::Write;

use crate::budget::Budget;
use crate::files;
use crate::protocol::Request;

const MAX_ENTRIES: usize = 300;

pub async fn list(request: &Request) -> Result<String, String> {
    let path = request.arg("path").filter(|p| !p.is_empty()).unwrap_or(".");

    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(entries) => entries,
        Err(err) => return Err(format!("opendir failed: {}", files::err_message(&err))),
    };

    let mut out = Budget::for_output();
    let _ = writeln!(out, "{path}:");
    let mut shown = 0;
    let mut more = false;
    loop {
        if shown >= MAX_ENTRIES {
            // The note means an entry was left out, not that the cap was reached, so
            // read one more and let whether it exists decide.  The C agent stops with
            // the 301st `readdir` result in hand and says nothing when the directory
            // ended at the cap.  A failed read ends the directory there too.
            more = matches!(entries.next_entry().await, Ok(Some(_)));
            break;
        }
        let Ok(Some(entry)) = entries.next_entry().await else {
            break;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // lstat, not stat, so a symlink reads as a pointer.
        let full = entry.path();
        let Ok(meta) = tokio::fs::symlink_metadata(&full).await else {
            continue;
        };
        let file_type = meta.file_type();
        let kind = if file_type.is_dir() {
            'd'
        } else if file_type.is_symlink() {
            'l'
        } else if file_type.is_file() {
            '-'
        } else {
            '?'
        };
        let suffix = if file_type.is_dir() { "/" } else { "" };
        // The width matches `%10lld`, so sizes line up.
        let _ = writeln!(out, "{kind} {:>10} {name}{suffix}", meta.len());
        shown += 1;
    }
    if more {
        let _ = writeln!(out, "... more entries omitted ...");
    }
    Ok(out.into_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_request;

    fn request(path: &str) -> crate::protocol::Request {
        parse_request(format!(r#"{{"id":1,"tool":"list","args":{{"path":"{path}"}}}}"#).as_bytes())
            .unwrap()
    }

    #[tokio::test]
    async fn a_directory_shows_kinds_sizes_and_a_trailing_slash() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("file.txt"), "12345").unwrap();
        std::os::unix::fs::symlink("file.txt", dir.join("link")).unwrap();

        let text = list(&request(dir.to_str().unwrap())).await.unwrap();
        assert!(text.starts_with(&format!("{}:\n", dir.display())), "{text}");
        let sub = text
            .lines()
            .find(|l| l.ends_with("sub/"))
            .unwrap_or_default();
        assert!(sub.starts_with('d'), "{sub}");
        let file = text
            .lines()
            .find(|l| l.ends_with("file.txt"))
            .unwrap_or_default();
        assert!(file.starts_with("- "), "{file}");
        assert_eq!(file, "-          5 file.txt");
        let link = text
            .lines()
            .find(|l| l.ends_with("link"))
            .unwrap_or_default();
        assert!(link.starts_with('l'), "{link}");
    }

    #[tokio::test]
    async fn a_missing_directory_reports_the_errno() {
        let err = list(&request("/definitely/not/a/directory"))
            .await
            .unwrap_err();
        assert!(err.starts_with("opendir failed: "), "{err}");
    }

    /// `count` plain files in a directory of their own.  The guard is what keeps the
    /// directory alive, so a caller holds it for as long as it uses the path.
    fn dir_with_files(count: usize) -> (std::path::PathBuf, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..count {
            std::fs::write(temp.path().join(format!("f{index}")), b"x").unwrap();
        }
        (temp.path().to_path_buf(), temp)
    }

    fn entry_lines(text: &str) -> usize {
        text.lines().filter(|l| l.starts_with("- ")).count()
    }

    const NOTE: &str = "... more entries omitted ...";

    /// Past the cap the note is a promise that something was left out, and it is true.
    #[tokio::test]
    async fn the_entry_cap_says_so() {
        let (dir, _dir) = dir_with_files(MAX_ENTRIES + 40);
        let text = list(&request(dir.to_str().unwrap())).await.unwrap();
        assert_eq!(entry_lines(&text), MAX_ENTRIES);
        assert!(text.contains(NOTE), "tail: {}", tail(&text));
    }

    /// The boundary the cap is decided at.  300 are printed and a 301st entry is still
    /// unread, which is the entry the note refers to.
    #[tokio::test]
    async fn one_entry_past_the_cap_is_worth_the_note() {
        let (dir, _dir) = dir_with_files(MAX_ENTRIES + 1);
        let text = list(&request(dir.to_str().unwrap())).await.unwrap();
        assert_eq!(entry_lines(&text), MAX_ENTRIES);
        assert!(
            text.ends_with(&format!("{NOTE}\n")),
            "tail: {}",
            tail(&text)
        );
    }

    /// A directory that happens to end at the cap leaves nothing out, so the note would
    /// be a lie.  The C agent stops with `readdir`'s next result in hand and prints
    /// nothing here, and the answers are meant to be the same text.
    #[tokio::test]
    async fn a_directory_that_ends_exactly_at_the_cap_is_not_called_incomplete() {
        let (dir, _dir) = dir_with_files(MAX_ENTRIES);
        let text = list(&request(dir.to_str().unwrap())).await.unwrap();
        assert_eq!(entry_lines(&text), MAX_ENTRIES);
        assert!(!text.contains("more entries"), "tail: {}", tail(&text));
        // The last line is an entry, not a note.  Order is the filesystem's, so the
        // name is not pinned down, but nothing follows the 300th line.
        assert!(text.lines().last().is_some_and(|l| l.starts_with("- ")));
    }

    fn tail(text: &str) -> String {
        text.chars()
            .rev()
            .take(120)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }

    #[tokio::test]
    async fn an_absent_path_lists_the_working_directory() {
        let request = parse_request(br#"{"id":1,"tool":"list","args":{}}"#).unwrap();
        let text = list(&request).await.unwrap();
        assert!(text.starts_with(".:\n"), "{text}");
    }
}
