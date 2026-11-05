/// The program's version: RUNNER_UP_VERSION at build time, "dev" otherwise.
/// A test build installed in a version's folder, such as program/v9.0.2, is
/// that version, as Go's tests set it with -ldflags; so is one the Windows
/// service installed, in program files' v9.0.2-<checksum>.
pub fn version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| test_version().unwrap_or_else(|| option_env!("RUNNER_UP_VERSION").unwrap_or("dev").into()))
}

fn test_version() -> Option<String> {
    if !cfg!(test) {
        return None;
    }
    let me = std::env::current_exe().ok()?.canonicalize().ok()?;
    let folder = me.parent()?.file_name()?.to_str()?.split('-').next()?;
    let numbers = folder.strip_prefix('v')?;
    let is_version = numbers.split('.').count() == 3 && numbers.split('.').all(|n| n.parse::<u32>().is_ok());
    is_version.then(|| folder.to_string())
}
