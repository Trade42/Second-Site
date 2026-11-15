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
    let router = http::router(app);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => return fail("could not bind", e),
    };
    let bound = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
    logging::info("app.main", "listening", &[("addr", Value::String(bound))]);
    match axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail("server error", e),
    }
}
