//! Test harness: drives the router in-process on the in-memory store always, and again on
//! PostgreSQL when `TEST_DATABASE_URL` is set (each test gets its own schema, migrated by the
//! Rust runner). Without `TEST_DATABASE_URL` the PostgreSQL variants print `SKIPPED` and return.

#![allow(dead_code)]

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgConnectOptions;
use sqlx::ConnectOptions;
use ss_server::app::AppState;
use ss_server::config::Settings;
use ss_server::store::memory::MemoryStore;
use ss_server::store::postgres::PgStore;
use ss_server::store::Store;
use tower::ServiceExt;
use uuid::Uuid;

pub const SECRET: &str = "test-secret";

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .expect("repository root")
}

pub fn read_json(rel: &str) -> Value {
    let path = repo_root().join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

pub fn golden(name: &str) -> Value {
    read_json(&format!("apps/backend/tests-golden/server/{name}"))
}

fn check(schema: &Value, name: &str, instance: &Value) {
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .unwrap_or_else(|e| panic!("schema {name}: {e}"));
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{name} does not conform to its JSON Schema: {errors:#?}\n{instance:#}"
    );
}

/// Validate against `packages/contracts/schemas/<name>.json`.
pub fn assert_conforms(name: &str, instance: &Value) {
    let schema = read_json(&format!("packages/contracts/schemas/{name}.json"));
    check(&schema, name, instance);
}

/// The schema must reject `instance` (proves the check has teeth).
pub fn assert_rejected(name: &str, instance: &Value) {
    let schema = read_json(&format!("packages/contracts/schemas/{name}.json"));
    let validator = jsonschema::options().build(&schema).expect("schema");
    assert!(
        !validator.is_valid(instance),
        "{name} accepted {instance:#}"
    );
}

/// Validate against `components/schemas/<name>` of `packages/contracts/openapi.json`.
pub fn assert_conforms_openapi(name: &str, instance: &Value) {
    let openapi = read_json("packages/contracts/openapi.json");
    let schema = json!({
        "$ref": format!("#/components/schemas/{name}"),
        "components": openapi["components"],
    });
    check(&schema, name, instance);
}

pub fn test_settings(extra: &[(&str, &str)]) -> Settings {
    let mut vars: Vec<(String, String)> = vec![
        ("APP_ENV".into(), "test".into()),
        ("AUTH_SECRET".into(), SECRET.into()),
        ("SUPERVISOR_MODEL".into(), "".into()),
    ];
    for (k, v) in extra {
        vars.push(((*k).into(), (*v).into()));
    }
    Settings::from_vars(vars).expect("settings")
}

pub fn pg_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

/// A fresh PostgreSQL schema, migrated by the Rust runner. Returns the store and the schema name.
pub async fn pg_store() -> (Arc<PgStore>, String) {
    let url = pg_url().expect("TEST_DATABASE_URL");
    let base = PgConnectOptions::from_str(&ss_server::config::normalize_database_url(&url))
        .expect("TEST_DATABASE_URL");
    let schema = format!("rt_{}", Uuid::new_v4().simple());
    let mut admin = base
        .clone()
        .connect()
        .await
        .expect("connect to TEST_DATABASE_URL");
    sqlx::Executor::execute(&mut admin, format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create schema");
    drop(admin);
    let opts = base.options([("search_path", schema.as_str())]);
    let store = PgStore::connect_with(opts, 4).await.expect("pool");
    store.migrate().await.expect("migrate");
    (Arc::new(store), schema)
}

pub async fn drop_schema(schema: &str) {
    if let Some(url) = pg_url() {
        if let Ok(opts) =
            PgConnectOptions::from_str(&ss_server::config::normalize_database_url(&url))
        {
            if let Ok(mut c) = opts.connect().await {
                let _ = sqlx::Executor::execute(
                    &mut c,
                    format!("DROP SCHEMA {schema} CASCADE").as_str(),
                )
                .await;
            }
        }
    }
}

/// Which backend a test runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Memory,
    Postgres,
}

/// A store for one test, cleaned up by [`TestDb::finish`].
pub struct TestDb {
    pub store: Arc<dyn Store>,
    pub kind: Kind,
    schema: Option<String>,
}

impl TestDb {
    pub async fn new(kind: Kind) -> Option<TestDb> {
        match kind {
            Kind::Memory => Some(TestDb {
                store: Arc::new(MemoryStore::new()),
                kind,
                schema: None,
            }),
            Kind::Postgres => {
                if pg_url().is_none() {
                    eprintln!("SKIPPED: TEST_DATABASE_URL is not set; PostgreSQL variant not run");
                    return None;
                }
                let (store, schema) = pg_store().await;
                Some(TestDb {
                    store,
                    kind,
                    schema: Some(schema),
                })
            }
        }
    }

    pub async fn finish(mut self) {
        if let Some(s) = self.schema.take() {
            drop_schema(&s).await;
        }
    }

    /// The PostgreSQL schema of this test (`None` on the in-memory store).
    pub fn schema_name(&self) -> Option<&str> {
        self.schema.as_deref()
    }
}

/// A test that panics never reaches [`TestDb::finish`]: drop its schema here instead, on a
/// separate thread with its own runtime (this may run during unwinding, inside a runtime).
impl Drop for TestDb {
    fn drop(&mut self) {
        if let Some(s) = self.schema.take() {
            let _ = std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(drop_schema(&s));
                }
            })
            .join();
        }
    }
}

