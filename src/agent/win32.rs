//! Windows calls the service makes, wrapped once: owned handles, SIDs and
//! security descriptors that free themselves, and the account, profile and
//! desktop calls that windows-sys leaves raw.

use std::io;

use windows_sys::Win32::Foundation::{BOOL, CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    LOCALGROUP_MEMBERS_INFO_0, NetLocalGroupAddMembers, NetUserAdd, NetUserDel, NetUserSetInfo, UF_DONT_EXPIRE_PASSWD, UF_NORMAL_ACCOUNT,
    UF_PASSWD_CANT_CHANGE, UF_SCRIPT, USER_INFO_1, USER_PRIV_USER,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{
    ACL, CreateWellKnownSid, EqualSid, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, LOGON32_LOGON_BATCH,
    LOGON32_PROVIDER_DEFAULT, LogonUserW, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_PROCESS_INFO_EXW, WTSEnumerateProcessesExW, WTSFreeMemoryExW, WTSTypeProcessInfoLevel1,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW, GetProcessWindowStation, HDESK, HWINSTA,
    SetProcessWindowStation,
};
use windows_sys::Win32::UI::Shell::{DeleteProfileW, LoadUserProfileW, PROFILEINFOW, UnloadUserProfile};

pub use super::winpipe::{from_wide_ptr, sid_string, wide};

pub const NERR_USER_EXISTS: u32 = 2224;
pub const NERR_USER_NOT_FOUND: u32 = 2221;
const ERROR_MEMBER_IN_ALIAS: u32 = 1378;
pub const UF_ACCOUNTDISABLE: u32 = 0x0002;
/// Every job account's flags, disabled at cleanup.
pub const ACCOUNT_FLAGS: u32 = UF_SCRIPT | UF_NORMAL_ACCOUNT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE;
const WTS_ANY_SESSION: u32 = 0xFFFF_FFFE;
const PI_NOUI: u32 = 1;
const WINSTA_ALL_ACCESS: u32 = 0x37F;
const DESKTOP_ALL_ACCESS: u32 = 0x01FF;

pub fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// Ok when a Win32 call returned non-zero, else the thread's last error.
pub fn check(ok: BOOL) -> io::Result<()> {
    if ok == 0 { Err(last_error()) } else { Ok(()) }
}

/// A status a Net or Reg call returns.
pub fn status(r: u32) -> io::Result<()> {
    if r == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(r as i32)) }
}

pub fn is_code(e: &io::Error, code: u32) -> bool {
    e.raw_os_error() == Some(code as i32)
}

/// A handle closed when it drops.
pub struct Handle(pub HANDLE);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// h, or the last error when h is null or INVALID_HANDLE_VALUE.
    pub fn new(h: HANDLE) -> io::Result<Handle> {
        if h.is_null() || h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE { Err(last_error()) } else { Ok(Handle(h)) }
    }
}

impl Default for Handle {
    fn default() -> Handle {
        Handle(std::ptr::null_mut())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// h as a File, which owns it from here on.
pub fn into_file(h: Handle) -> std::fs::File {
    use std::os::windows::io::FromRawHandle;
    let f = unsafe { std::fs::File::from_raw_handle(h.0 as _) };
    std::mem::forget(h);
    f
}

/// A pipe whose two ends a child may inherit: (read, write).
pub fn inheritable_pipe() -> io::Result<(Handle, Handle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
    check(unsafe { windows_sys::Win32::System::Pipes::CreatePipe(&mut r, &mut w, &sa, 0) })?;
    Ok((Handle(r), Handle(w)))
}

/// A process attribute list naming the only handles a child inherits.
pub struct AttributeList<'a> {
    buf: Vec<u64>,
    _handles: &'a [HANDLE],
}

impl<'a> AttributeList<'a> {
    pub fn handles(handles: &'a [HANDLE]) -> io::Result<AttributeList<'a>> {
        use windows_sys::Win32::System::Threading::{
            InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, UpdateProcThreadAttribute,
        };
        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut l = AttributeList { buf: vec![0u64; size.div_ceil(8)], _handles: handles };
        check(unsafe { InitializeProcThreadAttributeList(l.ptr(), 1, 0, &mut size) })?;
        check(unsafe {
            UpdateProcThreadAttribute(
                l.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr() as _,
                std::mem::size_of_val(handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        })?;
        Ok(l)
    }

    pub fn ptr(&mut self) -> windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr() as _
    }
}

impl Drop for AttributeList<'_> {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList(self.ptr()) };
    }
}

/// An account's own environment block, as CreateEnvironmentBlock makes it.
pub struct Environment(*mut core::ffi::c_void);

impl Environment {
    pub fn of(token: HANDLE) -> io::Result<Environment> {
        let mut env = std::ptr::null_mut();
        check(unsafe { windows_sys::Win32::System::Environment::CreateEnvironmentBlock(&mut env, token, 0) })?;
        Ok(Environment(env))
    }

