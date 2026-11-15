//! HTTP integration tests, in-process, on both stores. Ports the applicable assertions of
//! `apps/api/tests/test_api_smoke.py` and `test_approvals_comms.py`, plus isolation tests with a
//! second owner for every route of this commission.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use ss_server::app::AppState;
use ss_server::contracts::{ActionType, ApprovalRequest, Subsystem, TaskType};
use ss_server::error::AppError;
use ss_server::services::approvals::ApprovalSpec;
use uuid::Uuid;

use common::*;

fn obj(v: Value) -> serde_json::Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => panic!("not an object"),
    }
}

async fn request_approval(
    app: &AppState,
    project_id: Uuid,
    owner: Uuid,
    key: &str,
    payload: Value,
) -> ApprovalRequest {
    let mut tx = app.store.begin().await.unwrap();
    let a = app
        .approvals()
        .request(
            &mut *tx,
            ApprovalSpec::new(
                project_id,
                owner,
                "outreach",
                ActionType::PlaceCall,
                obj(payload),
                "Call the broker",
                key,
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    a
}

both_backends!(health_and_ready, |db| {
    let t = TestApp::new(db, &[]);
    let r = t.get("/health", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, json!({"status": "ok", "version": "0.1.0"}));
    assert_conforms("HealthResponse", &r.body);

    let r = t.get("/ready", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_conforms("ReadyResponse", &r.body);
    match t.kind {
        Kind::Postgres => {
            assert_eq!(r.body["checks"]["database"], "ok");
            assert_eq!(r.body["status"], "ready");
        }
        Kind::Memory => {
            assert!(r.body["checks"]["database"]
                .as_str()
                .unwrap()
                .contains("not durable"));
            assert_eq!(r.body["status"], "degraded");
        }
    }
    let providers = r.body["providers"].as_array().unwrap();
    let names: Vec<&str> = providers
        .iter()
        .map(|p| p["provider"].as_str().unwrap())
        .filter(|n| *n != "nessie") // asserted on its own below
        .collect();
    // The Nessie sandbox entry: the fixture bank unless NESSIE_PROVIDER_MODE=live with a key.
    let nessie: Vec<&Value> = providers
        .iter()
        .filter(|p| p["provider"] == "nessie")
        .collect();
    assert_eq!(nessie.len(), 1, "{}", r.text);
    if std::env::var("NESSIE_PROVIDER_MODE").is_err() {
        assert_eq!(
            *nessie[0],
            serde_json::to_value(ss_nessie::NessieBank::status(&ss_nessie::FixtureBank::new()))
                .unwrap()
        );
        assert_eq!(nessie[0]["status"], "MOCK");
    }
    assert_eq!(
        names,
        [
            "costar",
            "la_open_data",
            "matterport",
            "vexcel",
            "photon",
            "telnyx",
            "xai_voice",
            "product_catalog",
            "langchain_deep_agents"
        ]
    );
    // The ported providers report their own status, exactly as Python's fixture providers do
    // (values printed by `build_container(settings).<provider>.status()`); nothing that does not
    // exist here is reported as LIVE or MOCK.
    assert_eq!(
        providers[..4].to_vec(),
        vec![
            json!({"provider": "costar", "mode": "fixture", "status": "MOCK", "credentials_present": false, "note": "Synthetic listings (provider='fixture'). Set PROPERTY_PROVIDER_MODE=live to use the CoStar kit."}),
            json!({"provider": "la_open_data", "mode": "fixture", "status": "MOCK", "credentials_present": false, "note": "Real public records snapshot (FIXTURE_REAL); set PERMIT_PROVIDER_MODE=live for live API."}),
            json!({"provider": "matterport", "mode": "fixture", "status": "MOCK", "credentials_present": false, "note": "Synthetic rectangle geometry. Set MATTERPORT_PROVIDER_MODE=live with Model API token."}),
            json!({"provider": "vexcel", "mode": "fixture", "status": "MOCK", "credentials_present": false, "note": "No imagery in fixture mode. Set VEXCEL_PROVIDER_MODE=live with VEXCEL_API_KEY."}),
        ]
    );
    // The fixture catalog (its price table feeds the bill of materials), as Python reports it.
    assert_eq!(
        providers[7],
        json!({"provider": "product_catalog", "mode": "fixture", "status": "MOCK", "credentials_present": false, "note": "Illustrative prices only; no supplier integration."})
    );
    assert!(
        providers[4..7]
            .iter()
            .chain(&providers[8..])
            .filter(|p| p["provider"] != "nessie")
            .all(|p| p["status"] == "NOT_STARTED"),
        "{}",
        r.text
    );
    assert!(r.body["checks"].get("live_config").is_none());
});

both_backends!(ready_reports_live_providers_honestly, |db| {
    let t = TestApp::new(
        db,
        &[
            ("PROPERTY_PROVIDER_MODE", "live"),
            ("PERMIT_PROVIDER_MODE", "live"),
            ("MATTERPORT_PROVIDER_MODE", "live"),
            ("VEXCEL_PROVIDER_MODE", "live"),
            ("COSTAR_KIT_DIR", "/nonexistent-kit"),
        ],
    );
    let r = t.get("/ready", None).await;
    let providers = r.body["providers"].as_array().unwrap();
    // Python, same settings: `build_container(Settings(... mode="live" ...))`.
    assert_eq!(
        providers[..4].to_vec(),
        vec![
            json!({"provider": "costar", "mode": "live", "status": "BLOCKED", "credentials_present": false, "note": "CoStar data kit not extracted; set COSTAR_KIT_DIR."}),
            json!({"provider": "la_open_data", "mode": "live", "status": "LIVE", "credentials_present": true, "note": "Public Socrata API; app token optional. Coverage 2020-present."}),
            json!({"provider": "matterport", "mode": "live", "status": "BLOCKED", "credentials_present": false, "note": "MATTERPORT_API_KEY / MATTERPORT_API_TOKEN missing."}),
            json!({"provider": "vexcel", "mode": "live", "status": "BLOCKED", "credentials_present": false, "note": "VEXCEL_API_KEY missing."}),
        ]
    );
});

both_backends!(ready_lists_missing_live_credentials, |db| {
    let t = TestApp::new(
        db,
        &[
            ("VEXCEL_PROVIDER_MODE", "live"),
            ("MATTERPORT_PROVIDER_MODE", "live"),
        ],
    );
    let r = t.get("/ready", None).await;
    assert_eq!(
        r.body["checks"]["live_config"],
        "missing: matterport=MATTERPORT_API_KEY,MATTERPORT_API_TOKEN; vexcel=VEXCEL_API_KEY"
    );
    assert!(!r.text.contains("hunter2"));
});

both_backends!(dev_token_and_fixture, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, user) = t.dev_token().await;
    let (tok2, user2) = t.dev_token().await;
    assert_eq!(user, user2, "the demo owner is reused");
    assert_eq!(
        ss_server::security::verify_token(SECRET, &tok, "access").unwrap(),
        user
    );
    assert_eq!(
        ss_server::security::verify_token(SECRET, &tok2, "access").unwrap(),
        user
    );
    let r = t.call(Method::POST, "/api/v1/dev/token", None, None).await;
    assert_conforms("DevTokenResponse", &r.body);
    assert_eq!(r.body["token_type"], "bearer");

    let r = t.get("/api/v1/dev/fixtures/project", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, read_json("fixtures/projects/facial_studio.json"));
    assert_eq!(r.body["_meta"]["data_origin"], "FIXTURE_SYNTHETIC");
});

both_backends!(dev_token_disabled, |db| {
    for extra in [[("ALLOW_DEV_TOKENS", "false")], [("APP_ENV", "production")]] {
        let t = TestApp::new(db, &extra);
        let r = t.call(Method::POST, "/api/v1/dev/token", None, None).await;
        assert_problem(&r, 403, "FORBIDDEN");
        assert_eq!(r.body["message"], "dev tokens disabled");
    }
});

both_backends!(project_create_get_list, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, user) = t.dev_token().await;
    let env = t.create_project(&tok, &demo_project_body()).await;
    assert_conforms("ProjectEnvelope", &env);
    let pid = env["project"]["project_id"].as_str().unwrap().to_owned();
    assert_eq!(env["project"]["owner_id"], user.to_string());
    assert_eq!(env["project"]["project_status"], "ONBOARDING");
    assert_eq!(env["business_profile"]["confirmation_status"], "CONFIRMED");
    assert_eq!(env["business_profile"]["project_id"], pid.as_str());
    assert_eq!(
        env["project"]["business_profile_id"],
        env["business_profile"]["profile_id"]
    );

    let r = t.get(&format!("/api/v1/projects/{pid}"), Some(&tok)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, env);

    let plain = t
        .create_project(
            &tok,
            &json!({"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    assert!(plain["business_profile"].is_null());
    assert_eq!(plain["project"]["market"]["country"], "US");
    let empty_profile = t
        .create_project(&tok, &json!({"project_name": "y", "market": {"city": "LA", "state": "CA"}, "business_profile": {}}))
        .await;
    assert!(
        empty_profile["business_profile"].is_null(),
        "an empty dict is falsy in Python"
    );

    let r = t.get("/api/v1/projects", Some(&tok)).await;
    let ids: Vec<&str> = r
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["project_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            pid.as_str(),
            plain["project"]["project_id"].as_str().unwrap(),
            empty_profile["project"]["project_id"].as_str().unwrap()
        ]
    );
    for p in r.body.as_array().unwrap() {
        assert_conforms("ProjectContext", p);
    }

    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
        .await;
    let types: Vec<&str> = events.body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["project.profile.confirmed"]);
    let ev = &events.body["events"][0];
    assert_eq!(
        ev["idempotency_key"],
        format!(
            "profile:{}:confirmed",
            env["business_profile"]["profile_id"].as_str().unwrap()
        )
    );
    assert_eq!(ev["actor_type"], "user");
    assert_eq!(ev["actor_id"], user.to_string());
    assert_conforms_openapi("EventPage", &events.body);
});

