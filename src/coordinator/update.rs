// Once the coordinator runs the version the setup's release names, it copies
// that release to every machine it reaches over SSH, as the machine's agent:
// it reads the machine's platform, takes the platform's archive (checked
// against the setup's SHA-256), sends the program over SSH into a folder of
// its version - .local/share/runner-up/<version> in the machine's home, or
// %LOCALAPPDATA%\runner-up\<version> on Windows - where the machine checks
// its SHA-256 before keeping it, and asks it its version. From then on that
// machine's jobs run with it; jobs already running keep the agent they
// started with, in its own version's folder. The coordinator's own machine
// runs the coordinator's program as its agent. agents.json under the state
// folder records each machine's copy; a machine that cannot be reached keeps
// its earlier agent and is tried again later.
//
// A Windows machine with account places also runs runner-up's service, as
// SYSTEM, which only an administrator could replace. It replaces itself, but
// only with a release the release signing key signed (release.rs): the
// coordinator holds the machine's account places from new jobs, waits for
// their jobs to end, and has the machine's agent hand the service the
// release's Windows archive with checksums.txt and its signature. The service
// checks them, installs the program in a folder of its own version, and
// restarts as it; a new service that stops within a minute goes back to the
// one before. Once the service answers with the new version, the places take
// jobs again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::place::{agent_command, ssh_args};
use super::{Event, Program, Service};
use crate::config::{self, Config, Machine, PROVIDER_ACCOUNT, Release};
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::go::time::Time;
use crate::{agent, lock, proc, quote, release};

/// How long a machine whose copy or service update failed waits before the
/// coordinator tries it again.
const AGENT_RETRY: Duration = Duration::from_secs(10 * 60);

/// How long the coordinator waits for a machine's service to answer with its
/// new version after handing it the release.
const SERVICE_WAIT: Duration = Duration::from_secs(3 * 60);

/// A machine's copy of a release.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct InstalledAgent {
    #[serde(default)]
    version: String,
    #[serde(default)]
    path: String,
    /// The version the machine's runner-up service answered with last, for a
    /// Windows machine with account places.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    service: String,
}

#[derive(Default)]
struct UpdaterState {
    agents: BTreeMap<String, InstalledAgent>,
    working: HashSet<String>,
    retry: HashMap<String, Time>,
    /// The release last asked of Program::want, and ask_again when it is
    /// asked again after a failure.
    asked: String,
    ask_again: Time,
    wanting: bool,
}

/// Keeps the coordinator, its machines' agents and their Windows services on
/// the setup's release. It says on events that a machine's agent changed
/// (Event::Changed: the setup is to be applied again) or that a new version
/// is ready to restart into (Event::Ready).
pub struct Updater {
    pub program: Program,
    log: Logger,
    /// The place runners' program, which the coordinator's own machine runs
    /// as its agent.
    self_path: String,
    /// The coordinator's, whose account places a service update holds.
    pub service: OnceLock<Arc<Service>>,
    events: Sender<Event>,
    pub agent_retry: Duration,
    pub service_wait: Duration,
    mu: Mutex<UpdaterState>,
}

impl Updater {
    pub fn new(program: Program, self_path: &str, log: Logger, events: Sender<Event>) -> Updater {
        let u = Updater {
            program,
            log,
            self_path: self_path.into(),
            service: OnceLock::new(),
            events,
            agent_retry: AGENT_RETRY,
            service_wait: SERVICE_WAIT,
            mu: Mutex::default(),
        };
        u.mu.lock().unwrap().agents = config::read_json(&u.agents_path()).unwrap_or_default();
        u
    }

    fn agents_path(&self) -> PathBuf {
        self.program.dir.parent().unwrap().join("agents.json")
    }

