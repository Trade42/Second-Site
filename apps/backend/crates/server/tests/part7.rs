//! Server part 7 (rust-server-7): no lock-order deadlock between the approval decision and the
//! other writers of one project.
//!
//! Each test races real requests through the router: on PostgreSQL each request runs on its own
//! pool connection on a multi-thread runtime; on the in-memory store the same requests serialize
//! (that variant runs fewer rounds and checks that callers see the same answers). Each round
//! starts its jobs together (a barrier, then a stagger of 0 to 3 ms per job that changes every
//! round) and fails the test for any answer of 500 or more, any answer that mentions a deadlock
//! (`40P01`), any task that did not complete, and any status outside the job's expected set. A
//! summary line `PART7 <test>: ...` is printed either way.
//!
//! `PART7_ROUNDS` overrides every round count (exploratory runs only; the defaults are the
//! commission's minimums: 200 for the reported case, 50 for the call start pair on PostgreSQL).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::Method;
use serde_json::{json, Value};
use ss_server::contracts::{ActionType, JsonMap};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use uuid::Uuid;

use common::*;

const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";

fn rounds(default: usize) -> usize {
    std::env::var("PART7_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---- Jobs, races and the tally --------------------------------------------------------------------

#[derive(Clone)]
struct Job {
    name: &'static str,
    method: Method,
    path: String,
    body: Option<Value>,
    /// Expected statuses; anything else fails the test.
    ok: &'static [u16],
}

/// The answers of one round, in job order.
async fn race(t: &TestApp, tok: &str, round: usize, jobs: Vec<Job>) -> Vec<(Job, Resp)> {
    let n = jobs.len();
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for (i, job) in jobs.into_iter().enumerate() {
        let tt = TestApp {
            app: t.app.clone(),
            router: t.router.clone(),
            kind: t.kind,
        };
        let tok = tok.to_owned();
        let barrier = barrier.clone();
        // A different stagger every round, so every job is sometimes first and sometimes last.
        let delay = Duration::from_millis(((round + i * 3) % 4) as u64);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            tokio::time::sleep(delay).await;
            let r = tt
                .call(job.method.clone(), &job.path, Some(&tok), job.body.as_ref())
                .await;
            (job, r)
        }));
    }
    let mut out = Vec::with_capacity(n);
    for h in handles {
        out.push(h.await.expect("job panicked"));
    }
    out
}

#[derive(Default)]
struct Tally {
    rounds: usize,
    answers: usize,
    statuses: BTreeMap<String, usize>,
    server_errors: usize,
    deadlocks: usize,
    failed_tasks: usize,
    unexpected: usize,
    problems: Vec<String>,
}

impl Tally {
    fn round(&mut self, round: usize, answers: &[(Job, Resp)]) {
        self.rounds += 1;
        for (job, r) in answers {
            self.answers += 1;
            let status = r.status.as_u16();
            *self
                .statuses
                .entry(format!("{} {status}", job.name))
                .or_default() += 1;
            let mut bad = Vec::new();
            if status >= 500 {
                self.server_errors += 1;
                bad.push("5xx");
            }
            if r.text.contains("40P01") || r.text.to_lowercase().contains("deadlock") {
                self.deadlocks += 1;
                bad.push("deadlock");
            }
            let task = &r.body["task"];
            if task.is_object() && task["status"] != "COMPLETED" {
                self.failed_tasks += 1;
                bad.push("task not completed");
            }
            if !job.ok.contains(&status) {
                self.unexpected += 1;
                bad.push("unexpected status");
            }
            if !bad.is_empty() && self.problems.len() < 40 {
                self.problems.push(format!(
                    "round {round}: {} {status} [{}] {}",
                    job.name,
                    bad.join(", "),
                    r.text.chars().take(240).collect::<String>()
                ));
            }
        }
    }

    fn assert_clean(&self, test: &str, kind: Kind, started: Instant) {
        eprintln!(
            "PART7 {test} ({kind:?}): {} rounds, {} answers in {:.1} s; {} answers >= 500, {} \
             deadlock answers, {} tasks not completed, {} unexpected statuses; statuses {:?}",
            self.rounds,
            self.answers,
            started.elapsed().as_secs_f64(),
            self.server_errors,
            self.deadlocks,
            self.failed_tasks,
            self.unexpected,
            self.statuses
        );
        for p in &self.problems {
            eprintln!("PART7 {test} ({kind:?}) problem: {p}");
        }
        assert!(
            self.server_errors == 0
                && self.deadlocks == 0
                && self.failed_tasks == 0
                && self.unexpected == 0,
            "{test} ({kind:?}): {} answers >= 500, {} deadlock answers, {} tasks not completed, \
             {} unexpected statuses in {} rounds; first problems: {:#?}",
            self.server_errors,
            self.deadlocks,
            self.failed_tasks,
            self.unexpected,
            self.rounds,
            self.problems.iter().take(8).collect::<Vec<_>>()
        );
    }
}

// ---- One project with a completed search ---------------------------------------------------------

struct Env {
    t: TestApp,
    tok: String,
    user: Uuid,
    pid: Uuid,
    hollywood: Uuid,
}

async fn env(db: &TestDb) -> Env {
    let t = TestApp::new(db, &[]);
    // Owner texts go to a recorder, which writes nothing of its own in the background.
    t.app.extensions.insert(Arc::new(OwnerNotify::new(Arc::new(
        RecordingNotifier::new(),
    ))));
    let (tok, user) = t.dev_token().await;
    let pid: Uuid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await;
    let hollywood = props
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["property_identity"]["street_address"] == HOLLYWOOD)
        .expect("the Hollywood property")["property_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    Env {
        t,
        tok,
        user,
        pid,
        hollywood,
    }
}