both_backends!(unauthorized_project_access, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t.get(&format!("/api/v1/projects/{pid}"), None).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    assert_eq!(r.body["message"], "missing bearer token");
    let other = t.token_for(Uuid::new_v4());
    let r = t
        .get(&format!("/api/v1/projects/{pid}"), Some(&other))
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "project not found");
    assert_eq!(
        t.get(&format!("/api/v1/projects/{pid}/events"), Some(&other))
            .await
            .status,
        404
    );
    let bad = t
        .get(&format!("/api/v1/projects/{pid}"), Some("junk"))
        .await;
    assert_problem(&bad, 401, "UNAUTHORIZED");
    assert_eq!(bad.body["message"], "malformed token");
    // Every malformed or foreign token is a 401, never a 500.
    for raw in [
        "Basic abc",
        "Bearer",
        "bearer  ",
        "Bearer a.b.c.d",
        "Bearer \u{e9}.\u{e9}",
        "Bearer YQ.zz",
    ] {
        let req = Request::builder()
            .uri(format!("/api/v1/projects/{pid}"))
            .header(
                "authorization",
                axum::http::HeaderValue::from_bytes(raw.as_bytes()).unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        let r = t.send(req).await;
        assert_problem(&r, 401, "UNAUTHORIZED");
    }
    let wrong_secret = ss_server::security::issue_token("other-secret", Uuid::new_v4());
    assert_problem(
        &t.get("/api/v1/projects", Some(&wrong_secret)).await,
        401,
        "UNAUTHORIZED",
    );
    let media = ss_server::security::issue_token_with(SECRET, Uuid::new_v4(), 60, "media");
    let r = t.get("/api/v1/projects", Some(&media)).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    assert_eq!(r.body["message"], "token purpose mismatch");
    let expired = ss_server::security::issue_token_with(SECRET, Uuid::new_v4(), -10, "access");
    let r = t.get("/api/v1/projects", Some(&expired)).await;
    assert_eq!(r.body["message"], "token expired");
    // Lower-case scheme and surrounding spaces are accepted, as in Python.
    let req = Request::builder()
        .uri("/api/v1/projects")
        .header("authorization", format!("bearer   {tok}  "))
        .body(Body::empty())
        .unwrap();
    assert_eq!(t.send(req).await.status, StatusCode::OK);
});