    /// base with each machine's agent the release's: the coordinator's
    /// program on its own machine, and each other machine's recorded copy. A
    /// setup that names no release keeps its agents.
    pub fn setup(&self, base: &Config) -> Config {
        let mut out = base.clone();
        if base.release.is_none() {
            return out;
        }
        let st = self.mu.lock().unwrap();
        for m in &mut out.machines {
            if m.local() {
                m.agent = self.self_path.clone();
            } else if let Some(a) = st.agents.get(&m.name) {
                m.agent = a.path.clone();
            }
        }
        out
    }

    /// Starts whatever the release still needs, each in the background: the
    /// coordinator's own new version, then each machine's agent and service.
    pub fn tick(self: &Arc<Self>, ctx: &Ctx, base: &Config) {
        let Some(rel) = base.release.clone() else {
            return;
        };
        let mut st = self.mu.lock().unwrap();
        let sum = rel.sha256.get(&release::this_platform()).cloned().unwrap_or_default();
        let key = format!("{} {sum} {}", rel.version, rel.url);
        let now = Time::now();
        if !st.wanting && (key != st.asked || !st.ask_again.is_zero() && now.after(st.ask_again)) {
            st.asked = key;
            st.ask_again = Time::default();
            st.wanting = true;
            let (u, ctx, rel) = (self.clone(), ctx.clone(), rel.clone());
            std::thread::spawn(move || u.want(&ctx, &rel));
        }
        if rel.version != self.program.running {
            return;
        }
        for m in &base.machines {
            if m.local() || st.working.contains(&m.name) || st.retry.get(&m.name).is_some_and(|t| now.before(*t)) {
                continue;
            }
            let a = st.agents.get(&m.name).cloned().unwrap_or_default();
            let copy = a.version != rel.version;
            if !copy && !(m.windows() && has_account_places(m) && a.service != rel.version) {
                continue;
            }
            st.working.insert(m.name.clone());
            let (u, ctx, rel, m) = (self.clone(), ctx.clone(), rel.clone(), m.clone());
            std::thread::spawn(move || {
                let res = if copy { u.copy_agent(&ctx, &m, &rel) } else { u.update_service(&ctx, &m, &rel) };
                let mut st = u.mu.lock().unwrap();
                st.working.remove(&m.name);
                if let Err(e) = res
                    && !ctx.is_done()
                {
                    st.retry.insert(m.name.clone(), Time::now().add(u.agent_retry));
                    let retry = crate::go::duration::format(u.agent_retry);
                    u.log.error(
                        "could not bring the machine to the setup's release; it keeps what it runs and is tried again later",
                        &[("machine", &m.name), ("version", &rel.version), ("retry", &retry), ("err", &e)],
                    );
                }
            });
        }
    }

    fn want(&self, ctx: &Ctx, rel: &Release) {
        let res = self.program.want(ctx, Some(rel));
        {
            let mut st = self.mu.lock().unwrap();
            st.wanting = false;
            match &res {
                Err(e) if e.starts_with("runner-up refused ") => self.log.warn(
                    "the setup names a version runner-up refused; it stays on its version",
                    &[("version", &rel.version), ("running", &self.program.running), ("err", e)],
                ),
                Err(e) if !ctx.is_done() => {
                    st.ask_again = Time::now().add(self.agent_retry);
                    let retry = crate::go::duration::format(self.agent_retry);
                    self.log.error(
                        "could not ready the setup's release; runner-up stays on its version and tries again later",
                        &[("version", &rel.version), ("running", &self.program.running), ("retry", &retry), ("err", e)],
                    );
                }
                _ => {}
            }
        }
        if let Ok(Some(r)) = res
            && !ctx.is_done()
        {
            let _ = self.events.send(Event::Ready(r));
        }
    }

    /// Keeps machine name's copy and says the setup is to be applied again.
    fn record(&self, name: &str, change: impl FnOnce(&mut InstalledAgent)) {
        {
            let mut st = self.mu.lock().unwrap();
            change(st.agents.entry(name.to_string()).or_default());
            if let Err(e) = config::write_json(&self.agents_path(), &st.agents) {
                self.log.error("could not record the machine's agent", &[("machine", &name), ("err", &e)]);
            }
        }
        let _ = self.events.send(Event::Changed);
    }