    pub fn ptr(&self) -> *const core::ffi::c_void {
        self.0
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Environment::DestroyEnvironmentBlock(self.0) };
    }
}

/// s with its %VARIABLES% filled in, or s as it is when that fails.
pub fn expand_env(s: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let w = wide(s);
    let n = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), std::ptr::null_mut(), 0) };
    if n == 0 {
        return s.to_string();
    }
    let mut buf = vec![0u16; n as usize];
    let n = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), buf.as_mut_ptr(), n) };
    if n == 0 {
        return s.to_string();
    }
    from_wide_ptr(buf.as_ptr())
}

/// This machine's NetBIOS name, as Go's windows.ComputerName.
pub fn computer_name() -> io::Result<String> {
    use windows_sys::Win32::System::SystemInformation::{ComputerNameNetBIOS, GetComputerNameExW};
    let mut buf = [0u16; 64];
    let mut n = buf.len() as u32;
    check(unsafe { GetComputerNameExW(ComputerNameNetBIOS, buf.as_mut_ptr(), &mut n) })?;
    Ok(String::from_utf16_lossy(&buf[..n as usize]))
}

/// The SID of the account name, or None when no account has it.
pub fn lookup_sid(name: &str) -> io::Result<Option<String>> {
    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_NONE_MAPPED};
    use windows_sys::Win32::Security::LookupAccountNameW;
    let n = wide(name);
    let mut sid = vec![0u64; 9];
    let mut domain = vec![0u16; 64];
    loop {
        let mut sid_len = (sid.len() * 8) as u32;
        let mut domain_len = domain.len() as u32;
        let mut kind = 0;
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                n.as_ptr(),
                sid.as_mut_ptr() as _,
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        if ok != 0 {
            return sid_string(sid.as_mut_ptr() as _).map(Some).map_err(io::Error::other);
        }
        let e = last_error();
        if is_code(&e, ERROR_NONE_MAPPED) {
            return Ok(None);
        }
        if !is_code(&e, ERROR_INSUFFICIENT_BUFFER) {
            return Err(e);
        }
        sid = vec![0u64; (sid_len as usize).div_ceil(8)];
        domain = vec![0u16; domain_len as usize];
    }
}

/// The name of the account sid, without its domain.
pub fn lookup_account_name(sid: &Sid) -> io::Result<String> {
    use windows_sys::Win32::Security::LookupAccountSidW;
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut n, mut d, mut kind) = (name.len() as u32, domain.len() as u32, 0);
    check(unsafe { LookupAccountSidW(std::ptr::null(), sid.ptr(), name.as_mut_ptr(), &mut n, domain.as_mut_ptr(), &mut d, &mut kind) })?;
    Ok(String::from_utf16_lossy(&name[..n as usize]))
}

fn open_key(path: &str, access: u32) -> io::Result<HKEY> {
    let mut k = std::ptr::null_mut();
    status(unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide(path).as_ptr(), 0, access, &mut k) })?;
    Ok(k)
}

/// The string value name of HKLM\path.
pub fn reg_string(path: &str, name: &str) -> Option<String> {
    let k = open_key(path, KEY_QUERY_VALUE).ok()?;
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let mut kind = 0;
    let r = unsafe { RegQueryValueExW(k, wide(name).as_ptr(), std::ptr::null(), &mut kind, buf.as_mut_ptr() as _, &mut size) };
    unsafe { RegCloseKey(k) };
    if r != 0 || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
        return None;
    }
    buf.truncate(size as usize / 2);
    Some(from_wide_ptr(buf.iter().copied().chain([0]).collect::<Vec<_>>().as_ptr()))
}

