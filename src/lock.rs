//! The file locks that mark a place busy.
//!
//! A place is busy exactly while some process holds its busy.lock. The
//! coordinator takes the lock, hands the open file to the place's runner
//! process and lets go of its own copy, so the kernel keeps the lock for as
//! long as that process lives, across coordinator restarts, and drops it if
//! the process dies. On Windows it is a byte-range lock, which Windows drops
//! when the holding process ends; only the agent takes locks there.

use std::fs::File;
use std::io;
use std::path::Path;

/// Takes path's exclusive lock without waiting. It returns the open, locked
/// file, or None when someone else holds it.
pub fn busy(path: &Path) -> io::Result<Option<File>> {
    imp::take(path, false)
}

/// Takes path's exclusive lock, waiting for whoever holds it to let go.
/// Dropping the file lets go.
pub fn wait(path: &Path) -> io::Result<File> {
    imp::take(path, true).map(|f| f.expect("a waiting lock is always taken"))
}

/// Reports whether another process holds path's lock. A missing file is free.
/// It never creates, writes or removes the file, and lets go at once.
pub fn held_by(path: &Path) -> io::Result<bool> {
    imp::held_by(path)
}

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub fn take(path: &Path, wait: bool) -> io::Result<Option<File>> {
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)?;
        let op = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 {
                return Ok(Some(f));
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) if wait => continue,
                Some(libc::EWOULDBLOCK) if !wait => return Ok(None),
                _ => return Err(err),
            }
        }
    }

    pub fn held_by(path: &Path) -> io::Result<bool> {
        let f = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(true);
            }
            return Err(err);
        }
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        Ok(false)
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_PATH_NOT_FOUND};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// The file may be deleted while it is locked, as an agent's place lock
    /// is when the agent lets go; until the holder closes it, Windows may
    /// refuse to open it, which is the same as busy.
    pub fn take(path: &Path, wait: bool) -> io::Result<Option<File>> {
        let f = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => return Ok(None),
            Err(e) if e.raw_os_error() == Some(ERROR_PATH_NOT_FOUND as i32) => {
                return Err(io::Error::new(io::ErrorKind::NotFound, e));
            }
            Err(e) => return Err(e),
        };
        let flags = if wait { LOCKFILE_EXCLUSIVE_LOCK } else { LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY };
        match lock(&f, flags) {
            Ok(()) => Ok(Some(f)),
            Err(e) if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn lock(f: &File, flags: u32) -> io::Result<()> {
        let mut ol: OVERLAPPED = unsafe { std::mem::zeroed() };
        if unsafe { LockFileEx(f.as_raw_handle() as _, flags, 0, 1, 0, &mut ol) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn held_by(path: &Path) -> io::Result<bool> {
        let f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        match lock(&f, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY) {
            Ok(()) => {
                let mut ol: OVERLAPPED = unsafe { std::mem::zeroed() };
                unsafe { UnlockFileEx(f.as_raw_handle() as _, 0, 1, 0, &mut ol) };
                Ok(false)
            }
            Err(e) if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) => Ok(true),
            Err(e) => Err(e),
        }
    }
}
