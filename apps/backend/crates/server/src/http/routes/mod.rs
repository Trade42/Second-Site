//! Route groups (ports of `apps/api/app/api/routes/*.py`). A later commission mounts its group
//! by adding one `.merge(...)` line to [`api_routes`].

pub mod approvals;
pub mod dev;
pub mod events;
pub mod health;
pub mod projects;
pub mod tasks;

use axum::Router;

use crate::app::AppState;

/// Every route group, as `main.py` includes its routers.
pub fn api_routes() -> Router<AppState> {
    Router::new()
        .merge(health::router())
        .merge(dev::router())
        .merge(projects::router())
        .merge(approvals::router())
        .merge(tasks::router())
        .merge(events::router())
}

/// Open a transaction, run `$body` with `tx: &mut dyn Tx`, commit on success. An early return
/// with `?` drops the transaction, which rolls it back.
#[macro_export]
macro_rules! in_tx {
    ($app:expr, |$tx:ident| $body:block) => {{
        let mut __tx = $app.store.begin().await?;
        let __out = {
            let $tx: &mut dyn $crate::store::Tx = &mut *__tx;
            $body
        };
        __tx.commit().await?;
        __out
    }};
}
