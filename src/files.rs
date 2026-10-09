// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! File access shared by the read, write, edit, search and list tools.
//!
//! Two rules from the C agent carry over, both about not destroying something the
//! user did not ask to destroy: a replaced file is replaced atomically through a
//! temporary in the same directory, and anything that is not a plain exclusive file is
//! refused rather than followed.

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

/// Largest file a tool will hold in memory.  `edit` needs the whole file to prove that
/// its `old` selector is unique, and an unbounded read turns a wrong path into a crash.
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Everything that is not a plain read or write runs on the blocking pool.  Tokio has
/// no equivalent for the flags these opens need (`O_NOFOLLOW` on the target,
/// `O_NONBLOCK` so a fifo cannot hang us), the exclusive create, or the ownership and
/// extended-attribute calls.  And replacing a file is a *unit* — check what is there,
/// write a temporary next to it, rename over the original — which is only safe if no
/// other request interleaves with it.
async fn blocking<T, E>(f: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, E>
where
    T: Send + 'static,
    E: NeverRan + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|err| Err(E::never_ran(&did_not_finish(&err))))
}

/// What a blocking body reports when it never ran, in whichever error type the body
/// already uses.  A file open answers with an errno and a tool answers with a sentence,
/// and both have to say the same thing about a body that was never finished.
trait NeverRan {
    fn never_ran(reason: &str) -> Self;
}

impl NeverRan for String {
    fn never_ran(reason: &str) -> Self {
        reason.to_string()
    }
}

impl NeverRan for io::Error {
    fn never_ran(reason: &str) -> Self {
        io::Error::other(reason)
    }
}

/// A panic inside a blocking body, or a runtime torn down mid-write, becomes a failed
/// tool.  The frame stream still works, so the process is worth keeping.
fn did_not_finish(err: &tokio::task::JoinError) -> String {
    format!("file operation did not finish: {err}")
}

