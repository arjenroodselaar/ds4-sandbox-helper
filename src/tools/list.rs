//! `list`: one directory, in the order the filesystem gives it.
//!
//! Not sorted: the C agent prints `readdir()` order, and a model that has learned to
//! recognise "the build products are the three newest entries" is relying on the
//! order the directory actually has, not on an alphabetical reordering this tool
//! would invent.

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
            // The cap counts entries we printed, so a skipped entry still means
            // there was more to see.
            more = true;
            break;
        }
        let Ok(Some(entry)) = entries.next_entry().await else {
            break;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // lstat, not stat: a symlink is reported as a symlink, which is the only way
        // a model sees that a name in the tree is a pointer somewhere else.
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
        // The width matches `%10lld`: sizes line up, which is what makes a scanned
        // listing readable.
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

    /// Cargo runs the tests in one process in parallel, so a name built from the
    /// process id alone would be shared by two tests at the same moment.
    fn unique() -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn request(path: &str) -> crate::protocol::Request {
        parse_request(format!(r#"{{"id":1,"tool":"list","args":{{"path":"{path}"}}}}"#).as_bytes())
            .unwrap()
    }

    #[tokio::test]
    async fn a_directory_shows_kinds_sizes_and_a_trailing_slash() {
        let dir = std::env::temp_dir().join(format!(
            "ds4-helper-list-{}-{}",
            std::process::id(),
            unique()
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
        // The size column is `%10lld` wide, which is what lines a scanned listing up.
        assert_eq!(file, "-          5 file.txt");
        let link = text
            .lines()
            .find(|l| l.ends_with("link"))
            .unwrap_or_default();
        assert!(link.starts_with('l'), "{link}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_missing_directory_reports_the_errno() {
        let err = list(&request("/definitely/not/a/directory"))
            .await
            .unwrap_err();
        assert!(err.starts_with("opendir failed: "), "{err}");
    }

    #[tokio::test]
    async fn the_entry_cap_says_so() {
        let dir = std::env::temp_dir().join(format!(
            "ds4-helper-cap-{}-{}",
            std::process::id(),
            unique()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..340 {
            std::fs::write(dir.join(format!("f{index}")), b"x").unwrap();
        }
        let text = list(&request(dir.to_str().unwrap())).await.unwrap();
        assert_eq!(
            text.lines().filter(|l| l.starts_with("- ")).count(),
            MAX_ENTRIES
        );
        assert!(
            text.contains("... more entries omitted ..."),
            "tail: {}",
            tail(&text)
        );
        std::fs::remove_dir_all(&dir).unwrap();
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
