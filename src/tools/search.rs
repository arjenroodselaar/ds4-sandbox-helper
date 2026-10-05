//! `search`: walk a tree and report matching lines.
//!
//! Two rules do the heavy lifting.  Results stream into a byte budget and the walk
//! stops when it is full, because a repository-wide search for a common word has no
//! natural end and the model's context does.  And anything the walk could not read is
//! counted and reported in a tail note rather than ignored: a search that quietly
//! skipped a third of the tree and said "3 matches" would be worse than one that said
//! nothing, because the model would treat an incomplete answer as a complete one.

use std::fmt::Write;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;

/// A recursive `async fn` cannot return `impl Future` (it would need itself by
/// value), so the walk hands back a boxed future instead.  The allocation is per
/// directory, which is nothing next to reading the files inside it.
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

use regex::Regex;
use regex::RegexBuilder;

use crate::budget::Budget;
use crate::files;
use crate::protocol::Request;

const MAX_DEPTH: usize = 24;
/// One more than the largest context the tool accepts, so the lines around a match
/// are already in hand when the match is found.
const RING: usize = 6;
const MAX_LINE_BYTES: usize = crate::budget::MAX_TOOL_BYTES;

enum Matcher {
    Literal { query: String, case_sensitive: bool },
    Regex(Regex),
}

impl Matcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Matcher::Literal {
                query,
                case_sensitive: true,
            } => line.contains(query),
            Matcher::Literal {
                query,
                case_sensitive: false,
            } => line.to_lowercase().contains(&query.to_lowercase()),
            Matcher::Regex(re) => re.is_match(line),
        }
    }
}

struct Ctx<'a> {
    matcher: Matcher,
    glob: Option<&'a str>,
    context: usize,
    max_results: usize,
    results: usize,
    skipped: usize,
    first_skip: String,
    out: Budget,
}

impl Ctx<'_> {
    fn stop(&mut self, path: &str, why: &str) {
        self.skipped += 1;
        if self.first_skip.is_empty() {
            self.first_skip = format!("{path}: {why}");
        }
    }

    fn full(&self) -> bool {
        self.results >= self.max_results || self.out.truncated()
    }
}

pub async fn search(request: &Request) -> Result<String, String> {
    let Some(query) = request.arg("query").filter(|q| !q.is_empty()) else {
        return Err("search requires query".into());
    };
    let path = request.arg("path").filter(|p| !p.is_empty()).unwrap_or(".");
    let mode = request.arg("mode").unwrap_or("literal");
    if mode != "literal" && mode != "regex" {
        return Err("search mode must be literal or regex (POSIX extended)".into());
    }
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return Err(format!(
            "search path is missing, unreadable, or not a regular file/directory: {path}"
        ));
    };
    if !meta.is_dir() && !meta.is_file() {
        return Err(format!(
            "search path is missing, unreadable, or not a regular file/directory: {path}"
        ));
    }

    let case_sensitive = request.bool_or("case_sensitive", true);
    let matcher = if mode == "regex" {
        // The tool documents POSIX extended regex, and this crate's syntax is a
        // superset of ERE for everything a model normally writes.  Where they differ
        // (backreferences, which ERE has and this does not) the pattern simply fails
        // to compile and the error text says so.
        match RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(re) => Matcher::Regex(re),
            Err(err) => return Err(format!("invalid regex: {err}")),
        }
    } else {
        Matcher::Literal {
            query: query.to_string(),
            case_sensitive,
        }
    };

    let mut ctx = Ctx {
        matcher,
        glob: request.arg("glob").filter(|g| !g.is_empty()),
        context: request.arg_or("context", 0, 0, 5) as usize,
        max_results: request.arg_or("max_results", 50, 1, 500) as usize,
        results: 0,
        skipped: 0,
        first_skip: String::new(),
        out: Budget::for_output(),
    };

    walk(&mut ctx, path, 0).await;

    let body = ctx.out.text().to_string();
    let mut out = String::new();
    if body.is_empty() {
        out.push_str("No matches in searched text\n");
    } else {
        let _ = write!(
            out,
            "{} match{} shown\n\n{body}",
            ctx.results,
            if ctx.results == 1 { "" } else { "es" }
        );
    }
    if ctx.skipped > 0 || ctx.results >= ctx.max_results {
        let limit = if ctx.results >= ctx.max_results {
            "; match limit reached"
        } else {
            ""
        };
        let _ = write!(
            out,
            "\nSearch incomplete: {} skipped paths{limit}. {}\n",
            ctx.skipped, ctx.first_skip
        );
    }
    Ok(out)
}