both_backends!(request_validation_errors, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let send_raw = |body: Option<&'static [u8]>, ct: Option<&'static str>| {
        let mut b = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/projects")
            .header("authorization", format!("Bearer {tok}"));
        if let Some(ct) = ct {
            b = b.header("content-type", ct);
        }
        b.body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap()
    };
    let r = t.send(send_raw(None, None)).await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["message"], "request validation failed");
    assert_eq!(r.body["details"]["errors"][0]["type"], "missing");
    assert_eq!(r.body["details"]["errors"][0]["loc"], json!(["body"]));

    let r = t
        .send(send_raw(Some(b"{bad"), Some("application/json")))
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["details"]["errors"][0]["type"], "json_invalid");
    assert_eq!(r.body["details"]["errors"][0]["loc"], json!(["body", 1]));

    let r = t
        .send(send_raw(Some(b"[]"), Some("application/json")))
        .await;
    assert_eq!(
        r.body["details"]["errors"][0]["type"],
        "model_attributes_type"
    );
    let r = t.send(send_raw(Some(b"{}"), Some("text/plain"))).await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(
        r.body["details"]["errors"][0]["type"],
        "model_attributes_type"
    );

    for (body, loc) in [
        (
            json!({"project_name": "", "market": {"city": "LA", "state": "CA"}}),
            json!(["body", "project_name"]),
        ),
        (
            json!({"project_name": "x", "market": {"city": "LA", "state": "CAX"}}),
            json!(["body", "market", "state"]),
        ),
        (
            json!({"project_name": "x", "market": {"city": "LA", "state": "CA"}, "extra": 1}),
            json!(["body", "extra"]),
        ),
        (
            json!({"market": {"city": "LA", "state": "CA"}}),
            json!(["body", "project_name"]),
        ),
    ] {
        let r = t.post("/api/v1/projects", Some(&tok), &body).await;
        assert_problem(&r, 422, "VALIDATION_ERROR");
        assert_eq!(r.body["details"]["errors"][0]["loc"], loc, "{}", r.text);
    }
    // A bad nested business profile is a contract validation error raised inside the handler.
    let r = t
        .post("/api/v1/projects", Some(&tok), &json!({"project_name": "x", "market": {"city": "LA", "state": "CA"}, "business_profile": {"business_type": "spa"}}))
        .await;
    assert_problem(&r, 422, "CONTRACT_VALIDATION_ERROR");
    assert_eq!(r.body["message"], "contract validation failed");
    assert_eq!(
        t.get("/api/v1/projects", Some(&tok)).await.body,
        json!([]),
        "nothing was written"
    );

    let r = t.get("/api/v1/projects/not-a-uuid", Some(&tok)).await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["details"]["errors"][0]["type"], "uuid_parsing");
    assert_eq!(
        r.body["details"]["errors"][0]["loc"],
        json!(["path", "project_id"])
    );
    // Authentication is checked before path validation, as FastAPI's dependency order.
    assert_eq!(t.get("/api/v1/projects/not-a-uuid", None).await.status, 401);
});

