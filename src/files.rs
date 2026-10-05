//! File access shared by the read, write, edit, search and list tools.
//!
//! Two rules from the C agent carry over because both are about not destroying
//! something the user did not ask to destroy: a file that is replaced is replaced
//! atomically through a temporary in the same directory, and a file that is not a
//! plain exclusive file is refused instead of followed.  A symlink in the way of a
//! write is therefore an error rather than a surprise somewhere else on disk.

use std::fs::File;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::NamedTempFile;

/// Largest file a tool will hold in memory.  `edit` needs the whole file to prove
/// that its `old` selector is unique, and an unbounded read here would turn a
/// mistaken path into an out-of-memory crash.
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Everything here that is not a plain read or write goes through
/// [`tokio::task::spawn_blocking`], for two reasons that are the same reason.
/// Tokio has no equivalent for the flags these opens need (`O_NOFOLLOW` on the
/// target, `O_NONBLOCK` so a fifo cannot hang us), nor for the exclusive create of
/// the temporary or the ownership and extended-attribute calls that follow.
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

    // Nothing to clean up here: the temporary a replacement is written into deletes
    // itself when it is dropped, which is every path that did not get as far as the
    // rename.
    replace_inner(target, existing.as_ref(), data, expected)
}

fn replace_inner(
    target: &Path,
    existing: Option<&Metadata>,
    data: &[u8],
    expected: Option<&[u8]>,
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
            return Err(changed(target));
        }
        let temp = copy_metadata(&source, data, &resolved, mode)?;
        // The last look before committing, taken exactly where the C agent takes it.
        // Everything between reading the file and this moment is the window a
        // concurrent writer can slip into, and matching bytes are not enough to shut
        // it: a writer that replaced the file with identical content moved a
        // different inode into the name, and the edit would then have replaced a
        // file that nobody had read.
        if !still_the_same(target, &resolved, &source_meta) {
            return Err(changed(target));
        }
        // Renamed onto the resolved name rather than the one we were given: the
        // target file is replaced in place and a symlink pointing at it goes on
        // pointing at it, where renaming over the name would turn the link into a
        // ordinary file and orphan whatever it referred to.
        return rename(temp, &resolved);
    }

    if expected.is_some() {
        // The edit read a file that is no longer there; that is the same race as a
        // changed file and gets the same answer.
        return Err(changed(target));
    }
    link_new(create_new(target, data)?, target)
}

/// What the agent says when the file is not the one the edit was worked out against.
/// The wording is the agent's own, because it is what models have been tuned against,
/// and it tells the model the one useful next step: read again.
fn changed(target: &Path) -> String {
    format!(
        "file changed while editing; read it again: {}",
        target.display()
    )
}