/// Recurses through a file or directory.  The root is followed (`stat`), nested
/// symlinks are not (`lstat`): a tree that contains a link to `/` should still be one
/// search, not an exit.
fn walk<'a>(ctx: &'a mut Ctx<'_>, path: &'a str, depth: usize) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        if ctx.full() {
            return;
        }
        if depth > MAX_DEPTH {
            ctx.stop(path, "directory depth limit reached");
            return;
        }
        // The root is followed (a link named on the command line is what the model
        // meant); anything below it is lstat'd, so a link inside the tree is reported
        // rather than walked.
        let meta = if depth == 0 {
            tokio::fs::metadata(path).await
        } else {
            tokio::fs::symlink_metadata(path).await
        };
        let Ok(meta) = meta else {
            ctx.stop(
                path,
                &files::err_message(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            );
            return;
        };
        if meta.is_file() {
            search_file(ctx, path).await;
            return;
        }
        if !meta.is_dir() {
            ctx.stop(
                path,
                "not a regular file or directory (nested symlinks are not followed)",
            );
            return;
        }
        let mut entries = match tokio::fs::read_dir(path).await {
            Ok(entries) => entries,
            Err(err) => {
                ctx.stop(path, &files::err_message(&err));
                return;
            }
        };
        loop {
            if ctx.full() {
                return;
            }
            let Ok(Some(entry)) = entries.next_entry().await else {
                break;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "." || name == ".." {
                continue;
            }
            // Version control internals are object files and packfiles: searching them
            // spends the whole result budget on content the model did not write.
            if name == ".git" {
                continue;
            }
            let child: PathBuf = entry.path();
            walk(ctx, &child.to_string_lossy(), depth + 1).await;
        }
    })
}

async fn search_file(ctx: &mut Ctx<'_>, path: &str) {
    if ctx.full() {
        return;
    }
    if let Some(glob) = ctx.glob {
        let base = Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        // Either the name or the whole path matching is enough, which is what lets
        // `"glob": "*.c"` and `"glob": "src/*.c"` both mean what they say.
        if !glob_match(glob, &base) && !glob_match(glob, path) {
            return;
        }
    }

    let file = match files::open_regular(path).await {
        Ok(file) => file,
        Err(err) => {
            ctx.stop(path, &files::err_message(&err));
            return;
        }
    };
    let mut reader = tokio::io::BufReader::with_capacity(64 * 1024, file);

    // Only the current line and the context ring are resident: a file can be a
    // gigabyte and a search over it still has to cost a fixed amount.
    let mut ring: Vec<Option<String>> = vec![None; RING];
    let mut printed_file = false;
    let mut line = 0usize;
    let mut last_emitted = 0usize;
    let mut after = 0usize;

    loop {
        // Trailing context has to be read even after the result cap is reached, or
        // the last match's context would be cut off mid-sentence.
        if (ctx.results >= ctx.max_results && after == 0) || ctx.out.truncated() {
            break;
        }
        let text = match read_line(&mut reader).await {
            ReadLine::Line(text) => text,
            ReadLine::Eof => break,
            ReadLine::Binary => {
                ctx.stop(path, "binary data or line exceeds 128 KiB");
                break;
            }
            ReadLine::Io(message) => {
                ctx.stop(path, &message);
                break;
            }
        };

        line += 1;
        ring[line % RING] = Some(text);
        let matched = ctx.results < ctx.max_results
            && ctx
                .matcher
                .matches(ring[line % RING].as_deref().unwrap_or(""));
        if matched && !printed_file {
            let _ = writeln!(ctx.out, "{path}");
            printed_file = true;
        }
        if matched {
            let from = line.saturating_sub(ctx.context).max(last_emitted + 1);
            for number in from..=line {
                emit_line(ctx, &ring, number);
            }
            last_emitted = line;
            after = ctx.context;
            ctx.results += 1;
        } else if after > 0 {
            emit_line(ctx, &ring, line);
            last_emitted = line;
            after -= 1;
        }
    }

    if printed_file {
        let _ = writeln!(ctx.out);
    }
}

/// `  N line`: indented so a result block is visually inside the file it names.
fn emit_line(ctx: &mut Ctx, ring: &[Option<String>], number: usize) {
    let text = ring[number % RING].clone().unwrap_or_default();
    let _ = writeln!(ctx.out, "  {number} {text}");
}

