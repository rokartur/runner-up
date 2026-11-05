//! Go's signal.NotifyContext: a context that ends when the process is asked
//! to stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::ctx::Ctx;

static ASKED: AtomicBool = AtomicBool::new(false);

/// A child of parent that ends once the process gets SIGTERM, SIGINT or
/// SIGHUP. On Windows it ends on Ctrl-C.
pub fn notify_stop(parent: &Ctx) -> Ctx {
    install();
    let ctx = parent.with_cancel();
    let watched = ctx.clone();
    std::thread::spawn(move || {
        while !watched.is_done() {
            if ASKED.load(Ordering::SeqCst) {
                watched.cancel();
                return;
            }
            watched.sleep(Duration::from_millis(100));
        }
    });
    ctx
}

#[cfg(unix)]
fn install() {
    extern "C" fn on_signal(_: libc::c_int) {
        ASKED.store(true, Ordering::SeqCst);
    }
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
}

#[cfg(windows)]
fn install() {
    unsafe extern "system" fn on_ctrl(_: u32) -> windows_sys::Win32::Foundation::BOOL {
        ASKED.store(true, Ordering::SeqCst);
        1
    }
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1) };
}
