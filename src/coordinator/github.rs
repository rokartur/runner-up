use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::github_api::TokenSource;
use super::{Service, SetClient, WaitGroup};
use crate::config::Config;
use crate::ctx::Ctx;
use crate::go::slog::Logger;
use crate::scaleset::{self, Client, Label, Message, MessageSession, RunnerScaleSet, RunnerSetting, SystemInfo};
use crate::version::version;

/// A repository's runners belong to its "Default" runner group, id 1.
const DEFAULT_RUNNER_GROUP_ID: i64 = 1;

/// An acknowledgement already in flight may finish after cancellation, then
/// the session must close. Bound both so they fit inside SHUTDOWN_TIMEOUT.
const SESSION_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Makes a scale set client for a repository or organization.
pub type ClientFactory = Arc<dyn Fn(&Ctx, &str) -> Result<Arc<Client>, String> + Send + Sync>;

/// Makes clients that sign in with the GitHub command line's token, read
/// afresh for every session, so a new gh sign-in takes effect at the next
/// session.
pub fn gh_clients(token: TokenSource) -> ClientFactory {
    Arc::new(move |ctx: &Ctx, target: &str| {
        let t = token(ctx)?;
        let info = SystemInfo { system: "runner-up".into(), version: version().into(), subsystem: "coordinator".into(), ..Default::default() };
        Client::with_token(&format!("https://github.com/{target}"), &t, info).map(Arc::new).map_err(|e| e.to_string())
    })
}

/// One scale set's live session.
struct LiveClient {
    client: Arc<Client>,
    session: Arc<MessageSession>,
    id: i64,
}

impl SetClient for LiveClient {
    fn acquire_jobs(&self, ctx: &Ctx, ids: &[i64]) -> Result<Vec<i64>, scaleset::Error> {
        self.session.acquire_jobs(ctx, ids)
    }

    fn generate_jit(&self, ctx: &Ctx, name: &str, work_folder: &str) -> Result<(String, i64), scaleset::Error> {
        let cfg = self.client.generate_jit_runner_config(ctx, name, work_folder, self.id)?;
        Ok((cfg.encoded_jit_config, cfg.runner.map_or(0, |r| r.id)))
    }

    fn remove_runner(&self, ctx: &Ctx, id: i64) -> Result<(), scaleset::Error> {
        self.client.remove_runner(ctx, id)
    }
}

/// Whether a target is an organization rather than owner/name.
fn is_org(target: &str) -> bool {
    !target.contains('/')
}