impl Env {
    fn search(&self) -> Job {
        Job {
            name: "search",
            method: Method::POST,
            path: format!("/api/v1/projects/{}/properties/search", self.pid),
            body: Some(json!({"city": "Los Angeles", "state": "CA"})),
            ok: &[202],
        }
    }

    fn profile_update(&self) -> Job {
        Job {
            name: "profile update",
            method: Method::PUT,
            path: format!("/api/v1/projects/{}/business-profile", self.pid),
            body: Some(demo_project_body()["business_profile"].clone()),
            ok: &[200],
        }
    }

    /// A PENDING approval of this project (a message, never sent), committed.
    async fn pending_approval(&self, tag: &str) -> (Uuid, String) {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let mut payload = JsonMap::new();
        payload.insert("part7".into(), json!(tag));
        let a = self
            .t
            .app
            .approvals()
            .request(
                &mut *tx,
                ApprovalSpec::new(
                    self.pid,
                    self.user,
                    "part7",
                    ActionType::SendMessage,
                    payload,
                    "part7 probe",
                    format!("part7:{}", Uuid::new_v4()),
                ),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        (a.approval_id, a.action_payload_hash)
    }

    fn decision(&self, approval: &(Uuid, String), decision: &str) -> Job {
        Job {
            name: "decision",
            method: Method::POST,
            path: format!("/api/v1/approvals/{}/decision", approval.0),
            body: Some(json!({"decision": decision, "action_payload_hash": approval.1})),
            ok: &[200],
        }
    }

    /// A new voice outreach of this project (a new recipient, so a new PENDING approval).
    async fn outreach(&self, n: usize) -> Value {
        let r = self
            .t
            .post(
                &format!("/api/v1/projects/{}/outreach", self.pid),
                Some(&self.tok),
                &json!({
                    "property_id": self.hollywood.to_string(),
                    "channel": "VOICE",
                    "recipient": {"role": "broker", "name": format!("Broker {n}"), "phone": "+15555550100"},
                    "purpose": "Verify electrical, plumbing and HVAC",
                }),
            )
            .await;
        assert_eq!(r.status.as_u16(), 202, "{}", r.text);
        r.body
    }

    async fn finish(self) {
        self.t.app.owner_notify().idle().await;
    }
}

macro_rules! race_test {
    ($name:ident, $pg:expr, $mem:expr, $body:ident) => {
        mod $name {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
            async fn postgres() {
                let Some(db) = TestDb::new(Kind::Postgres).await else {
                    return;
                };
                $body(&db, rounds($pg), stringify!($name)).await;
                db.finish().await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
            async fn memory() {
                let db = TestDb::new(Kind::Memory).await.expect("memory");
                $body(&db, rounds($mem), stringify!($name)).await;
                db.finish().await;
            }
        }
    };
}

// ---- The reported case ------------------------------------------------------------------------------

/// The decision, a business-profile update and a search of one project at the same time (the
/// nessie-3 report's reproduction, section 9): before the fix PostgreSQL killed one of them with
/// `40P01` in most rounds.
async fn decision_profile_update_and_search(db: &TestDb, n: usize, name: &str) {
    let e = env(db).await;
    let (started, mut tally) = (Instant::now(), Tally::default());
    for round in 0..n {
        let a = e.pending_approval(&format!("r{round}")).await;
        let d = if round % 2 == 0 {
            "REJECTED"
        } else {
            "APPROVED"
        };
        let jobs = vec![e.decision(&a, d), e.profile_update(), e.search()];
        tally.round(round, &race(&e.t, &e.tok, round, jobs).await);
    }
    tally.assert_clean(name, db.kind, started);
    e.finish().await;
}

race_test!(
    reported_decision_profile_update_and_search,
    200,
    20,
    decision_profile_update_and_search
);

// ---- The decision against a call start for the same approval ------------------------------------------

/// The decision of an outreach's approval and the call start on that approval: the start takes
/// the project row, then the approval row; the decision takes the approval row, then the event
/// lock and (inserting its event) the key share of the project row.
async fn decision_and_call_start(db: &TestDb, n: usize, name: &str) {
    let e = env(db).await;
    let (started, mut tally) = (Instant::now(), Tally::default());
    let (mut placed, mut refused) = (0, 0);
    for round in 0..n {
        let out = e.outreach(round).await;
        let appr = &out["approval"];
        let a = (
            appr["approval_id"].as_str().unwrap().parse().unwrap(),
            appr["action_payload_hash"].as_str().unwrap().to_owned(),
        );
        let start = Job {
            name: "call start",
            method: Method::POST,
            path: format!("/api/v1/projects/{}/calls", e.pid),
            body: Some(json!({"communication_id": out["communication"]["communication_id"]})),
            ok: &[202, 409],
        };
        let answers = race(&e.t, &e.tok, round, vec![e.decision(&a, "APPROVED"), start]).await;
        match answers[1].1.status.as_u16() {
            202 => placed += 1,
            409 => {
                // The only acceptable refusal: the start came before the decision.
                assert_eq!(
                    answers[1].1.body["error_code"], "APPROVAL_REQUIRED",
                    "{}",
                    answers[1].1.text
                );
                refused += 1;
            }
            _ => {}
        }
        tally.round(round, &answers);
    }
    eprintln!(
        "PART7 {name} ({:?}): {placed} calls placed after the decision, {refused} starts refused \
         before it",
        db.kind
    );
    tally.assert_clean(name, db.kind, started);
    e.finish().await;
}

race_test!(
    decision_against_call_start_for_the_same_approval,
    50,
    10,
    decision_and_call_start
);
