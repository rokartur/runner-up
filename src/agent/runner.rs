use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::release::{goarch, goos};

/// Every place runs this version of GitHub's runner. Its scale sets turn
/// updates off, so a newer runner is a change here.
macro_rules! runner_version {
    () => {
        "2.338.1"
    };
}
pub(crate) use runner_version;
pub const RUNNER_VERSION: &str = runner_version!();

/// The published SHA-256 sums of the runner's Linux and macOS release
/// archives, from its release page.
pub fn runner_sha256(platform: &str) -> Option<&'static str> {
    Some(match platform {
        "linux-x64" => "08e9ccb3604d2462c24b5e33604dc30810296ca15cc65f23cffd4317eb144c37",
        "linux-arm64" => "475e0c149fd3b611610127b9664ba5bacf5c6928795cc448ab520bb5f324bede",
        "osx-x64" => "f06e329ffe90a2a4bda8b460a20b9e26dcb7525a093bf9b4f89125c72bc1f39e",
        "osx-arm64" => "52c3fb801b931fbd454979bae5ec3443ea3da2c4a1b4c44fa546fb630d61a0a9",
        _ => return None,
    })
}

/// The same for its Windows release archives, by architecture.
#[cfg(any(windows, test))]
pub fn runner_windows_sha256(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x64" => "478f533ac2c349040fbaea7e158f7f7d0235085cf35bc81f661af0c715207ff9",
        "arm64" => "424fa096d48a7e082f13025fc5c01048520e39164e720a27130490f2dd86701d",
        _ => return None,
    })
}

/// The runner archive every place uses instead of the pinned release: a
/// test's stand-in runner (testprog.rs).
fn runner_archive() -> Option<String> {
    crate::testprog::knob("RUNNER_UP_TEST_RUNNER_ARCHIVE")
}

/// The runner's name for this machine's architecture.
pub fn runner_arch() -> &'static str {
    match goarch() {
        "amd64" => "x64",
        arch => arch,
    }
}

/// Names this machine's runner release, such as osx-arm64. A tart clone runs
/// the same system and architecture as its Mac.
pub fn runner_platform() -> String {
    let system = match goos() {
        "darwin" => "osx",
        os => os,
    };
    format!("{system}-{}", runner_arch())
}

/// "ok" when a runner release exists for this machine, or why not.
#[cfg(unix)]
pub fn runner_ready() -> String {
    if runner_sha256(&runner_platform()).is_none() && runner_archive().is_none() {
        return format!("no runner release for {}/{}", goos(), goarch());
    }
    "ok".into()
}

/// The runner's release archive for this machine, kept in cache_dir after the
/// first download and checked against its published sum on every use.
pub fn runner_tarball(ctx: &Ctx, cache_dir: &Path) -> Result<PathBuf, String> {
    if let Some(archive) = runner_archive() {
        return Ok(archive.into());
    }
    let platform = runner_platform();
    let Some(sum) = runner_sha256(&platform) else {
        return Err(format!("no runner release for {}/{}", goos(), goarch()));
    };
    runner_download(ctx, cache_dir, &format!("actions-runner-{platform}-{RUNNER_VERSION}.tar.gz"), sum)
}

/// The same for the runner's Windows release archive.
#[cfg(windows)]
pub fn runner_zip(ctx: &Ctx, cache_dir: &Path) -> Result<PathBuf, String> {
    let arch = runner_arch();
    let Some(sum) = runner_windows_sha256(arch) else {
        return Err(format!("no Windows runner release for {}", goarch()));
    };
    runner_download(ctx, cache_dir, &format!("actions-runner-win-{arch}-{RUNNER_VERSION}.zip"), sum)
}

/// Where GitHub publishes the runner's release archive name.
pub fn runner_url(name: &str) -> String {
    format!("https://github.com/actions/runner/releases/download/v{RUNNER_VERSION}/{name}")
}

/// The runner's Linux release archive for arch.
pub fn runner_tarball_name(arch: &str) -> String {
    format!("actions-runner-linux-{arch}-{RUNNER_VERSION}.tar.gz")
}

/// The runner release archive name, kept in cache_dir and checked against its
/// published sum on every use.
pub fn runner_download(ctx: &Ctx, cache_dir: &Path, name: &str, sum: &str) -> Result<PathBuf, String> {
    let path = cache_dir.join(name);
    if file_sha256(&path).is_ok_and(|got| got == sum) {
        return Ok(path);
    }
    crate::config::create_dir_all_mode(cache_dir, 0o700).map_err(|e| e.to_string())?;
    let mut req = crate::http::agent().get(&runner_url(name));
    if let Some(left) = ctx.remaining() {
        req = req.timeout(left);
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return Err(format!("download {name}: HTTP {code}")),
        Err(e) => return Err(format!("download {name}: {e}")),
    };
    let tmp_path = cache_dir.join(format!(".runner-{}", crate::random::hex(8)));
    let mut tmp = std::fs::File::create_new(&tmp_path).map_err(|e| e.to_string())?;
    let result = fill(ctx, name, sum, resp, &mut tmp);
    drop(tmp);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.to_string());
    }
    Ok(path)
}

/// Writes the download into tmp and checks its sum.
fn fill(ctx: &Ctx, name: &str, sum: &str, resp: ureq::Response, tmp: &mut std::fs::File) -> Result<(), String> {
    let mut h = Sha256::new();
    let mut body = resp.into_reader();
    let mut b = vec![0u8; 64 << 10];
    loop {
        if ctx.is_done() {
            return Err(format!("download {name}: {}", ctx.err().unwrap_or("stopped")));
        }
        let n = body.read(&mut b).map_err(|e| format!("download {name}: {e}"))?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
        tmp.write_all(&b[..n]).map_err(|e| format!("download {name}: {e}"))?;
    }
    let got = crate::release::hex(&h.finalize());
    if got != sum {
        return Err(format!("download {name}: its SHA-256 is {got}, not the published {sum}"));
    }
    Ok(())
}

pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(crate::release::hex(&h.finalize()))
}
