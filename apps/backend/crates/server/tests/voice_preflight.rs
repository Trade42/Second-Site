//! A live call is not dialed unless the server can hear what happens on it (voice handoff check,
//! H1): in live mode the dial is refused, before any request to the carrier, while a setting the
//! call's events depend on is missing or plainly unusable. The carrier here is a local mock
//! server; no test places a call or sends anything to a provider.

mod common;

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_domain::integrations::telnyx::{StreamOptions, StreamTrack, TelephonyError};
use ss_server::contracts::{ActionType, ApprovalStatus};
use ss_server::integrations::telnyx::Voice;
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{CallOrchestrator, StartCall};
use ss_server::store::comms::CALLS;
use ss_server::store::repo::APPROVALS;
use ss_server::store::Query;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::{test_settings, TestApp};

// Recognisable values: none may appear in a refusal or in `/ready`.
const API_KEY: &str = "KEY-secret-value-1";
const PUBLIC_KEY: &str = "PUBKEY-value-2";
const CONNECTION: &str = "conn-value-3";
const NUMBER: &str = "+15555550111";
const BASE: &str = "https://calls.example.com";
const XAI: &str = "XAI-secret-value-4";

/// Live voice, the outbound switch on, every setting a live call needs.
const COMPLETE: [(&str, &str); 7] = [
    ("VOICE_PROVIDER_MODE", "live"),
    ("ALLOW_LIVE_OUTBOUND_CALLS", "true"),
    ("TELNYX_API_KEY", API_KEY),
    ("TELNYX_CONNECTION_ID", CONNECTION),
    ("TELNYX_FROM_NUMBER", NUMBER),
    ("TELNYX_PUBLIC_KEY", PUBLIC_KEY),
    ("PUBLIC_BASE_URL", BASE),
];

/// `COMPLETE` without the named settings, plus `extra` (a later pair wins).
fn vars(
    without: &[&str],
    extra: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    COMPLETE
        .iter()
        .copied()
        .filter(|(k, _)| !without.contains(k))
        .chain(extra.iter().copied())
        .collect()
}

fn carrier() -> MockServer {
    MockServer::start_with(|_req: &Recorded| Canned {
        status: 200,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&json!({"data": {"call_control_id": "v3:abc"}})).unwrap(),
    })
}

fn no_value_in(text: &str) {
    for v in [
        API_KEY,
        PUBLIC_KEY,
        CONNECTION,
        NUMBER,
        BASE,
        XAI,
        "calls.example.com",
    ] {
        assert!(!text.contains(v), "a value is printed: {text}");
    }
}