/// The agent's wording for a replacement the filesystem itself refused.
fn failed(target: &Path, err: &io::Error) -> String {
    format!("replace {}: {}", target.display(), err_message(err))
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

/// The last look before a replacement is committed: the name must still resolve to
/// the file that was opened, and that file must still be the version that was read.
fn still_the_same(target: &Path, resolved: &Path, before: &Metadata) -> bool {
    std::fs::canonicalize(target).is_ok_and(|now| now == *resolved)
        && same_file_version(before, resolved)
}

/// True when `path` still describes `before`: the same inode on the same device, with
/// the same size, link count and timestamps to the nanosecond.  A byte compare cannot
/// tell any of this — a rewrite that produced the same bytes leaves the name pointing
/// at a different inode, and `ctime` moves even when `mtime` is put back by hand.
fn same_file_version(before: &Metadata, path: &Path) -> bool {
    let Ok(after) = std::fs::metadata(path) else {
        return false;
    };
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.size() == after.size()
        && before.nlink() == after.nlink()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

/// The temporary a replacement is written into, next to the target and named the way
/// the C agent names its own — `<name>.ds4-XXXXXX` — so that a leftover from either
/// program reads the same to whoever finds it.  `tempfile` supplies the random tail,
/// creates the file exclusively, and deletes it if it is dropped before the rename;
/// the mode goes to `open`, so a brand new file still has the umask applied to it
/// while a replaced one is born with the target's own bits.
///
/// The builder is what makes that mode part of the create; the shorter
/// `NamedTempFile::with_prefix_in` cannot say it, and setting the permissions
/// afterwards would go through `chmod`, which ignores the umask and would leave a
/// brand new file world-writable.
fn temp_for(target: &Path, mode: u32) -> Result<NamedTempFile, String> {
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    tempfile::Builder::new()
        .prefix(&format!("{name}.ds4-"))
        .permissions(std::fs::Permissions::from_mode(mode))
        .tempfile_in(dir)
        .map_err(|err| err_message(&err))
}

/// A temporary holding the new bytes, with the mode the finished file should have.
fn write_temp(target: &Path, data: &[u8], mode: u32) -> Result<NamedTempFile, String> {
    let mut temp = temp_for(target, mode)?;
    temp.write_all(data).map_err(|err| err_message(&err))?;
    Ok(temp)
}

fn create_new(target: &Path, data: &[u8]) -> Result<NamedTempFile, String> {
    // 0o666 and let open() apply the umask, which is what a brand new file wants.
    write_temp(target, data, 0o666)
}

/// Replaces an existing file, keeping its owner and permissions.  The temporary is
/// created with the target's mode so an executable or a 0600 file stays what it is
/// across an edit.
fn copy_metadata(
    source: &File,
    data: &[u8],
    target: &Path,
    mode: u32,
) -> Result<NamedTempFile, String> {
    let temp = write_temp(target, data, mode & 0o7777)?;

    let owner = source.metadata().ok();
    let uid = owner.as_ref().map(MetadataExt::uid).unwrap_or(u32::MAX);
    let gid = owner.as_ref().map(MetadataExt::gid).unwrap_or(u32::MAX);
    // Ownership can only be given away by root; failing that is not a reason to
    // lose the edit, since the file is still the right bytes and mode.
    unsafe { libc::fchown(temp.as_raw_fd(), uid as libc::uid_t, gid as libc::gid_t) };

    #[cfg(target_os = "macos")]
    {
        // Carry the ACL and extended attributes the same way the C agent does.
        // Unlike the C version this is best effort: losing an xattr is worth
        // reporting, not worth losing the edit over.  Both descriptors are already
        // open in the direction this needs: the target for reading, the temporary
        // for writing.
        unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                temp.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_ACL | libc::COPYFILE_XATTR,
            )
        };
    }
    #[cfg(target_os = "linux")]
    {
        // Linux has no single call that copies "the rest" of a file's metadata, but
        // it keeps both the extended attributes and the POSIX ACL in one place: the
        // ACL is `system.posix_acl_access`, an attribute like any other.  So the
        // attributes are carried over, and the ACL travels with them.  The same
        // bargain as the macOS branch: an attribute the kernel will not let this
        // process write — `security.selinux` and `trusted.*` are the usual ones — is
        // skipped rather than costing the edit.
        copy_xattrs(source.as_raw_fd(), temp.as_raw_fd());
    }
    Ok(temp)
}