/// Opens a path for reading, refusing anything that is not a regular file.  A
/// device or a fifo would block the read tool forever, and a directory is not text.
pub async fn open_regular(path: &str) -> io::Result<tokio::fs::File> {
    let path = path.to_string();
    let file = blocking(move || open_regular_blocking(&path)).await?;
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

/// Reads a whole file, with the error wording the model is used to seeing.
pub async fn read_bytes(path: &str) -> Result<Vec<u8>, String> {
    let path = path.to_string();
    blocking(move || read_bytes_blocking(&path)).await
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

/// The wording `strerror()` would have given, which is what the model has learned.
/// A wrapper error such as `tempfile`'s keeps the kind of a failure, loses its errno,
/// and adds the path it tried.  That path is a temporary the model never saw.
pub fn err_message(err: &io::Error) -> String {
    match err.raw_os_error().or_else(|| errno_of_kind(err.kind())) {
        Some(code) => errno_message(code),
        None => err.to_string(),
    }
}

/// The number a kind was made from.  Only the ones a file replacement can meet, so an
/// unlisted kind keeps the message it arrived with.
fn errno_of_kind(kind: io::ErrorKind) -> Option<i32> {
    Some(match kind {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        io::ErrorKind::NotADirectory => libc::ENOTDIR,
        io::ErrorKind::IsADirectory => libc::EISDIR,
        io::ErrorKind::InvalidFilename => libc::ENAMETOOLONG,
        io::ErrorKind::DirectoryNotEmpty => libc::ENOTEMPTY,
        io::ErrorKind::StorageFull => libc::ENOSPC,
        io::ErrorKind::ReadOnlyFilesystem => libc::EROFS,
        _ => return None,
    })
}

/// Read `strerror_r(3)` into a buffer owned by the caller since `strerror(3)` is
/// marked `MT-Unsafe` and this may be called by several `spawn_blocking` workers
/// at once.
fn errno_message(code: i32) -> String {
    // 256 is what POSIX asks `strerror_r` to be able to write.  Shorter asks for an
    // `ERANGE` answer that says less to the model than the words would have.
    let mut buf = [0u8; 256];
    let text = unsafe {
        // The `libc` crate binds the XSI variant everywhere, so this is the one that
        // answers with a status rather than with a pointer that may not be `buf`.
        if libc::strerror_r(code, buf.as_mut_ptr().cast(), buf.len()) != 0 {
            return format!("errno {code}");
        }
        std::ffi::CStr::from_ptr(buf.as_ptr().cast())
    };
    text.to_string_lossy().into_owned()
}

/// Writes `data` to `path`, replacing it atomically.
///
/// With `expected`, the current contents must match it exactly.  `edit` passes the
/// bytes it read when it found the `old` selector, so a file that changed between
/// reading and writing is reported instead of silently overwritten.
pub async fn replace(path: &str, data: Vec<u8>, expected: Option<Vec<u8>>) -> Result<(), String> {
    let path = path.to_string();
    blocking(move || replace_blocking(&path, &data, expected.as_deref())).await
}

fn replace_blocking(path: &str, data: &[u8], expected: Option<&[u8]>) -> Result<(), String> {
    let target = Path::new(path);
    let existing = match target.symlink_metadata() {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(failed(target, &err)),
    };

    // Nothing to clean up.  The temporary deletes itself on every path that did not
    // reach the rename.
    replace_inner(target, existing.as_ref(), data, expected)
}

fn replace_inner(
    target: &Path,
    existing: Option<&Metadata>,
    data: &[u8],
    expected: Option<&[u8]>,
) -> Result<(), String> {
    if existing.is_some() {
        // Resolve the name so the temporary lands next to the file.
        let resolved = std::fs::canonicalize(target).map_err(|err| failed(target, &err))?;
        let source = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&resolved)
            .map_err(|err| failed(target, &err))?;
        let source_meta = source.metadata().map_err(|err| failed(target, &err))?;
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
        let temp =
            copy_metadata(&source, data, &resolved, mode).map_err(|err| failed(target, &err))?;
        // The last look before committing.  Matching bytes are not enough.  A writer
        // that replaced the file with identical content moved a different inode into
        // the name, and this edit would replace a file nobody had read.
        if !still_the_same(target, &resolved, &source_meta) {
            return Err(changed(target));
        }
        // Renamed onto the resolved name so the file is replaced in place and a symlink
        // to it keeps pointing at it, instead of becoming an ordinary file.
        return rename(temp, &resolved).map_err(|err| failed(target, &err));
    }

    if expected.is_some() {
        // The file the edit read is gone.  That is the same race, and the same answer.
        return Err(changed(target));
    }
    let temp = create_new(target, data).map_err(|err| failed(target, &err))?;
    link_new(temp, target).map_err(|err| failed(target, &err))
}

/// What the agent says when the file is not the one the edit was worked out against.
/// It is the agent's own wording, and the useful next step it gives is to read again.
fn changed(target: &Path) -> String {
    format!(
        "file changed while editing; read it again: {}",
        target.display()
    )
}

/// The agent's wording for a replacement the filesystem itself refused.  Every such
/// refusal is said here, with the path that was asked for.  A model can do nothing with
/// the name of a temporary it never mentioned.
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
        // Once `wanted` runs out the file has to run out at the same byte.  A file that
        // merely starts with these bytes is a file that changed.
        let room = if want == 0 { 1 } else { want };
        match file.read(&mut buf[..room]) {
            Ok(0) => return want == 0,
            Ok(_) if want == 0 => return false,
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

/// True when `path` still describes `before`, meaning the same inode on the same
/// device with the same size, link count and timestamps.  A byte compare cannot see
/// this — a rewrite with identical bytes leaves a different inode behind the name.
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

/// The temporary a replacement is written into, next to the target and named
/// `<name>.ds4-XXXXXX` so a leftover from either program reads the same.  `tempfile`
/// supplies the tail, creates the file exclusively, and deletes it if it is dropped
/// before the rename.  The mode goes to `open` so a brand new file is umasked like any
/// other new file.  A replacement sets its bits again afterwards, because the umask has
/// no business changing the bits a file already had.
fn temp_for(target: &Path, mode: u32) -> io::Result<NamedTempFile> {
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
}

/// A temporary holding the new bytes, with the mode the finished file should have.
fn write_temp(target: &Path, data: &[u8], mode: u32) -> io::Result<NamedTempFile> {
    let mut temp = temp_for(target, mode)?;
    temp.write_all(data)?;
    Ok(temp)
}

fn create_new(target: &Path, data: &[u8]) -> io::Result<NamedTempFile> {
    // 0o666 and let open() apply the umask, which is what a brand new file wants.
    seal(write_temp(target, data, 0o666)?)
}

/// Replaces an existing file keeping its owner and every bit of its mode, so an
/// executable stays one and a setuid program stays setuid.
fn copy_metadata(
    source: &File,
    data: &[u8],
    target: &Path,
    mode: u32,
) -> io::Result<NamedTempFile> {
    let temp = write_temp(target, data, mode & 0o7777)?;

    let owner = source.metadata().ok();
    let uid = owner.as_ref().map(MetadataExt::uid).unwrap_or(u32::MAX);
    let gid = owner.as_ref().map(MetadataExt::gid).unwrap_or(u32::MAX);
    // The temporary belongs to whoever is editing, so the target's owner and group have
    // to be handed over.  A caller may set a group only to one they belong to, and an
    // ownership that cannot be claimed is worth refusing the edit over.  A metadata read
    // that failed leaves `-1`, which `chown` takes as no change.
    if unsafe { libc::fchown(temp.as_raw_fd(), uid as libc::uid_t, gid as libc::gid_t) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // Neither of the calls that made this file can carry the target's bits.  `open`
    // masks the mode it is handed with the umask, and a write drops the setuid and
    // setgid bits.  This is the one that can, and it comes after the write and the
    // ownership change because both of them clear the bits again.
    if unsafe { libc::fchmod(temp.as_raw_fd(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }

    #[cfg(target_os = "macos")]
    {
        // Carry the ACL and extended attributes, best effort.  Losing one is worth
        // reporting, but not worth losing the edit over.
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
        // Linux keeps the POSIX ACL as `system.posix_acl_access`, one attribute among
        // the rest, so carrying the attributes carries it.  An attribute the kernel
        // will not let this process write (`security.selinux`, `trusted.*`) is skipped
        // rather than costing the edit.
        copy_xattrs(source.as_raw_fd(), temp.as_raw_fd());
    }
    seal(temp)
}

/// Carries every extended attribute from one open file to another, skipping the ones
/// this process has no business setting.  The bytes and the mode are the contract.
#[cfg(target_os = "linux")]
fn copy_xattrs(from: std::os::fd::RawFd, to: std::os::fd::RawFd) {
    for name in xattr_names(from) {
        if let Some(value) = xattr_value(from, &name) {
            // flags 0 creates or replaces, which is all a copy needs.
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

/// Puts everything a replacement is made of onto the device before the name is
/// published, which is what the agent's `fsync` does here on both paths.  The answer
/// that says the bytes went in is a promise about the file, and a helper that never
/// syncs is promising the page cache instead.
fn seal(temp: NamedTempFile) -> io::Result<NamedTempFile> {
    temp.as_file().sync_all()?;
    Ok(temp)
}

/// Moves the temporary onto the target, which is what makes the replacement atomic,
/// and takes it out of the destructor's reach on the way.
fn rename(temp: NamedTempFile, target: &Path) -> io::Result<()> {
    temp.persist(target).map_err(|err| err.error)?;
    Ok(())
}

/// The last step for a file that was not there is a `link`, not a `rename`.  A link
/// will not overwrite a name that appeared while the temporary was being written, so
/// the first writer keeps its file and this one comes back with `File exists`.  A
/// rename would have quietly won a race nobody was watching.
///
/// Where linking is unsupported — a FAT card, one FUSE mount out of several — the link
/// is retried as a rename, because the race protection was never available there.
fn link_new(temp: NamedTempFile, target: &Path) -> io::Result<()> {
    if let Err(err) = std::fs::hard_link(temp.path(), target) {
        if !link_was_unsupported(&err) {
            return Err(err);
        }
        return rename(temp, target);
    }
    // Closing also unlinks the temporary's own name, and the target keeps the inode.
    temp.close()
}

/// The refusals that mean "this filesystem has no link operation", as opposed to
/// "something is wrong with this file".  `ENOTSUP` and `EOPNOTSUPP` are one value on
/// Linux and two on macOS, so both are named and one of them is always a duplicate.
const LINK_UNSUPPORTED: &[i32] = &[libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS];

fn link_was_unsupported(err: &io::Error) -> bool {
    // `EEXIST` must stay an error.  Falling back on it would hand back the race this
    // link exists to lose.  `EXDEV` is absent because the temporary is made next to
    // its target, so a rename could not cross the device either.
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

    /// A file path in a self-deleting directory.  The caller keeps the directory.
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

    /// The words have to be the C library's own, because that is what the agent prints
    /// for the same failure.  `Display` for an os error says the same thing and then
    /// adds the number, so it is the oracle for the words without the suffix.
    #[test]
    fn an_errno_gets_the_c_librarys_own_words() {
        for code in [
            libc::ENOENT,
            libc::EACCES,
            libc::ENOTDIR,
            libc::ELOOP,
            libc::ENOSPC,
        ] {
            let said = errno_message(code);
            let suffix = format!(" (os error {code})");
            let expected = std::io::Error::from_raw_os_error(code)
                .to_string()
                .strip_suffix(suffix.as_str())
                .unwrap_or_else(|| panic!("std said something else: {said}"))
                .to_string();
            assert_eq!(said, expected);
            assert!(!said.contains("os error"), "{said}");
        }
        // The one almost every tool ends up saying, written out so a change to it is a
        // decision rather than a surprise.
        assert_eq!(errno_message(libc::ENOENT), "No such file or directory");
    }

    /// Every refusal is worded the way the agent's own tool words it, with the path the
    /// model wrote in it.  A temporary's name is nothing a model can act on, and a
    /// resolved path is a different file from the one it named.
    #[tokio::test]
    async fn a_failed_replacement_names_the_path_that_was_asked_for() {
        let (path, dir) = temp_path("wording");
        std::fs::write(&path, b"one\n").unwrap();

        // This is the failure that used to answer with a temporary's name.
        let missing = dir.path().join("gone/file");
        let missing = missing.to_str().unwrap().to_string();
        assert_eq!(
            replace(&missing, b"x".to_vec(), None).await.unwrap_err(),
            format!("replace {missing}: No such file or directory")
        );

        // A name with a plain file where a directory has to be.
        let through = path.join("deeper");
        let through = through.to_str().unwrap().to_string();
        assert_eq!(
            replace(&through, b"x".to_vec(), None).await.unwrap_err(),
            format!("replace {through}: Not a directory")
        );

        // A directory is not a file to replace.
        let a_dir = dir.path().to_str().unwrap().to_string();
        assert_eq!(
            replace(&a_dir, b"x".to_vec(), None).await.unwrap_err(),
            format!("replace {a_dir}: Is a directory")
        );

        // A name that points at nothing.
        let gone = dir.path().join("gone");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&gone, &link).unwrap();
        let link = link.to_str().unwrap().to_string();
        assert_eq!(
            replace(&link, b"x".to_vec(), None).await.unwrap_err(),
            format!("replace {link}: No such file or directory")
        );
    }

    /// A number the C library has no words for still has to say something the model can
    /// quote back.
    #[test]
    fn an_errno_with_no_words_gives_its_number() {
        assert_eq!(errno_message(65_000), "errno 65000");
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
        // `file.ds4-XXXXXX` in the target's own directory, which is why the last step
        // is a rename rather than a copy that could cross onto another filesystem.
        let tail = name.strip_prefix("file.ds4-").unwrap_or_default();
        assert_eq!(tail.len(), 6, "temporary named {name}");
        assert_eq!(temp.path().parent().unwrap(), path.parent().unwrap());
        assert_eq!(
            temp.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        // And the handle owns it, so nothing is left next to a file whose replacement
        // failed halfway.
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
            // Not every filesystem accepts user attributes, and one that will not hold
            // one is not a verdict on this code.
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
        // Only this file's temporaries.  Other tests replace their own files here at
        // the same moment.
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

    /// The guard is about the whole file.  A file that grew keeps the bytes the edit was
    /// built from at its front, so a compare that stops when those bytes run out reports
    /// an unchanged file and the append is overwritten without a word.
    #[tokio::test]
    async fn a_version_guard_rejects_a_file_that_grew_by_a_prefix() {
        let (path, _dir) = temp_path("prefixguard");
        let text = path.to_str().unwrap();
        std::fs::write(text, b"original plus an append\n").unwrap();
        let err = replace(text, b"edited".to_vec(), Some(b"original".to_vec()))
            .await
            .unwrap_err();
        assert!(
            err.contains("file changed while editing; read it again"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(text).unwrap(),
            b"original plus an append\n",
            "the append had to survive"
        );

        // An expectation with no bytes is exact too, so a file with bytes in it is not
        // an empty file.
        let err = replace(text, b"edited".to_vec(), Some(Vec::new()))
            .await
            .unwrap_err();
        assert!(err.contains("read it again"), "{err}");

        // Bytes that match all the way to the end still pass, and so do no bytes on
        // either side.
        std::fs::write(text, b"original").unwrap();
        replace(text, b"edited".to_vec(), Some(b"original".to_vec()))
            .await
            .unwrap();
        assert_eq!(std::fs::read(text).unwrap(), b"edited");

        std::fs::write(text, b"").unwrap();
        replace(text, b"edited".to_vec(), Some(Vec::new()))
            .await
            .unwrap();
        assert_eq!(std::fs::read(text).unwrap(), b"edited");
        std::fs::remove_file(text).unwrap();
    }

    /// The wording is part of the interface.  It tells the model to read again rather
    /// than repeat the edit.
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

        // A file that is gone is the same race, and gets the same answer.
        std::fs::remove_file(text).unwrap();
        let err = replace(text, b"edited".to_vec(), Some(b"original\n".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(err, expected, "a file that vanished");
    }

    /// The look taken just before the rename, checked without winning a race: a name
    /// that moved, an inode that moved, and a file that went away.
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

        // Same name, same bytes, another inode.  That is what a byte compare cannot see.
        let moved = dir.join("moved");
        std::fs::write(&moved, b"bytes\n").unwrap();
        std::fs::rename(&moved, &quiet).unwrap();
        assert!(
            !still_the_same(&quiet, &resolved, &before),
            "another inode under the same name"
        );

        // A name resolving somewhere else is not the file that was read, however equal
        // its bytes.
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
    /// contents.  The last step of a new file is a link, and a link loses a race
    /// instead of winning it.
    #[test]
    fn a_file_created_underneath_a_write_is_not_overwritten() {
        let (path, _dir) = temp_path("link");
        let text = path.to_str().unwrap();
        // Not through `replace`.  The temporary is held while another writer creates
        // the name it was written for.
        let temp = create_new(&path, b"the helper wrote this\n").unwrap();
        std::fs::write(text, b"someone else got here first\n").unwrap();

        let err = link_new(temp, &path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST), "{err}");
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
    /// the link was put there to lose.  Falling back on `EEXIST` would hand it straight
    /// back.  A filesystem that links happily cannot show either half, so the decision
    /// itself is what is checked here.
    #[test]
    fn only_a_link_the_filesystem_cannot_do_falls_back_to_the_rename() {
        let refused = |code| io::Error::from_raw_os_error(code);
        assert!(link_was_unsupported(&refused(libc::EPERM)), "no link here");
        assert!(link_was_unsupported(&refused(libc::ENOTSUP)));
        assert!(link_was_unsupported(&refused(libc::EOPNOTSUPP)));
        assert!(link_was_unsupported(&refused(libc::ENOSYS)));

        // The race, and failures about the file or disk rather than the operation.
        assert!(!link_was_unsupported(&refused(libc::EEXIST)), "the race");
        assert!(!link_was_unsupported(&refused(libc::ENOENT)));
        assert!(!link_was_unsupported(&refused(libc::ENOSPC)));
        assert!(!link_was_unsupported(&refused(libc::EROFS)));
        assert!(!link_was_unsupported(&refused(libc::EXDEV)));
        // An error that never came from a filesystem cannot report on one.
        assert!(!link_was_unsupported(&io::Error::other("no errno at all")));
    }

    /// Every bit of the target's mode survives, not just the ones the umask leaves
    /// alone.  `open` masks the mode it is handed, and a write clears the setuid and
    /// setgid bits, so a replacement that only uses those two calls loses what the model
    /// never asked to lose.
    #[tokio::test]
    async fn the_file_mode_survives_a_replace() {
        for mode in [0o755u32, 0o777, 0o640, 0o4755, 0o2755, 0o1755] {
            let (path, _dir) = temp_path("mode");
            let text = path.to_str().unwrap();
            replace(text, b"#!/bin/sh\n".to_vec(), None).await.unwrap();
            std::fs::set_permissions(text, std::fs::Permissions::from_mode(mode)).unwrap();
            replace(
                text,
                b"#!/bin/sh\necho hi\n".to_vec(),
                Some(b"#!/bin/sh\n".to_vec()),
            )
            .await
            .unwrap();
            let got = std::fs::metadata(text).unwrap().permissions().mode() & 0o7777;
            assert_eq!(got, mode, "a replacement of a file at {mode:o}");
            std::fs::remove_file(text).unwrap();
        }
    }

    /// The group is part of the ownership a replacement keeps.  A temporary is made with
    /// the group of whoever is editing, so only the hand-over can put the target's group
    /// back, and a replacement that drops it takes the file out of a group nobody asked
    /// to leave.  Setting a foreign group is root's, so root is the uid that can check
    /// the keeping in a test.
    #[tokio::test]
    async fn the_group_survives_a_replace() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("cannot set a foreign group as a non-root caller, nothing to check");
            return;
        }
        let (path, _dir) = temp_path("group");
        let text = path.to_str().unwrap();
        replace(text, b"one\n".to_vec(), None).await.unwrap();
        // A group the temporary cannot arrive with.  `-1` leaves the owner alone.
        let group = 1u32;
        let name = std::ffi::CString::new(text).unwrap();
        assert_eq!(
            unsafe { libc::chown(name.as_ptr(), u32::MAX as libc::uid_t, group as libc::gid_t) },
            0
        );

        replace(text, b"two\n".to_vec(), Some(b"one\n".to_vec()))
            .await
            .unwrap();
        let meta = std::fs::metadata(text).unwrap();
        assert_eq!(
            MetadataExt::gid(&meta),
            group,
            "the group was not handed over"
        );
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
        // The path is resolved first and then opened without following.  A name that is
        // a symlink reaches its target, and a symlink swapped in afterwards is refused.
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

    /// A plain open on a fifo with no writer waits for one that may never come, and
    /// `O_NONBLOCK` is what stops that.  The open itself succeeds on a fifo, so the file
    /// type is what refuses it.  The deadline makes a regression here fail rather than
    /// hang.
    #[tokio::test]
    async fn a_fifo_with_no_writer_is_refused_rather_than_waited_for() {
        let (path, _dir) = temp_path("fifo");
        let c_path = std::ffi::CString::new(path.to_str().unwrap().to_owned()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let opened = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            open_regular(path.to_str().unwrap()),
        )
        .await;
        let err = match opened {
            Err(_) => panic!("opening a fifo with no writer waited for a writer"),
            Ok(Ok(_)) => panic!("a fifo was taken for a regular file"),
            Ok(Err(err)) => err,
        };
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{err}");
    }

    /// `/dev/null` opens without waiting and reads as nothing, so only the file type
    /// stops it.
    #[tokio::test]
    async fn opening_a_device_for_reading_fails() {
        let err = open_regular("/dev/null").await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{err}");
    }

    /// The limit is on the whole file, so a file past it is refused before any of it is
    /// kept, and one that fits exactly is read.  A sparse file counts its bytes without
    /// writing them, which is what makes this cheap to test.
    #[tokio::test]
    async fn a_file_at_the_size_limit_is_read_and_one_beyond_is_not() {
        let (path, _dir) = temp_path("size");
        let spread = |len: u64| {
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(len).unwrap();
        };

        spread(MAX_FILE_BYTES as u64);
        read_bytes(path.to_str().unwrap())
            .await
            .expect("a file the size of the limit should be read");

        spread(MAX_FILE_BYTES as u64 + 1);
        let err = read_bytes(path.to_str().unwrap()).await.unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert!(err.contains(&MAX_FILE_BYTES.to_string()), "{err}");
    }

    #[tokio::test]
    async fn error_messages_read_like_strerror() {
        let err = std::fs::File::open("/no/such/file/here").unwrap_err();
        assert_eq!(err_message(&err), errno_message(libc::ENOENT));
    }
}