pub fn reg_key_exists(path: &str) -> bool {
    let Ok(k) = open_key(path, KEY_QUERY_VALUE) else {
        return false;
    };
    unsafe { RegCloseKey(k) };
    true
}

pub fn reg_delete_key(path: &str) -> io::Result<()> {
    status(unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, wide(path).as_ptr()) })
}

pub fn reg_delete_value(path: &str, name: &str) -> io::Result<()> {
    let k = open_key(path, KEY_SET_VALUE)?;
    let r = unsafe { RegDeleteValueW(k, wide(name).as_ptr()) };
    unsafe { RegCloseKey(k) };
    status(r)
}

/// Sets the DWORD name of HKLM\path, making the key when it is missing.
pub fn reg_set_dword(path: &str, name: &str, v: u32) -> io::Result<()> {
    let mut k = std::ptr::null_mut();
    status(unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(path).as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut k,
            std::ptr::null_mut(),
        )
    })?;
    let r = unsafe { RegSetValueExW(k, wide(name).as_ptr(), 0, REG_DWORD, &v as *const u32 as _, 4) };
    unsafe { RegCloseKey(k) };
    status(r)
}

/// A SID that LocalAlloc or a buffer of ours holds.
pub struct Sid {
    ptr: PSID,
    local: bool,
    _buf: Vec<u8>,
}

unsafe impl Send for Sid {}
unsafe impl Sync for Sid {}

impl Sid {
    pub fn parse(s: &str) -> io::Result<Sid> {
        let mut p = std::ptr::null_mut();
        check(unsafe { ConvertStringSidToSidW(wide(s).as_ptr(), &mut p) })?;
        Ok(Sid { ptr: p, local: true, _buf: Vec::new() })
    }

    pub fn well_known(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Sid> {
        let mut buf = vec![0u8; 68];
        let mut n = buf.len() as u32;
        check(unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), buf.as_mut_ptr() as _, &mut n) })?;
        Ok(Sid { ptr: buf.as_mut_ptr() as _, local: false, _buf: buf })
    }

    pub fn ptr(&self) -> PSID {
        self.ptr
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        if self.local {
            unsafe { LocalFree(self.ptr as _) };
        }
    }
}

/// Whether a equals the SID at b.
pub fn sid_is(a: PSID, b: &Sid) -> bool {
    unsafe { EqualSid(a, b.ptr()) != 0 }
}

/// A security descriptor made from SDDL.
pub struct Descriptor(PSECURITY_DESCRIPTOR);

unsafe impl Send for Descriptor {}
unsafe impl Sync for Descriptor {}

impl Descriptor {
    pub fn parse(sddl: &str) -> io::Result<Descriptor> {
        let mut sd = std::ptr::null_mut();
        check(unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), 1, &mut sd, std::ptr::null_mut()) })?;
        Ok(Descriptor(sd))
    }

    /// Attributes that give an object this descriptor; valid while it lives.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.0, bInheritHandle: 0 }
    }

    pub fn owner(&self) -> PSID {
        let mut owner = std::ptr::null_mut();
        let mut defaulted = 0;
        unsafe { GetSecurityDescriptorOwner(self.0, &mut owner, &mut defaulted) };
        owner
    }

    pub fn dacl(&self) -> *const ACL {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl = std::ptr::null_mut();
        unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) };
        dacl
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0 as _) };
    }
}

/// name, password and comment end in NUL.
pub fn net_user_add(name: &[u16], password: &[u16], comment: &[u16]) -> io::Result<()> {
    let info = USER_INFO_1 {
        usri1_name: name.as_ptr() as _,
        usri1_password: password.as_ptr() as _,
        usri1_password_age: 0,
        usri1_priv: USER_PRIV_USER,
        usri1_home_dir: std::ptr::null_mut(),
        usri1_comment: comment.as_ptr() as _,
        usri1_flags: ACCOUNT_FLAGS,
        usri1_script_path: std::ptr::null_mut(),
    };
    status(unsafe { NetUserAdd(std::ptr::null(), 1, &info as *const _ as _, std::ptr::null_mut()) })
}

pub fn net_user_del(name: &str) -> io::Result<()> {
    status(unsafe { NetUserDel(std::ptr::null(), wide(name).as_ptr()) })
}