enum ReadLine {
    Line(String),
    Eof,
    /// A NUL, or a line so long it would fill the whole answer by itself.  Both mean
    /// "this is not text the model can read", and the file is reported as skipped.
    Binary,
    Io(String),
}

async fn read_line(reader: &mut tokio::io::BufReader<tokio::fs::File>) -> ReadLine {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        // One byte at a time out of the buffer, not out of the file: the line has to
        // stop at the first NUL or the 128 KiB mark, and either can fall between two
        // reads.
        let byte = match reader.fill_buf().await {
            Ok(buf) => match buf.first() {
                Some(byte) => *byte,
                None => {
                    return if bytes.is_empty() {
                        ReadLine::Eof
                    } else {
                        ReadLine::Line(String::from_utf8_lossy(&bytes).into_owned())
                    };
                }
            },
            Err(err) => return ReadLine::Io(files::err_message(&err)),
        };
        reader.consume(1);
        match byte {
            0 => return ReadLine::Binary,
            b'\n' => {
                return ReadLine::Line(String::from_utf8_lossy(&bytes).into_owned());
            }
            b'\r' => {
                // A CRLF is one line break: the newline after a carriage return
                // belongs to the line just read, not to the next one.
                if let Ok(buf) = reader.fill_buf().await
                    && buf.first() == Some(&b'\n')
                {
                    reader.consume(1);
                }
                return ReadLine::Line(String::from_utf8_lossy(&bytes).into_owned());
            }
            byte => {
                if bytes.len() >= MAX_LINE_BYTES {
                    return ReadLine::Binary;
                }
                bytes.push(byte);
            }
        }
    }
}

