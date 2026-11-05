use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::Labels;
use crate::ctx::Ctx;
use crate::plan;

/// Gives the GitHub token for one use. The token stays in memory: never in
/// an argument, a log, a file or the environment of a job.
pub type TokenSource = Arc<dyn Fn(&Ctx) -> Result<String, String> + Send + Sync>;

/// Asks the GitHub command line for its sign-in's token, so runner-up has no
/// sign-in of its own. gh reads it from the system credential store, or from
/// GH_TOKEN or its own file where there is none.
pub fn gh_token(gh: &str) -> TokenSource {
    let gh = gh.to_string();
    Arc::new(move |ctx: &Ctx| {
        let mut cmd = Command::new(&gh);
        cmd.args(["auth", "token", "--hostname", "github.com"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "the GitHub command line ({gh}) is not installed: install it from https://cli.github.com and run gh auth login"
                ));
            }
            Err(e) => return Err(format!("the GitHub command line has no sign-in for github.com ({e}): run gh auth login")),
        };
        let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = se.read_to_string(&mut s);
            s
        });
        let mut out = String::new();
        let _ = so.read_to_string(&mut out);
        let res = crate::proc::wait_ctx(ctx, &mut child);
        let stderr = err_reader.join().unwrap_or_default();
        let token = out.trim();
        if res.is_err() || token.is_empty() {
            let msg: String = stderr.trim().chars().take(200).collect();
            return Err(format!("the GitHub command line has no sign-in for github.com ({msg}): run gh auth login"));
        }
        Ok(token.to_string())
    })
}

/// Reads GitHub's REST API with the sign-in's token.
#[derive(Clone)]
pub struct GithubApi {
    pub base: String,
    token: TokenSource,
    agent: ureq::Agent,
}

