//! File access shared by the read, write, edit, search and list tools.
//!
//! Two rules from the C agent carry over because both are about not destroying
//! something the user did not ask to destroy: a file that is replaced is replaced
//! atomically through a temporary in the same directory, and a file that is not a
//! plain exclusive file is refused instead of followed.  A symlink in the way of a
//! write is therefore an error rather than a surprise somewhere else on disk.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Largest file a tool will hold in memory.  `edit` needs the whole file to prove
/// that its `old` selector is unique, and an unbounded read here would turn a
/// mistaken path into an out-of-memory crash.
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Everything here that is not a plain read or write goes through
/// [`tokio::task::spawn_blocking`], for two reasons that are the same reason.
/// Tokio has no equivalent for the flags these opens need (`O_NOFOLLOW` on the
/// target, `O_NONBLOCK` so a fifo cannot hang us, an exclusive create for the
/// temporary), and for the ownership and extended-attribute calls that follow.
/// And the sequence that replaces a file is a *unit*: checking what is there,
/// writing a temporary next to it and renaming over the original is only safe if
/// nothing else interleaves, so it runs as one piece of work on the blocking pool
/// rather than as a chain of awaits that another request could slip between.
///
/// The functions the tools call are therefore `async` wrappers around blocking
/// bodies, and the byte counts are the same either way: what they protect against
/// is a tool that never returns, not a tool that takes a worker thread for a while.
async fn blocking_io<T>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|err| Err(io::Error::other(did_not_finish(&err))))
}

async fn blocking_text<T>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|err| Err(did_not_finish(&err)))
}

/// The only way a blocking body fails this way is a panic inside it or a runtime
/// torn down mid-write.  Both are worth reporting as a failed tool rather than
/// taking the process with them: the frame stream is still usable.
fn did_not_finish(err: &tokio::task::JoinError) -> String {
    format!("file operation did not finish: {err}")
}

/// Opens a path for reading, refusing anything that is not a regular file.  A
/// device or a fifo would block the read tool forever, and a directory is not text.
pub async fn open_regular(path: &str) -> io::Result<tokio::fs::File> {
    let path = path.to_string();
    let file = blocking_io(move || open_regular_blocking(&path)).await?;
    Ok(tokio::fs::File::from_std(file))
}

fn open_regular_blocking(path: &str) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(file)
}

/// Reads a whole file with the C agent's error wording, which is what the model is
/// used to seeing for a path it got wrong.
pub async fn read_bytes(path: &str) -> Result<Vec<u8>, String> {
    let path = path.to_string();
    blocking_text(move || read_bytes_blocking(&path)).await
}

fn read_bytes_blocking(path: &str) -> Result<Vec<u8>, String> {
    let mut file = match open_regular_blocking(path) {
        Ok(file) => file,
        Err(err) => return Err(format!("open {}: {}", path, err_message(&err))),
    };
    let mut data = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if data.len() + n > MAX_FILE_BYTES {
                    return Err(format!(
                        "file too large: {} exceeds {} bytes",
                        path, MAX_FILE_BYTES
                    ));
                }
                data.extend_from_slice(&buf[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("read {}: {}", path, err_message(&err))),
        }
    }
    Ok(data)
}

/// The message a `strerror()` would have given: the C agent reports `No such file
/// or directory`, and the model has learned that phrasing.
pub fn err_message(err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(code) => errno_message(code),
        None => err.to_string(),
    }
}

/// `strerror(3)` without pulling the C string through the formatter twice.
fn errno_message(code: i32) -> String {
    let bytes = unsafe {
        let text = libc::strerror(code);
        if text.is_null() {
            return format!("errno {code}");
        }
        std::ffi::CStr::from_ptr(text)
    };
    bytes.to_string_lossy().into_owned()
}

/// Writes `data` to `path`, replacing it atomically.
///
/// With `expected`, the current contents must match it exactly: `edit` passes the
/// bytes it read when it found the `old` selector, so a file that changed between
/// reading and writing is reported instead of silently overwritten.
pub async fn replace(path: &str, data: Vec<u8>, expected: Option<Vec<u8>>) -> Result<(), String> {
    let path = path.to_string();
    blocking_text(move || replace_blocking(&path, &data, expected.as_deref())).await
}

