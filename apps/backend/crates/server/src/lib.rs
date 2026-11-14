//! `ss-server`: the Second Site HTTP API in Rust, a port of `apps/api` (FastAPI).
//!
//! | Python (`apps/api/app/`)                         | Rust (`ss_server::`)                      |
//! |--------------------------------------------------|-------------------------------------------|
//! | `main.py`                                        | `http` (router, middleware), `main.rs`    |
//! | `core/config.py`                                 | `config`                                  |
//! | `core/errors.py`, `api/errors.py`                | `error`                                   |
//! | `core/security.py`                               | `security`                                |
//! | `core/logging.py`                                | `logging`                                 |
//! | `core/container.py`                              | `app`, [`build_app`], [`open_store`]      |
//! | `api/deps.py`                                    | `http::extract`                           |
//! | `db/base.py`, `db/models.py`, alembic            | `store::schema`, `migrations/`            |
//! | `db/session.py`                                  | `store` (`Store`, `Tx`), `store::postgres`|
//! | `repositories/records.py`, `registry.py`         | `store::repo`                             |
//! | `services/authorization.py`                      | `services::authorization`                 |
//! | `services/events.py`                             | `services::events`                        |
//! | `services/tasks.py`                              | `services::tasks`                         |
//! | `services/approvals.py`                          | `services::approvals`                     |
//! | `contracts/{api,approvals,tasks,events}.py`      | `contracts::{api,approvals,tasks,events}` |
//! | `api/routes/{health,dev,projects,tasks,events,approvals}.py` | `http::routes::*`             |

pub mod app;
pub mod config;
pub mod contracts;
pub mod error;
pub mod http;
pub mod integrations;
pub mod logging;
pub mod pyjson;
pub mod security;
pub mod services;
pub mod store;
pub mod workflows;

use std::sync::Arc;

use serde_json::Value;

use crate::app::{App, AppState};
use crate::config::{Settings, StoreBackend};
use crate::store::memory::MemoryStore;
use crate::store::postgres::{MigrationOutcome, PgStore};
use crate::store::{Store, StoreError};

/// Event subscribers (Python registers them by importing `app.workflows.*`). Later commissions
/// add one line here per workflow.
pub fn register_subscribers(app: &App) {
    workflows::property_analysis::register(app);
}

/// Provider adapters and their `/ready` status reporters, selected from the settings as
/// `core/container.py` selects them (see [`integrations`]).
pub fn register_providers(app: &App) {
    let selected = integrations::Integrations::from_settings(&app.settings);
    integrations::register(app, Arc::new(selected));
}

/// The composition root (`build_container`).
pub fn build_app(settings: Settings, store: Arc<dyn Store>) -> AppState {
    let app = App::new(settings, store);
    register_providers(&app);
    register_subscribers(&app);
    Arc::new(app)
}

/// Open the configured store. PostgreSQL is migrated to alembic head first (a no-op when alembic
/// or an earlier run already did it).
pub async fn open_store(settings: &Settings) -> Result<Arc<dyn Store>, StoreError> {
    match settings.store_backend {
        StoreBackend::Memory => {
            logging::warning(
                "app.store",
                "STORE_BACKEND=memory: using the in-memory store; data is NOT durable and is lost \
                 when the process exits",
                &[],
            );
            Ok(Arc::new(MemoryStore::new()))
        }
        StoreBackend::Postgres => {
            let store = PgStore::connect(&settings.database_url).await?;
            match store.migrate().await? {
                MigrationOutcome::Applied(revs) => logging::info(
                    "app.store",
                    "applied alembic revisions",
                    &[("revisions", Value::from(revs))],
                ),
                MigrationOutcome::UpToDate => {
                    logging::info("app.store", "schema is at alembic head", &[])
                }
                MigrationOutcome::UnknownRevision(rev) => logging::warning(
                    "app.store",
                    "database is at an alembic revision this binary does not know; not migrating",
                    &[("revision", Value::String(rev))],
                ),
            }
            Ok(Arc::new(store))
        }
    }
}