/// Finds the target's scale set for label, or creates it. The scale set is
/// never deleted on shutdown: waiting jobs stay queued for it.
fn ensure_scale_set(ctx: &Ctx, c: &Client, target: &str, group: &str, label: &str) -> Result<RunnerScaleSet, String> {
    let mut group_id = DEFAULT_RUNNER_GROUP_ID;
    if is_org(target) {
        let g = c.get_runner_group_by_name(ctx, group).map_err(|e| format!("runner group {}: {e}", crate::go::quote(group)))?;
        group_id = g.id;
    }
    if let Some(set) = c.get_runner_scale_set(ctx, group_id, label).map_err(|e| e.to_string())? {
        return Ok(set);
    }
    c.create_runner_scale_set(
        ctx,
        RunnerScaleSet {
            name: label.into(),
            runner_group_id: group_id,
            labels: vec![Label { name: label.into(), kind: "System".into() }],
            // Every place starts a fresh runner of a pinned version; none
            // updates itself.
            runner_setting: RunnerSetting { disable_update: true },
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

/// One session: a target's scale set for a pool, found in the runner group
/// an organization's scale sets join.
#[derive(Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    target: String,
    pool: String,
    group: String,
}

/// Keeps one scale set session open for each repository or organization and
/// pool of the setup the coordinator runs. A setup change opens the sessions
/// of the scale sets it adds and closes those of the ones it removes; every
/// other session stays open through it.
pub struct Sessions {
    ctx: Ctx,
    svc: Arc<Service>,
    log: Logger,
    clients: ClientFactory,
    wg: WaitGroup,
    open: HashMap<SessionKey, Ctx>,
}

impl Sessions {
    pub fn new(ctx: &Ctx, svc: Arc<Service>, log: Logger, clients: ClientFactory) -> Sessions {
        Sessions { ctx: ctx.clone(), svc, log, clients, wg: WaitGroup::default(), open: HashMap::new() }
    }

    /// Opens a session for each scale set cfg serves that has none, and
    /// closes each session of a scale set cfg no longer serves.
    pub fn match_setup(&mut self, cfg: &Config) {
        let mut want = Vec::new();
        for target in cfg.targets() {
            let group = if is_org(&target) { cfg.runner_group().to_string() } else { String::new() };
            for label in cfg.labels() {
                want.push(SessionKey { target: target.clone(), pool: label, group: group.clone() });
            }
        }
        let log = self.log.clone();
        self.open.retain(|k, ctx| {
            if want.contains(k) {
                return true;
            }
            log.info("the setup no longer serves this scale set; closing its session", &[("target", &k.target), ("pool", &k.pool)]);
            ctx.cancel();
            false
        });
        for k in want {
            if self.open.contains_key(&k) {
                continue;
            }
            let ctx = self.ctx.with_cancel();
            self.open.insert(k.clone(), ctx.clone());
            let call = self.wg.add();
            let (svc, log, clients) = (self.svc.clone(), self.log.clone(), self.clients.clone());
            std::thread::spawn(move || {
                let _call = call;
                run_set(&ctx, &svc, &log, &k.target, &k.pool, &clients);
            });
        }
    }

    /// Waits until every session has ended, after the coordinator's context
    /// ended, or d passed; says whether they all ended.
    pub fn wait_done(&self, d: Duration) -> bool {
        self.wg.wait_timeout(d)
    }
}

/// Keeps one scale set's session open and feeds its messages to the
/// service, until ctx ends. It never acquires a job itself: the serve loop's
/// placement pass does, once a place is chosen, with the serving context.
/// Errors restart the session after a pause; the session's messages stay
/// queued at GitHub until acknowledged.
pub fn run_set(ctx: &Ctx, svc: &Service, log: &Logger, target: &str, label: &str, clients: &ClientFactory) {
    let log = log.with(&[("target", &target), ("pool", &label)]);
    while !ctx.is_done() {
        let err = serve_set(ctx, svc, &log, target, label, clients);
        if ctx.is_done() {
            return;
        }
        log.warn("scale set session ended; retrying in 30s", &[("err", &err)]);
        ctx.sleep(Duration::from_secs(30));
    }
}

fn serve_set(ctx: &Ctx, svc: &Service, log: &Logger, target: &str, label: &str, clients: &ClientFactory) -> String {
    let c = match clients(ctx, target) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let set = match ensure_scale_set(ctx, &c, target, svc.setup().runner_group(), label) {
        Ok(s) => s,
        Err(e) => return format!("scale set: {e}"),
    };
    c.set_system_info(SystemInfo {
        system: "runner-up".into(),
        version: version().into(),
        subsystem: "coordinator".into(),
        scale_set_id: set.id,
        ..Default::default()
    });
    // One session per scale set: a second coordinator waits here until this
    // one lets go.
    let session = match c.message_session(ctx, set.id, &format!("runner-up-{}", crate::proc::hostname())) {
        Ok(s) => Arc::new(s),
        Err(e) => return format!("session: {e}"),
    };
    log.info("scale set session open", &[("scaleSetId", &set.id)]);
    let sc: Arc<dyn SetClient> = Arc::new(LiveClient { client: c.clone(), session: session.clone(), id: set.id });
    svc.add_set(target, label, sc.clone());
    let err = poll(ctx, svc, target, label, &session);
    // Before the session closes: no job is acquired through a closing session.
    svc.remove_set(target, label, &sc);
    let close_ctx = Ctx::detached_timeout(SESSION_CLEANUP_TIMEOUT);
    match session.close(&close_ctx) {
        Err(e) => log.warn("could not close scale set session; it may remain active", &[("err", &e)]),
        Ok(()) => log.info("scale set session closed", &[("scaleSetId", &set.id)]),
    }
    err
}

/// Hands the session's messages to the service until ctx ends or a call
/// fails, and says why it stopped.
fn poll(ctx: &Ctx, svc: &Service, target: &str, label: &str, session: &MessageSession) -> String {
    let first = Message { message_id: 0, statistics: session.session().statistics, ..Default::default() };
    svc.handle(target, label, Some(&first));
    let mut last = 0;
    while !ctx.is_done() {
        let msg = match session.get_message(ctx, last, svc.setup().announce_capacity) {
            Ok(m) => m,
            Err(e) => return format!("get message: {e}"),
        };
        svc.handle(target, label, msg.as_ref());
        if let Some(msg) = msg {
            last = msg.message_id;
            let ack_ctx = Ctx::detached_timeout(SESSION_CLEANUP_TIMEOUT);
            if let Err(e) = session.delete_message(&ack_ctx, msg.message_id) {
                return format!("ack message {}: {e}", msg.message_id);
            }
        }
    }
    ctx.err().unwrap_or("context canceled").to_string()
}