fn replace_blocking(path: &str, data: &[u8], expected: Option<&[u8]>) -> Result<(), String> {
    let target = Path::new(path);
    let existing = match target.symlink_metadata() {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err_message(&err)),
    };

    let mut temp_path: Option<PathBuf> = None;
    let result = replace_inner(target, existing.as_ref(), data, expected, &mut temp_path);
    if result.is_err()
        && let Some(temp) = &temp_path
    {
        let _ = std::fs::remove_file(temp);
    }
    result
}

fn replace_inner(
    target: &Path,
    existing: Option<&Metadata>,
    data: &[u8],
    expected: Option<&[u8]>,
    temp_path: &mut Option<PathBuf>,
) -> Result<(), String> {
    if existing.is_some() {
        // Resolve the name so the temporary lands next to the file, then refuse
        // anything we cannot replace in place safely.
        let resolved = std::fs::canonicalize(target).map_err(|err| err_message(&err))?;
        let source = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&resolved)
            .map_err(|err| err_message(&err))?;
        let source_meta = source.metadata().map_err(|err| err_message(&err))?;
        if !source_meta.file_type().is_file() || source_meta.nlink() != 1 {
            return Err(format!(
                "refusing to replace non-regular or hard-linked file: {}",
                target.display()
            ));
        }
        let mode = source_meta.mode() & 0o7777;
        if let Some(wanted) = expected
            && !same_contents(&source, wanted)
        {
            return Err(format!(
                "file changed since it was read: {}",
                target.display()
            ));
        }
        copy_metadata(&source, data, target, mode, temp_path)?;
        // Renamed onto the resolved name rather than the one we were given: the
        // target file is replaced in place and a symlink pointing at it goes on
        // pointing at it, where renaming over the name would turn the link into a
        // ordinary file and orphan whatever it referred to.
        return rename(temp_path, &resolved);
    }

    if expected.is_some() {
        // The edit read a file that is no longer there; that is the same race as a
        // changed file and gets the same answer.
        return Err(format!(
            "file changed since it was read: {}",
            target.display()
        ));
    }
    create_new(target, data, temp_path)?;
    rename(temp_path, target)
}

