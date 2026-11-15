//! REVIEW (temporary, part 4 independent review): live mode with the carrier on a loopback mock
//! only; dial outcomes the carrier did not confirm; owner leg with outbound calls disabled.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use ss_domain::integrations::telnyx::{
    client_state, encode_client_state, DialResult, TelephonyError, TelephonyProvider,
};
use ss_server::contracts::ApprovalStatus;
use ss_server::integrations::telnyx::{LiveTelnyxProvider, Voice};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::{CALLS, OWNER_CHANNELS};
use ss_server::store::repo::APPROVALS;
use ss_server::store::Query;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::{demo_project_body, TestApp};

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn signed(body: &[u8]) -> Request<Body> {
    let ts = chrono::Utc::now().timestamp();
    let mut msg = format!("{ts}|").into_bytes();
    msg.extend_from_slice(body);
    let sig = base64::engine::general_purpose::STANDARD.encode(key().sign(&msg).to_bytes());
    Request::builder()
        .method(Method::POST)
        .uri("/webhooks/telnyx")
        .header("telnyx-signature-ed25519", sig)
        .header("telnyx-timestamp", ts.to_string())
        .header("content-type", "application/json")
        .body(Body::from(body.to_vec()))
        .unwrap()
}

struct Ctx {
    t: TestApp,
    token: String,
    owner: Uuid,
}

impl Ctx {
    fn new(db: &common::TestDb) -> Ctx {
        let pk = base64::engine::general_purpose::STANDARD.encode(key().verifying_key().to_bytes());
        let t = TestApp::new(db, &[("TELNYX_PUBLIC_KEY", pk.as_str())]);
        t.app
            .extensions
            .insert(Arc::new(OwnerNotify::new(Arc::new(RecordingNotifier::new()))));
        let owner = Uuid::new_v4();
        let token = t.token_for(owner);
        Ctx { t, token, owner }
    }

