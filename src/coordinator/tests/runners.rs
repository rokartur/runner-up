// A runner that is already gone from its scale set counts as removed, and any
// other failure does not, so its place keeps it until GitHub answers. The
// answer is read by its error kind from the scale set client, not by
// searching its text: a refused removal of runner 404 names 404 in its URL.

use crate::coordinator::runners::runner_gone;
use crate::ctx::Ctx;
use crate::testserver::Response;

#[test]
fn only_a_missing_runner_counts_as_removed() {
    let cases = [
        (
            "GitHub names the runner missing",
            404,
            r#"{"$id":"1","message":"Agent 404 does not exist in pool 1.","typeName":"GitHub.DistributedTask.WebApi.TaskAgentNotFoundException, Sdk"}"#,
            true,
        ),
        ("404 Not Found", 404, r#"{"message":"Not Found"}"#, true),
        ("GitHub refuses", 403, r#"{"message":"Forbidden"}"#, false),
    ];
    for (name, status, body, gone) in cases {
        let (client, log) = crate::scaleset::tests::client(move |_| {
            let mut resp = Response::new(status, body);
            resp.headers.push(("Content-Type".into(), "application/json".into()));
            resp
        });
        let err = client.remove_runner(&Ctx::background(), 404).expect_err(name);
        let last = log.lock().unwrap().last().cloned().unwrap_or_default();
        assert!(last.starts_with("DELETE ") && last.ends_with("/agents/404"), "{name}: unexpected request {last}");
        assert_eq!(runner_gone(&err), gone, "{name}: runner_gone({err})");
    }
}
