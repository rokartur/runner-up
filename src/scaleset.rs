//! The client for GitHub's Actions service scale set API, the part of
//! github.com/actions/scaleset v0.5.0 runner-up uses: a repository's or
//! organization's scale sets, their message sessions, just-in-time runner
//! configurations and runner removal.

use crate::ctx::Ctx;
use crate::go::time::Time;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;

const RUNNER_ENDPOINT: &str = "_apis/distributedtask/pools/0/agents";
const SCALE_SET_ENDPOINT: &str = "_apis/runtime/runnerscalesets";
pub const HEADER_SCALE_SET_MAX_CAPACITY: &str = "X-ScaleSetMaxCapacity";

/// The longest one request attempt may take, and one message poll.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MESSAGE_POLL_TIMEOUT: Duration = Duration::from_secs(2 * 60);

const RETRY_WAIT_MIN: Duration = Duration::from_secs(1);
const RETRY_WAIT_MAX: Duration = Duration::from_secs(30);
pub const DEFAULT_RETRY_MAX: u32 = 4;

pub const MESSAGE_TYPE_JOB_AVAILABLE: &str = "JobAvailable";
pub const MESSAGE_TYPE_JOB_ASSIGNED: &str = "JobAssigned";
pub const MESSAGE_TYPE_JOB_STARTED: &str = "JobStarted";
pub const MESSAGE_TYPE_JOB_COMPLETED: &str = "JobCompleted";

/// What a scale set error means, as the scaleset package's sentinel errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    RunnerNotFound,
    RunnerExists,
    JobStillRunning,
    MessageQueueTokenExpired,
    BadRequest,
    NotFound,
    Unauthorized,
    Conflict,
}

impl Kind {
    fn text(self) -> &'static str {
        match self {
            Kind::RunnerNotFound => "runner not found",
            Kind::RunnerExists => "runner exists",
            Kind::JobStillRunning => "job still running",
            Kind::MessageQueueTokenExpired => "message queue token expired",
            Kind::BadRequest => "bad request",
            Kind::NotFound => "not found",
            Kind::Unauthorized => "unauthorized",
            Kind::Conflict => "conflict",
        }
    }
}

/// An error with the kinds errors.Is would find in its chain.
#[derive(Clone, Debug)]
pub struct Error {
    pub kinds: Vec<Kind>,
    pub msg: String,
}

impl Error {
    pub fn new(msg: impl Into<String>) -> Error {
        Error { kinds: vec![], msg: msg.into() }
    }

    pub fn of(kind: Kind) -> Error {
        Error { kinds: vec![kind], msg: kind.text().into() }
    }

    pub fn is(&self, kind: Kind) -> bool {
        self.kinds.contains(&kind)
    }

