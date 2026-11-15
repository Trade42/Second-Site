//! The routes the Python service keeps serving, forwarded to it unchanged
//! ([`crate::services::forward`] has what is passed and how failure is answered).
//!
//! The lists are explicit (methods and path templates, never a prefix wildcard), so nothing else
//! can be reached through this service: in particular nothing under `/internal/`, which is the
//! private link between the two services and is answered here with the ordinary 404.
//!
//! The raw path must match its template segment by segment before anything is forwarded: a
//! literal segment byte for byte (`bom.csv` included), and a parameter by its own rule
//! ([`segment_ok`]): `{plan_id}` is 1 to 128 characters of ASCII letters, digits, `.`, `_`, `~`
//! and `-`, not dots only; every other parameter (a UUID, for example) is ASCII letters, digits,
//! `-` and `_`. A segment with `%` (an encoded slash or dot), a dot segment or anything else is
//! answered with the ordinary 404 and never reaches the Python service, so no path can become
//! another one there.
//!
//! A route that exists natively in Rust always wins over a forwarded one. Each forwarded path is
//! mounted as a method router with no method handlers and one fallback; axum merges it with a
//! native router of the same path (in any merge order), keeps the native methods, and calls the
//! fallback only for methods the native router does not handle. The fallback forwards the
//! methods listed here and answers any other with the ordinary 405.
//!
//! History: until the Rust call flow landed (server part 4) the call routes (outreach,
//! communications, calls, call simulation, handoffs, the Telnyx webhook) were forwarded too, in
//! a temporary list; they are native now and the list is gone.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use axum::{Json, Router};
use indexmap::IndexMap;
use serde_json::json;

use crate::app::AppState;
use crate::services::forward::{Budget, PythonLink};

/// One forwarded route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forwarded {
    pub method: &'static str,
    /// An axum path template.
    pub path: &'static str,
    pub budget: Budget,
}

const fn route(method: &'static str, path: &'static str) -> Forwarded {
    Forwarded {
        method,
        path,
        budget: Budget::Default,
    }
}

const fn long(method: &'static str, path: &'static str) -> Forwarded {
    Forwarded {
        method,
        path,
        budget: Budget::Chat,
    }
}

/// What the Python service keeps for good: Photon owner messaging (owner channel, simulated
/// texts, polling, the Photon webhook), the LangChain supervisor's chat, and the voice session
/// (an xAI client secret plus instructions built from the chat's facts, in
/// `apps/api/app/agents/voice_session.py`; it gathers the same facts as the chat, so it gets the
/// chat's timeout).
pub const PYTHON_ROUTES: &[Forwarded] = &[
    route("GET", "/api/v1/owner/channel"),
    route("PUT", "/api/v1/owner/channel"),
    route("POST", "/api/v1/owner/channel/simulate-text"),
    route("POST", "/api/v1/owner/channel/poll"),
    route("POST", "/webhooks/photon"),
    long("POST", "/api/v1/projects/{project_id}/chat"),
    long("POST", "/api/v1/projects/{project_id}/voice/session"),
];

/// The Studio slice: a Python router owned by the frontend-product stream (announced on the team
/// board, post 6886711370), forwarded unchanged. None of its routes streams or uploads; some call
/// external search services, so all get the long (chat) timeout. `bom.csv` answers a plain CSV
/// body, passed back byte for byte with its content type.
pub const STUDIO_ROUTES: &[Forwarded] = &[
    long("GET", "/api/v1/studio/status"),
    long("GET", "/api/v1/studio/sample"),
    long("POST", "/api/v1/studio/plans"),
    long("GET", "/api/v1/studio/plans/{plan_id}"),
    long("POST", "/api/v1/studio/plans/{plan_id}/mutate"),
    long("POST", "/api/v1/studio/plans/{plan_id}/propose"),
    long("GET", "/api/v1/studio/plans/{plan_id}/bom"),
    long("GET", "/api/v1/studio/plans/{plan_id}/bom.csv"),
    long("POST", "/api/v1/studio/products/search"),
    long("POST", "/api/v1/studio/products/import"),
    long("POST", "/api/v1/studio/contractors/discover"),
    long("POST", "/api/v1/studio/scopes"),
];

/// Every forwarded route.
pub fn forwarded() -> impl Iterator<Item = &'static Forwarded> {
    PYTHON_ROUTES.iter().chain(STUDIO_ROUTES)
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response()
}

fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({"detail": "Method Not Allowed"})),
    )
        .into_response()
}

/// Is `seg` (raw, still percent-encoded) acceptable for the template parameter `name`?
pub fn segment_ok(name: &str, seg: &str) -> bool {
    match name {
        "plan_id" => {
            (1..=128).contains(&seg.len())
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
                && !seg.bytes().all(|b| b == b'.')
        }
        _ => {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        }
    }
}

/// Does the raw `path` match `template` segment by segment (literals exactly, parameters by
/// [`segment_ok`])?
pub fn path_matches(template: &str, path: &str) -> bool {
    let (Some(t), Some(p)) = (template.strip_prefix('/'), path.strip_prefix('/')) else {
        return false;
    };
    let (t, p): (Vec<&str>, Vec<&str>) = (t.split('/').collect(), p.split('/').collect());
    t.len() == p.len()
        && t.iter().zip(&p).all(|(ts, ps)| {
            match ts.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) => segment_ok(name, ps),
                None => ts == ps,
            }
        })
}