both_backends!(unknown_routes_and_methods, |db| {
    let t = TestApp::new(db, &[]);
    let r = t.get("/nope", None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body, json!({"detail": "Not Found"}));
    let r = t.call(Method::DELETE, "/health", None, None).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.body, json!({"detail": "Method Not Allowed"}));
    assert!(r.headers.contains_key("x-request-id"));
});

both_backends!(request_id_and_cors, |db| {
    let t = TestApp::new(
        db,
        &[("CORS_ORIGINS", "http://localhost:3000, https://app.example")],
    );
    let req = Request::builder()
        .uri("/api/v1/projects")
        .header("x-request-id", "rid-123")
        .body(Body::empty())
        .unwrap();
    let r = t.send(req).await;
    assert_eq!(r.headers["x-request-id"], "rid-123");
    assert_eq!(r.body["request_id"], "rid-123");
    let r = t.get("/health", None).await;
    let generated = r.headers["x-request-id"].to_str().unwrap();
    assert!(Uuid::parse_str(generated).is_ok());

    let preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/v1/projects")
        .header("origin", "https://app.example")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .body(Body::empty())
        .unwrap();
    let r = t.send(preflight).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.headers["access-control-allow-origin"],
        "https://app.example"
    );
    assert_eq!(r.headers["access-control-allow-credentials"], "true");
    assert!(r.headers["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .contains("POST"));
    assert!(r.headers.contains_key("x-request-id"));
    let req = Request::builder()
        .uri("/health")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let r = t.send(req).await;
    assert!(!r.headers.contains_key("access-control-allow-origin"));
});

both_backends!(internal_errors_do_not_leak, |db| {
    let base = TestApp::new(db, &[]);
    let routes = ss_server::http::routes::api_routes()
        .route(
            "/boom",
            get(|| async { Err::<(), _>(AppError::internal("password=hunter2 in SQL")) }),
        )
        .route(
            "/panic",
            get(|| async {
                if Uuid::new_v4().is_nil() {
                    return "unreachable";
                }
                panic!("secret detail")
            }),
        );
    let router = ss_server::http::with_middleware(routes, base.app.clone());
    let t = TestApp {
        app: base.app.clone(),
        router,
        kind: base.kind,
    };
    for path in ["/boom", "/panic"] {
        let req = Request::builder()
            .uri(path)
            .header("x-request-id", "rid-500")
            .body(Body::empty())
            .unwrap();
        let r = t.send(req).await;
        assert_problem(&r, 500, "INTERNAL_ERROR");
        assert_eq!(r.body["message"], "unexpected server error");
        assert_eq!(r.body["retryable"], true);
        assert_eq!(r.body["details"], json!({}));
        assert_eq!(r.body["request_id"], "rid-500");
        assert!(!r.text.contains("hunter2") && !r.text.contains("secret detail"));
    }
});

both_backends!(business_profile_put, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let env = t
        .create_project(
            &tok,
            &json!({"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid = env["project"]["project_id"].as_str().unwrap().to_owned();
    let mut profile = demo_project_body()["business_profile"].clone();
    profile["confirmation_status"] = json!("DRAFT");
    let r = t
        .put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&tok),
            &profile,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_conforms("BusinessExpansionProfile", &r.body);
    let profile_id = r.body["profile_id"].as_str().unwrap().to_owned();
    assert_eq!(r.body["project_id"], pid.as_str());
    let got = t.get(&format!("/api/v1/projects/{pid}"), Some(&tok)).await;
    assert_eq!(
        got.body["project"]["business_profile_id"],
        profile_id.as_str()
    );
    assert_eq!(
        got.body["business_profile"]["profile_id"],
        profile_id.as_str()
    );
    // DRAFT emits nothing.
    assert_eq!(
        t.get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
            .await
            .body["events"],
        json!([])
    );

    // Update the same profile (client sends its id) and confirm it.
    let mut confirmed = r.body.clone();
    confirmed["confirmation_status"] = json!("CONFIRMED");
    let r1 = t
        .put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&tok),
            &confirmed,
        )
        .await;
    assert_eq!(r1.status, StatusCode::OK, "{}", r1.text);
    assert_eq!(r1.body["profile_id"], profile_id.as_str());
    // The same confirmed profile again: the idempotency key is stable, so no second event.
    let r2 = t
        .put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&tok),
            &confirmed,
        )
        .await;
    assert_eq!(r2.status, StatusCode::OK);
    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
        .await
        .body["events"]
        .clone();
    assert_eq!(events.as_array().unwrap().len(), 1);
    let canonical = ss_server::pyjson::dumps_canonical(&r1.body);
    let digest = hex::encode(Sha256::digest(canonical.as_bytes()));
    assert_eq!(
        events[0]["idempotency_key"],
        format!("profile:{profile_id}:confirmed:{digest}")
    );
    assert_eq!(events[0]["payload"], json!({"profile_id": profile_id}));
    // A changed confirmed profile is a new logical event.
    let mut changed = confirmed.clone();
    changed["business_description"] = json!("changed");
    t.put(
        &format!("/api/v1/projects/{pid}/business-profile"),
        Some(&tok),
        &changed,
    )
    .await;
    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
        .await
        .body["events"]
        .clone();
    assert_eq!(events.as_array().unwrap().len(), 2);

    // Validation: not an object -> request validation; bad fields -> contract validation.
    let r = t
        .put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&tok),
            &json!([1]),
        )
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["details"]["errors"][0]["type"], "dict_type");
    let r = t
        .put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&tok),
            &json!({"business_type": ""}),
        )
        .await;
    assert_problem(&r, 422, "CONTRACT_VALIDATION_ERROR");
    // A falsy profile_id means "new profile".
    let mut fresh = profile.clone();
    fresh["profile_id"] = json!("");
    let r = t
        .put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&tok),
            &fresh,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_ne!(r.body["profile_id"], profile_id.as_str());
    let got = t.get(&format!("/api/v1/projects/{pid}"), Some(&tok)).await;
    assert_eq!(
        got.body["business_profile"]["profile_id"], r.body["profile_id"],
        "latest profile wins"
    );
});