impl GithubApi {
    pub fn new(token: TokenSource) -> GithubApi {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(30)).redirects(0).build();
        GithubApi { base: "https://api.github.com".into(), token, agent }
    }

    /// Reads path into a T, with the scopes the response's X-OAuth-Scopes
    /// header names.
    fn get<T: DeserializeOwned>(&self, ctx: &Ctx, path: &str) -> Result<(T, String), String> {
        let token = (self.token)(ctx)?;
        let mut req = self
            .agent
            .get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        if let Some(left) = ctx.remaining() {
            req = req.timeout(left);
        }
        let resp = match req.call() {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(_) => return Err("GitHub unreachable".into()),
        };
        if resp.status() != 200 {
            // GitHub's header names the permissions the endpoint accepts;
            // quote it rather than guess.
            let want = accepted_permissions(resp.header("X-Accepted-GitHub-Permissions").unwrap_or(""));
            if !want.is_empty() {
                return Err(format!("GET {path} returned HTTP {}; the sign-in needs {want}", resp.status()));
            }
            return Err(format!("GET {path} returned HTTP {}", resp.status()));
        }
        let scopes = resp.header("X-OAuth-Scopes").unwrap_or("").to_string();
        let mut body = Vec::new();
        let read = resp.into_reader().take(4 << 20).read_to_end(&mut body);
        match read.ok().and_then(|_| serde_json::from_slice(&body).ok()) {
            Some(v) => Ok((v, scopes)),
            None => Err(format!("GET {path} returned an unreadable answer")),
        }
    }

    /// Reads the labels of every open pull request whose head is in repo
    /// itself, by each ref its jobs run on: the pull request's merge and head
    /// refs and its head branch. A fork's pull request earns nothing.
    pub fn pull_labels(&self, ctx: &Ctx, repo: &str) -> Result<Labels, String> {
        #[derive(Deserialize)]
        struct Pull {
            #[serde(default)]
            number: i64,
            #[serde(default)]
            state: String,
            #[serde(default)]
            labels: Vec<Name>,
            #[serde(default)]
            head: Head,
        }
        #[derive(Deserialize)]
        struct Name {
            #[serde(default)]
            name: String,
        }
        #[derive(Deserialize, Default)]
        struct Head {
            #[serde(default, rename = "ref")]
            reference: String,
            #[serde(default)]
            repo: Option<FullName>,
        }
        let mut out: Labels = BTreeMap::new();
        for page in 1..=10 {
            let (pulls, _): (Vec<Pull>, _) = self.get(ctx, &format!("/repos/{repo}/pulls?state=open&per_page=100&page={page}"))?;
            for p in &pulls {
                let head_repo = p.head.repo.as_ref().map_or("", |r| r.full_name.as_str());
                if p.state != "open" || head_repo != repo || p.number <= 0 {
                    continue;
                }
                let pull = format!("refs/pull/{}", p.number);
                for l in &p.labels {
                    out.entry(format!("{pull}/merge")).or_default().insert(l.name.clone(), true);
                    out.entry(format!("{pull}/head")).or_default().insert(l.name.clone(), true);
                    if !p.head.reference.is_empty() {
                        out.entry(format!("refs/heads/{}", p.head.reference)).or_default().insert(l.name.clone(), true);
                    }
                }
            }
            if pulls.len() < 100 {
                return Ok(out);
            }
        }
        Err(format!("pull requests of {repo} exceeded ten pages"))
    }

    /// Reads a workflow run's top-level workflow file, such as
    /// .github/workflows/release.yml, checking it is the run asked for.
    pub fn run_workflow(&self, ctx: &Ctx, repo: &str, id: i64) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Run {
            #[serde(default)]
            id: i64,
            #[serde(default)]
            path: String,
            #[serde(default)]
            repository: Option<FullName>,
        }
        let (run, _): (Run, _) = self.get(ctx, &format!("/repos/{repo}/actions/runs/{id}"))?;
        let p = plan::workflow_path(&run.path);
        let file = p.strip_prefix(".github/workflows/").unwrap_or(p);
        let full_name = run.repository.as_ref().map_or("", |r| r.full_name.as_str());
        if run.id != id
            || full_name != repo
            || file == p
            || file.is_empty()
            || file.contains('/')
            || !(file.ends_with(".yml") || file.ends_with(".yaml"))
        {
            return Err(format!("run {id} of {repo} has no matching workflow file"));
        }
        Ok(p.to_string())
    }

    /// Reads whether GitHub has closed a workflow run, checking it is the run
    /// asked for: a run cancelled or finished is completed, and so is every
    /// job of it, until the run is run again.
    pub fn run_closed(&self, ctx: &Ctx, repo: &str, id: i64) -> Result<bool, String> {
        #[derive(Deserialize)]
        struct Run {
            #[serde(default)]
            id: i64,
            #[serde(default)]
            status: String,
            #[serde(default)]
            repository: Option<FullName>,
        }
        let (run, _): (Run, _) = self.get(ctx, &format!("/repos/{repo}/actions/runs/{id}"))?;
        let full_name = run.repository.as_ref().map_or("", |r| r.full_name.as_str());
        if run.id != id || full_name != repo || run.status.is_empty() {
            return Err(format!("run {id} of {repo} has no status"));
        }
        Ok(run.status == "completed")
    }

    /// Reads who the token signs in as and the scopes GitHub granted it.
    pub fn sign_in(&self, ctx: &Ctx) -> Result<(String, Vec<String>), String> {
        #[derive(Deserialize)]
        struct User {
            #[serde(default)]
            login: String,
        }
        let (user, scopes): (User, _) = self.get(ctx, "/user")?;
        let scopes = scopes.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        Ok((user.login, scopes))
    }

    /// Whether the sign-in may administer repo, which creating its runners
    /// needs.
    pub fn repo_admin(&self, ctx: &Ctx, repo: &str) -> Result<bool, String> {
        #[derive(Deserialize, Default)]
        struct Permissions {
            #[serde(default)]
            admin: bool,
        }
        #[derive(Deserialize)]
        struct Repo {
            #[serde(default)]
            permissions: Permissions,
        }
        let (r, _): (Repo, _) = self.get(ctx, &format!("/repos/{repo}"))?;
        Ok(r.permissions.admin)
    }
}

#[derive(Deserialize)]
struct FullName {
    #[serde(default)]
    full_name: String,
}

/// Reads GitHub's X-Accepted-GitHub-Permissions header, such as
/// "pull_requests=read", keeping only permission-name characters so a
/// response cannot write arbitrary text into the log.
fn accepted_permissions(v: &str) -> String {
    let v = v.trim();
    if v.is_empty() || v.len() > 200 {
        return String::new();
    }
    if !v.chars().all(|r| r.is_ascii_lowercase() || "_=,; ".contains(r)) {
        return String::new();
    }
    v.to_string()
}
