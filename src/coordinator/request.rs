// A borrow or a drain is a request its caller owns: a folder of its own
// under the requests' root in the state folder (borrows/ or drains/). The
// caller makes the folder, takes its live.lock and then publishes the
// request by writing request.json. From then on the folder belongs to the
// coordinator, and only the caller letting go of live.lock (by finishing,
// crashing or being stopped) withdraws the request, even while no
// coordinator runs. The coordinator removes the folder of a request whose
// caller let go.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config;
use crate::lock;

/// The caller's side of a request folder.
pub struct CallerRequest {
    pub dir: PathBuf,
    live: Option<File>,
    published: bool,
}

/// Makes a new request folder under root, its name prefix and a random
/// suffix, and takes its live lock. Close it when the request ends.
pub fn open_request(root: &Path, prefix: &str) -> Result<CallerRequest, String> {
    config::create_dir_all_mode(root, 0o700).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
    let dir = make_temp_dir(root, prefix).map_err(|e| format!("mkdirtemp {}: {e}", root.display()))?;
    match lock::busy(&dir.join("live.lock")) {
        Ok(Some(live)) => Ok(CallerRequest { dir, live: Some(live), published: false }),
        Ok(None) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err("another process holds a new request's lock".into())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e.to_string())
        }
    }
}

fn make_temp_dir(root: &Path, prefix: &str) -> io::Result<PathBuf> {
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    loop {
        let dir = root.join(format!("{prefix}{}", config::random_digits()));
        match b.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

impl CallerRequest {
    /// The request's name, its folder's.
    pub fn id(&self) -> String {
        self.dir.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    /// Writes the request as request.json, handing it to the coordinator.
    pub fn publish(&mut self, request: &impl Serialize) -> Result<(), String> {
        config::write_json(&self.dir.join("request.json"), request)?;
        self.published = true;
        Ok(())
    }

    /// Lets go of the request. The coordinator ignores a folder that was
    /// never published, so close removes it; a published one is the
    /// coordinator's to remove.
    pub fn close(&mut self) {
        self.live = None;
        if !self.published {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Says whether a request's caller still holds it.
pub fn request_held(dir: &Path) -> io::Result<bool> {
    lock::held_by(&dir.join("live.lock"))
}

/// Lists the published request folders under root by name: live while their
/// callers hold them, gone once they let go. A folder counts as published
/// once one of the files named by published exists; before that its caller
/// may still be making it, so it is in neither list. A folder whose lock
/// cannot be read is in neither list either.
pub fn scan_requests(root: &Path, published: &[&str]) -> (Vec<String>, Vec<String>) {
    let (mut live, mut gone) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(root) else {
        return (live, gone);
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let dir = root.join(&name);
        if !published.iter().any(|f| dir.join(f).exists()) {
            continue;
        }
        match request_held(&dir) {
            Ok(true) => live.push(name),
            Ok(false) => gone.push(name),
            Err(_) => {}
        }
    }
    (live, gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A request counts only once published, is live while its caller holds
    // it, and is gone once the caller lets go; an unpublished one leaves no
    // folder behind.
    #[test]
    fn requests_are_live_until_their_caller_lets_go() {
        let root = tempfile::tempdir().unwrap();
        let mut unpublished = open_request(root.path(), "drain-").unwrap();
        let mut r = open_request(root.path(), "drain-").unwrap();
        assert_eq!(scan_requests(root.path(), &["request.json"]), (vec![], vec![]));

        r.publish(&serde_json::json!({"place": "p1"})).unwrap();
        assert_eq!(scan_requests(root.path(), &["request.json"]), (vec![r.id()], vec![]));

        r.close();
        assert_eq!(scan_requests(root.path(), &["request.json"]), (vec![], vec![r.id()]));

        unpublished.close();
        assert!(!unpublished.dir.exists());
        assert!(r.dir.exists());
    }
}
