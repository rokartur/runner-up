//! Fetches and checks runner-up's own release archives, as the release
//! workflow publishes them: one archive per platform,
//! runner-up-<version>-<os>-<arch>.tar.gz, or .zip for Windows, holding the
//! program alone; checksums.txt, each archive's SHA-256 as sha256sum prints
//! it; and checksums.txt.sig, the release signing key's Ed25519 signature of
//! checksums.txt.
//!
//! The coordinator trusts an archive whose SHA-256 the setup names.
//! runner-up's Windows service, which runs as SYSTEM, trusts only an archive
//! that checksums.txt lists and the release signing key signed.

use ed25519_dalek::Verifier;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;

/// Where GitHub serves runner-up's releases: an archive is at
/// DEFAULT_URL/<version>/<archive>.
pub const DEFAULT_URL: &str = "https://github.com/kunchenguid/runner-up/releases/download";

/// The platforms each release has an archive for, as os-arch.
pub const PLATFORMS: [&str; 6] =
    ["darwin-arm64", "darwin-amd64", "linux-arm64", "linux-amd64", "windows-arm64", "windows-amd64"];

/// The base64 public halves of the release signing keys, separated by
/// spaces: Ed25519 keys whose private halves sign each release's
/// checksums.txt in the release workflow. Whoever holds a private half
/// controls every Windows machine running the service. A test build trusts a
/// key of its own with RUNNER_UP_SIGNING_KEYS at build time.
const SIGNING_KEYS: &str = match option_env!("RUNNER_UP_SIGNING_KEYS") {
    Some(k) => k,
    None => "ETVXJEADqxXNJEbNpCs2ceglGJDucN7rEBigoqFX8RY=",
};

/// Bounds an archive or the program in it, so a wrong address cannot fill
/// the disk or memory.
pub const MAX_ARCHIVE: u64 = 256 << 20;

/// The os name as a release names it: darwin, linux or windows.
pub fn goos() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    }
}

/// The architecture as a release names it: amd64 or arm64.
pub fn goarch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => arch,
    }
}

/// This program's own platform, such as darwin-arm64.
pub fn this_platform() -> String {
    platform(goos(), goarch())
}

pub fn platform(os: &str, arch: &str) -> String {
    format!("{os}-{arch}")
}

/// The release archive of version for platform.
pub fn archive_name(version: &str, platform: &str) -> String {
    if platform.starts_with("windows-") {
        format!("runner-up-{version}-{platform}.zip")
    } else {
        format!("runner-up-{version}-{platform}.tar.gz")
    }
}

/// The program's file name in platform's archive.
pub fn program_name(platform: &str) -> &'static str {
    if platform.starts_with("windows-") { "runner-up.exe" } else { "runner-up" }
}

/// The three numbers and the pre-release of a release version
/// vMAJOR.MINOR.PATCH[-PRE], or None for another string.
fn parse_version(v: &str) -> Option<([u64; 3], &str)> {
    let rest = v.strip_prefix('v')?;
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
                return None;
            }
            (c, p)
        }
        None => (rest, ""),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut nums = [0u64; 3];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
            return None;
        }
        nums[i] = p.parse().ok()?;
    }
    Some((nums, pre))
}

/// Says whether v names a release, such as v0.1.11.
pub fn valid_version(v: &str) -> bool {
    parse_version(v).is_some()
}

/// Orders two release versions as semantic versions. A version that names no
/// release is older than every release.
pub fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (ma, mb) = (parse_version(a), parse_version(b));
    let ((na, pa), (nb, pb)) = match (ma, mb) {
        (None, None) => return a.cmp(b),
        (None, _) => return Less,
        (_, None) => return Greater,
        (Some(x), Some(y)) => (x, y),
    };
    if na != nb {
        return na.cmp(&nb);
    }
    // A pre-release comes before its release.
    match (pa, pb) {
        _ if pa == pb => Equal,
        ("", _) => Greater,
        (_, "") => Less,
        _ => compare_pre(&pa.split('.').collect::<Vec<_>>(), &pb.split('.').collect::<Vec<_>>()),
    }
}

fn compare_pre(a: &[&str], b: &[&str]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    for (x, y) in a.iter().zip(b) {
        match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) if x != y => return x.cmp(&y),
            (Ok(_), Err(_)) => return Less,
            (Err(_), Ok(_)) => return Greater,
            (Err(_), Err(_)) if x != y => return x.cmp(y),
            _ => {}
        }
    }
    a.len().cmp(&b.len())
}

/// Reads url whole, at most limit bytes.
pub fn fetch(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let resp = match crate::http::agent().get(url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            return Err(format!("{url} answered {code} {}", r.status_text()));
        }
        Err(e) => return Err(e.to_string()),
    };
    let mut b = Vec::new();
    resp.into_reader().take(limit + 1).read_to_end(&mut b).map_err(|e| format!("{url}: {e}"))?;
    if b.len() as u64 > limit {
        return Err(format!("{url} is larger than {limit} bytes"));
    }
    Ok(b)
}

