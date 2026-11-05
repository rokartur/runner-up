use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::run;

const RELEASE: &str = "https://github.com/kunchenguid/runner-up/releases/";

/// A release's archive for platform, holding a runner-up program, and a
/// checksums.txt as the release workflow's sha256sum writes it, listing every
/// platform's archive. listed is the SHA-256 it records for this platform's
/// archive; "" records the archive's real one and "-" leaves it out.
struct ReleaseFixture {
    archive: PathBuf,
    checksums: PathBuf,
    #[cfg_attr(not(unix), allow(dead_code))]
    program: Vec<u8>,
}

fn release_fixture(dir: &Path, version: &str, platform: &str, listed: &str) -> ReleaseFixture {
    let program = format!("runner-up {version} {platform}\n").into_bytes();
    let windows = platform.starts_with("windows-");
    let (name, bytes) = if windows {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("runner-up.exe", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(&program).unwrap();
        (format!("runner-up-{version}-{platform}.zip"), zip.finish().unwrap().into_inner())
    } else {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(program.len() as u64);
        tar.append_data(&mut header, "runner-up", program.as_slice()).unwrap();
        (format!("runner-up-{version}-{platform}.tar.gz"), tar.into_inner().unwrap().finish().unwrap())
    };
    let archive = dir.join(name);
    std::fs::write(&archive, &bytes).unwrap();

    let mut sums = String::new();
    for p in ["darwin-amd64", "darwin-arm64", "linux-amd64", "linux-arm64", "windows-amd64", "windows-arm64"] {
        let ext = if p.starts_with("windows-") { ".zip" } else { ".tar.gz" };
        let hash = match (p == platform, listed) {
            (true, "-") => continue,
            (true, "") => sha256_hex(&bytes),
            (true, listed) => listed.to_string(),
            (false, _) => sha256_hex(p.as_bytes()),
        };
        sums += &format!("{hash}  runner-up-{version}-{p}{ext}\n");
    }
    let checksums = dir.join("checksums.txt");
    std::fs::write(&checksums, sums).unwrap();
    ReleaseFixture { archive, checksums, program }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
const UNAME: &str = "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 99 ;; esac\n";

#[cfg(unix)]
fn path_with(dirs: &[&Path]) -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(dirs.iter().map(|d| d.to_path_buf()).chain(std::env::split_paths(&inherited))).unwrap()
}

// Exercise the installer through sh, stopping at the asset request: no network,
// archive extraction, installation, or sudo is needed to test version resolution.
#[cfg(unix)]
#[test]
fn install_script_latest_redirect() {
    let cases = [
        ("versioned tag", format!("{RELEASE}tag/v3.1.5"), "v3.1.5", false),
        ("URL suffixes", format!("{RELEASE}tag/v3.1.5/?source=latest#asset"), "v3.1.5", false),
        ("unprefixed tag", format!("{RELEASE}tag/3.1.5"), "3.1.5", false),
        ("empty result", String::new(), "", false),
        ("empty tag", format!("{RELEASE}tag/"), "", false),
        ("no tag redirect", format!("{RELEASE}latest"), "", false),
        ("HTTP failure", format!("{RELEASE}tag/v3.1.5"), "", true),
    ];
    for (name, effective_url, version, curl_failure) in cases {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let log = dir.path().join("curl.log");
        let curl = r#"#!/bin/sh
printf '%s\n' "$*" >> "$CURL_LOG"
if [ "$*" = '-fsSLI -o /dev/null -w %{url_effective} https://github.com/kunchenguid/runner-up/releases/latest' ]; then
  [ "$CURL_FAILURE" = false ] || exit 22
  printf '%s' "$EFFECTIVE_URL"
  exit 0
fi
# Refuse every other request, including the asset: never perform a real install.
exit 42
"#;
        crate::write_programs(&bin, &[("uname", UNAME), ("curl", curl)]);
        let mut cmd = Command::new("sh");
        cmd.arg("docs/install.sh")
            .env("HOME", dir.path())
            .env("TMPDIR", dir.path())
            .env("PATH", path_with(&[&bin]))
            .env("CURL_LOG", &log)
            .env("EFFECTIVE_URL", &effective_url)
            .env("CURL_FAILURE", curl_failure.to_string());
        let (code, output) = run(&mut cmd, Duration::from_secs(60));
        let code = code.unwrap_or_else(|| panic!("{name}: expected controlled refusal, got a signal: {output}"));
        assert_ne!(code, 0, "{name}: expected controlled refusal: {output}");
        let data = std::fs::read_to_string(&log).unwrap();
        let requests: Vec<_> = data.trim().split('\n').collect();
        assert!(!data.contains("api.github.com"), "{name}: must not use the rate-limited API: {data}");
        if version.is_empty() {
            assert!(
                requests.len() == 1 && !output.contains("Downloading"),
                "{name}: unresolved version must not request an asset: {data}\n{output}"
            );
            if !curl_failure {
                assert!(output.contains("Failed to determine latest version"), "{name}: missing empty-version diagnostic: {output}");
            }
            assert_eq!(code, if curl_failure { 22 } else { 1 }, "{name}: exit: {output}");
            continue;
        }
        let asset = format!("{RELEASE}download/{version}/runner-up-v{}-linux-amd64.tar.gz", version.trim_start_matches('v'));
        assert!(
            code == 42 && requests.len() == 2 && requests[1].starts_with(&format!("-fsSL {asset} -o ")),
            "{name}: expected versioned asset request {asset}, got exit {code}:\n{data}\n{output}"
        );
    }
}

// A user installing with docs/install.sh gets only the archive the release's
// checksums.txt lists, byte for byte: a tampered or truncated download, an
// archive the list leaves out, or a missing checksums.txt installs nothing.
// The script runs for real against a stand-in curl serving the files.
#[cfg(unix)]
#[test]
fn install_script_checksum() {
    let other = "ab".repeat(32);
    let cases = [
        ("matching archive", "", "", false),
        ("mismatched archive", other.as_str(), "does not match checksums.txt", false),
        ("unlisted archive", "-", "checksums.txt does not list runner-up-v0.1.10-linux-amd64.tar.gz", false),
        ("no checksums.txt", "", "", true),
    ];
    for (name, listed, refusal, no_checksums) in cases {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let install_dir = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&install_dir).unwrap();
        let mut fixture = release_fixture(dir.path(), "v0.1.10", "linux-amd64", listed);
        if no_checksums {
            fixture.checksums = dir.path().join("missing");
        }
        let curl = format!(
            r#"#!/bin/sh
case "$*" in
  "-fsSLI -o /dev/null -w %{{url_effective}} {RELEASE}latest")
    printf '%s' "{RELEASE}tag/v0.1.10"; exit 0 ;;
  "-fsSL {RELEASE}download/v0.1.10/runner-up-v0.1.10-linux-amd64.tar.gz -o "*)
    cp "$ASSET" "$4"; exit 0 ;;
  "-fsSL {RELEASE}download/v0.1.10/checksums.txt -o "*)
    [ -f "$CHECKSUMS" ] || exit 22
    cp "$CHECKSUMS" "$4"; exit 0 ;;