    async fn approved_comm(&self) -> (Uuid, String, Uuid) {
        let p = self.t.create_project(&self.token, &demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
        let r = self
            .t
            .post(
                &format!("/api/v1/projects/{pid}/properties/search"),
                Some(&self.token),
                &json!({"city": "Los Angeles", "state": "CA"}),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED);
        let props = self
            .t
            .get(&format!("/api/v1/projects/{pid}/properties"), Some(&self.token))
            .await;
        let prop = props.body.as_array().unwrap()[0]["property_id"].as_str().unwrap().to_owned();
        let out = self
            .t
            .post(
                &format!("/api/v1/projects/{pid}/outreach"),
                Some(&self.token),
                &json!({"property_id": prop, "channel": "VOICE",
                        "recipient": {"role": "broker", "name": "Dana", "phone": "+15555550100"},
                        "purpose": "Verify electrical"}),
            )
            .await;
        assert_eq!(out.status, StatusCode::ACCEPTED, "{}", out.text);
        let d = self
            .t
            .post(
                &format!("/api/v1/approvals/{}/decision", out.body["approval"]["approval_id"].as_str().unwrap()),
                Some(&self.token),
                &json!({"decision": "APPROVED", "action_payload_hash": out.body["approval"]["action_payload_hash"]}),
            )
            .await;
        assert_eq!(d.status, StatusCode::OK);
        (
            pid,
            out.body["communication"]["communication_id"].as_str().unwrap().to_owned(),
            out.body["approval"]["approval_id"].as_str().unwrap().parse().unwrap(),
        )
    }

    async fn start(&self, pid: Uuid, comm: &str) -> common::Resp {
        self.t
            .post(
                &format!("/api/v1/projects/{pid}/calls"),
                Some(&self.token),
                &json!({"communication_id": comm}),
            )
            .await
    }

    async fn approval(&self, id: Uuid) -> ApprovalStatus {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let a = APPROVALS.require(&mut *tx, id).await.unwrap();
        tx.commit().await.unwrap();
        a.approval_status
    }

    async fn calls(&self, pid: Uuid) -> Vec<ss_domain::contracts::CallSession> {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let c = CALLS.list(&mut *tx, Query::new().eq("project_id", pid)).await.unwrap();
        tx.commit().await.unwrap();
        c
    }
}

/// A carrier whose dial times out (the request may have reached it).
struct TimeoutCarrier {
    dials: AtomicUsize,
    kind: ProviderErrorKind,
    message: &'static str,
}

impl TelephonyProvider for TimeoutCarrier {
    fn mode(&self) -> &str {
        "live"
    }
    fn dial(
        &self,
        _to: &str,
        _from: &str,
        _w: &str,
        _k: &str,
        _cs: Option<&IndexMap<String, Value>>,
    ) -> Result<DialResult, TelephonyError> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        Err(TelephonyError::Provider(ProviderError::new("telnyx", self.kind, self.message)))
    }
    fn speak(&self, _: &str, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn play(&self, _: &str, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn start_transcription(&self, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn hangup(&self, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn create_conference(&self, _: &str, _: &str) -> Result<String, TelephonyError> {
        Ok("c".into())
    }
    fn join_conference(&self, _: &str, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn status(&self) -> ProviderStatusEntry {
        ss_domain::integrations::base::status_entry(
            "telnyx",
            "live",
            ss_core::IntegrationStatus::Partial,
            true,
            "review",
        )
    }
}

both_backends!(dial_outcomes_the_carrier_did_not_confirm, |db| {
    for (kind, message) in [
        (ProviderErrorKind::Timeout, "timed out"),
        (ProviderErrorKind::Unavailable, "HTTP 503"),
        (ProviderErrorKind::BadResponse, "response without data.call_control_id"),
        (ProviderErrorKind::BadResponse, "HTTP 422"),
    ] {
        let c = Ctx::new(db);
        let carrier = Arc::new(TimeoutCarrier {
            dials: AtomicUsize::new(0),
            kind,
            message,
        });
        c.t.app.extensions.insert(Arc::new(Voice::live(carrier.clone())));
        let (pid, comm, appr) = c.approved_comm().await;
        let r1 = c.start(pid, &comm).await;
        let after1 = c.approval(appr).await;
        let r2 = c.start(pid, &comm).await;
        println!(
            "REVIEW dial {kind:?} {message:?}: first {} state={} reason={} approval_after={after1:?}; second {} state={}; dial attempts {}",
            r1.status, r1.body["state"], r1.body["failure_reason"], r2.status, r2.body["state"],
            carrier.dials.load(Ordering::SeqCst)
        );
        let _ = c.calls(pid).await;
    }
});

fn telnyx_mock() -> MockServer {
    MockServer::start_with(|r: &Recorded| {
        let body = if r.path == "/calls" {
            let to = serde_json::from_slice::<Value>(&r.body).unwrap()["to"].as_str().unwrap().to_owned();
            json!({"data": {"call_control_id": format!("v3:leg-{to}"), "state": "initiated"}})
        } else if r.path == "/conferences" {
            json!({"data": {"id": "conf-1"}})
        } else {
            json!({"data": {"result": "ok"}})
        };
        Canned {
            status: 200,
            content_type: Some("application/json".into()),
            body: serde_json::to_vec(&body).unwrap(),
        }
    })
}

both_backends!(outbound_disabled_refuses_before_anything_is_sent, |db| {
    let c = Ctx::new(db);
    let mock = telnyx_mock();
    c.t.app.extensions.insert(Arc::new(Voice::live(Arc::new(
        LiveTelnyxProvider::with_base_url("test-key", "conn", false, &mock.base),
    ))));
    let (pid, comm, appr) = c.approved_comm().await;
    let r = c.start(pid, &comm).await;
    println!("REVIEW outbound disabled: {} {} mock requests {}", r.status, r.text, mock.requests().len());
    assert_eq!(r.status.as_u16(), 403);
    assert_eq!(mock.requests().len(), 0);
    assert_eq!(c.calls(pid).await.len(), 0);
    assert_eq!(c.approval(appr).await, ApprovalStatus::Approved);
});

both_backends!(owner_leg_with_outbound_disabled, |db| {
    let c = Ctx::new(db);
    let mock = telnyx_mock();
    c.t.app.extensions.insert(Arc::new(Voice::live(Arc::new(
        LiveTelnyxProvider::with_base_url("test-key", "conn", true, &mock.base),
    ))));
    let (pid, comm, _) = c.approved_comm().await;
    let mut tx = c.t.app.store.begin().await.unwrap();
    OWNER_CHANNELS
        .add(
            &mut *tx,
            &ss_domain::contracts::OwnerChannel {
                owner_channel_id: Uuid::new_v4(),
                owner_id: c.owner,
                platform: "imessage".into(),
                handle: "+15555550142".into(),
                status: ss_domain::contracts::OwnerChannelStatus::Linked,
                space_id: Some("spc".into()),
                agent_user_id: Some("usr".into()),
                last_inbound_message_id: None,
                linked_at: None,
                updated_at: ss_core::UtcDateTime::now(),
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let r = c.start(pid, &comm).await;
    assert_eq!(r.status.as_u16(), 202, "{}", r.text);
    let call_id: Uuid = r.body["call_id"].as_str().unwrap().parse().unwrap();
    let leg = r.body["provider_call_id"].as_str().unwrap().to_owned();
    let st = encode_client_state(&client_state(&call_id.to_string(), "counterparty"));
    for (i, (et, extra)) in [
        ("call.answered", json!({})),
        ("call.speak.ended", json!({})),
        (
            "call.transcription",
            json!({"transcription_data": {"transcript": "Can you commit to a deposit by Friday?", "is_final": true}}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut payload = json!({"call_control_id": leg, "client_state": st});
        if let Value::Object(m) = extra {
            for (k, v) in m {
                payload[k] = v;
            }
        }
        let body = serde_json::to_vec(&json!({"data": {"id": format!("ev-{call_id}-{i}"), "event_type": et, "payload": payload}})).unwrap();
        let s = c.t.send(signed(&body)).await.status;
        assert_eq!(s.as_u16(), 204);
    }
    let calls = c.calls(pid).await;
    println!("REVIEW owner-leg setup: call state {}", calls[0].state.as_str());
    assert_eq!(calls[0].state.as_str(), "HANDOFF_PENDING");
    // Outbound calls are now disabled (a restart with ALLOW_LIVE_OUTBOUND_CALLS=false).
    let mock2 = telnyx_mock();
    c.t.app.extensions.insert(Arc::new(Voice::live(Arc::new(
        LiveTelnyxProvider::with_base_url("test-key", "conn", false, &mock2.base),
    ))));
    let hs = c.t.get(&format!("/api/v1/projects/{pid}/handoffs"), Some(&c.token)).await;
    let hid = hs.body[0]["handoff_request_id"].as_str().unwrap().to_owned();
    let r = c
        .t
        .post(
            &format!("/api/v1/projects/{pid}/handoffs/{hid}/response"),
            Some(&c.token),
            &json!({"response": "ACCEPTED"}),
        )
        .await;
    let sent: Vec<String> = mock2.requests().iter().map(|r| r.path.clone()).collect();
    println!("REVIEW owner leg, outbound disabled: {} {}; carrier requests sent before the refusal: {sent:?}", r.status, r.text);
    let calls = c.calls(pid).await;
    println!("REVIEW after: call state {} conference {:?}", calls[0].state.as_str(), calls[0].conference_id);
});