#[test]
fn the_helper_names_what_a_live_call_lacks() {
    // Fixture mode (the default) needs nothing.
    assert!(test_settings(&[]).voice_live_missing().is_empty());
    assert!(test_settings(&[("VOICE_BRIDGE_ENABLED", "true")])
        .voice_live_missing()
        .is_empty());
    // Live with nothing else: every name, the default base URL (http://localhost:8000) included.
    assert_eq!(
        test_settings(&[("VOICE_PROVIDER_MODE", "live")]).voice_live_missing(),
        vec![
            "TELNYX_API_KEY",
            "TELNYX_CONNECTION_ID",
            "TELNYX_FROM_NUMBER",
            "TELNYX_PUBLIC_KEY",
            "PUBLIC_BASE_URL"
        ]
    );
    assert!(test_settings(&vars(&[], &[]))
        .voice_live_missing()
        .is_empty());
    // The bridge switch adds the xAI key.
    assert_eq!(
        test_settings(&vars(&[], &[("VOICE_BRIDGE_ENABLED", "true")])).voice_live_missing(),
        vec!["XAI_API_KEY"]
    );
    assert!(test_settings(&vars(
        &[],
        &[("VOICE_BRIDGE_ENABLED", "true"), ("XAI_API_KEY", XAI)]
    ))
    .voice_live_missing()
    .is_empty());
    // A base URL the carrier cannot reach, or from which no `wss` address can be built.
    for bad in [
        "http://calls.example.com",
        "https://localhost:8010",
        "https://app.localhost",
        "https://127.0.0.1:8010",
        "https://10.0.0.5",
        "https://192.168.1.20:8443/x",
        "https://172.16.0.1",
        "https://169.254.1.1",
        "https://0.0.0.0",
        "https://[::1]:8010",
        "https://[fd00::1]",
        "https://[fe80::1]",
        "https://[::ffff:127.0.0.1]",
        "https://user@127.0.0.1",
        "https://backend",
        "https://printer.local",
        "https://",
        "calls.example.com",
        "wss://calls.example.com",
        "",
        // Review fix: the value is used as written, so `{base}/webhooks/telnyx` must be a path
        // this server has; and addresses the carrier cannot reach in other spellings.
        "https://calls.example.com/",
        "https://calls.example.com:8443/base",
        "https://calls.example.com?x=1",
        " https://calls.example.com",
        "https://calls.example.com ",
        "https://127.1",
        "https://100.64.0.1",
    ] {
        assert_eq!(
            test_settings(&vars(&[], &[("PUBLIC_BASE_URL", bad)])).voice_live_missing(),
            vec!["PUBLIC_BASE_URL"],
            "{bad}"
        );
    }
    for good in [
        "https://calls.example.com",
        "https://calls.example.com:8443",
        "https://8.8.8.8",
        "https://[2606:4700::1111]",
    ] {
        assert!(
            test_settings(&vars(&[], &[("PUBLIC_BASE_URL", good)]))
                .voice_live_missing()
                .is_empty(),
            "{good}"
        );
    }
    // A caller number that is not a number, and blank values.
    for bad in ["5555550111", "+1555", "+1555abc0111", " "] {
        assert_eq!(
            test_settings(&vars(&[], &[("TELNYX_FROM_NUMBER", bad)])).voice_live_missing(),
            vec!["TELNYX_FROM_NUMBER"],
            "{bad}"
        );
    }
    assert_eq!(
        test_settings(&vars(
            &[],
            &[("TELNYX_PUBLIC_KEY", "  "), ("TELNYX_CONNECTION_ID", "")]
        ))
        .voice_live_missing(),
        vec!["TELNYX_CONNECTION_ID", "TELNYX_PUBLIC_KEY"]
    );
}

