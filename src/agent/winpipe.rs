//! Named pipes as the agent opens them on Windows: the Hyper-V serial port
//! and runner-up's service.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSID};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
use windows_sys::Win32::System::IO::CancelIoEx;

use super::Conn;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// The NUL-terminated UTF-16 string at p.
pub fn from_wide_ptr(p: *const u16) -> String {
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// One attempt to open the pipe at path, as a client that lets the server
/// only identify it, not act as it.
pub fn open(path: &str, access: u32) -> std::io::Result<HANDLE> {
    let name = wide(path);
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(h)
}

/// The SID that owns h, as a string such as S-1-5-18.
pub fn owner(h: HANDLE) -> Result<String, String> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    let rc = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32).to_string());
    }
    let s = sid_string(owner);
    unsafe { LocalFree(sd as _) };
    s
}

pub fn sid_string(sid: PSID) -> Result<String, String> {
    let mut p = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut p) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let s = from_wide_ptr(p);
    unsafe { LocalFree(p as _) };
    Ok(s)
}

/// Closes h, for a pipe that failed its owner check.
pub fn close(h: HANDLE) {
    unsafe { CloseHandle(h) };
}

struct Pipe(HANDLE);
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

/// h as a Conn, which owns it from here on.
pub fn conn(h: HANDLE) -> std::io::Result<Conn> {
    use std::os::windows::io::FromRawHandle;
    let file = unsafe { std::fs::File::from_raw_handle(h as _) };
    let writer = file.try_clone()?;
    let state = Arc::new(PipeState { h: Pipe(h), closed: AtomicBool::new(false), reading: AtomicBool::new(false) });
    let closer = state.clone();
    Ok(Conn { reader: Box::new(PipeReader { file, state }), writer: Box::new(writer), close: Arc::new(move || closer.close()) })
}

/// reading is set around each read, so a close cannot cancel too early: a
/// CancelIoEx before the read begins would miss it.
struct PipeState {
    h: Pipe,
    closed: AtomicBool,
    reading: AtomicBool,
}

impl PipeState {
    /// Ends a read blocked on the pipe in another thread, and every read
    /// after; the handles close as the reader and writer drop.
    fn close(&self) {
        self.closed.store(true, SeqCst);
        while self.reading.load(SeqCst) {
            unsafe { CancelIoEx(self.h.0, std::ptr::null()) };
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct PipeReader {
    file: std::fs::File,
    state: Arc<PipeState>,
}

impl Read for PipeReader {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.state.reading.store(true, SeqCst);
        let n = if self.state.closed.load(SeqCst) { Err(std::io::Error::other("file already closed")) } else { self.file.read(b) };
        self.state.reading.store(false, SeqCst);
        n
    }
}
