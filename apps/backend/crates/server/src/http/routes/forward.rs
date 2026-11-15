//! The routes the Python service keeps serving, forwarded to it unchanged
//! ([`crate::services::forward`] has what is passed and how failure is answered).
//!
//! The list is explicit (methods and path templates, never a prefix wildcard), so nothing else
//! can be reached through this service: in particular nothing under `/internal/`, which is the
//! private link between the two services and is answered here with the ordinary 404. A path
//! parameter is forwarded only when it is one plain segment of ASCII letters, digits, `-` and
//! `_` (a UUID, for example): a segment with `%`, `.` or anything else is answered with the
//! ordinary 404 and never reaches the Python service, so percent-encoded slashes or dot
//! segments cannot turn a forwarded path into another one there.
//!
//! A route that exists natively in Rust always wins over a forwarded one. Each forwarded path is
//! mounted as a method router with no method handlers and one fallback; axum merges it with a
//! native router of the same path (in any merge order), keeps the native methods, and calls the
//! fallback only for methods the native router does not handle. The fallback forwards the
//! methods listed here and answers any other with the ordinary 405.

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

/// What the Python service keeps for good: Photon owner messaging (owner channel, simulated
/// texts, polling, the Photon webhook) and the LangChain supervisor's chat.
pub const PYTHON_ROUTES: &[Forwarded] = &[
    route("GET", "/api/v1/owner/channel"),
    route("PUT", "/api/v1/owner/channel"),
    route("POST", "/api/v1/owner/channel/simulate-text"),
    route("POST", "/api/v1/owner/channel/poll"),
    route("POST", "/webhooks/photon"),
    Forwarded {
        method: "POST",
        path: "/api/v1/projects/{project_id}/chat",
        budget: Budget::Chat,
    },
];

/// TEMPORARY: the call flow, which Python still serves until the Rust call flow (server part 4)
/// lands, so the switch-over does not wait for it. Part 4 removes these entries one by one as it
/// implements each route natively (a native route wins over an entry left here anyway).
pub const CALL_FLOW_ROUTES_UNTIL_PART_4: &[Forwarded] = &[
    route("POST", "/api/v1/projects/{project_id}/outreach"),
    route("GET", "/api/v1/projects/{project_id}/communications"),
    route("POST", "/api/v1/projects/{project_id}/calls"),
    route("GET", "/api/v1/projects/{project_id}/calls"),
    route(
        "POST",
        "/api/v1/projects/{project_id}/calls/{call_id}/simulate",
    ),
    route("GET", "/api/v1/projects/{project_id}/handoffs"),
    route(
        "POST",
        "/api/v1/projects/{project_id}/handoffs/{handoff_id}/response",
    ),
    route("POST", "/webhooks/telnyx"),
];

/// Every forwarded route.
pub fn forwarded() -> impl Iterator<Item = &'static Forwarded> {
    PYTHON_ROUTES.iter().chain(CALL_FLOW_ROUTES_UNTIL_PART_4)
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

/// One plain segment per `/`: ASCII letters, digits, `-`, `_`.
fn plain_path(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|rest| {
        rest.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    })
}

async fn forward(app: AppState, routes: Arc<[&'static Forwarded]>, req: Request) -> Response {
    // The path first: a segment that is not plain is answered as if no route matched.
    if !plain_path(req.uri().path()) {
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

    #[test]
    fn only_plain_segments_are_forwarded() {
        for ok in [
            "/api/v1/owner/channel",
            "/api/v1/owner/channel/simulate-text",
            "/api/v1/projects/0b5d3c4e-8f1a-4c2b-9d3e-1f2a3b4c5d6e/chat",
            "/webhooks/photon",
        ] {
            assert!(plain_path(ok), "{ok}");
        }
        for bad in [
            "/api/v1/projects/..%2F..%2Finternal%2Fv1%2Fowner%2Fnotify/chat",
            "/api/v1/projects/%2e%2e/chat",
            "/api/v1/projects/../chat",
            "/api/v1/projects/a.b/chat",
            "/api/v1/projects//chat",
            "/webhooks/photon/",
            "",
            "webhooks/photon",
        ] {
            assert!(!plain_path(bad), "{bad}");
        }
    }

    #[test]
    fn the_lists_are_explicit_and_disjoint() {
        let all: Vec<(&str, &str)> = forwarded().map(|f| (f.method, f.path)).collect();
        let mut dedup = all.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), all.len());
        assert_eq!(PYTHON_ROUTES.len(), 6);
        assert_eq!(CALL_FLOW_ROUTES_UNTIL_PART_4.len(), 8);
        for f in forwarded() {
            assert!(!f.path.contains('*'), "no wildcard: {}", f.path);
            assert!(!f.path.starts_with("/internal"), "{}", f.path);
            assert!(matches!(f.method, "GET" | "PUT" | "POST"), "{}", f.method);
        }
        assert_eq!(
            forwarded()
                .filter(|f| f.budget == Budget::Chat)
                .map(|f| f.path)
                .collect::<Vec<_>>(),
            ["/api/v1/projects/{project_id}/chat"]
        );
    }
}
