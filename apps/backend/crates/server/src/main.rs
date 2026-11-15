//! `second-site-backend`: serve the API (default) or `second-site-backend migrate` to only bring
//! the PostgreSQL schema to alembic head.

use std::process::ExitCode;

use serde_json::Value;
use ss_server::config::{Settings, StoreBackend};
use ss_server::{build_app, http, logging, open_store};

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

fn fail(msg: &str, err: impl std::fmt::Display) -> ExitCode {
    logging::error(
        "app.main",
        msg,
        &[("error", Value::String(err.to_string()))],
    );
    ExitCode::FAILURE
}

#[tokio::main]
async fn main() -> ExitCode {
    let settings = match Settings::from_env() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    logging::configure(&settings.log_level);
    if let Err(e) = settings.check_startup() {
        return fail("refusing to start", e);
    }
    let migrate_only = std::env::args().nth(1).as_deref() == Some("migrate");
    if migrate_only && settings.store_backend != StoreBackend::Postgres {
        return fail("migrate needs PostgreSQL", "STORE_BACKEND=memory");
    }
    let store = match open_store(&settings).await {
        Ok(s) => s,
        Err(e) => return fail("could not open the store", e),
    };
    if migrate_only {
        return ExitCode::SUCCESS;
    }
    let addr = format!("{}:{}", settings.host, settings.port);
    let app = build_app(settings, store);
    let router = http::router(app.clone());
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => return fail("could not bind", e),
    };
    let bound = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
    logging::info("app.main", "listening", &[("addr", Value::String(bound))]);
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    wait_for_nessie_flows(&app).await;
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail("server error", e),
    }
}

/// A simulated Nessie payment runs on a task of its own (its client may already have gone), with
/// its claim committed before the bank call and its outcome recorded after it. `axum::serve` does
/// not wait for such a task, so wait for it here, for a bounded time (the send window plus the
/// bank's deadline plus 5 s); an execute still in flight after that is recorded `MAY_EXIST` by the
/// stale-claim rule when its plan item is next read.
async fn wait_for_nessie_flows(app: &ss_server::app::App) {
    let state = app.nessie();
    let n = state.flights().count();
    if n == 0 {
        return;
    }
    let max = state.timing().shutdown_wait();
    logging::info(
        "app.main",
        "waiting for simulated Nessie payments between their two phases",
        &[
            ("in_flight", Value::from(n)),
            ("max_wait_s", Value::from(max.as_secs())),
        ],
    );
    let left = ss_server::services::nessie::wait_for_flows_within(app, max).await;
    if left > 0 {
        logging::warning(
            "app.main",
            "stopping with simulated Nessie payments still in flight; their claims become MAY_EXIST",
            &[("in_flight", Value::from(left))],
        );
    }
}