    /// Copies rel to machine m and checks it runs there.
    fn copy_agent(&self, ctx: &Ctx, m: &Machine, rel: &Release) -> Result<(), String> {
        let c = ctx.with_timeout(Duration::from_secs(15 * 60));
        let platform = machine_platform(&c, m)?;
        let archive = self.program.archive(&c, rel, &platform)?;
        let prog = release::program(&archive, &platform)?;
        let path = send_agent(&c, m, &rel.version, prog)?;
        let mut try_m = m.clone();
        try_m.agent = path.clone();
        let out = run(&c, &agent_command(&try_m, &["version".into()]), Vec::new());
        if let Err(e) = out.status {
            return Err(format!("the copy at {path} does not run: {}", agent::first_line(&String::from_utf8_lossy(&out.stderr), &e)));
        }
        let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if got != rel.version {
            return Err(format!("the copy at {path} says it is {}", crate::go::quote(&got)));
        }
        self.record(&m.name, |a| {
            a.version = rel.version.clone();
            a.path = path.clone();
        });
        self.log.info(
            "copied runner-up to the machine as its agent",
            &[("machine", &m.name), ("version", &rel.version), ("platform", &platform), ("path", &path)],
        );
        Ok(())
    }

    /// Brings Windows machine m's runner-up service to rel.
    fn update_service(&self, ctx: &Ctx, m: &Machine, rel: &Release) -> Result<(), String> {
        let running = service_version(ctx, m)?;
        if running == rel.version {
            self.record(&m.name, |a| a.service = running);
            return Ok(());
        }
        if release::compare(&running, &rel.version).is_gt() {
            return Err(format!(
                "its service runs {running}, newer than the setup's {}, and never goes back to an older version",
                rel.version
            ));
        }
        let platform = machine_platform(ctx, m)?;
        let signed = self.signed_archive(ctx, rel, &platform)?;
        let ids: Vec<String> = m.places.iter().filter(|p| p.provider == PROVIDER_ACCOUNT).map(|p| p.id.clone()).collect();
        let svc = self.service.get().expect("the updater's service is set before it ticks");
        svc.hold(&ids, &format!("runner-up's service on the machine is updating to {}", rel.version));
        let res = self.hand_over(ctx, m, rel, &running, &ids, &signed);
        svc.unhold(&ids);
        res
    }

    /// Waits for the held account places' jobs to end, hands the service the
    /// release and waits for it to answer as the new version.
    fn hand_over(
        &self,
        ctx: &Ctx,
        m: &Machine,
        rel: &Release,
        running: &str,
        ids: &[String],
        signed: &release::Signed,
    ) -> Result<(), String> {
        self.log.info(
            "waiting for the machine's account jobs to end to update its service",
            &[("machine", &m.name), ("service", &running), ("version", &rel.version)],
        );
        let state_dir = self.service.get().unwrap().setup().state_dir.clone();
        while ids
            .iter()
            .any(|id| lock::held_by(&std::path::Path::new(&state_dir).join("places").join(id).join("busy.lock")).unwrap_or(true))
        {
            if ctx.sleep(Duration::from_secs(5)) {
                return Err(ctx.err().unwrap_or("context canceled").to_string());
            }
        }
        hand_service(ctx, m, signed)?;
        let deadline = Time::now().add(self.service_wait);
        loop {
            let got = service_version(ctx, m);
            if let Ok(v) = &got
                && *v == rel.version
            {
                self.record(&m.name, |a| a.service = v.clone());
                self.log.info("the machine's service updated itself", &[("machine", &m.name), ("version", v)]);
                return Ok(());
            }
            if Time::now().after(deadline) {
                let (last, err) = match got {
                    Ok(v) => (v, "<nil>".to_string()),
                    Err(e) => (String::new(), e),
                };
                return Err(format!(
                    "its service did not answer as {} within {} (last: {}, {err}); see service.log on the machine",
                    rel.version,
                    crate::go::duration::format(self.service_wait),
                    crate::go::quote(&last)
                ));
            }
            if ctx.sleep(Duration::from_secs(5)) {
                return Err(ctx.err().unwrap_or("context canceled").to_string());
            }
        }
    }

