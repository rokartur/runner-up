//! The test binary started again as runner-up, as Go's TestMain did: a copy
//! of it named runner-up (cmd::tests::runner_up) is the program from before
//! main, so the test harness never starts.

/// A setting a test hands the program it runs; always None outside the test
/// build.
pub fn knob(name: &str) -> Option<String> {
    if cfg!(test) { std::env::var(name).ok() } else { None }
}

#[cfg(test)]
mod as_program {
    // Runs before main from the platform's initializer list. std reads the
    // arguments in an earlier one on Linux (.init_array.00099) and lazily
    // elsewhere.
    #[used]
    #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
    #[cfg_attr(target_vendor = "apple", unsafe(link_section = "__DATA,__mod_init_func"))]
    #[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
    static AS_PROGRAM: extern "C" fn() = as_program;

    extern "C" fn as_program() {
        // A supervisor starts program/current, a link to vX.Y.Z/runner-up.
        let me = std::env::current_exe().and_then(|p| p.canonicalize()).ok();
        if me.as_deref().and_then(std::path::Path::file_stem) != Some(std::ffi::OsStr::new("runner-up")) {
            return;
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let code = std::panic::catch_unwind(|| crate::cmd::tests::program(&args)).unwrap_or(101);
        std::process::exit(code);
    }
}