async fn forward(app: AppState, routes: Arc<[&'static Forwarded]>, req: Request) -> Response {
    // The path first: a path that does not match its template exactly is answered as if no
    // route matched. All entries of one router share the template.
    if !routes
        .first()
        .is_some_and(|f| path_matches(f.path, req.uri().path()))
    {
        return not_found();
    }
    let Some(f) = routes.iter().find(|f| f.method == req.method().as_str()) else {
        return method_not_allowed();
    };
    let budget = f.budget;
    PythonLink::of(&app).forward(req, budget).await
}

pub fn router() -> Router<AppState> {
    let mut by_path: IndexMap<&'static str, Vec<&'static Forwarded>> = IndexMap::new();
    for f in forwarded() {
        by_path.entry(f.path).or_default().push(f);
    }
    let mut router = Router::new();
    for (path, routes) in by_path {
        let routes: Arc<[&'static Forwarded]> = routes.into();
        let fallback = move |State(app): State<AppState>, req: Request| {
            let routes = routes.clone();
            async move { forward(app, routes, req).await }
        };
        router = router.route(path, MethodRouter::new().fallback(fallback));
    }
    router
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "/api/v1/projects/{project_id}/chat";
    const VOICE: &str = "/api/v1/projects/{project_id}/voice/session";
    const PLAN: &str = "/api/v1/studio/plans/{plan_id}/bom.csv";

    #[test]
    fn paths_must_match_their_template_exactly() {
        for (t, ok) in [
            ("/api/v1/owner/channel", "/api/v1/owner/channel"),
            (
                "/api/v1/owner/channel/simulate-text",
                "/api/v1/owner/channel/simulate-text",
            ),
            (
                CHAT,
                "/api/v1/projects/0b5d3c4e-8f1a-4c2b-9d3e-1f2a3b4c5d6e/chat",
            ),
            ("/webhooks/photon", "/webhooks/photon"),
            (
                VOICE,
                "/api/v1/projects/0b5d3c4e-8f1a-4c2b-9d3e-1f2a3b4c5d6e/voice/session",
            ),
            (PLAN, "/api/v1/studio/plans/plan_01.v2~a-b/bom.csv"),
            (PLAN, "/api/v1/studio/plans/..a/bom.csv"),
            (PLAN, "/api/v1/studio/plans/a../bom.csv"),
        ] {
            assert!(path_matches(t, ok), "{ok}");
        }
        let long_id = format!("/api/v1/studio/plans/{}/bom.csv", "x".repeat(128));
        assert!(path_matches(PLAN, &long_id));
        let too_long = format!("/api/v1/studio/plans/{}/bom.csv", "x".repeat(129));
        assert!(!path_matches(PLAN, &too_long));
        for (t, bad) in [
            (
                CHAT,
                "/api/v1/projects/..%2F..%2Finternal%2Fv1%2Fowner%2Fnotify/chat",
            ),
            (CHAT, "/api/v1/projects/%2e%2e/chat"),
            (CHAT, "/api/v1/projects/../chat"),
            (CHAT, "/api/v1/projects/a.b/chat"),
            (CHAT, "/api/v1/projects//chat"),
            (CHAT, "/api/v1/projects/x/chat/"),
            (VOICE, "/api/v1/projects/../voice/session"),
            (VOICE, "/api/v1/projects/%2e%2e/voice/session"),
            (VOICE, "/api/v1/projects/x/voice/session/"),
            (VOICE, "/api/v1/projects/x/voice"),
            (VOICE, "/api/v1/projects/x/voice/sessions"),
            (VOICE, "/api/v1/projects/x/voice%2Fsession"),
            ("/webhooks/photon", "/webhooks/photon/"),
            ("/webhooks/photon", "webhooks/photon"),
            ("/webhooks/photon", ""),
            (PLAN, "/api/v1/studio/plans/./bom.csv"),
            (PLAN, "/api/v1/studio/plans/../bom.csv"),
            (PLAN, "/api/v1/studio/plans/.../bom.csv"),
            (PLAN, "/api/v1/studio/plans/a%2Fb/bom.csv"),
            (PLAN, "/api/v1/studio/plans/%2e%2e/bom.csv"),
            (PLAN, "/api/v1/studio/plans/a%20b/bom.csv"),
            (PLAN, "/api/v1/studio/plans/a b/bom.csv"),
            (PLAN, "/api/v1/studio/plans//bom.csv"),
            (PLAN, "/api/v1/studio/plans/a/b/bom.csv"),
            (PLAN, "/api/v1/studio/plans/a/bom.CSV"),
            (PLAN, "/api/v1/studio/plans/a/bom%2Ecsv"),
        ] {
            assert!(!path_matches(t, bad), "{bad}");
        }
    }

    #[test]
    fn the_lists_are_explicit_and_disjoint() {
        let all: Vec<(&str, &str)> = forwarded().map(|f| (f.method, f.path)).collect();
        let mut dedup = all.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), all.len());
        assert_eq!(PYTHON_ROUTES.len(), 7);
        assert_eq!(STUDIO_ROUTES.len(), 12);
        for f in forwarded() {
            assert!(!f.path.contains('*'), "no wildcard: {}", f.path);
            assert!(!f.path.starts_with("/internal"), "{}", f.path);
            assert!(matches!(f.method, "GET" | "PUT" | "POST"), "{}", f.method);
        }
        for f in STUDIO_ROUTES {
            assert!(f.path.starts_with("/api/v1/studio/"), "{}", f.path);
            assert_eq!(f.budget, Budget::Chat, "{}", f.path);
        }
        // The call routes are native now (server part 4): none is forwarded.
        for f in forwarded() {
            for native in [
                "/outreach",
                "/communications",
                "/calls",
                "/handoffs",
                "/webhooks/telnyx",
            ] {
                assert!(!f.path.contains(native), "{}", f.path);
            }
        }
        assert_eq!(
            PYTHON_ROUTES
                .iter()
                .filter(|f| f.budget == Budget::Chat)
                .map(|f| f.path)
                .collect::<Vec<_>>(),
            [CHAT, VOICE]
        );
    }
}