    /// rel's archive for platform with its signed checksums, checked here as
    /// the service will check them.
    fn signed_archive(&self, ctx: &Ctx, rel: &Release, platform: &str) -> Result<release::Signed, String> {
        let archive = self.program.archive(ctx, rel, platform)?;
        let checksums = release::fetch(&rel.archive_url("checksums.txt"), 1 << 20)?;
        let signature =
            release::fetch(&rel.archive_url("checksums.txt.sig"), 1 << 10).map_err(|e| format!("the release has no signature: {e}"))?;
        let s = release::Signed { name: release::archive_name(&rel.version, platform), archive, checksums, signature };
        s.verify(platform, &release::signing_keys()).map_err(|e| format!("its service accepts only a signed release: {e}"))?;
        Ok(s)
    }
}

fn has_account_places(m: &Machine) -> bool {
    m.places.iter().any(|p| p.provider == PROVIDER_ACCOUNT)
}

fn run(ctx: &Ctx, argv: &[String], input: Vec<u8>) -> proc::Output {
    proc::output(ctx, Command::new(&argv[0]).args(&argv[1..]), input)
}

/// Reads machine m's platform, as a release names it.
fn machine_platform(ctx: &Ctx, m: &Machine) -> Result<String, String> {
    let mut line = "uname -sm".to_string();
    if m.windows() {
        line = powershell_line("[Console]::Out.WriteLine($env:PROCESSOR_ARCHITECTURE)");
    }
    let mut argv = ssh_args(m);
    argv.push(line);
    let out = run(ctx, &argv, Vec::new());
    if let Err(e) = out.status {
        return Err(format!("could not read the machine's platform: {}", agent::first_line(&String::from_utf8_lossy(&out.stderr), &e)));
    }
    let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let platform = match got.as_str() {
        "Darwin arm64" => "darwin-arm64",
        "Darwin x86_64" => "darwin-amd64",
        "Linux x86_64" => "linux-amd64",
        "Linux aarch64" | "Linux arm64" => "linux-arm64",
        "AMD64" => "windows-amd64",
        "ARM64" => "windows-arm64",
        _ => return Err(format!("the machine runs {}, which no runner-up release is built for", crate::go::quote(&got))),
    };
    Ok(platform.into())
}

/// A command line that runs script in Windows PowerShell, read the same by
/// cmd and PowerShell: the script travels base64 encoded.
fn powershell_line(script: &str) -> String {
    agent::encoded_script(script).join(" ")
}

/// Sends prog, version's program, to machine m over SSH, where it is kept
/// only when its SHA-256 matches, and returns its path there.
fn send_agent(ctx: &Ctx, m: &Machine, version: &str, prog: Vec<u8>) -> Result<String, String> {
    let (line, mut path) = agent_copy(m, version, &release::sha256(&prog));
    let mut argv = ssh_args(m);
    argv.push(line);
    let out = run(ctx, &argv, prog);
    if let Err(e) = out.status {
        return Err(format!(
            "could not copy runner-up {version} to the machine: {}",
            agent::first_line(&String::from_utf8_lossy(&out.stderr), &e)
        ));
    }
    if m.windows() {
        path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        quote::check_win_word(&path, true).map_err(|e| format!("the copy's path {e}"))?;
    }
    Ok(path)
}

/// The command line that keeps version's program, whose SHA-256 is sum, from
/// standard input on machine m, and the program's path there, which a
/// Windows machine prints instead.
pub(super) fn agent_copy(m: &Machine, version: &str, sum: &str) -> (String, String) {
    if m.windows() {
        return (powershell_line(&WINDOWS_AGENT_COPY.replace("@VERSION@", version).replace("@SHA256@", sum)), String::new());
    }
    let dir = format!(".local/share/runner-up/{version}");
    let script = UNIX_AGENT_COPY.replace("@DIR@", &quote::sh_quote(&dir)).replace("@SHA256@", sum);
    (format!("sh -c {}", quote::sh_quote(&script)), format!("{dir}/runner-up"))
}

