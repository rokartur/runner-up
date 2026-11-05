//! Deleting what a job account left, as SYSTEM, without ever being led
//! somewhere else. A live process of another job's account may swap any
//! folder it can write for a junction at any moment, so nothing here goes
//! by path below a trusted root: every entry is opened relative to its
//! parent's open handle, as the entry itself (a link is opened, never
//! followed), with backup intent so the account's own permissions cannot
//! lock SYSTEM out, and is deleted through that handle.

use std::io;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, HANDLE, NTSTATUS, STATUS_ACCESS_DENIED, STATUS_DELETE_PENDING, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SHARING_VIOLATION, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfoEx, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, READ_CONTROL, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;

use super::win32::{Handle, Sid, check, is_code, last_error, sid_is, wide};

// Where FILE_FULL_DIR_INFO keeps its fields.
const DIR_INFO_NAME_LENGTH: usize = 60;
const DIR_INFO_NAME: usize = 68;

const TREE_ACCESS: u32 = DELETE | READ_CONTROL | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

/// An open file or folder, never a link's target.
pub struct Entry {
    pub h: Handle,
    attrs: u32,
    /// Someone holds it without sharing, so it was opened only to read its
    /// owner.
    held: bool,
}

impl Entry {
    pub fn dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0 && self.attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
}

/// Why open_at failed.
pub enum Open {
    /// It vanished between listing and opening it.
    Gone,
    /// Someone holds it so that not even its owner can be read.
    Shared,
    /// Refused to SYSTEM even to read with backup rights.
    Denied,
    Failed(String),
}

impl Open {
    fn text(&self) -> String {
        match self {
            Open::Gone => "it is gone".into(),
            Open::Shared => "it is held by another process".into(),
            Open::Denied => "access is denied".into(),
            Open::Failed(e) => e.clone(),
        }
    }
}

fn nt_open(status: NTSTATUS) -> Open {
    match status {
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_DELETE_PENDING => Open::Gone,
        STATUS_SHARING_VIOLATION => Open::Shared,
        STATUS_ACCESS_DENIED => Open::Denied,
        s => Open::Failed(format!("NTSTATUS {:#x}", s as u32)),
    }
}

/// Opens path itself, a link included, with backup intent.
pub fn open(path: &str, access: u32) -> io::Result<Handle> {
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, OPEN_EXISTING};
    Handle::new(unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    })
}

/// Opens a trusted folder, such as C:\ProgramData, by its path. It refuses
/// one that is a link.
pub fn open_root(path: &str) -> io::Result<Entry> {
    let e = describe(open(path, TREE_ACCESS & !DELETE)?)?;
    if !e.dir() {
        return Err(io::Error::other(format!("{path} is not a plain folder")));
    }
    Ok(e)
}

/// Opens name inside the open folder parent: the entry itself, a link
/// included.
pub fn open_at(parent: HANDLE, name: &str) -> Result<Entry, Open> {
    let mut u: Vec<u16> = name.encode_utf16().collect();
    let bytes = (u.len() * 2) as u16;
    let us = UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: u.as_mut_ptr() };
    let oa = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: &us,
        Attributes: OBJ_CASE_INSENSITIVE as u32,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let nt_open_with = |access: u32| -> Result<HANDLE, NTSTATUS> {
        let mut h = std::ptr::null_mut();
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let s = unsafe {
            NtCreateFile(
                &mut h,
                access,
                &oa,
                &mut iosb,
                std::ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT | FILE_SYNCHRONOUS_IO_NONALERT,
                std::ptr::null(),
                0,
            )
        };
        if s < 0 { Err(s) } else { Ok(h) }
    };
    let mut held = false;
    let mut r = nt_open_with(TREE_ACCESS);
    if r == Err(STATUS_ACCESS_DENIED) {
        // Windows' own protected folders, such as ProgramData's Windows
        // Defender, refuse even SYSTEM the right to delete. It is still
        // opened to read its owner and list it, and deleting it then fails
        // as an error.
        r = nt_open_with(TREE_ACCESS & !DELETE);
    }
    if r == Err(STATUS_SHARING_VIOLATION) {
        // Someone holds it without sharing reading or deleting: it is still
        // opened to read its owner, and deleting it then fails as an error.
        held = true;
        r = nt_open_with(READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE);
    }
    let h = Handle(r.map_err(nt_open)?);
    let mut e = describe(h).map_err(|e| Open::Failed(e.to_string()))?;
    e.held = held;
    Ok(e)
}

pub fn describe(h: Handle) -> io::Result<Entry> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    check(unsafe { GetFileInformationByHandle(h.0, &mut info) })?;
    Ok(Entry { h, attrs: info.dwFileAttributes, held: false })
}