/// Carries every extended attribute from one open file to another, forgetting the
/// ones this process has no business setting.  Best effort by design: the bytes and
/// the mode are the contract, and an ACL that could not be reproduced is worth
/// reporting to whoever looks, not worth losing the write over.
#[cfg(target_os = "linux")]
fn copy_xattrs(from: std::os::fd::RawFd, to: std::os::fd::RawFd) {
    for name in xattr_names(from) {
        if let Some(value) = xattr_value(from, &name) {
            // flags 0: create or replace, which is all a copy needs.
            unsafe { libc::fsetxattr(to, name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
        }
    }
}

/// The attribute names on an open file.  `flistxattr` answers with one buffer of
/// NUL-separated names, and with the size it would need when given none of it.
#[cfg(target_os = "linux")]
fn xattr_names(fd: std::os::fd::RawFd) -> Vec<std::ffi::CString> {
    let needed = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u8; needed as usize];
    let read = unsafe { libc::flistxattr(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if read <= 0 {
        return Vec::new();
    }
    buffer.truncate(read as usize);
    buffer
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| std::ffi::CString::new(name.to_vec()).ok())
        .collect()
}

/// One attribute's value, or nothing when it cannot be read or vanished in between.
#[cfg(target_os = "linux")]
fn xattr_value(fd: std::os::fd::RawFd, name: &std::ffi::CString) -> Option<Vec<u8>> {
    let needed = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
    if needed < 0 {
        return None;
    }
    let mut value = vec![0u8; needed as usize];
    let read =
        unsafe { libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
    if read < 0 {
        return None;
    }
    value.truncate(read as usize);
    Some(value)
}

/// Moves the temporary onto the target, which is what makes the replacement atomic,
/// and takes it out of the destructor's reach on the way.
fn rename(temp: NamedTempFile, target: &Path) -> Result<(), String> {
    temp.persist(target)
        .map_err(|err| failed(target, &err.error))?;
    Ok(())
}

/// The last step for a file that was not there: `link`, not `rename`.  A link will not
/// overwrite a name that appeared while the temporary was being written, so the writer
/// that got there first keeps its file and this one comes back with `File exists`.  A
/// rename would have quietly won a race nobody was watching, which is the one answer
/// the model cannot recover from.
///
/// Filesystems that cannot link at all — a FAT card, one FUSE mount out of several —
/// still have to be able to create a file, so the link is retried as a rename when the
/// refusal is about the link rather than about the file.  On such a filesystem the race
/// protection was never available; everywhere it is, it is used.
fn link_new(temp: NamedTempFile, target: &Path) -> Result<(), String> {
    if let Err(err) = std::fs::hard_link(temp.path(), target) {
        if !link_was_unsupported(&err) {
            return Err(failed(target, &err));
        }
        return rename(temp, target);
    }
    // Closing the handle is also the unlink of the temporary's own name: the target
    // keeps the inode, and nothing is left behind either way.
    temp.close().map_err(|err| failed(target, &err))
}

/// The refusals that mean "this filesystem has no link operation", as opposed to
/// "something is wrong with this file".  `ENOTSUP` and `EOPNOTSUPP` are one value on
/// Linux and two on macOS, so both are named and one of them is always a duplicate.
const LINK_UNSUPPORTED: &[i32] = &[libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS];

fn link_was_unsupported(err: &io::Error) -> bool {
    // `EEXIST` is the one code that must stay an error: it is the race this link
    // exists to lose, and falling back on it would hand the race straight back.
    // `EXDEV` is absent for a different reason — the temporary is made next to its
    // target, so a rename could not cross the device either.
    err.raw_os_error()
        .is_some_and(|code| LINK_UNSUPPORTED.contains(&code))
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
    use std::path::PathBuf;

    /// A file path inside a fresh directory that deletes itself with the test.  The
    /// caller keeps the directory by holding the second half of the pair, which is
    /// what the underscore in front of its name means.
    fn temp_path(tag: &str) -> (PathBuf, tempfile::TempDir) {
        let dir = tempfile::TempDir::with_prefix(format!("ds4-helper-{tag}-")).unwrap();
        (dir.path().join("file"), dir)
    }

    #[tokio::test]
    async fn line_spans_count_an_unterminated_last_line() {
        assert_eq!(line_spans(b"a\nb\nc"), vec![(0, 2), (2, 4), (4, 5)]);
        assert_eq!(line_spans(b"a\r\nb"), vec![(0, 3), (3, 4)]);
        assert_eq!(line_spans(b""), Vec::<(usize, usize)>::new());
        assert_eq!(line_for_offset(&line_spans(b"a\nb\nc"), 3), 2);
        assert_eq!(line_for_offset(&line_spans(b"a\nb\nc"), 4), 3);
    }

    #[test]
    fn the_temporary_bears_the_targets_name_and_the_agents_suffix() {
        let (path, _dir) = temp_path("tempfor");
        let temp = temp_for(&path, 0o600).unwrap();
        let name = temp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // `file.ds4-XXXXXX`, in the target's own directory: the shape the C agent's
        // mkstemp leaves behind, and the reason the last step is a rename rather
        // than a copy that could cross onto another filesystem.
        let tail = name.strip_prefix("file.ds4-").unwrap_or_default();
        assert_eq!(tail.len(), 6, "temporary named {name}");
        assert_eq!(temp.path().parent().unwrap(), path.parent().unwrap());
        assert_eq!(
            temp.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        // And it is the handle that owns it: nothing is left to rot next to a file
        // whose replacement failed halfway.
        let left = temp.path().to_path_buf();
        drop(temp);
        assert!(!left.exists(), "the temporary outlived its handle");
    }

    /// What an `edit` did not touch comes across with the file.  On Linux this is
    /// also how a POSIX ACL travels, since the ACL is one of these attributes.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn extended_attributes_survive_a_replace() {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;

        let (path, _dir) = temp_path("xattr");
        std::fs::write(&path, b"one\n").unwrap();
        let name = CString::new("user.ds4-test").unwrap();
        let target = CString::new(path.to_str().unwrap()).unwrap();
        let value = b"carried over";
        let set = unsafe {
            libc::setxattr(
                target.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if set != 0 {
            // Not every filesystem accepts user attributes — tmpfs only learned it
            // recently — and one that will not hold one is not a verdict on this code.
            return;
        }

        replace(path.to_str().unwrap(), b"two\n".to_vec(), None)
            .await
            .unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(
            xattr_value(file.as_raw_fd(), &name).as_deref(),
            Some(&value[..]),
            "the attribute did not survive the replacement"
        );
    }

    #[tokio::test]
    async fn replacing_a_new_file_leaves_no_temporary_behind() {
        let (path, _dir) = temp_path("new");
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
        let (path, _dir) = temp_path("guard");
        let text = path.to_str().unwrap();
        replace(text, b"original".to_vec(), None).await.unwrap();
        let err = replace(text, b"edited".to_vec(), Some(b"something else".to_vec()))
            .await
            .unwrap_err();
        assert!(
            err.contains("file changed while editing; read it again"),
            "{err}"
        );
        assert_eq!(std::fs::read(text).unwrap(), b"original");
        replace(text, b"edited".to_vec(), Some(b"original".to_vec()))
            .await
            .unwrap();
        assert_eq!(std::fs::read(text).unwrap(), b"edited");
        std::fs::remove_file(text).unwrap();
    }

    /// The wording is part of the interface: a model tuned against it knows the next
    /// step is to read the file again, not to try the same edit a second time.
    #[tokio::test]
    async fn a_lost_race_is_answered_in_the_agents_words() {
        let (path, _dir) = temp_path("wording");
        let text = path.to_str().unwrap();
        std::fs::write(text, b"original\n").unwrap();
        let expected = format!("file changed while editing; read it again: {text}");

        let err = replace(text, b"edited".to_vec(), Some(b"other".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(err, expected, "a file whose bytes moved");

        // The same answer when the file is gone: an edit that read a file which is no
        // longer there is the same race wearing a different hat.
        std::fs::remove_file(text).unwrap();
        let err = replace(text, b"edited".to_vec(), Some(b"original\n".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(err, expected, "a file that vanished");
    }

    /// The look taken just before the rename, checked without needing to win a race.
    /// A name that moved, an inode that moved, and a file that went away are three
    /// different ways an edit can be about something that is no longer there.
    #[test]
    fn the_guard_notices_a_name_that_no_longer_means_the_same_file() {
        let scratch = tempfile::TempDir::with_prefix("ds4-helper-version-").unwrap();
        let dir = scratch.path();

        let quiet = dir.join("quiet");
        std::fs::write(&quiet, b"bytes\n").unwrap();
        let resolved = std::fs::canonicalize(&quiet).unwrap();
        let before = std::fs::metadata(&quiet).unwrap();
        assert!(
            still_the_same(&quiet, &resolved, &before),
            "nothing happened to it"
        );

        // Same name, same bytes, another inode: the part a byte compare cannot see.
        let moved = dir.join("moved");
        std::fs::write(&moved, b"bytes\n").unwrap();
        std::fs::rename(&moved, &quiet).unwrap();
        assert!(
            !still_the_same(&quiet, &resolved, &before),
            "another inode under the same name"
        );

        // A name that resolves somewhere else is not the file that was read, even to
        // a file with the same bytes in it.
        let elsewhere = dir.join("elsewhere");
        std::fs::write(&elsewhere, b"bytes\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&quiet, &link).unwrap();
        let resolved_link = std::fs::canonicalize(&link).unwrap();
        let before_link = std::fs::metadata(&link).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(
            !still_the_same(&link, &resolved_link, &before_link),
            "a retargeted name"
        );

        std::fs::remove_file(&quiet).unwrap();
        assert!(
            !still_the_same(&quiet, &resolved, &before),
            "a name that resolves nowhere"
        );
    }

    /// A file that appears while the temporary is being written keeps its own
    /// contents: the last step of a new file is a link, and a link loses a race
    /// instead of winning it.
    #[test]
    fn a_file_created_underneath_a_write_is_not_overwritten() {
        let (path, _dir) = temp_path("link");
        let text = path.to_str().unwrap();
        // Not through `replace`: the point is to hold a finished temporary while
        // something else creates the name it was written for.
        let temp = create_new(&path, b"the helper wrote this\n").unwrap();
        std::fs::write(text, b"someone else got here first\n").unwrap();

        let err = link_new(temp, &path).unwrap_err();
        assert!(err.contains("File exists"), "{err}");
        assert_eq!(
            std::fs::read(text).unwrap(),
            b"someone else got here first\n"
        );
        // And the loser takes its temporary out with it.
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(left.len(), 1, "left behind: {left:?}");
    }

    /// Retrying a failed link is for a filesystem that cannot link, never for the race
    /// the link was put there to lose: falling back on `EEXIST` would hand it straight
    /// back.  Which half of this the branch is cannot be tried on a filesystem that
    /// links happily, so the decision itself is what is checked here.
    #[test]
    fn only_a_link_the_filesystem_cannot_do_falls_back_to_the_rename() {
        let refused = |code| io::Error::from_raw_os_error(code);
        assert!(link_was_unsupported(&refused(libc::EPERM)), "no link here");
        assert!(link_was_unsupported(&refused(libc::ENOTSUP)));
        assert!(link_was_unsupported(&refused(libc::EOPNOTSUPP)));
        assert!(link_was_unsupported(&refused(libc::ENOSYS)));

        // The race, and the failures that are about the file or the disk rather than
        // about the operation.
        assert!(!link_was_unsupported(&refused(libc::EEXIST)), "the race");
        assert!(!link_was_unsupported(&refused(libc::ENOENT)));
        assert!(!link_was_unsupported(&refused(libc::ENOSPC)));
        assert!(!link_was_unsupported(&refused(libc::EROFS)));
        assert!(!link_was_unsupported(&refused(libc::EXDEV)));
        // An error that never came from a filesystem cannot report on one.
        assert!(!link_was_unsupported(&io::Error::other("no errno at all")));
    }

    #[tokio::test]
    async fn the_file_mode_survives_a_replace() {
        let (path, _dir) = temp_path("mode");
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
        let (path, _dir) = temp_path("hardlink-src");
        let (other, _other) = temp_path("hardlink-dst");
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
        let dir = tempfile::tempdir().unwrap();
        let (target, _target_dir) = temp_path("symlink-target");
        let link = dir.path().join("link");
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