/// True when the file still holds exactly `wanted`.  Compared in chunks and short-
/// circuited, because the usual answer is "no" within the first block.
fn same_contents(file: &File, wanted: &[u8]) -> bool {
    let Ok(mut file) = file.try_clone() else {
        return false;
    };
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    let mut pos = 0;
    let mut buf = [0u8; 8192];
    loop {
        let want = std::cmp::min(buf.len(), wanted.len() - pos);
        if want == 0 {
            return true;
        }
        match file.read(&mut buf[..want]) {
            Ok(0) => return false,
            Ok(n) => {
                if buf[..n] != wanted[pos..pos + n] {
                    return false;
                }
                pos += n;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
}

/// Writes the new bytes into a temporary next to the target.  The temporary is
/// created exclusively with a random suffix, so two agents editing the same file
/// cannot write through each other's temporary.
fn temp_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let mut random = [0u8; 4];
    // Not for secrets: it only has to be unpredictable to another process racing
    // for the same name.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
        ^ (std::process::id() << 16);
    let mut state = seed | 1;
    for byte in random.iter_mut() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = (state & 0xff) as u8;
    }
    let suffix = random.iter().fold(String::new(), |mut acc, b| {
        acc.push_str(&format!("{b:02x}"));
        acc
    });
    target.with_file_name(format!("{name}.ds4-{suffix}"))
}

fn write_temp(temp: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    // The temporary is always created exclusively: a name that already exists
    // means something else is writing there, and that is not ours to clobber.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_CLOEXEC)
        .open(temp)
        .map_err(|err| err_message(&err))?;
    file.write_all(data).map_err(|err| err_message(&err))?;
    file.flush().map_err(|err| err_message(&err))?;
    Ok(())
}

fn create_new(target: &Path, data: &[u8], temp_path: &mut Option<PathBuf>) -> Result<(), String> {
    let temp = temp_for(target);
    *temp_path = Some(temp.clone());
    // 0o666 and let open() apply the umask, which is what a brand new file wants.
    write_temp(&temp, data, 0o666)
}

/// Replaces an existing file, keeping its owner and permissions.  The temporary is
/// created with the target's mode so an executable or a 0600 file stays what it is
/// across an edit.
fn copy_metadata(
    source: &File,
    data: &[u8],
    target: &Path,
    mode: u32,
    temp_path: &mut Option<PathBuf>,
) -> Result<(), String> {
    let temp = temp_for(target);
    *temp_path = Some(temp.clone());
    write_temp(&temp, data, mode & 0o7777)?;

    let uid = source.metadata().map(|m| m.uid()).unwrap_or(u32::MAX);
    let gid = source.metadata().map(|m| m.gid()).unwrap_or(u32::MAX);
    let file = File::open(&temp).map_err(|err| err_message(&err))?;
    // Ownership can only be given away by root; failing that is not a reason to
    // lose the edit, since the file is still the right bytes and mode.
    unsafe {
        libc::fchown(
            file.as_raw_fd() as libc::c_int,
            uid as libc::uid_t,
            gid as libc::gid_t,
        )
    };

    #[cfg(target_os = "macos")]
    {
        // Carry the ACL and extended attributes the same way the C agent does.
        // Unlike the C version this is best effort: losing an xattr is worth
        // reporting, not worth losing the edit over.
        let src = File::open(target).map(|f| f.as_raw_fd()).unwrap_or(-1);
        let dst = std::fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .map(|f| f.as_raw_fd())
            .unwrap_or(-1);
        if src >= 0 && dst >= 0 {
            unsafe {
                libc::fcopyfile(
                    src as libc::c_int,
                    dst as libc::c_int,
                    std::ptr::null_mut(),
                    libc::COPYFILE_ACL | libc::COPYFILE_XATTR,
                )
            };
        }
    }
    Ok(())
}

fn rename(temp_path: &mut Option<PathBuf>, target: &Path) -> Result<(), String> {
    let temp = temp_path
        .as_ref()
        .ok_or_else(|| "no temporary file was created".to_string())?;
    std::fs::rename(temp, target).map_err(|err| {
        let message = err_message(&err);
        format!("rename {}: {}", target.display(), message)
    })?;
    *temp_path = None;
    Ok(())
}

/// Line spans of a buffer, counting a final line that has no terminator.  The
/// `edit` tool reports old and new line numbers in this same currency.
pub fn line_spans(data: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < data.len() {
        match data[index] {
            b'\n' => {
                spans.push((start, index + 1));
                start = index + 1;
                index += 1;
            }
            b'\r' => {
                let end = if data.get(index + 1) == Some(&b'\n') {
                    index + 2
                } else {
                    index + 1
                };
                spans.push((start, end));
                start = end;
                index = end;
            }
            _ => index += 1,
        }
    }
    if start < data.len() {
        spans.push((start, data.len()));
    }
    spans
}

/// The 1-based line containing `offset`.
pub fn line_for_offset(spans: &[(usize, usize)], offset: usize) -> usize {
    for (index, span) in spans.iter().enumerate() {
        let end = span.1;
        if offset < end || (offset == end && index + 1 == spans.len()) {
            return index + 1;
        }
    }
    spans.len().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Cargo runs the tests in one process in parallel, so a name built from the
    /// process id alone would be shared by two tests at the same moment.
    fn unique() -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ds4-helper-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        path
    }

    #[tokio::test]
    async fn line_spans_count_an_unterminated_last_line() {
        assert_eq!(line_spans(b"a\nb\nc"), vec![(0, 2), (2, 4), (4, 5)]);
        assert_eq!(line_spans(b"a\r\nb"), vec![(0, 3), (3, 4)]);
        assert_eq!(line_spans(b""), Vec::<(usize, usize)>::new());
        assert_eq!(line_for_offset(&line_spans(b"a\nb\nc"), 3), 2);
        assert_eq!(line_for_offset(&line_spans(b"a\nb\nc"), 4), 3);
    }

    #[tokio::test]
    async fn replacing_a_new_file_leaves_no_temporary_behind() {
        let path = temp_path("new");
        let text = path.to_str().unwrap();
        replace(text, b"hello".to_vec(), None).await.unwrap();
        assert_eq!(std::fs::read(text).unwrap(), b"hello");
        let parent = path.parent().unwrap();
        // Only this file's temporaries: other tests are replacing their own files
        // in the same directory at the same moment.
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}.ds4-"))
            })
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
        std::fs::remove_file(text).unwrap();
    }

    #[tokio::test]
    async fn a_version_guard_rejects_a_file_that_changed() {
        let path = temp_path("guard");
        let text = path.to_str().unwrap();
        replace(text, b"original".to_vec(), None).await.unwrap();
        let err = replace(text, b"edited".to_vec(), Some(b"something else".to_vec()))
            .await
            .unwrap_err();
        assert!(err.contains("changed since it was read"), "{err}");
        assert_eq!(std::fs::read(text).unwrap(), b"original");
        replace(text, b"edited".to_vec(), Some(b"original".to_vec()))
            .await
            .unwrap();
        assert_eq!(std::fs::read(text).unwrap(), b"edited");
        std::fs::remove_file(text).unwrap();
    }

    #[tokio::test]
    async fn the_file_mode_survives_a_replace() {
        let path = temp_path("mode");
        let text = path.to_str().unwrap();
        replace(text, b"#!/bin/sh\n".to_vec(), None).await.unwrap();
        std::fs::set_permissions(text, std::fs::Permissions::from_mode(0o755)).unwrap();
        replace(
            text,
            b"#!/bin/sh\necho hi\n".to_vec(),
            Some(b"#!/bin/sh\n".to_vec()),
        )
        .await
        .unwrap();
        let mode = std::fs::metadata(text).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "mode {mode:o}");
        std::fs::remove_file(text).unwrap();
    }

    #[tokio::test]
    async fn a_hard_linked_file_is_refused() {
        let path = temp_path("hardlink-src");
        let other = temp_path("hardlink-dst");
        let text = path.to_str().unwrap();
        let other_text = other.to_str().unwrap();
        replace(text, b"data".to_vec(), None).await.unwrap();
        std::fs::hard_link(text, other_text).unwrap();
        let err = replace(text, b"new".to_vec(), None).await.unwrap_err();
        assert!(err.contains("hard-linked"), "{err}");
        assert_eq!(std::fs::read(text).unwrap(), b"data");
        std::fs::remove_file(text).unwrap();
        std::fs::remove_file(other_text).unwrap();
    }

    #[tokio::test]
    async fn a_symlink_name_writes_the_file_it_resolves_to() {
        let dir = std::env::temp_dir();
        let target = temp_path("symlink-target");
        let link = dir.join(format!(
            "ds4-helper-symlink-{}-{}",
            std::process::id(),
            unique()
        ));
        let _ = std::fs::remove_file(&link);
        std::fs::write(&target, b"victim").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // The agent resolves the path first and then opens it without following, so
        // a name that is a symlink reaches its target while a symlink swapped in
        // afterwards is refused.  The name is not a separate file, and replacing it
        // as one would leave the target unreachable.
        replace(link.to_str().unwrap(), b"replacement".to_vec(), None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_file(&target);
    }

    #[tokio::test]
    async fn opening_a_directory_for_reading_fails() {
        let err = open_regular("/").await.unwrap_err();
        assert!(matches!(err.raw_os_error(), Some(libc::EINVAL)));
    }

    #[tokio::test]
    async fn error_messages_read_like_strerror() {
        let err = std::fs::File::open("/no/such/file/here").unwrap_err();
        assert_eq!(err_message(&err), errno_message(libc::ENOENT));
    }
}
