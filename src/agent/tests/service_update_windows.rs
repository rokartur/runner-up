use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine;

use crate::agent::jobhost::{SERVICE_UPDATING_TO, ServiceUpdate};
use crate::agent::service::SERVICE_NAME;
use crate::release::{archive_name, platform, program_name, sha256};

/// Runs program with args and stdin, and returns its output and whether it
/// exited 0.
fn run(stdin: &[u8], program: &Path, args: &[&str]) -> (String, bool) {
    let mut c = Command::new(program);
    c.args(args);
    let (code, out) = super::run(&mut c, stdin);
    (out, code == 0)
}

fn powershell(script: &str) -> String {
    let (code, out) = super::run(Command::new("powershell.exe").args(["-NoProfile", "-Command", script]), "");
    assert_eq!(code, 0, "powershell {script}: {out}");
    out.trim().to_string()
}

/// The service's program and process.
fn service() -> (String, String) {
    let out =
        powershell(&format!("$s = Get-CimInstance Win32_Service -Filter \"Name='{SERVICE_NAME}'\"; $s.PathName + '|' + $s.ProcessId"));
    let (bin, pid) = out.split_once('|').unwrap();
    (bin.to_string(), pid.to_string())
}

fn installed(program: &Path, version: &str) -> String {
    let b = std::fs::read(program).unwrap();
    let folder = PathBuf::from(std::env::var("ProgramFiles").unwrap()).join("runner-up").join(format!("{version}-{}", &sha256(&b)[..12]));
    format!("\"{}\" service", folder.join("runner-up.exe").display())
}

fn answers(older: &Path, version: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (out, ok) = run(b"", older, &["agent", "service-version"]);
        if ok && out.trim() == version {
            return;
        }
        if Instant::now() > deadline {
            let log = std::fs::read_to_string(PathBuf::from(std::env::var("ProgramData").unwrap()).join("runner-up/service.log"))
                .unwrap_or_default();
            panic!("the service answers {out:?} ({ok}), want {version}\nservice.log:\n{log}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The test program in a folder named for version, which makes it that
/// version (version.rs).
fn build(dir: &Path, version: &str) -> PathBuf {
    let path = dir.join(version).join("runner-up.exe");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::copy(crate::cmd::tests::runner_up(), &path).unwrap();
    path
}

fn signing_key() -> ed25519_dalek::SigningKey {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).unwrap();
    ed25519_dalek::SigningKey::from_bytes(&b)
}

struct RemoveService;

impl Drop for RemoveService {
    fn drop(&mut self) {
        let _ = Command::new("sc.exe").args(["stop", SERVICE_NAME]).output();
        let _ = Command::new("sc.exe").args(["delete", SERVICE_NAME]).output();
    }
}

// Intent (README, "Windows machines" and "Updating runner-up"): runner-up setup
// installs the service from a folder of its version and starts it; run again
// with the same program, it leaves the running service as it is. Handed a newer
// release through the agent, as the coordinator hands it, the service refuses
// one the key it trusts did not sign, and installs a signed one, restarts as it
// and answers with its version. An administrator's setup then installs the
// version before again.
//
// It installs runner-up's service on this machine, so it runs only on a
// GitHub-hosted runner, which is thrown away after the run.
#[test]
fn the_service_sets_up_once_and_updates_itself_from_a_signed_release() {
    if std::env::var("RUNNER_ENVIRONMENT").as_deref() != Ok("github-hosted") {
        eprintln!("installs runner-up's service on this machine; runs only on a GitHub-hosted runner");
        return;
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let private = signing_key();
    let tmp = crate::cmd::tests::temp_dir();
    let (older, newer) = (build(tmp.path(), "v9.0.1"), build(tmp.path(), "v9.0.2"));
    let user = std::env::var("USERNAME").unwrap();
    let _remove = RemoveService;

    let (out, ok) = run(b"", &older, &["setup", "--user", &user]);
    assert!(ok && out.contains("runner-up service running v9.0.1."), "setup failed:\n{out}");
    // Go builds the trusted key into the program; the service, started by
    // Windows, gets the test's key from its service's environment instead.
    let key = b64.encode(private.verifying_key().as_bytes());
    powershell(&format!(
        "Set-ItemProperty -Path HKLM:\\SYSTEM\\CurrentControlSet\\Services\\{SERVICE_NAME} -Name Environment -Type MultiString -Value 'RUNNER_UP_TEST_SIGNING_KEYS={key}'; Restart-Service -Name {SERVICE_NAME}"
    ));
    let (bin, pid) = service();
    assert_eq!(bin, installed(&older, "v9.0.1"), "the service runs");
    answers(&older, "v9.0.1");
    let (out, ok) = run(b"", &older, &["setup", "--user", &user]);
    assert!(ok && out.contains("runner-up service already runs v9.0.1, unchanged."), "setup again failed:\n{out}");
    assert_eq!(service(), (bin.clone(), pid), "setup with the same program restarted the service");

    // The release as the release workflow makes it: the program in a zip,
    // listed in checksums.txt, which the private key signs.
    let platform = platform("windows", crate::release::goarch());
    let name = archive_name("v9.0.2", &platform);
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zw.start_file(program_name(&platform), zip::write::SimpleFileOptions::default()).unwrap();
    zw.write_all(&std::fs::read(&newer).unwrap()).unwrap();
    let archive = zw.finish().unwrap().into_inner();
    let checksums = format!("{}  {name}\n", sha256(&archive));
    let hand = |sig: ed25519_dalek::Signature| {
        let head = ServiceUpdate {
            archive: name.clone(),
            size: archive.len() as i64,
            checksums: checksums.clone(),
            signature: b64.encode(sig.to_bytes()),
        };
        let mut input = serde_json::to_vec(&head).unwrap();
        input.push(b'\n');
        input.extend_from_slice(&archive);
        run(&input, &older, &["agent", "update-service"])
    };
    use ed25519_dalek::Signer;
    // the_service_installs_only_a_newer_release_the_signing_key_signed owns the
    // refusal's words; here the service refuses and keeps its program.
    let (out, ok) = hand(signing_key().sign(checksums.as_bytes()));
    assert!(!ok, "the service took a release another key signed:\n{out}");
    answers(&older, "v9.0.1");
    assert_eq!(service().0, bin, "after a release another key signed the service runs");
    let (out, ok) = hand(private.sign(checksums.as_bytes()));
    assert!(ok && out.contains(&format!("{SERVICE_UPDATING_TO}v9.0.2")), "the service did not take the signed release:\n{out}");
    answers(&older, "v9.0.2");
    assert_eq!(service().0, installed(&newer, "v9.0.2"), "the service runs");

    // An administrator installs any version.
    let (out, ok) = run(b"", &older, &["setup", "--user", &user]);
    assert!(ok, "setup with the version before failed:\n{out}");
    answers(&older, "v9.0.1");
    assert_eq!(service().0, bin, "after setup with the version before the service runs");
}