both_backends!(foreign_profile_id_cannot_be_overwritten, |db| {
    let t = TestApp::new(db, &[]);
    let (tok_a, _) = t.dev_token().await;
    let tok_b = t.token_for(Uuid::new_v4());
    let a = t
        .create_project(
            &tok_a,
            &json!({"project_name": "a", "market": {"city": "LA", "state": "CA"}}),
        )
        .await;
    let b = t.create_project(&tok_b, &demo_project_body()).await;
    let pid_a = a["project"]["project_id"].as_str().unwrap();
    let pid_b = b["project"]["project_id"].as_str().unwrap();
    let victim = b["business_profile"].clone();
    let mut attack = victim.clone();
    attack["business_description"] = json!("overwritten by another owner");
    let r = t
        .put(
            &format!("/api/v1/projects/{pid_a}/business-profile"),
            Some(&tok_a),
            &attack,
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "business profile not found");
    // The same owner's other project cannot take it either.
    let b2 = t
        .create_project(
            &tok_b,
            &json!({"project_name": "b2", "market": {"city": "LA", "state": "CA"}}),
        )
        .await;
    let pid_b2 = b2["project"]["project_id"].as_str().unwrap();
    let r = t
        .put(
            &format!("/api/v1/projects/{pid_b2}/business-profile"),
            Some(&tok_b),
            &attack,
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    let still = t
        .get(&format!("/api/v1/projects/{pid_b}"), Some(&tok_b))
        .await;
    assert_eq!(still.body["business_profile"], victim);
    // Creating a project with an existing profile_id is refused the same way.
    let mut body = demo_project_body();
    body["business_profile"]["profile_id"] = victim["profile_id"].clone();
    let r = t.post("/api/v1/projects", Some(&tok_a), &body).await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(
        t.get("/api/v1/projects", Some(&tok_a))
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );
});

both_backends!(every_route_hides_other_owners_projects, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, owner) = t.dev_token().await;
    let env = t.create_project(&tok, &demo_project_body()).await;
    let pid: Uuid = env["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let task = {
        let mut tx = t.app.store.begin().await.unwrap();
        let task = t
            .app
            .tasks()
            .create(
                &mut *tx,
                pid,
                TaskType::SearchProperties,
                Subsystem::PropertySpatial,
                Default::default(),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        task
    };
    let appr = request_approval(
        &t.app,
        pid,
        owner,
        "call-1",
        json!({"phone": "+15555550100"}),
    )
    .await;
    let intruder = t.token_for(Uuid::new_v4());
    let gets = [
        format!("/api/v1/projects/{pid}"),
        format!("/api/v1/projects/{pid}/tasks/{}", task.task_id),
        format!("/api/v1/projects/{pid}/events"),
        format!("/api/v1/projects/{pid}/events/stream"),
        format!("/api/v1/projects/{pid}/approvals"),
    ];
    for path in &gets {
        let r = t.get(path, Some(&intruder)).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "project not found", "{path}");
        // ...and the owner can read them (the stream is not read to its end).
        assert_eq!(
            t.status_of(path, Some(&tok)).await,
            StatusCode::OK,
            "{path}"
        );
    }
    let r = t
        .put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&intruder),
            &env["business_profile"],
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(
        t.get("/api/v1/projects", Some(&intruder)).await.body,
        json!([])
    );
    // Deciding someone else's approval is refused (403, as Python), and nothing changes.
    let r = t
        .post(
            &format!("/api/v1/approvals/{}/decision", appr.approval_id),
            Some(&intruder),
            &json!({"decision": "APPROVED", "action_payload_hash": appr.action_payload_hash}),
        )
        .await;
    assert_problem(&r, 403, "FORBIDDEN");
    let list = t
        .get(&format!("/api/v1/projects/{pid}/approvals"), Some(&tok))
        .await;
    assert_eq!(list.body[0]["approval_status"], "PENDING");
    // Unknown approval id: 404.
    let r = t
        .post(
            &format!("/api/v1/approvals/{}/decision", Uuid::new_v4()),
            Some(&tok),
            &json!({"decision": "APPROVED", "action_payload_hash": "x"}),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
});