/// Does this schema exist in the `TEST_DATABASE_URL` database?
pub async fn schema_exists(schema: &str) -> bool {
    let url = pg_url().expect("TEST_DATABASE_URL");
    let opts = PgConnectOptions::from_str(&ss_server::config::normalize_database_url(&url))
        .expect("TEST_DATABASE_URL");
    let mut c = opts.connect().await.expect("connect");
    let row: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM information_schema.schemata WHERE schema_name = $1",
    )
    .bind(schema)
    .fetch_one(&mut c)
    .await
    .expect("query");
    row.0 == 1
}

/// The application plus an in-process client.
pub struct TestApp {
    pub app: AppState,
    pub router: Router,
    pub kind: Kind,
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
    pub text: String,
}

impl TestApp {
    pub fn new(db: &TestDb, extra_settings: &[(&str, &str)]) -> TestApp {
        let app = ss_server::build_app(test_settings(extra_settings), db.store.clone());
        let router = ss_server::http::router(app.clone());
        TestApp {
            app,
            router,
            kind: db.kind,
        }
    }

    pub async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.router.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp {
            status,
            headers,
            body,
            text,
        }
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Resp {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(v).expect("json")))
                .expect("request"),
            None => b.body(Body::empty()).expect("request"),
        };
        self.send(req).await
    }

    /// Status of a GET without reading the body (for streams that do not end).
    pub async fn status_of(&self, path: &str, token: Option<&str>) -> StatusCode {
        let mut b = Request::builder().method(Method::GET).uri(path);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let resp = self
            .router
            .clone()
            .oneshot(b.body(Body::empty()).expect("request"))
            .await
            .expect("infallible");
        resp.status()
    }

    pub async fn get(&self, path: &str, token: Option<&str>) -> Resp {
        self.call(Method::GET, path, token, None).await
    }

    pub async fn post(&self, path: &str, token: Option<&str>, body: &Value) -> Resp {
        self.call(Method::POST, path, token, Some(body)).await
    }

    pub async fn put(&self, path: &str, token: Option<&str>, body: &Value) -> Resp {
        self.call(Method::PUT, path, token, Some(body)).await
    }

    /// `POST /api/v1/dev/token` -> (token, user_id).
    pub async fn dev_token(&self) -> (String, Uuid) {
        let r = self
            .call(Method::POST, "/api/v1/dev/token", None, None)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        (
            r.body["access_token"].as_str().expect("token").to_owned(),
            r.body["user_id"]
                .as_str()
                .expect("user")
                .parse()
                .expect("uuid"),
        )
    }

    /// A token for a brand-new user (a second owner).
    pub fn token_for(&self, user: Uuid) -> String {
        ss_server::security::issue_token(SECRET, user)
    }

    pub async fn create_project(&self, token: &str, body: &Value) -> Value {
        let r = self.post("/api/v1/projects", Some(token), body).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        r.body
    }
}

/// `demo_project_body` fixture of `apps/api/tests/conftest.py`.
pub fn demo_project_body() -> Value {
    let fx = read_json("fixtures/projects/facial_studio.json");
    json!({
        "project_name": fx["project_name"],
        "market": fx["market"],
        "business_profile": fx["business_profile"],
    })
}

/// The ProblemDetail key set the Python smoke test asserts.
pub fn assert_problem(r: &Resp, status: u16, code: &str) {
    assert_eq!(r.status.as_u16(), status, "{}", r.text);
    let keys: std::collections::BTreeSet<&str> = r
        .body
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {}", r.text))
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "details",
            "error_code",
            "message",
            "request_id",
            "retryable"
        ]
        .into_iter()
        .collect(),
        "{}",
        r.text
    );
    assert_eq!(r.body["error_code"], code, "{}", r.text);
    assert_conforms("ProblemDetail", &r.body);
}

/// Define a test that runs once on the in-memory store and once on PostgreSQL.
/// The body receives a `&TestDb`.
#[macro_export]
macro_rules! both_backends {
    ($name:ident, |$db:ident| $body:block) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            async fn run($db: &$crate::common::TestDb) $body

            #[tokio::test]
            async fn memory() {
                let db = $crate::common::TestDb::new($crate::common::Kind::Memory).await.expect("memory");
                run(&db).await;
                db.finish().await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(db) = $crate::common::TestDb::new($crate::common::Kind::Postgres).await else {
                    return;
                };
                run(&db).await;
                db.finish().await;
            }
        }
    };
}