esac
exit 42
"#
        );
        crate::write_programs(&bin, &[("uname", UNAME), ("curl", &curl)]);
        let mut cmd = Command::new("sh");
        cmd.arg("docs/install.sh")
            .env("HOME", dir.path())
            .env("TMPDIR", dir.path())
            .env("PATH", path_with(&[&bin, &install_dir]))
            .env("ASSET", &fixture.archive)
            .env("CHECKSUMS", &fixture.checksums);
        let (code, output) = run(&mut cmd, Duration::from_secs(60));
        let installed = std::fs::read(install_dir.join("runner-up"));
        if refusal.is_empty() && !no_checksums {
            assert!(
                code == Some(0) && installed.as_ref().is_ok_and(|b| *b == fixture.program),
                "{name}: matching archive not installed: exit {code:?} {installed:?}\n{output}"
            );
            continue;
        }
        assert!(
            code != Some(0) && installed.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "{name}: refused archive installed (exit {code:?}, installed {installed:?}):\n{output}"
        );
        assert!(output.contains(refusal), "{name}: missing {refusal:?} in:\n{output}");
    }
}

// A user installing with docs/install.ps1 gets only the archive the release's
// checksums.txt lists, and the script finds the latest release through the
// releases page's redirect, never the rate-limited REST API. It runs for real
// in Windows PowerShell and PowerShell 7, whichever are present, with a
// stand-in Invoke-WebRequest serving the files in each shell's response
// shape, and stops at extraction so the test never changes the user's PATH.
#[test]
fn install_power_shell_checksum() {
    let shells: Vec<_> = ["powershell", "pwsh"].into_iter().filter_map(crate::look_path).collect();
    if shells.is_empty() {
        eprintln!("no PowerShell");
        return;
    }
    let other = "AB".repeat(32);
    let tagged = format!("{RELEASE}tag/v0.1.10");
    let cases = [
        ("matching archive", "", tagged.as_str(), false, "REACHED EXTRACTION"),
        ("matching archive, PowerShell 7 response", "", tagged.as_str(), true, "REACHED EXTRACTION"),
        ("mismatched archive", other.as_str(), tagged.as_str(), false, "does not match checksums.txt"),
        ("unlisted archive", "-", tagged.as_str(), false, "checksums.txt does not list runner-up-v0.1.10-windows-amd64.zip"),
        ("no tag redirect", "", &format!("{RELEASE}latest"), false, "Failed to determine latest version"),
    ];
    for shell in &shells {
        for (name, listed, effective_url, ps7, want) in &cases {
            let dir = tempfile::tempdir().unwrap();
            let fixture = release_fixture(dir.path(), "v0.1.10", "windows-amd64", listed);
            let log = dir.path().join("requests.log");
            let shape = if *ps7 {
                "@{ RequestMessage = [pscustomobject]@{ RequestUri = [uri]$env:EFFECTIVE_URL } }"
            } else {
                "@{ ResponseUri = [uri]$env:EFFECTIVE_URL }"
            };
            let script = Path::new("docs").join("install.ps1");
            let stubs = format!(
                r#"
function Invoke-WebRequest {{ param($Uri, $Method, $OutFile, [switch]$UseBasicParsing)
  Add-Content -Path $env:REQUEST_LOG -Value "$Method $Uri"
  switch ($Uri) {{
    "{RELEASE}latest" {{ return [pscustomobject]@{{ BaseResponse = [pscustomobject]{shape} }} }}
    "{RELEASE}download/v0.1.10/runner-up-v0.1.10-windows-amd64.zip" {{ Copy-Item $env:ASSET $OutFile; return }}
    "{RELEASE}download/v0.1.10/checksums.txt" {{ Copy-Item $env:CHECKSUMS $OutFile; return }}
  }}
  throw "refused request $Uri"
}}
function Expand-Archive {{ throw "REACHED EXTRACTION" }}
. "{}"
"#,
                script.display()
            );
            let mut cmd = Command::new(shell);
            cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &stubs]);
            // Each shell finds its own modules: a PSModulePath inherited from
            // PowerShell 7 (as on a CI runner) hides Windows PowerShell's.
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().eq_ignore_ascii_case("PSModulePath") {
                    cmd.env_remove(key);
                }
            }
            cmd.env("REQUEST_LOG", &log)
                .env("ASSET", &fixture.archive)
                .env("CHECKSUMS", &fixture.checksums)
                .env("EFFECTIVE_URL", effective_url)
                .env("PROCESSOR_ARCHITECTURE", "AMD64")
                .env("LOCALAPPDATA", dir.path());
            let (code, output) = run(&mut cmd, Duration::from_secs(120));
            let case = format!("{}/{name}", shell.display());
            assert!(code != Some(0) && output.contains(want), "{case}: want failure with {want:?}, got exit {code:?}:\n{output}");
            let data = std::fs::read_to_string(&log).unwrap();
            assert!(!data.contains("api.github.com"), "{case}: must not use the rate-limited API: {data}");
            assert!(!dir.path().join("runner-up").join("runner-up.exe").exists(), "{case}: installed despite stopping");
        }
    }
}