both_backends!(task_route, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let p1: Uuid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let p2: Uuid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let task = t
        .app
        .tasks()
        .create(
            &mut *tx,
            p1,
            TaskType::RankProperties,
            Subsystem::AgentsFinance,
            obj(json!({"k": 1})),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let r = t
        .get(
            &format!("/api/v1/projects/{p1}/tasks/{}", task.task_id),
            Some(&tok),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_conforms("TaskRecord", &r.body);
    assert_eq!(r.body["status"], "QUEUED");
    assert_eq!(r.body["input_reference"], json!({"k": 1}));
    // The same owner's other project: the task does not belong to it.
    let r = t
        .get(
            &format!("/api/v1/projects/{p2}/tasks/{}", task.task_id),
            Some(&tok),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "task not found");
    let r = t
        .get(
            &format!("/api/v1/projects/{p1}/tasks/{}", Uuid::new_v4()),
            Some(&tok),
        )
        .await;
    assert_eq!(r.body["message"], "task not found");
    let r = t
        .get(&format!("/api/v1/projects/{p1}/tasks/nope"), Some(&tok))
        .await;
    assert_eq!(
        r.body["details"]["errors"][0]["loc"],
        json!(["path", "task_id"])
    );
    // FastAPI runs `owned_project` before validating the endpoint's `task_id`: another owner's
    // project is a 404 even with a malformed task id (Python: 404 "project not found").
    let other = t.token_for(Uuid::new_v4());
    let r = t
        .get(&format!("/api/v1/projects/{p1}/tasks/nope"), Some(&other))
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "project not found");
});