#[test]
fn a_live_dial_is_refused_before_any_request_and_names_every_missing_setting() {
    let server = carrier();
    let base = format!("{}/v2", server.base);
    let stream = StreamOptions {
        stream_url: "wss://calls.example.com/api/v1/voice/stream/b?token=t".into(),
        track: StreamTrack::Inbound,
        stream_codec: None,
        bidirectional: None,
        auth_token: None,
    };

    // Everything but the key and the switch is missing: one message, every name, no value.
    let s = test_settings(&vars(
        &[
            "TELNYX_CONNECTION_ID",
            "TELNYX_FROM_NUMBER",
            "TELNYX_PUBLIC_KEY",
            "PUBLIC_BASE_URL",
        ],
        &[("VOICE_BRIDGE_ENABLED", "true")],
    ));
    let voice = Voice::from_settings_with_base_url(&s, &base);
    assert_eq!(voice.mode(), "live");
    let refused = [
        voice.telephony.dial(
            "+15555550100",
            NUMBER,
            "http://localhost:8000/webhooks/telnyx",
            "k1",
            None,
        ),
        voice.telephony.dial_streaming(
            "+15555550100",
            NUMBER,
            "http://localhost:8000/webhooks/telnyx",
            "k2",
            None,
            &stream,
        ),
    ];
    for r in refused {
        let Err(TelephonyError::Forbidden(m)) = r else {
            panic!("not refused: {r:?}");
        };
        assert_eq!(
            m,
            "live call not dialed, nothing was sent to the carrier: these settings are missing \
             or unusable: TELNYX_CONNECTION_ID, TELNYX_FROM_NUMBER, TELNYX_PUBLIC_KEY, \
             PUBLIC_BASE_URL, XAI_API_KEY (the server could not receive or verify the call's \
             events; PUBLIC_BASE_URL must be https://<public host>, no path, no trailing \
             slash)"
        );
        no_value_in(&m);
    }
    assert!(server.requests().is_empty());

    // Each setting alone is enough to refuse.
    for (without, extra, name) in [
        (vec!["TELNYX_PUBLIC_KEY"], vec![], "TELNYX_PUBLIC_KEY"),
        (vec!["PUBLIC_BASE_URL"], vec![], "PUBLIC_BASE_URL"),
        (
            vec![],
            vec![("PUBLIC_BASE_URL", "https://127.0.0.1:8010")],
            "PUBLIC_BASE_URL",
        ),
        (vec!["TELNYX_CONNECTION_ID"], vec![], "TELNYX_CONNECTION_ID"),
        (vec!["TELNYX_FROM_NUMBER"], vec![], "TELNYX_FROM_NUMBER"),
        (
            vec![],
            vec![("VOICE_BRIDGE_ENABLED", "true")],
            "XAI_API_KEY",
        ),
    ] {
        let s = test_settings(&vars(&without, &extra));
        let voice = Voice::from_settings_with_base_url(&s, &base);
        match voice.telephony.dial(
            "+15555550100",
            NUMBER,
            "https://calls.example.com/webhooks/telnyx",
            "k",
            None,
        ) {
            Err(TelephonyError::Forbidden(m)) => {
                assert!(m.contains(&format!("unusable: {name} (")), "{m}");
                no_value_in(&m);
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    assert!(server.requests().is_empty());

    // The outbound switch off is still the first refusal, with its own text.
    let s = test_settings(&vars(
        &["TELNYX_PUBLIC_KEY", "ALLOW_LIVE_OUTBOUND_CALLS"],
        &[],
    ));
    match Voice::from_settings_with_base_url(&s, &base)
        .telephony
        .dial("+15555550100", NUMBER, "x", "k", None)
    {
        Err(TelephonyError::Forbidden(m)) => assert_eq!(
            m,
            "live outbound calls disabled (ALLOW_LIVE_OUTBOUND_CALLS=false)"
        ),
        other => panic!("{other:?}"),
    }
    assert!(server.requests().is_empty());

    // With every setting present the guard lets the dial through (to the mock carrier only).
    let s = test_settings(&vars(&[], &[]));
    let ok = Voice::from_settings_with_base_url(&s, &base)
        .telephony
        .dial(
            "+15555550100",
            NUMBER,
            "https://calls.example.com/webhooks/telnyx",
            "k",
            None,
        )
        .unwrap();
    assert_eq!(ok.call_control_id, "v3:abc");
    assert_eq!(server.requests().len(), 1);

    // Live mode without the API key selects the fixture provider (Python's selection): no
    // carrier client exists, nothing can be dialed, and `/ready` names the key (below).
    let s = test_settings(&vars(&["TELNYX_API_KEY"], &[]));
    assert_eq!(
        Voice::from_settings_with_base_url(&s, &base).mode(),
        "fixture"
    );
    assert_eq!(s.voice_live_missing(), vec!["TELNYX_API_KEY"]);
}

// Through the orchestrator, with an APPROVED approval and the outbound switch on: 403, nothing
// sent, no call stored, and the approval is still APPROVED, so the owner can fix the settings
// and start the same approved call again.
both_backends!(a_refused_dial_leaves_the_approval_unused, |db| {
    let server = carrier();
    let base = format!("{}/v2", server.base);
    let t = TestApp::new(db, &vars(&["TELNYX_PUBLIC_KEY", "PUBLIC_BASE_URL"], &[]));
    let voice = Voice::from_settings_with_base_url(&t.app.settings, &base);
    assert_eq!(voice.mode(), "live");
    t.app.extensions.insert(Arc::new(voice));
    let owner = Uuid::new_v4();
    let p = t
        .create_project(
            &t.token_for(owner),
            &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid: Uuid = p["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut recipient = IndexMap::new();
    recipient.insert("phone".to_owned(), "+15555550100".to_owned());
    let mut tx = t.app.store.begin().await.unwrap();
    let a = t
        .app
        .approvals()
        .request(
            &mut *tx,
            ApprovalSpec::new(
                pid,
                owner,
                "t",
                ActionType::PlaceCall,
                ss_domain::communications::call_payload(pid, &recipient, None),
                "call",
                format!("c-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    t.app
        .approvals()
        .decide(
            &mut *tx,
            a.approval_id,
            owner,
            ApprovalStatus::Approved,
            &a.action_payload_hash,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let start = || StartCall {
        project_id: pid,
        owner_id: owner,
        recipient: recipient.clone(),
        property_id: None,
        approval_id: Some(a.approval_id),
        intro_recording_reference: None,
        communication_id: None,
        simulated: false,
    };
    let mut tx = t.app.store.begin().await.unwrap();
    let err = CallOrchestrator::new(&t.app)
        .start(&mut *tx, start())
        .await
        .unwrap_err();
    assert_eq!(err.status().as_u16(), 403);
    let text = format!("{err:?}");
    assert!(
        text.contains("TELNYX_PUBLIC_KEY, PUBLIC_BASE_URL"),
        "{text}"
    );
    no_value_in(&text);
    drop(tx);
    assert!(server.requests().is_empty());
    let mut tx = t.app.store.begin().await.unwrap();
    assert!(CALLS
        .list(&mut *tx, Query::new().eq("project_id", pid))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        APPROVALS
            .require(&mut *tx, a.approval_id)
            .await
            .unwrap()
            .approval_status,
        ApprovalStatus::Approved
    );
    tx.commit().await.unwrap();

    // The settings fixed (a client built from complete settings), the same approval dials once.
    let fixed = test_settings(&vars(&[], &[]));
    t.app
        .extensions
        .insert(Arc::new(Voice::from_settings_with_base_url(&fixed, &base)));
    let mut tx = t.app.store.begin().await.unwrap();
    let call = CallOrchestrator::new(&t.app)
        .start(&mut *tx, start())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(call.provider_call_id.as_deref(), Some("v3:abc"));
    assert_eq!(server.requests().len(), 1);
});

fn telnyx_line(ready: &Value) -> Value {
    ready["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "telnyx")
        .unwrap()
        .clone()
}

both_backends!(ready_names_the_missing_settings_in_live_mode_only, |db| {
    // Fixture mode: no such check, and the Telnyx line is what it was.
    let fixture = TestApp::new(db, &[]).get("/ready", None).await;
    assert!(fixture.body["checks"].get("telnyx_live_dial").is_none());
    assert_eq!(telnyx_line(&fixture.body)["mode"], "fixture");

    // Live, incomplete: the names, no value; the provider line itself is unchanged.
    let t = TestApp::new(
        db,
        &vars(
            &["TELNYX_PUBLIC_KEY", "TELNYX_FROM_NUMBER"],
            &[("PUBLIC_BASE_URL", "https://127.0.0.1:8010")],
        ),
    );
    let ready = t.get("/ready", None).await;
    assert_eq!(
        ready.body["checks"]["telnyx_live_dial"],
        "refused until set: TELNYX_FROM_NUMBER, TELNYX_PUBLIC_KEY, PUBLIC_BASE_URL"
    );
    no_value_in(&ready.text);
    assert_eq!(
        telnyx_line(&ready.body),
        json!({"provider": "telnyx", "mode": "live", "status": "PARTIAL", "credentials_present": true,
               "note": "Call flow (dial, intro, transcription turns, handoff conference) per API reference; never dialed from CI."})
    );

    // Live without the API key: the fixture provider is selected, and the check names the key.
    let t = TestApp::new(db, &vars(&["TELNYX_API_KEY"], &[]));
    let ready = t.get("/ready", None).await;
    assert_eq!(
        ready.body["checks"]["telnyx_live_dial"],
        "refused until set: TELNYX_API_KEY"
    );

    // Live, complete: no check.
    let t = TestApp::new(db, &vars(&[], &[]));
    let ready = t.get("/ready", None).await;
    assert!(ready.body["checks"].get("telnyx_live_dial").is_none());
    no_value_in(&ready.text);
});