    fn context(mut self, prefix: &str) -> Error {
        self.msg = format!("{prefix}: {}", self.msg);
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SystemInfo {
    pub system: String,
    pub version: String,
    pub commit_sha: String,
    pub scale_set_id: i64,
    pub subsystem: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Label {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunnerSetting {
    #[serde(rename = "disableUpdate", default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_update: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Statistics {
    pub total_available_jobs: i64,
    pub total_acquired_jobs: i64,
    pub total_assigned_jobs: i64,
    pub total_running_jobs: i64,
    pub total_registered_runners: i64,
    pub total_busy_runners: i64,
    pub total_idle_runners: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunnerScaleSet {
    #[serde(skip_serializing_if = "is_zero")]
    pub id: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub runner_group_id: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub runner_group_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<Label>,
    #[serde(rename = "RunnerSetting")]
    pub runner_setting: RunnerSetting,
    pub created_on: Time,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub runner_jit_config_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics: Option<Statistics>,
}

/// One job message of any type: JobAvailable, JobAssigned, JobStarted or
/// JobCompleted. The fields a type does not carry stay at their zero values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobMessage {
    pub message_type: String,
    pub runner_request_id: i64,
    pub repository_name: String,
    pub owner_name: String,
    pub job_id: String,
    pub job_workflow_ref: String,
    pub job_display_name: String,
    pub workflow_run_id: i64,
    pub event_name: String,
    pub request_labels: Vec<String>,
    pub queue_time: Time,
    pub scale_set_assign_time: Time,
    pub runner_assign_time: Time,
    pub finish_time: Time,
    /// JobAvailable only.
    pub acquire_job_url: String,
    /// JobStarted and JobCompleted only.
    pub runner_id: i64,
    pub runner_name: String,
    /// JobCompleted only.
    pub result: String,
}

/// One message of a scale set's session, sorted into its job messages. The
/// field names are Go's, which recorded messages use.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Message {
    #[serde(rename = "MessageID")]
    pub message_id: i64,
    #[serde(rename = "Statistics")]
    pub statistics: Option<Statistics>,
    #[serde(rename = "JobAvailableMessages", deserialize_with = "crate::config::null_as_default")]
    pub job_available: Vec<JobMessage>,
    #[serde(rename = "JobAssignedMessages", deserialize_with = "crate::config::null_as_default")]
    pub job_assigned: Vec<JobMessage>,
    #[serde(rename = "JobStartedMessages", deserialize_with = "crate::config::null_as_default")]
    pub job_started: Vec<JobMessage>,
    #[serde(rename = "JobCompletedMessages", deserialize_with = "crate::config::null_as_default")]
    pub job_completed: Vec<JobMessage>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Session {
    pub session_id: String,
    pub owner_name: String,
    pub runner_scale_set: Option<RunnerScaleSet>,
    pub message_queue_url: String,
    pub message_queue_access_token: String,
    pub statistics: Option<Statistics>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RunnerReference {
    pub id: i64,
    pub name: String,
    #[serde(rename = "runnerScaleSetId")]
    pub runner_scale_set_id: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct JitConfig {
    pub runner: Option<RunnerReference>,
    #[serde(rename = "encodedJITConfig")]
    pub encoded_jit_config: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RunnerGroup {
    pub id: i64,
    pub name: String,
    pub size: i64,
    #[serde(rename = "isDefaultGroup")]
    pub is_default: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Enterprise,
    Organization,
    Repository,
}

/// Where a GitHub config URL points: an enterprise, organization or repository.
struct Config {
    url: String,
    scheme: String,
    host: String,
    scope: Scope,
    owner: String,
    repository: String,
}

fn parse_config_url(input: &str) -> Result<Config, String> {
    let trimmed = input.trim_matches('/');
    let (scheme, rest) = trimmed.split_once("://").ok_or_else(|| format!("failed to parse URL: {input:?} has no scheme"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let invalid = || format!("{trimmed:?}: invalid config URL, should point to an enterprise, org, or repository");
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (scope, owner, repository) = match parts.as_slice() {
        [""] => return Err(invalid()),
        [org] => (Scope::Organization, org.to_string(), String::new()),
        [e, name] if e.eq_ignore_ascii_case("enterprises") => (Scope::Enterprise, name.to_string(), String::new()),
        [org, repo] => (Scope::Repository, org.to_string(), repo.to_string()),
        _ => return Err(invalid()),
    };
    Ok(Config { url: trimmed.into(), scheme: scheme.into(), host: host.into(), scope, owner, repository })
}

impl Config {
    fn hosted(&self) -> bool {
        if std::env::var_os("GITHUB_ACTIONS_FORCE_GHES").is_some() {
            return false;
        }
        let h = self.host.to_ascii_lowercase();
        h == "github.com" || h == "www.github.com" || h == "github.localhost" || h.ends_with(".ghe.com")
    }

    fn api_url(&self, path: &str) -> String {
        if !self.hosted() {
            return format!("{}://{}/api/v3{path}", self.scheme, self.host);
        }
        let host = if self.host.eq_ignore_ascii_case("www.github.com") { "api.github.com".into() } else { format!("api.{}", self.host) };
        format!("{}://{host}{path}", self.scheme)
    }

    fn registration_token_path(&self) -> String {
        match self.scope {
            Scope::Organization => format!("/orgs/{}/actions/runners/registration-token", self.owner),
            Scope::Enterprise => format!("/enterprises/{}/actions/runners/registration-token", self.owner),
            Scope::Repository => format!("/repos/{}/{}/actions/runners/registration-token", self.owner, self.repository),
        }
    }
}

#[derive(Clone)]
struct AdminToken {
    token: String,
    expires_at: Time,
    url: String,
}

/// A scale set client for one repository or organization, signed in with a
/// personal access token. It is shared by every thread and session using it.
pub struct Client {
    config: Config,
    token: String,
    retry_max: u32,
    user_agent: Mutex<String>,
    admin: Mutex<Option<AdminToken>>,
    refresh: Mutex<()>,
}

impl Client {
    pub fn with_token(config_url: &str, token: &str, info: SystemInfo) -> Result<Client, Error> {
        let config = parse_config_url(config_url).map_err(|e| Error::new(format!("failed to parse githubConfigURL: {e}")))?;
        if token.is_empty() {
            return Err(Error::new("invalid credentials: either GitHub App credentials or personal access token is required"));
        }
        let c = Client {
            config,
            token: token.into(),
            retry_max: DEFAULT_RETRY_MAX,
            user_agent: Mutex::default(),
            admin: Mutex::default(),
            refresh: Mutex::default(),
        };
        c.set_system_info(info);
        Ok(c)
    }

    /// A client that sends every request once, as WithRetry(RetryConfig{Max: 0}).
    #[cfg(test)]
    pub fn without_retries(mut self) -> Client {
        self.retry_max = 0;
        self
    }

    pub fn set_system_info(&self, info: SystemInfo) {
        #[derive(Serialize)]
        struct UserAgent {
            #[serde(flatten)]
            info: SystemInfo,
            build_version: &'static str,
            build_commit_sha: &'static str,
            kind: &'static str,
        }
        let ua = UserAgent { info, build_version: "v0.5.0", build_commit_sha: "unknown", kind: "scaleset" };
        *self.user_agent.lock().unwrap() = serde_json::to_string(&ua).unwrap();
    }

    fn user_agent(&self) -> String {
        self.user_agent.lock().unwrap().clone()
    }

    pub fn get_runner_group_by_name(&self, ctx: &Ctx, name: &str) -> Result<RunnerGroup, Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct List {
            count: i64,
            value: Vec<RunnerGroup>,
        }
        let req = self.service_request(ctx, "GET", "/_apis/runtime/runnergroups/", &[("groupName", name)], None)?;
        let resp = self.expect(ctx, &req, 200)?;
        let mut list: List = decode(&req, &resp, "failed to decode runner group list")?;
        match list.count {
            1 if !list.value.is_empty() => Ok(list.value.remove(0)),
            0 => Err(response_error(&req, &resp, Error::new(format!("no runner group found with name {}", crate::go::quote(name))))),
            _ => Err(response_error(&req, &resp, Error::new(format!("multiple runner group found with name {}", crate::go::quote(name))))),
        }
    }

    /// The scale set named name in the runner group, or None when there is none.
    pub fn get_runner_scale_set(&self, ctx: &Ctx, group_id: i64, name: &str) -> Result<Option<RunnerScaleSet>, Error> {
        let group = group_id.to_string();
        let req = self.service_request(ctx, "GET", SCALE_SET_ENDPOINT, &[("runnerGroupId", &group), ("name", name)], None)?;
        let resp = self.expect(ctx, &req, 200)?;
        let mut list: ScaleSetList = decode(&req, &resp, "failed to decode runner scale set list")?;
        match list.count {
            1 if !list.value.is_empty() => Ok(Some(list.value.remove(0))),
            0 => Ok(None),
            _ => Err(response_error(
                &req,
                &resp,
                Error::new(format!("multiple runner scale sets found with name {}", crate::go::quote(name))),
            )),
        }
    }

    pub fn create_runner_scale_set(&self, ctx: &Ctx, mut set: RunnerScaleSet) -> Result<RunnerScaleSet, Error> {
        if set.labels.is_empty() {
            if set.name.is_empty() {
                return Err(Error::new("validating runner scale set: runner scale set must have a name or at least one label"));
            }
            set.labels = vec![Label { kind: "System".into(), name: set.name.clone() }];
        }
        for l in &mut set.labels {
            if l.kind.is_empty() {
                l.kind = "System".into();
            }
        }
        let body = serde_json::to_vec(&set).unwrap();
        let req = self.service_request(ctx, "POST", SCALE_SET_ENDPOINT, &[], Some(body))?;
        let resp = self.expect(ctx, &req, 200)?;
        decode(&req, &resp, "failed to decode runner scale set")
    }

    pub fn generate_jit_runner_config(&self, ctx: &Ctx, name: &str, work_folder: &str, scale_set_id: i64) -> Result<JitConfig, Error> {
        let path = format!("/{SCALE_SET_ENDPOINT}/{scale_set_id}/generatejitconfig");
        let body = serde_json::to_vec(&serde_json::json!({"name": name, "workFolder": work_folder})).unwrap();
        let req = self.service_request(ctx, "POST", &path, &[], Some(body))?;
        let resp = self.expect(ctx, &req, 200)?;
        decode(&req, &resp, "failed to decode runner JIT config")
    }

    pub fn remove_runner(&self, ctx: &Ctx, runner_id: i64) -> Result<(), Error> {
        let req = self.service_request(ctx, "DELETE", &format!("/{RUNNER_ENDPOINT}/{runner_id}"), &[], None)?;
        self.expect(ctx, &req, 204).map(drop)
    }

    /// Opens a message session on the scale set, owned by owner.
    pub fn message_session(self: &std::sync::Arc<Self>, ctx: &Ctx, scale_set_id: i64, owner: &str) -> Result<MessageSession, Error> {
        let s = MessageSession { client: self.clone(), scale_set_id, session: Mutex::default(), refresh: Mutex::default() };
        let body =
            serde_json::to_vec(&serde_json::json!({"sessionId": "00000000-0000-0000-0000-000000000000", "ownerName": owner})).unwrap();
        let created: Session = s
            .session_request(ctx, "POST", &format!("/{SCALE_SET_ENDPOINT}/{scale_set_id}/sessions"), Some(body), 200)
            .and_then(|b| parse_body(&b))
            .map_err(|e| e.context("failed to create message session: failed to do the session request"))?;
        *s.session.lock().unwrap() = created;
        Ok(s)
    }

    fn expect(&self, ctx: &Ctx, req: &Request, status: u16) -> Result<Response, Error> {
        let resp = self.send(ctx, req, &[]).map_err(|e| e.context("failed to issue the request"))?;
        if resp.status != status {
            return Err(response_error(req, &resp, Error::new(format!("unexpected status code: {}", resp.status))));
        }
        Ok(resp)
    }

    fn api_request(&self, method: &str, path: &str, body: Vec<u8>) -> Request {
        Request {
            method: method.into(),
            url: self.config.api_url(path),
            headers: vec![("User-Agent".into(), self.user_agent())],
            body: Some(body),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    fn service_request(
        &self,
        ctx: &Ctx,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<Request, Error> {
        let admin = self.admin_token(ctx).map_err(|e| e.context("failed to issue update token if needed"))?;
        let (path, raw_query) = path.split_once('?').unwrap_or((path, ""));
        let mut q: Vec<(String, String)> = parse_query(raw_query);
        q.extend(query.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        if !q.iter().any(|(k, v)| k == "api-version" && !v.is_empty()) {
            q.push(("api-version".into(), "6.0-preview".into()));
        }
        Ok(Request {
            method: method.into(),
            url: format!("{}?{}", join_url_path(&admin.url, path), encode_query(q)),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Authorization".into(), format!("Bearer {}", admin.token)),
                ("User-Agent".into(), self.user_agent()),
            ],
            body,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// The Actions service admin token, fetched afresh when it expires
    /// within a minute.
    fn admin_token(&self, ctx: &Ctx) -> Result<AdminToken, Error> {
        let fresh = |t: &Option<AdminToken>| t.clone().filter(|t| Time::now().add(Duration::from_secs(60)).before(t.expires_at));
        if let Some(t) = fresh(&self.admin.lock().unwrap()) {
            return Ok(t);
        }
        let _refreshing = self.refresh.lock().unwrap();
        if let Some(t) = fresh(&self.admin.lock().unwrap()) {
            return Ok(t);
        }
        let registration = self.registration_token(ctx).map_err(|e| e.context("failed to get runner registration token on refresh"))?;
        let (url, token) = self
            .admin_connection(ctx, &registration)
            .map_err(|e| e.context("failed to get actions service admin connection on refresh"))?;
        let expires_at = jwt_expiry(&token).map_err(|e| Error::new(format!("failed to get admin token expire at on refresh: {e}")))?;
        let t = AdminToken { token, expires_at, url };
        *self.admin.lock().unwrap() = Some(t.clone());
        Ok(t)
    }

    fn registration_token(&self, ctx: &Ctx) -> Result<String, Error> {
        let mut req = self.api_request("POST", &self.config.registration_token_path(), vec![]);
        req.headers.push(("Content-Type".into(), "application/vnd.github.v3+json".into()));
        req.headers.push(("Authorization".into(), format!("Bearer {}", self.token)));
        let resp = self.send(ctx, &req, &[]).map_err(|e| e.context("failed to issue the request"))?;
        if resp.status != 201 {
            return Err(response_error(
                &req,
                &resp,
                Error::new(format!("failed to get runner registration token ({})", resp.status_line())),
            ));
        }
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
        }
        let t: Token = decode(&req, &resp, "failed to decode registration token")?;
        t.token.ok_or_else(|| Error::new("registration token missing"))
    }

    fn admin_connection(&self, ctx: &Ctx, registration: &str) -> Result<(String, String), Error> {
        let body = serde_json::to_vec(&serde_json::json!({"url": self.config.url, "runner_event": "register"})).unwrap();
        let mut req = self.api_request("POST", "/actions/runner-registration", body);
        req.headers.push(("Content-Type".into(), "application/json".into()));
        req.headers.push(("Authorization".into(), format!("RemoteAuth {registration}")));
        let wrap = |e: Error| e.context("failed to get actions service admin connection");
        let resp = self.send(ctx, &req, &[401, 403]).map_err(|e| wrap(e.context("failed to issue the request")))?;
        if !(200..=299).contains(&resp.status) {
            return Err(wrap(response_error(&req, &resp, Error::new(format!("unexpected status code: {}", resp.status)))));
        }
        #[derive(Deserialize)]
        struct Conn {
            url: Option<String>,
            token: Option<String>,
        }
        let c: Conn = decode(&req, &resp, "failed to decode actions service admin connection").map_err(wrap)?;
        let url = c.url.filter(|u| !u.is_empty()).ok_or_else(|| wrap(Error::new("actions service admin connection missing url")))?;
        let token = c.token.filter(|t| !t.is_empty()).ok_or_else(|| wrap(Error::new("actions service admin connection missing token")))?;
        Ok((url, token))
    }

    /// Sends req, retrying transport errors, 429, and 5xx other than 501,
    /// plus the statuses in also, up to the client's retry count.
    fn send(&self, ctx: &Ctx, req: &Request, also: &[u16]) -> Result<Response, Error> {
        let mut attempt = 0;
        loop {
            if let Some(e) = ctx.err() {
                return Err(request_error(req, e));
            }
            let result = req.send_once(ctx);
            let retry = match &result {
                Err(_) => true,
                Ok(r) => r.status == 429 || (r.status >= 500 && r.status != 501) || also.contains(&r.status),
            };
            if !retry || attempt >= self.retry_max {
                return result.map_err(|e| request_error(req, &e));
            }
            let wait = backoff(attempt, result.as_ref().ok());
            if ctx.remaining().is_some_and(|left| wait >= left) {
                return result.map_err(|e| request_error(req, &e));
            }
            if ctx.sleep(wait) {
                return Err(request_error(req, ctx.err().unwrap_or("context canceled")));
            }
            attempt += 1;
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ScaleSetList {
    count: i64,
    value: Vec<RunnerScaleSet>,
}

/// One scale set's message session.
pub struct MessageSession {
    client: std::sync::Arc<Client>,
    scale_set_id: i64,
    session: Mutex<Session>,
    refresh: Mutex<()>,
}

impl MessageSession {
    pub fn session(&self) -> Session {
        self.session.lock().unwrap().clone()
    }

    pub fn close(&self, ctx: &Ctx) -> Result<(), Error> {
        let id = self.session().session_id;
        let path = format!("/{SCALE_SET_ENDPOINT}/{}/sessions/{id}", self.scale_set_id);
        self.session_request(ctx, "DELETE", &path, None, 204).map(drop)
    }

    /// The next message, or None when the poll ended without one.
    pub fn get_message(&self, ctx: &Ctx, last: i64, max_capacity: i64) -> Result<Option<Message>, Error> {
        self.with_refresh(ctx, "failed to get next message", |s| self.poll(ctx, s, last, max_capacity))
    }

    pub fn delete_message(&self, ctx: &Ctx, id: i64) -> Result<(), Error> {
        self.with_refresh(ctx, "failed to delete message", |s| {
            let (path, query) = s.message_queue_url.split_once('?').unwrap_or((&s.message_queue_url, ""));
            let mut url = format!("{path}/{id}");
            if !query.is_empty() {
                url += &format!("?{query}");
            }
            let req = Request {
                method: "DELETE".into(),
                url,
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Authorization".into(), format!("Bearer {}", s.message_queue_access_token)),
                    ("User-Agent".into(), self.client.user_agent()),
                ],
                body: None,
                timeout: DEFAULT_TIMEOUT,
            };
            let resp = self.client.send(ctx, &req, &[]).map_err(|e| e.context("failed to issue the request"))?;
            match resp.status {
                204 => Ok(()),
                401 => Err(response_error(&req, &resp, Error::of(Kind::MessageQueueTokenExpired))),
                _ => Err(response_error(&req, &resp, Error::new(format!("unexpected status code {}", resp.status_line())))),
            }
        })
    }

    /// Asks GitHub to give this scale set the jobs; returns those it got.
    pub fn acquire_jobs(&self, ctx: &Ctx, ids: &[i64]) -> Result<Vec<i64>, Error> {
        self.with_refresh(ctx, "failed to acquire jobs", |s| {
            let path = format!("/{SCALE_SET_ENDPOINT}/{}/acquirejobs", self.scale_set_id);
            let mut req = self
                .client
                .service_request(ctx, "POST", &path, &[], Some(serde_json::to_vec(ids).unwrap()))
                .map_err(|e| e.context("failed to create acquire jobs request"))?;
            set_header(&mut req, "Authorization", format!("Bearer {}", s.message_queue_access_token));
            let resp = self.client.send(ctx, &req, &[]).map_err(|e| e.context("failed to issue acquire jobs request"))?;
            match resp.status {
                401 => return Err(response_error(&req, &resp, Error::of(Kind::MessageQueueTokenExpired))),
                200 => {}
                _ => return Err(response_error(&req, &resp, Error::new(format!("unexpected status code {}", resp.status_line())))),
            }
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct Acquired {
                value: Vec<i64>,
            }
            let a: Acquired = decode(&req, &resp, "failed to decode acquire jobs response")?;
            Ok(a.value)
        })
    }

    /// Runs call once, and again after refreshing the session when its
    /// message queue token expired.
    fn with_refresh<T>(&self, ctx: &Ctx, what: &str, call: impl Fn(&Session) -> Result<T, Error>) -> Result<T, Error> {
        let s = self.session();
        match call(&s) {
            Ok(v) => Ok(v),
            Err(e) if !e.is(Kind::MessageQueueTokenExpired) => Err(e.context(what)),
            Err(_) => {
                self.refresh_session(ctx, &s).map_err(|e| e.context("failed to refresh message session"))?;
                call(&self.session())
            }
        }
    }

    fn refresh_session(&self, ctx: &Ctx, expired: &Session) -> Result<(), Error> {
        let _refreshing = self.refresh.lock().unwrap();
        let s = self.session();
        if s.session_id != expired.session_id || s.message_queue_access_token != expired.message_queue_access_token {
            return Ok(());
        }
        let path = format!("/{SCALE_SET_ENDPOINT}/{}/sessions/{}", self.scale_set_id, s.session_id);
        let body = self.session_request(ctx, "PATCH", &path, None, 200)?;
        *self.session.lock().unwrap() = parse_body(&body)?;
        Ok(())
    }

    fn session_request(&self, ctx: &Ctx, method: &str, path: &str, body: Option<Vec<u8>>, status: u16) -> Result<Vec<u8>, Error> {
        let req = self
            .client
            .service_request(ctx, method, path, &[], body)
            .map_err(|e| e.context("failed to create new actions service request"))?;
        let resp = self.client.send(ctx, &req, &[]).map_err(|e| e.context("failed to issue the request"))?;
        if resp.status != status {
            return Err(response_error(&req, &resp, Error::new(format!("unexpected status code {}", resp.status_line()))));
        }
        Ok(resp.body)
    }

    fn poll(&self, ctx: &Ctx, s: &Session, last: i64, max_capacity: i64) -> Result<Option<Message>, Error> {
        let mut url = s.message_queue_url.clone();
        if last > 0 {
            let (path, query) = url.split_once('?').unwrap_or((&url, ""));
            let mut q: Vec<(String, String)> = parse_query(query).into_iter().filter(|(k, _)| k != "lastMessageId").collect();
            q.push(("lastMessageId".into(), last.to_string()));
            url = format!("{path}?{}", encode_query(q));
        }
        let req = Request {
            method: "GET".into(),
            url,
            headers: vec![
                ("Accept".into(), "application/json; api-version=6.0-preview".into()),
                ("Authorization".into(), format!("Bearer {}", s.message_queue_access_token)),
                ("User-Agent".into(), self.client.user_agent()),
                (HEADER_SCALE_SET_MAX_CAPACITY.into(), max_capacity.to_string()),
            ],
            body: None,
            timeout: MESSAGE_POLL_TIMEOUT,
        };
        let resp = self.client.send(ctx, &req, &[]).map_err(|e| e.context("failed to issue the request"))?;
        match resp.status {
            202 => Ok(None),
            200 => parse_message(&resp.body)
                .map(Some)
                .map_err(|e| response_error(&req, &resp, Error::new(format!("failed to parse message response: {e}")))),
            401 => Err(response_error(&req, &resp, Error::of(Kind::MessageQueueTokenExpired))),
            _ => Err(response_error(&req, &resp, Error::new(format!("unexpected status code {}", resp.status_line())))),
        }
    }
}

/// Sorts a session message's batched job messages by their type.
pub fn parse_message(body: &[u8]) -> Result<Message, String> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Wire {
        message_id: i64,
        message_type: String,
        body: String,
        statistics: Option<Statistics>,
    }
    let w: Wire = serde_json::from_slice(body).map_err(|e| format!("failed to decode runner scale set message response: {e}"))?;
    if w.message_type != "RunnerScaleSetJobMessages" {
        return Err(format!("unsupported message type: {}", w.message_type));
    }
    let mut m = Message { message_id: w.message_id, statistics: w.statistics, ..Default::default() };
    if w.body.is_empty() {
        return Ok(m);
    }
    let batch: Vec<JobMessage> = serde_json::from_str(&w.body).map_err(|e| format!("failed to unmarshal batched messages: {e}"))?;
    for j in batch {
        match j.message_type.as_str() {
            MESSAGE_TYPE_JOB_AVAILABLE => m.job_available.push(j),
            MESSAGE_TYPE_JOB_ASSIGNED => m.job_assigned.push(j),
            MESSAGE_TYPE_JOB_STARTED => m.job_started.push(j),
            MESSAGE_TYPE_JOB_COMPLETED => m.job_completed.push(j),
            _ => {}
        }
    }
    Ok(m)
}

struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout: Duration,
}

fn set_header(req: &mut Request, name: &str, value: String) {
    req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    req.headers.push((name.into(), value));
}

struct Response {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The status as Go's Response.Status writes it: "404 Not Found".
    fn status_line(&self) -> String {
        format!("{} {}", self.status, self.status_text)
    }
}

impl Request {
    fn send_once(&self, ctx: &Ctx) -> Result<Response, String> {
        let timeout = ctx.remaining().map_or(self.timeout, |left| left.min(self.timeout));
        let mut r = crate::http::agent().request(&self.method, &self.url).timeout(timeout);
        for (k, v) in &self.headers {
            r = r.set(k, v);
        }
        let result = match &self.body {
            Some(b) => r.send_bytes(b),
            None => r.call(),
        };
        let resp = match result {
            Ok(resp) | Err(ureq::Error::Status(_, resp)) => resp,
            Err(e) => return Err(e.to_string()),
        };
        let status = resp.status();
        let status_text = resp.status_text().to_string();
        let headers = resp.headers_names().into_iter().map(|n| (n.clone(), resp.header(&n).unwrap_or("").to_string())).collect();
        let mut body = vec![];
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).map_err(|e| format!("failed to read the response body: {e}"))?;
        if body.starts_with(b"\xef\xbb\xbf") {
            body.drain(..3);
        }
        Ok(Response { status, status_text, headers, body })
    }
}

fn request_error(req: &Request, cause: &str) -> Error {
    Error::new(format!("request {} {} failed: failed to send request: {cause}", req.method, req.url))
}

/// The error for a response: its request and status, the cause, and what
/// the body says, with the kinds the status and any Actions exception mean.
fn response_error(req: &Request, resp: &Response, cause: Error) -> Error {
    let mut head = format!("request {} {} failed(status={}", req.method, req.url, crate::go::quote(&resp.status_line()));
    if let Some(id) = resp.header("ActivityId").filter(|v| !v.is_empty()) {
        head += &format!(", activity_id={}", crate::go::quote(id));
    }
    if let Some(id) = resp.header("X-GitHub-Request-Id").filter(|v| !v.is_empty()) {
        head += &format!(", github_request_id={}", crate::go::quote(id));
    }
    head += ")";
    let mut kinds = cause.kinds.clone();
    let body = String::from_utf8_lossy(&resp.body).to_string();
    let msg = if body.is_empty() {
        format!("{head}: {cause}: unknown error")
    } else if !cause.kinds.is_empty() || resp.header("Content-Type").is_some_and(|c| c.contains("text/plain")) {
        format!("{head}: {cause}: {body}")
    } else {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Exception {
            #[serde(rename = "typeName")]
            name: String,
            message: String,
        }
        match serde_json::from_str::<Exception>(&body) {
            Err(_) => format!("{head}: {cause}: failed to unmarshal error response body: {}", crate::go::quote(&body)),
            Ok(x) => {
                let known = [
                    ("AgentExistsException", Kind::RunnerExists),
                    ("AgentNotFoundException", Kind::RunnerNotFound),
                    ("JobStillRunningException", Kind::JobStillRunning),
                ];
                match known.iter().find(|(name, _)| x.name.contains(name)) {
                    Some((_, kind)) => {
                        kinds = vec![*kind];
                        format!("{head}: {}: {}", kind.text(), x.message)
                    }
                    None => format!("{head}: {cause}: {}: {}", x.name, x.message),
                }
            }
        }
    };
    let status_kind = match resp.status {
        400 => Some(Kind::BadRequest),
        401 => Some(Kind::Unauthorized),
        404 => Some(Kind::NotFound),
        409 => Some(Kind::Conflict),
        _ => None,
    };
    match status_kind {
        Some(k) => {
            kinds.push(k);
            Error { kinds, msg: format!("{}: {msg}", k.text()) }
        }
        None => Error { kinds, msg },
    }
}

fn decode<T: serde::de::DeserializeOwned>(req: &Request, resp: &Response, what: &str) -> Result<T, Error> {
    serde_json::from_slice(&resp.body).map_err(|e| response_error(req, resp, Error::new(format!("{what}: {e}"))))
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(body).map_err(|e| Error::new(format!("failed to unmarshal response body: {e}")))
}

/// How long to wait before attempt+1: the server's Retry-After on 429 and
/// 503, else doubling from a second with up to half again of jitter, at most
/// 30 seconds.
fn backoff(attempt: u32, resp: Option<&Response>) -> Duration {
    if let Some(r) = resp.filter(|r| r.status == 429 || r.status == 503)
        && let Some(secs) = r.header("Retry-After").and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Duration::from_secs(secs);
    }
    let wait = RETRY_WAIT_MIN.saturating_mul(1 << attempt.min(20));
    if wait >= RETRY_WAIT_MAX {
        return RETRY_WAIT_MAX;
    }
    let spread = wait.as_nanos() as u64 / 2;
    let mut b = [0u8; 8];
    crate::random::fill(&mut b);
    let jitter = if spread > 0 { u64::from_le_bytes(b) % (spread + 1) } else { 0 };
    (wait + Duration::from_nanos(jitter)).min(RETRY_WAIT_MAX)
}

fn join_url_path(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    match (base.is_empty(), path) {
        (_, "") => base.into(),
        (_, p) if p.starts_with('/') => format!("{base}{p}"),
        _ => format!("{base}/{path}"),
    }
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (unescape(k), unescape(v))
        })
        .collect()
}

/// Go's url.Values.Encode: sorted by key, each value query-escaped.
fn encode_query(mut q: Vec<(String, String)>) -> String {
    q.sort_by(|a, b| a.0.cmp(&b.0));
    q.iter().map(|(k, v)| format!("{}={}", escape(k), escape(v))).collect::<Vec<_>>().join("&")
}

fn escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out += &format!("%{b:02X}"),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |i: usize| b.get(i..i + 2).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], hex(i + 1)) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 2;
            }
            (c, _) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into()
}

/// When a JWT expires, read from its claims without checking its signature.
fn jwt_expiry(token: &str) -> Result<Time, String> {
    use base64::Engine;
    let claims = token.split('.').nth(1).ok_or("failed to parse jwt token: token is malformed")?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims.trim_end_matches('='))
        .map_err(|e| format!("failed to parse jwt token: {e}"))?;
    #[derive(Deserialize)]
    struct Claims {
        exp: Option<f64>,
    }
    let c: Claims = serde_json::from_slice(&raw).map_err(|e| format!("failed to parse jwt token: {e}"))?;
    let exp = c.exp.ok_or("failed to parse token claims to get expire at")?;
    Ok(Time::from_unix(exp as i64))
}

#[cfg(test)]
pub(crate) mod tests;