/// b's SHA-256 in lowercase hex.
pub fn sha256(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Says whether s is a SHA-256 in lowercase hex.
pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The program in platform's archive, which must hold it.
pub fn program(archive: &[u8], platform: &str) -> Result<Vec<u8>, String> {
    let name = program_name(platform);
    if platform.starts_with("windows-") {
        let mut zr = zip::ZipArchive::new(std::io::Cursor::new(archive))
            .map_err(|e| format!("the archive is not a zip file: {e}"))?;
        for i in 0..zr.len() {
            let f = zr.by_index(i).map_err(|e| e.to_string())?;
            if f.name() != name {
                continue;
            }
            if f.size() > MAX_ARCHIVE {
                return Err(format!("{name} in the archive is too large"));
            }
            return read_at_most(f, name);
        }
        return Err(format!("the archive holds no {name}"));
    }
    if !archive.starts_with(&[0x1f, 0x8b]) {
        return Err("the archive is not gzip: gzip: invalid header".into());
    }
    let gz = flate2::read::GzDecoder::new(archive);
    let mut tr = tar::Archive::new(gz);
    let entries = tr.entries().map_err(|e| format!("the archive is not a tar file: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("the archive is not a tar file: {e}"))?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        if path.strip_prefix("./").unwrap_or(&path) == name && entry.header().entry_type() == tar::EntryType::Regular {
            return read_at_most(entry, name);
        }
    }
    Err(format!("the archive holds no {name}"))
}

fn read_at_most(r: impl Read, name: &str) -> Result<Vec<u8>, String> {
    let mut b = Vec::new();
    r.take(MAX_ARCHIVE + 1)
        .read_to_end(&mut b)
        .map_err(|e| format!("could not read {name} from the archive: {e}"))?;
    if b.len() as u64 > MAX_ARCHIVE {
        return Err(format!("{name} in the archive is too large"));
    }
    Ok(b)
}

/// Reads checksums.txt: each line a SHA-256 in hex, two spaces (or a space
/// and *, for a binary file) and a file name.
pub fn parse_checksums(b: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let text = String::from_utf8_lossy(b);
    for (i, line) in text.trim_end_matches('\n').split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let bad = || format!("checksums line {} is not a SHA-256 and a file name", i + 1);
        let Some((sum, name)) = line.split_once(' ') else { return Err(bad()) };
        let name = name.strip_prefix(' ').unwrap_or(name);
        let name = name.strip_prefix('*').unwrap_or(name);
        let sum = sum.to_lowercase();
        if !valid_sha256(&sum) || name.is_empty() || name.contains(['/', '\\', ' ']) {
            return Err(bad());
        }
        if out.contains_key(name) {
            return Err(format!("checksums list {name} twice"));
        }
        out.insert(name.to_string(), sum);
    }
    Ok(out)
}

pub const ERR_NO_SIGNING_KEY: &str = "this runner-up has no release signing key, so it accepts no signed release";

pub type PublicKey = ed25519_dalek::VerifyingKey;

/// Checks that sig is an Ed25519 signature of checksums by one of keys.
pub fn verify_signature(checksums: &[u8], sig: &[u8], keys: &[PublicKey]) -> Result<(), String> {
    if keys.is_empty() {
        return Err(ERR_NO_SIGNING_KEY.into());
    }
    let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else {
        return Err(format!("the signature is {} bytes, not an Ed25519 signature's 64", sig.len()));
    };
    if keys.iter().any(|k| k.verify(checksums, &sig).is_ok()) {
        return Ok(());
    }
    Err("the release signing key did not sign these checksums".into())
}

/// The public halves of the release signing keys this runner-up trusts.
pub fn signing_keys() -> Vec<PublicKey> {
    use base64::Engine;
    let keys = crate::testprog::knob("RUNNER_UP_TEST_SIGNING_KEYS").unwrap_or_else(|| SIGNING_KEYS.into());
    keys.split_whitespace()
        .map(|s| {
            let b = base64::engine::general_purpose::STANDARD.decode(s).ok();
            let key = b.and_then(|b| <[u8; 32]>::try_from(b).ok()).and_then(|b| PublicKey::from_bytes(&b).ok());
            key.unwrap_or_else(|| panic!("release signing key {s:?} is not a base64 Ed25519 public key"))
        })
        .collect()
}

/// A release archive with the signed checksums that vouch for it.
pub struct Signed {
    /// The archive's file name, as checksums.txt lists it.
    pub name: String,
    pub archive: Vec<u8>,
    pub checksums: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Reads a release archive's name: runner-up-<version>-<os>-<arch>.tar.gz|zip.
fn parse_archive_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("runner-up-")?;
    let rest = rest.strip_suffix(".tar.gz").or_else(|| rest.strip_suffix(".zip"))?;
    let (version_os, arch) = rest.rsplit_once('-')?;
    let (version, os) = version_os.rsplit_once('-')?;
    let plain = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !plain(os) || !plain(arch) || !version.starts_with('v') {
        return None;
    }
    Some((version.to_string(), format!("{os}-{arch}")))
}

impl Signed {
    /// Checks a signed archive for platform with keys: the signature, that
    /// the checksums list the archive under its name, that the archive
    /// matches, and that the name is platform's archive of a release. It
    /// returns the archive's version.
    pub fn verify(&self, platform: &str, keys: &[PublicKey]) -> Result<String, String> {
        verify_signature(&self.checksums, &self.signature, keys)?;
        let parsed = parse_archive_name(&self.name)
            .filter(|(v, p)| valid_version(v) && archive_name(v, p) == self.name);
        let Some((version, p)) = parsed else {
            return Err(format!("{} is not a runner-up release archive", crate::go::quote(&self.name)));
        };
        if p != platform {
            return Err(format!("{} is for {p}, not {platform}", self.name));
        }
        let sums = parse_checksums(&self.checksums)?;
        let Some(want) = sums.get(&self.name) else {
            return Err(format!("the signed checksums do not list {}", self.name));
        };
        let got = sha256(&self.archive);
        if &got != want {
            return Err(format!("{}'s SHA-256 is {got}, not the {want} the signed checksums list", self.name));
        }
        Ok(version)
    }
}

#[cfg(test)]
mod tests;