/// An open folder's entries.
fn names(dir: HANDLE) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    // u64s keep the records aligned.
    let mut buf = vec![0u64; 8 << 10];
    let size = (buf.len() * 8) as u32;
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        let ok = unsafe { GetFileInformationByHandleEx(dir, class, buf.as_mut_ptr() as _, size) };
        class = FileFullDirectoryInfo;
        if ok == 0 {
            let e = last_error();
            if is_code(&e, ERROR_NO_MORE_FILES) {
                return Ok(out);
            }
            return Err(e.to_string());
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, size as usize) };
        let mut off = 0;
        loop {
            let rec = &bytes[off..];
            let n = u32::from_le_bytes(rec[DIR_INFO_NAME_LENGTH..DIR_INFO_NAME_LENGTH + 4].try_into().unwrap()) as usize / 2;
            let u: Vec<u16> = (0..n).map(|i| u16::from_le_bytes([rec[DIR_INFO_NAME + 2 * i], rec[DIR_INFO_NAME + 2 * i + 1]])).collect();
            let name = String::from_utf16_lossy(&u);
            if name != "." && name != ".." {
                out.push(name);
            }
            let next = u32::from_le_bytes(rec[..4].try_into().unwrap()) as usize;
            if next == 0 {
                break;
            }
            off += next;
        }
    }
}

/// Whether sid owns the file or folder h.
pub fn owner_is(h: HANDLE, sid: &Sid) -> io::Result<bool> {
    let mut owner = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    let rc = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let is = sid_is(owner, sid);
    unsafe { windows_sys::Win32::Foundation::LocalFree(sd as _) };
    Ok(is)
}

/// Deletes an open entry, a folder once empty, at once from its folder even
/// while someone else holds it open.
fn delete_open(h: HANDLE) -> io::Result<()> {
    let flags: u32 = FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE;
    check(unsafe { SetFileInformationByHandle(h, FileDispositionInfoEx, &flags as *const u32 as _, 4) })
}

fn join(errs: Vec<String>) -> Result<(), String> {
    if errs.is_empty() { Ok(()) } else { Err(errs.join("\n")) }
}

/// Deletes an open entry and everything below it. A link inside is deleted
/// itself.
fn remove_tree(e: &Entry) -> Result<(), String> {
    let mut errs = Vec::new();
    if e.dir() {
        let children = names(e.h.0).unwrap_or_else(|err| {
            errs.push(err);
            Vec::new()
        });
        for name in children {
            match open_at(e.h.0, &name) {
                Err(Open::Gone) => {}
                Err(err) => errs.push(format!("{name}: {}", err.text())),
                Ok(c) => {
                    if let Err(err) = remove_tree(&c) {
                        errs.push(format!("{name}\\{err}"));
                    }
                }
            }
        }
    }
    if errs.is_empty()
        && let Err(err) = delete_open(e.h.0)
    {
        errs.push(err.to_string());
    }
    join(errs)
}

/// Deletes name inside the trusted folder root, if it is there.
pub fn remove_at(root: &str, name: &str) -> Result<(), String> {
    let r = open_root(root).map_err(|e| e.to_string())?;
    match open_at(r.h.0, name) {
        Err(Open::Gone) => Ok(()),
        Err(e) => Err(e.text()),
        Ok(e) => remove_tree(&e),
    }
}

/// Whether name is inside the trusted folder root.
pub fn exists_at(root: &str, name: &str) -> bool {
    let Ok(r) = open_root(root) else {
        return false;
    };
    !matches!(open_at(r.h.0, name), Err(Open::Gone))
}

/// Deletes every entry sid owns in the open folder dir, and with deep in
/// every plain folder below it, never through a link.
pub fn remove_owned(dir: &Entry, sid: &Sid, deep: bool) -> Result<(), String> {
    let mut errs = Vec::new();
    let children = names(dir.h.0).unwrap_or_else(|err| {
        errs.push(err);
        Vec::new()
    });
    for name in children {
        let c = match open_at(dir.h.0, &name) {
            // Not even its owner can be read, as with C:\pagefile.sys: only
            // Windows holds a file that way, and the job's processes have
            // ended.
            Err(Open::Gone | Open::Shared) => continue,
            // Refused to SYSTEM even to read with backup rights: only a
            // driver of Windows' own guards a folder that way, and no job
            // account can write there either.
            Err(Open::Denied) => continue,
            Err(Open::Failed(e)) => {
                errs.push(format!("{name}: {e}"));
                continue;
            }
            Ok(c) => c,
        };
        match owner_is(c.h.0, sid) {
            Err(_) => {}
            Ok(true) => {
                if let Err(e) = remove_tree(&c) {
                    errs.push(format!("{name}: {e}"));
                }
            }
            Ok(false) if deep && c.dir() && !c.held => {
                if let Err(e) = remove_owned(&c, sid, true) {
                    errs.push(format!("{name}\\{e}"));
                }
            }
            Ok(false) => {}
        }
    }
    join(errs)
}
