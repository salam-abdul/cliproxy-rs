//! Advisory locks the operating system releases when their holder exits, so a crash,
//! a kill or a container restart (where the server is always PID 1) never leaves a
//! lock or an in-progress file that looks owned.

use std::fs::File;
use std::io;
use std::path::Path;

/// Takes an exclusive lock on `file` without waiting. `false`: another open handle
/// holds it, in this process or another.
pub(crate) fn try_lock(file: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is open for the call; flock only takes or refuses a lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(error)
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx};
        // SAFETY: the handle is open for the call, and a zeroed OVERLAPPED asks for the
        // range from offset 0 on a synchronous handle.
        let locked = unsafe {
            let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = std::mem::zeroed();
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if locked != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

/// `<dir>/.lock`, held while history files are deleted or rewritten: by
/// `history delete`, by size retention and by the writer applying a delete.
pub(crate) struct DirLock {
    _held: File,
}

impl DirLock {
    /// `None` while another holder has it.
    pub(crate) fn try_acquire(dir: &Path) -> io::Result<Option<Self>> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(dir.join(".lock"))?;
        Ok(try_lock(&file)?.then_some(Self { _held: file }))
    }
}

/// Whether nobody holds the in-progress file at `path`: its writer is gone. The lock
/// this takes ends with the call.
pub(crate) fn is_abandoned(path: &Path) -> io::Result<bool> {
    let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    try_lock(&file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_logging::history::file::tests::TempDir;

    #[test]
    fn a_held_dir_lock_refuses_a_second_holder_until_dropped() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        let first = DirLock::try_acquire(&dir.0).unwrap();
        assert!(first.is_some());
        assert!(DirLock::try_acquire(&dir.0).unwrap().is_none());
        drop(first);
        assert!(DirLock::try_acquire(&dir.0).unwrap().is_some());
    }

    #[test]
    fn a_lock_file_left_by_a_dead_holder_is_free() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        std::fs::write(dir.0.join(".lock"), b"1").unwrap();
        assert!(DirLock::try_acquire(&dir.0).unwrap().is_some());
    }

    #[test]
    fn only_an_unlocked_file_is_abandoned() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("x.partial");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        assert!(try_lock(&held).unwrap());
        assert!(!is_abandoned(&path).unwrap());
        drop(held);
        assert!(is_abandoned(&path).unwrap());
    }
}
