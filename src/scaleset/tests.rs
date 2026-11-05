use super::*;
use crate::testserver::{Request as Req, Response as Resp, Server};
use std::sync::{Arc, Mutex};

/// An Actions service admin token: the client reads only when it expires,
/// without checking its signature.
pub fn admin_token() -> String {
    use base64::Engine;
    let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let claims = format!(r#"{{"iss":"1","exp":{}}}"#, Time::now().unix() + 3600);
    format!("{}.{}.{}", enc(br#"{"alg":"HS256","typ":"JWT"}"#), enc(claims.as_bytes()), enc(b"unsigned"))
}

/// A stand-in for GitHub that signs the client in and hands every other
/// request to broker, logging each request's method and path.
pub(crate) fn client(broker: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> (Arc<Client>, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(vec![]));
    let seen = log.clone();
    let url = Arc::new(Mutex::new(String::new()));
    let tenant = url.clone();
    let srv = Server::start(move |r| {
        seen.lock().unwrap().push(format!("{} {}", r.method, r.path));
        if r.path.ends_with("/runners/registration-token") {
            assert_eq!(r.header("Authorization"), "Bearer token");
            return Resp::new(201, r#"{"token":"registration-token"}"#);
        }
        if r.path.ends_with("/actions/runner-registration") {
            assert_eq!(r.header("Authorization"), "RemoteAuth registration-token");
            let t = tenant.lock().unwrap().clone();
            return Resp::json(200, &serde_json::json!({"url": t + "/tenant/1/", "token": admin_token()}));
        }
        broker(r)
    });
    *url.lock().unwrap() = srv.url.clone();
    let info = SystemInfo { system: "runner-up".into(), subsystem: "test".into(), ..Default::default() };
    let c = Client::with_token(&format!("{}/octo-org/app", srv.url), "token", info).unwrap().without_retries();
    (Arc::new(c), log)
}

// Intent: a repository's runners sign in through GitHub's API under
// /api/v3 off github.com, then reach the Actions service with its admin
// token; a removal GitHub refuses because the runner runs a job, or finds no
// such runner, says which, so the coordinator can keep or forget the runner.
#[test]
fn removal_says_why_github_refused_it() {
    let (c, log) = client(|r| {
        assert!(r.header("Authorization").starts_with("Bearer ey"), "the admin token signs Actions requests");
        assert_eq!(r.query_value("api-version"), Some("6.0-preview"));
        match r.path.as_str() {
            "/tenant/1/_apis/distributedtask/pools/0/agents/1" => Resp::new(204, ""),
            "/tenant/1/_apis/distributedtask/pools/0/agents/2" => Resp::json(
                400,
                &serde_json::json!({"typeName": "GitHub.DistributedTask.WebApi.JobStillRunningException", "message": "busy"}),
            ),
            "/tenant/1/_apis/distributedtask/pools/0/agents/3" => Resp::json(
                404,
                &serde_json::json!({"typeName": "GitHub.DistributedTask.WebApi.AgentNotFoundException", "message": "gone"}),
            ),
            _ => Resp::new(404, ""),
        }
    });
    let ctx = Ctx::background();
    c.remove_runner(&ctx, 1).unwrap();
    let busy = c.remove_runner(&ctx, 2).unwrap_err();
    assert!(busy.is(Kind::JobStillRunning), "{busy}");
    let gone = c.remove_runner(&ctx, 3).unwrap_err();
    assert!(gone.is(Kind::RunnerNotFound) && gone.is(Kind::NotFound), "{gone}");
    let missing = c.remove_runner(&ctx, 4).unwrap_err();
    assert!(missing.is(Kind::NotFound) && !missing.is(Kind::RunnerNotFound), "{missing}");
    let log = log.lock().unwrap();
    assert_eq!(log[0], "POST /api/v3/repos/octo-org/app/actions/runners/registration-token");
    assert_eq!(log[1], "POST /api/v3/actions/runner-registration");
    assert_eq!(log.iter().filter(|l| l.contains("registration")).count(), 2, "the admin token is reused until it expires");
}

// Intent: a session's messages arrive sorted by job message type, a poll
// with none is no message, and a message queue token that expired is
// refreshed once and the call made again.
#[test]
fn session_reads_messages_and_refreshes_an_expired_token() {
    let queue = Arc::new(Mutex::new(String::new()));
    let q = queue.clone();
    let polls = Arc::new(Mutex::new(0));
    let p = polls.clone();
    let (c, log) = client(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/tenant/1/_apis/runtime/runnerscalesets/7/sessions") => {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(body["ownerName"], "runner-up-host");
            Resp::json(200, &serde_json::json!({"sessionId": "s-1", "messageQueueUrl": q.lock().unwrap().clone() + "/queue?x=1", "messageQueueAccessToken": "old", "statistics": {"totalAvailableJobs": 2}}))
        }
        ("PATCH", "/tenant/1/_apis/runtime/runnerscalesets/7/sessions/s-1") => {
            Resp::json(200, &serde_json::json!({"sessionId": "s-1", "messageQueueUrl": q.lock().unwrap().clone() + "/queue?x=1", "messageQueueAccessToken": "new"}))
        }
        ("GET", "/queue") => {
            assert_eq!(r.header(HEADER_SCALE_SET_MAX_CAPACITY), "50");
            if r.header("Authorization") == "Bearer old" {
                return Resp::new(401, "");
            }
            let mut n = p.lock().unwrap();
            *n += 1;
            if *n == 1 {
                return Resp::new(202, "");
            }
            assert_eq!(r.query_value("lastMessageId"), Some("4"));
            let batch = serde_json::json!([
                {"messageType": "JobAvailable", "runnerRequestId": 11, "jobDisplayName": "build", "queueTime": "2026-10-20T02:54:08Z"},
                {"messageType": "JobStarted", "runnerRequestId": 12, "runnerId": 3, "runnerName": "ru-1"},
                {"messageType": "JobCompleted", "runnerRequestId": 13, "result": "succeeded"},
                {"messageType": "Unknown"}
            ]);
            Resp::json(200, &serde_json::json!({"messageId": 5, "messageType": "RunnerScaleSetJobMessages", "body": batch.to_string()}))
        }
        ("DELETE", "/queue/5") => Resp::new(204, ""),
        ("POST", "/tenant/1/_apis/runtime/runnerscalesets/7/acquirejobs") => {
            assert_eq!(r.header("Authorization"), "Bearer new");
            Resp::json(200, &serde_json::json!({"count": 1, "value": [11]}))
        }
        _ => Resp::new(500, "unexpected"),
    });
    *queue.lock().unwrap() = c.config.url.trim_end_matches("/octo-org/app").to_string();
    let ctx = Ctx::background();
    let s = c.message_session(&ctx, 7, "runner-up-host").unwrap();
    assert_eq!(s.session().statistics.unwrap().total_available_jobs, 2);
    assert_eq!(s.get_message(&ctx, 0, 50).unwrap(), None);
    let m = s.get_message(&ctx, 4, 50).unwrap().unwrap();
    assert_eq!(m.message_id, 5);
    assert_eq!(m.job_available[0].runner_request_id, 11);
    assert_eq!(m.job_available[0].queue_time.rfc3339(), "2026-10-20T02:54:08Z");
    assert_eq!((m.job_started[0].runner_id, m.job_started[0].runner_name.as_str()), (3, "ru-1"));
    assert_eq!(m.job_completed[0].result, "succeeded");
    s.delete_message(&ctx, 5).unwrap();
    assert_eq!(s.acquire_jobs(&ctx, &[11, 12]).unwrap(), vec![11]);
    assert_eq!(log.lock().unwrap().iter().filter(|l| l.starts_with("PATCH")).count(), 1, "one refresh serves later calls");
}

// Intent: query values are sorted and escaped as Go's url.Values.Encode, so
// the requests GitHub sees match the Go client's.
#[test]
fn queries_encode_like_go() {
    let q = vec![("name".to_string(), "ru linux/large".to_string()), ("api-version".to_string(), "6.0-preview".to_string())];
    assert_eq!(encode_query(q), "api-version=6.0-preview&name=ru+linux%2Flarge");
    assert_eq!(parse_query("a=b%2Fc&d=e+f"), vec![("a".into(), "b/c".into()), ("d".into(), "e f".into())]);
    assert!(parse_config_url("https://github.com/").is_err());
    let org = parse_config_url("https://github.com/octo-org").unwrap();
    assert_eq!(org.api_url(&org.registration_token_path()), "https://api.github.com/orgs/octo-org/actions/runners/registration-token");
}