/// fnmatch(3) with flags 0: `*` crosses `/`, `?` is one character, and `[...]` is a
/// set that may start with `!` or `^` to negate.  Written here rather than taken from
/// a crate because the tool's contract is exactly this much pattern language.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut p = 0;
    let mut t = 0;
    let mut star: Option<usize> = None;
    let mut star_t = 0;
    while t < text.len() {
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    star = Some(p);
                    star_t = t;
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => match set_match(pattern, p, text[t]) {
                    Some(next) if next > p => {
                        p = next;
                        t += 1;
                        continue;
                    }
                    _ => {}
                },
                byte if byte == text[t] => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        }
        // Mismatch: give the last `*` one more character to cover.
        if let Some(at) = star {
            star_t += 1;
            t = star_t;
            p = at + 1;
            continue;
        }
        return false;
    }
    // Trailing stars can match the empty string.
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Matches one character against the set starting at `pattern[start] == '['`.
/// Returns the index just past `]`, or `None` when the character is not in the set.
fn set_match(pattern: &[u8], start: usize, ch: u8) -> Option<usize> {
    let mut index = start + 1;
    let negate = matches!(pattern.get(index), Some(&b'!') | Some(&b'^'));
    if negate {
        index += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let Some(&current) = pattern.get(index) else {
            return None; // unterminated: fnmatch treats the bracket literally
        };
        index += 1;
        // A `]` first in the set is the character itself, not the end of the set.
        if current == b']' && !first {
            return if hit != negate { Some(index) } else { None };
        }
        first = false;
        let range = if pattern.get(index) == Some(&b'-')
            && !matches!(pattern.get(index + 1), None | Some(&b']'))
        {
            let (lo, hi) = (current, pattern[index + 1]);
            index += 2;
            Some((lo, hi))
        } else {
            None
        };
        match range {
            Some((lo, hi)) if ch >= lo && ch <= hi => hit = true,
            None if ch == current => hit = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_request;

    /// A small source tree, in a directory that deletes itself with the test.  The
    /// caller keeps the directory by holding the second half of the pair.
    fn tree() -> (PathBuf, tempfile::TempDir) {
        let temp = tempfile::TempDir::with_prefix("ds4-helper-search-").unwrap();
        let root = temp.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "alpha\nneedle here\nomega\ngamma\n",
        )
        .unwrap();
        std::fs::write(root.join("src/other.rs"), "needle too\n").unwrap();
        std::fs::write(root.join("notes.txt"), "nothing\n").unwrap();
        std::fs::write(root.join(".git/config"), "needle in git\n").unwrap();
        (root, temp)
    }

    fn request(args: &str) -> crate::protocol::Request {
        parse_request(format!(r#"{{"id":1,"tool":"search","args":{{{args}}}}}"#).as_bytes())
            .unwrap()
    }

    #[tokio::test]
    async fn matches_are_grouped_under_their_file_with_a_count_header() {
        let (root, _dir) = tree();
        let text = search(&request(&format!(
            r#""query":"needle","path":"{}""#,
            root.display()
        )))
        .await
        .unwrap();
        assert!(text.starts_with("2 matches shown\n\n"), "{text}");
        let main = root.join("src/main.rs").to_string_lossy().into_owned();
        let other = root.join("src/other.rs").to_string_lossy().into_owned();
        assert!(
            text.contains(&format!("{main}\n  2 needle here\n")),
            "{text}"
        );
        assert!(
            text.contains(&format!("{other}\n  1 needle too\n")),
            "{text}"
        );
        assert!(!text.contains(".git"), "{text}");
    }

    #[tokio::test]
    async fn context_lines_come_around_the_match() {
        let (root, _dir) = tree();
        let main = root.join("src/main.rs");
        let text = search(&request(&format!(
            r#""query":"needle","path":"{}","context":"1""#,
            main.display()
        )))
        .await
        .unwrap();
        assert!(text.contains("  1 alpha\n"), "{text}");
        assert!(text.contains("  2 needle here\n"), "{text}");
        assert!(text.contains("  3 omega\n"), "{text}");
    }

    #[tokio::test]
    async fn regex_mode_case_insensitivity_and_bad_patterns() {
        let (root, _dir) = tree();
        let text = search(&request(&format!(
            r#""query":"N.*e","path":"{}","mode":"regex","case_sensitive":"false","glob":"*.rs""#,
            root.display()
        )))
        .await
        .unwrap();
        assert!(text.contains("needle here"), "{text}");
        assert!(!text.contains("nothing"), "{text}");

        let err = search(&request(
            r#""query":"([unclosed","mode":"regex","path":"/tmp""#,
        ))
        .await
        .unwrap_err();
        assert!(err.starts_with("invalid regex: "), "{err}");
        let err = search(&request(r#""query":"x","mode":"grep","path":"/tmp""#))
            .await
            .unwrap_err();
        assert!(err.contains("mode must be literal or regex"), "{err}");
    }

    #[tokio::test]
    async fn the_result_cap_is_reported_as_incomplete_coverage() {
        let (root, _dir) = tree();
        let text = search(&request(&format!(
            r#""query":"e","path":"{}","max_results":"1""#,
            root.display()
        )))
        .await
        .unwrap();
        assert!(text.starts_with("1 match shown\n\n"), "{text}");
        assert!(text.contains("Search incomplete:"), "{text}");
        assert!(text.contains("match limit reached"), "{text}");
    }

    #[tokio::test]
    async fn nothing_found_says_so_rather_than_printing_a_header() {
        let (root, _dir) = tree();
        let text = search(&request(&format!(
            r#""query":"zzzzz","path":"{}""#,
            root.display()
        )))
        .await
        .unwrap();
        assert_eq!(text, "No matches in searched text\n");
    }

    #[tokio::test]
    async fn a_bad_root_path_is_an_error_not_an_empty_result() {
        let err = search(&request(r#""query":"x","path":"/no/such/tree""#))
            .await
            .unwrap_err();
        assert!(err.contains("search path is missing"), "{err}");
        let err = search(&request(r#""path":"/tmp""#)).await.unwrap_err();
        assert_eq!(err, "search requires query");
    }

    #[tokio::test]
    async fn glob_matching_covers_the_forms_the_tool_documents() {
        assert!(glob_match("*.c", "main.c"));
        assert!(glob_match("*.c", "src/main.c"), "* crosses /");
        assert!(glob_match("src/*.c", "src/main.c"));
        assert!(glob_match("f?o", "foo"));
        assert!(!glob_match("f?o", "fooo"));
        assert!(glob_match("[ab]*", "beta"));
        assert!(!glob_match("[!ab]*", "beta"));
        assert!(glob_match("[!ab]*", "delta"));
        assert!(!glob_match("*.c", "main.rs"));
    }

    #[tokio::test]
    async fn a_binary_file_is_counted_as_a_skip_not_a_match_source() {
        let (root, _dir) = tree();
        std::fs::write(root.join("src/blob.bin"), b"needle\x00needle").unwrap();
        let text = search(&request(&format!(
            r#""query":"needle","path":"{}""#,
            root.display()
        )))
        .await
        .unwrap();
        assert!(text.contains("Search incomplete"), "{text}");
        assert!(text.contains("binary data or line exceeds"), "{text}");
    }
}