both_backends!(event_paging, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let env = t.create_project(&tok, &demo_project_body()).await;
    let pid: Uuid = env["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    for i in 0..4 {
        let task = t
            .app
            .tasks()
            .create(
                &mut *tx,
                pid,
                TaskType::AnalyzeProperty,
                Subsystem::AgentsFinance,
                Default::default(),
            )
            .await
            .unwrap();
        let _ = i;
        t.app
            .tasks()
            .run(&mut *tx, task, |_, _| {
                Box::pin(async { Ok(Default::default()) })
            })
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let all = t
        .get(
            &format!("/api/v1/projects/{pid}/events?limit=500"),
            Some(&tok),
        )
        .await;
    let events = all.body["events"].as_array().unwrap();
    assert_eq!(events.len(), 9, "1 confirmed + 4 x (started, completed)");
    let page1 = t
        .get(
            &format!("/api/v1/projects/{pid}/events?limit=4"),
            Some(&tok),
        )
        .await;
    assert_eq!(page1.body["events"].as_array().unwrap().len(), 4);
    let cursor = page1.body["next_cursor"].as_i64().unwrap();
    let page2 = t
        .get(
            &format!("/api/v1/projects/{pid}/events?after={cursor}&limit=500"),
            Some(&tok),
        )
        .await;
    assert_eq!(page2.body["events"].as_array().unwrap().len(), 5);
    assert_eq!(page2.body["events"][0], events[4]);
    let end = page2.body["next_cursor"].as_i64().unwrap();
    let empty = t
        .get(
            &format!("/api/v1/projects/{pid}/events?after={end}"),
            Some(&tok),
        )
        .await;
    assert_eq!(empty.body, json!({"events": [], "next_cursor": end}));
    let beyond = t
        .get(
            &format!("/api/v1/projects/{pid}/events?after=99999999999"),
            Some(&tok),
        )
        .await;
    assert_eq!(
        beyond.body,
        json!({"events": [], "next_cursor": 99999999999i64})
    );
    for q in ["limit=0", "limit=501", "after=-1", "after=x", "limit=1.5"] {
        let r = t
            .get(&format!("/api/v1/projects/{pid}/events?{q}"), Some(&tok))
            .await;
        assert_problem(&r, 422, "VALIDATION_ERROR");
        assert_eq!(r.body["details"]["errors"][0]["loc"][0], "query", "{q}");
    }
    let r = t
        .get(
            &format!("/api/v1/projects/{pid}/events?limit=0&after=-1"),
            Some(&tok),
        )
        .await;
    assert_eq!(
        r.body["details"]["errors"].as_array().unwrap().len(),
        2,
        "reported together"
    );
    // Ownership is checked before the query (FastAPI resolves the dependency first).
    let r = t
        .get(
            &format!("/api/v1/projects/{pid}/events?limit=0"),
            Some(&t.token_for(Uuid::new_v4())),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
});

async fn read_until(body: &mut Body, needle: &str, buf: &mut String) {
    let fut = async {
        while !buf.contains(needle) {
            match body.frame().await {
                Some(Ok(f)) => {
                    if let Some(d) = f.data_ref() {
                        buf.push_str(&String::from_utf8_lossy(d));
                    }
                }
                _ => panic!("stream ended before {needle:?}: {buf}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}: {buf}"));
}

both_backends!(event_stream, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let env = t.create_project(&tok, &demo_project_body()).await;
    let pid = env["project"]["project_id"].as_str().unwrap().to_owned();
    let listed = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
        .await
        .body["events"][0]
        .clone();

    let req = Request::builder()
        .uri(format!("/api/v1/projects/{pid}/events/stream"))
        .header("authorization", format!("Bearer {tok}"))
        .body(Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(t.router.clone(), req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["content-type"],
        "text/event-stream; charset=utf-8"
    );
    let mut body = resp.into_body();
    let mut buf = String::new();
    read_until(&mut body, ": keepalive\n\n", &mut buf).await;
    let first = buf.clone();
    let (frame, rest) = first.split_once("\n\n").unwrap();
    assert_eq!(rest, ": keepalive\n\n");
    let lines: Vec<&str> = frame.split('\n').collect();
    assert_eq!(lines[0], "id: 1");
    assert_eq!(lines[1], "event: project.profile.confirmed");
    let data = lines[2].strip_prefix("data: ").unwrap();
    assert!(
        data.starts_with("{\"schema_version\": \"1.0\", \"event_id\": "),
        "Python json.dumps separators: {data}"
    );
    assert!(data.is_ascii());
    let parsed: Value = serde_json::from_str(data).unwrap();
    assert_eq!(parsed, listed);

    // No transaction is held between polls: a write goes through while the stream is open, and
    // the next poll delivers it.
    let mut confirmed = env["business_profile"].clone();
    confirmed["business_description"] = json!("Ünïcode — description");
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        t.put(
            &format!("/api/v1/projects/{pid}/business-profile"),
            Some(&tok),
            &confirmed,
        ),
    )
    .await
    .expect("write not blocked by the stream");
    assert_eq!(r.status, StatusCode::OK);
    buf.clear();
    read_until(&mut body, "id: 2\n", &mut buf).await;
    read_until(&mut body, ": keepalive\n\n", &mut buf).await;
    assert!(buf.contains("event: project.profile.confirmed\n"));
    drop(body);

    // `after` skips what the client has seen.
    let req = Request::builder()
        .uri(format!("/api/v1/projects/{pid}/events/stream?after=2"))
        .header("authorization", format!("Bearer {tok}"))
        .body(Body::empty())
        .unwrap();
    let mut body = tower::ServiceExt::oneshot(t.router.clone(), req)
        .await
        .unwrap()
        .into_body();
    let mut buf = String::new();
    read_until(&mut body, ": keepalive\n\n", &mut buf).await;
    assert_eq!(buf, ": keepalive\n\n");
    let r = t
        .get(
            &format!("/api/v1/projects/{pid}/events/stream?after=-1"),
            Some(&tok),
        )
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
});

both_backends!(approval_decisions_over_http, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, owner) = t.dev_token().await;
    let pid: Uuid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let payload = json!({"project_id": pid.to_string(), "recipient": {"role": "broker", "name": "Test Broker", "phone": "+15555550100"}, "property_id": null});
    let appr = request_approval(&t.app, pid, owner, "call-http", payload.clone()).await;
    let listed = t
        .get(&format!("/api/v1/projects/{pid}/approvals"), Some(&tok))
        .await;
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
    assert_conforms("ApprovalRequest", &listed.body[0]);
    assert_eq!(listed.body[0]["approval_status"], "PENDING");
    let url = format!("/api/v1/approvals/{}/decision", appr.approval_id);

    // Executing before approval is refused at the action boundary.
    let mut tx = t.app.store.begin().await.unwrap();
    let e = t
        .app
        .approvals()
        .require_approved(
            &mut *tx,
            pid,
            Some(appr.approval_id),
            ActionType::PlaceCall,
            &obj(payload.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "APPROVAL_REQUIRED");
    drop(tx);
    // Hash mismatch (owner saw a different payload) is refused.
    let r = t
        .post(
            &url,
            Some(&tok),
            &json!({"decision": "APPROVED", "action_payload_hash": "0".repeat(64)}),
        )
        .await;
    assert_problem(&r, 409, "APPROVAL_PAYLOAD_MISMATCH");
    let r = t
        .post(
            &url,
            Some(&tok),
            &json!({"decision": "EXPIRED", "action_payload_hash": appr.action_payload_hash}),
        )
        .await;
    assert_problem(&r, 409, "INVALID_DECISION");
    let r = t
        .post(
            &url,
            Some(&tok),
            &json!({"decision": "MAYBE", "action_payload_hash": appr.action_payload_hash}),
        )
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    let ok = t.post(&url, Some(&tok), &json!({"decision": "APPROVED", "action_payload_hash": appr.action_payload_hash, "note": "go"})).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    assert_conforms("ApprovalRequest", &ok.body);
    assert_eq!(ok.body["approval_status"], "APPROVED");
    assert_eq!(ok.body["decided_by"], owner.to_string());
    assert!(ok.body["decision_timestamp"].is_string());
    let again = t
        .post(
            &url,
            Some(&tok),
            &json!({"decision": "REJECTED", "action_payload_hash": appr.action_payload_hash}),
        )
        .await;
    assert_problem(&again, 409, "INVALID_STATE_TRANSITION");
    assert_eq!(again.body["details"], json!({"current": "APPROVED"}));

    // Execute once (require + mark in one transaction); the second execution is refused.
    let mut tx = t.app.store.begin().await.unwrap();
    let approved = t
        .app
        .approvals()
        .require_approved(
            &mut *tx,
            pid,
            Some(appr.approval_id),
            ActionType::PlaceCall,
            &obj(payload.clone()),
        )
        .await
        .unwrap();
    let executed = t
        .app
        .approvals()
        .mark_executed(&mut *tx, approved)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(executed.approval_status.as_str(), "EXECUTED");
    let mut tx = t.app.store.begin().await.unwrap();
    let e = t
        .app
        .approvals()
        .require_approved(
            &mut *tx,
            pid,
            Some(appr.approval_id),
            ActionType::PlaceCall,
            &obj(payload),
        )
        .await
        .unwrap_err();
    assert_eq!(e.status().as_u16(), 409);
    assert_eq!(e.error_code(), "APPROVAL_REQUIRED");
    drop(tx);

    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
        .await
        .body["events"]
        .clone();
    let types: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "project.profile.confirmed",
            "approval.requested",
            "approval.decided"
        ]
    );
    assert_eq!(
        events[2]["idempotency_key"],
        format!("approval:{}:APPROVED", appr.approval_id)
    );
    assert_eq!(events[2]["payload"]["approval_status"], "APPROVED");
});