/// Keeps standard input as the program in @DIR@, once its SHA-256 is
/// @SHA256@. The program's own folder holds only it, so jobs of an earlier
/// version keep theirs.
const UNIX_AGENT_COPY: &str = r#"set -e
mkdir -p @DIR@
cd @DIR@
cat > runner-up.new
if command -v sha256sum >/dev/null 2>&1; then sum=$(sha256sum < runner-up.new); else sum=$(shasum -a 256 < runner-up.new); fi
case "$sum" in
@SHA256@*) ;;
*) rm -f runner-up.new; echo "the copy's SHA-256 is not the release's" >&2; exit 1 ;;
esac
chmod 0755 runner-up.new
mv -f runner-up.new runner-up"#;

/// UNIX_AGENT_COPY for Windows PowerShell, in the runner account's
/// %LOCALAPPDATA%. It prints the program's path. A program of the same
/// version already there may be running jobs, and stays. It hashes with
/// .NET, not Get-FileHash, which Windows PowerShell fails to load when it
/// inherits PowerShell 7's module path.
const WINDOWS_AGENT_COPY: &str = r#"$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:LOCALAPPDATA 'runner-up\@VERSION@'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$new = Join-Path $dir 'runner-up.exe.new'
$exe = Join-Path $dir 'runner-up.exe'
$in = [Console]::OpenStandardInput()
$out = [IO.File]::Create($new)
try { $in.CopyTo($out) } finally { $out.Close() }
function Get-Sum([string]$path) {
    $f = [IO.File]::OpenRead($path)
    try { [BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($f)).Replace('-', '').ToLowerInvariant() } finally { $f.Close() }
}
$sum = Get-Sum $new
if ($sum -ne '@SHA256@') { Remove-Item -LiteralPath $new; throw "the copy's SHA-256 is $sum, not the release's" }
if ((Test-Path -LiteralPath $exe) -and (Get-Sum $exe) -eq $sum) {
    Remove-Item -LiteralPath $new
} else {
    Move-Item -Force -LiteralPath $new -Destination $exe
}
[Console]::Out.WriteLine($exe)"#;

/// Asks machine m's agent the version of its runner-up service.
fn service_version(ctx: &Ctx, m: &Machine) -> Result<String, String> {
    let c = ctx.with_timeout(Duration::from_secs(60));
    let out = run(&c, &agent_command(m, &["agent".into(), "service-version".into()]), Vec::new());
    if let Err(e) = out.status {
        return Err(format!("could not read its service's version: {}", agent::first_line(&String::from_utf8_lossy(&out.stderr), &e)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Hands machine m's service the signed release through its agent.
fn hand_service(ctx: &Ctx, m: &Machine, s: &release::Signed) -> Result<(), String> {
    use base64::Engine;
    let c = ctx.with_timeout(Duration::from_secs(10 * 60));
    let head = agent::ServiceUpdate {
        archive: s.name.clone(),
        size: s.archive.len() as i64,
        checksums: String::from_utf8_lossy(&s.checksums).into_owned(),
        signature: base64::engine::general_purpose::STANDARD.encode(&s.signature),
    };
    let mut input = serde_json::to_vec(&head).unwrap();
    input.push(b'\n');
    input.extend_from_slice(&s.archive);
    let argv = agent_command(m, &["agent".into(), "update-service".into()]);
    let (out, res) = proc::combined_output_with(&c, Command::new(&argv[0]).args(&argv[1..]), input);
    if let Err(e) = res {
        // The service's own answer is the last thing the agent says.
        let text = String::from_utf8_lossy(&out);
        let last = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
        return Err(format!("its service did not take the release: {}", agent::first_line(last, &e)));
    }
    Ok(())
}