pub fn net_local_group_add_member(group: &str, sid: &Sid) -> io::Result<()> {
    let m = LOCALGROUP_MEMBERS_INFO_0 { lgrmi0_sid: sid.ptr() };
    let r = unsafe { NetLocalGroupAddMembers(std::ptr::null(), wide(group).as_ptr(), 0, &m as *const _ as _, 1) };
    if r == ERROR_MEMBER_IN_ALIAS {
        return Ok(());
    }
    status(r)
}

/// Sets a local account's flags (USER_INFO_1008).
pub fn net_user_set_flags(name: &str, flags: u32) -> io::Result<()> {
    status(unsafe { NetUserSetInfo(std::ptr::null(), wide(name).as_ptr(), 1008, &flags as *const u32 as _, std::ptr::null_mut()) })
}

/// Signs name in for a batch job; password ends in NUL.
pub fn logon_user(name: &str, password: &[u16]) -> io::Result<Handle> {
    let mut t = std::ptr::null_mut();
    check(unsafe {
        LogonUserW(wide(name).as_ptr(), wide(".").as_ptr(), password.as_ptr(), LOGON32_LOGON_BATCH, LOGON32_PROVIDER_DEFAULT, &mut t)
    })?;
    Ok(Handle(t))
}

pub fn load_user_profile(token: HANDLE, name: &str) -> io::Result<HANDLE> {
    let mut n = wide(name);
    let mut pi: PROFILEINFOW = unsafe { std::mem::zeroed() };
    pi.dwSize = std::mem::size_of::<PROFILEINFOW>() as u32;
    pi.dwFlags = PI_NOUI;
    pi.lpUserName = n.as_mut_ptr();
    check(unsafe { LoadUserProfileW(token, &mut pi) })?;
    Ok(pi.hProfile)
}

pub fn unload_user_profile(token: HANDLE, profile: HANDLE) -> io::Result<()> {
    check(unsafe { UnloadUserProfile(token, profile) })
}

pub fn delete_profile(sid: &str) -> io::Result<()> {
    check(unsafe { DeleteProfileW(wide(sid).as_ptr(), std::ptr::null(), std::ptr::null()) })
}

pub fn create_window_station(name: &str, sa: &SECURITY_ATTRIBUTES) -> io::Result<HWINSTA> {
    let h = unsafe { CreateWindowStationW(wide(name).as_ptr(), 0, WINSTA_ALL_ACCESS, sa) };
    if h.is_null() { Err(last_error()) } else { Ok(h) }
}

pub fn close_window_station(h: HWINSTA) {
    unsafe { CloseWindowStation(h) };
}

pub fn process_window_station() -> HWINSTA {
    unsafe { GetProcessWindowStation() }
}

pub fn set_process_window_station(h: HWINSTA) -> io::Result<()> {
    check(unsafe { SetProcessWindowStation(h) })
}

pub fn create_desktop(name: &str, sa: &SECURITY_ATTRIBUTES) -> io::Result<HDESK> {
    let h = unsafe { CreateDesktopW(wide(name).as_ptr(), std::ptr::null(), std::ptr::null(), 0, DESKTOP_ALL_ACCESS, sa) };
    if h.is_null() { Err(last_error()) } else { Ok(h) }
}

pub fn close_desktop(h: HDESK) {
    unsafe { CloseDesktop(h) };
}

/// Every process on the PC signed in as sid, by Windows' own reading of each
/// process's account.
pub fn process_ids_of(sid: &str) -> io::Result<Vec<u32>> {
    let mut level = 1u32;
    let mut list: *mut WTS_PROCESS_INFO_EXW = std::ptr::null_mut();
    let mut count = 0u32;
    check(unsafe { WTSEnumerateProcessesExW(std::ptr::null_mut(), &mut level, WTS_ANY_SESSION, &mut list as *mut _ as _, &mut count) })?;
    let mut out = Vec::new();
    for p in unsafe { std::slice::from_raw_parts(list, count as usize) } {
        if !p.pUserSid.is_null() && sid_string(p.pUserSid).is_ok_and(|s| s == sid) {
            out.push(p.ProcessId);
        }
    }
    unsafe { WTSFreeMemoryExW(WTSTypeProcessInfoLevel1, list as _, count) };
    Ok(out)
}
