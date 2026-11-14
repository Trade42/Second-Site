//! Provider adapters and their selection (the provider half of `apps/api/app/core/container.py`).
//!
//! Selection is Python's: a provider is live only when its mode setting is `live`; `fixture` and
//! `disabled` both select the fixture provider.
//!
//! | setting                             | live                                  | otherwise                        |
//! |-------------------------------------|---------------------------------------|----------------------------------|
//! | `PROPERTY_PROVIDER_MODE`            | CoStar data kit (`COSTAR_KIT_DIR`)    | fixture listings                 |
//! | `PERMIT_PROVIDER_MODE`              | LADBS Socrata API                     | committed real LADBS snapshot    |
//! | `MATTERPORT_PROVIDER_MODE`          | Matterport Model API (GraphQL)        | synthetic geometry fixtures      |
//! | `VEXCEL_PROVIDER_MODE`              | Vexcel API 2.0                        | no imagery                       |
//! | (always)                            | local object storage (`OBJECT_STORAGE_LOCAL_DIR`)                        |
//!
//! Fixture providers read from `FIXTURES_DIR` (a Rust-only setting; see [`crate::config`]).
//! API keys come only from settings; they are sent as request headers and never logged or put in
//! an error message.
//!
//! Adding a provider in a later commission: write the adapter module here, add a field to
//! [`Integrations`] and a line to [`Integrations::from_settings`], and register its `/ready`
//! reporter in [`register`] under the name Python's `/ready` uses.

pub mod http;
pub mod la_permits;
pub mod matterport;
pub mod matterport_geometry;
pub mod storage;
pub mod vexcel;

use std::sync::Arc;

use ss_core::IntegrationStatus;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{status_entry, ProviderError};
use ss_domain::integrations::costar::{CoStarProvider, FixtureCoStarProvider, KitCoStarProvider};
use ss_domain::integrations::la_permits::{FixtureLaPermitProvider, LaPermitProvider, RawPermit};

use crate::app::{App, ProviderStatus};
use crate::config::{ProviderMode, Settings};
use crate::error::{AppError, AppResult};
use crate::logging;

pub use la_permits::LiveLaPermitProvider;
pub use matterport::{
    FixtureMatterportProvider, LiveMatterportProvider, MatterportProvider, ModelSummary,
};
pub use storage::{LocalObjectStorage, ObjectStorage, StorageError};
pub use vexcel::{FixtureVexcelProvider, LiveVexcelProvider, VexcelProvider};

/// The selected provider of each kind (Python's `Container` fields).
#[derive(Clone)]
pub struct Integrations {
    pub costar: Arc<dyn CoStarProvider>,
    pub permits: Arc<dyn LaPermitProvider>,
    pub matterport: Arc<dyn MatterportProvider>,
    pub vexcel: Arc<dyn VexcelProvider>,
    pub storage: Arc<dyn ObjectStorage>,
}

fn secret(s: &Option<crate::config::Secret>) -> Option<&str> {
    s.as_ref().map(|v| v.expose())
}

impl Integrations {
    /// `build_container`'s provider choices.
    pub fn from_settings(s: &Settings) -> Self {
        let costar: Arc<dyn CoStarProvider> = if s.property_provider_mode == ProviderMode::Live {
            Arc::new(KitCoStarProvider::new(&s.costar_kit_dir))
        } else {
            Arc::new(FixtureCoStarProvider::from_path(
                s.fixtures_dir.join("properties").join("la_candidates.json"),
            ))
        };
        let permits: Arc<dyn LaPermitProvider> = if s.permit_provider_mode == ProviderMode::Live {
            Arc::new(LiveLaPermitProvider::new(
                &s.la_open_data_base_url,
                &s.la_electrical_dataset,
                &s.la_mechanical_dataset,
                secret(&s.la_open_data_app_token),
            ))
        } else {
            let path = s
                .fixtures_dir
                .join("permits")
                .join("la_permits_snapshot.json");
            match FixtureLaPermitProvider::from_path(&path) {
                Ok(p) => Arc::new(p),
                // Python's constructor raises and the service does not start. Here the service
                // starts, `/ready` reports the provider BLOCKED and every call fails with the
                // same error: nothing pretends to have permit data.
                Err(e) => {
                    logging::warning(
                        "app.integrations",
                        "permit fixture snapshot could not be loaded",
                        &[("error", serde_json::Value::String(e.to_string()))],
                    );
                    Arc::new(UnavailablePermits { error: e })
                }
            }
        };
        let matterport: Arc<dyn MatterportProvider> =
            if s.matterport_provider_mode == ProviderMode::Live {
                Arc::new(LiveMatterportProvider::new(
                    &s.matterport_graph_url,
                    secret(&s.matterport_api_key),
                    secret(&s.matterport_api_token),
                ))
            } else {
                Arc::new(FixtureMatterportProvider::new(&s.fixtures_dir))
            };
        let vexcel: Arc<dyn VexcelProvider> = if s.vexcel_provider_mode == ProviderMode::Live {
            Arc::new(LiveVexcelProvider::new(
                &s.vexcel_base_url,
                secret(&s.vexcel_api_key),
            ))
        } else {
            Arc::new(FixtureVexcelProvider)
        };
        Self {
            costar,
            permits,
            matterport,
            vexcel,
            storage: Arc::new(LocalObjectStorage::new(&s.object_storage_local_dir)),
        }
    }
}

/// The permit provider when the fixture snapshot cannot be read: every call fails with that
/// error, and `/ready` says BLOCKED.
pub struct UnavailablePermits {
    pub error: ProviderError,
}

impl LaPermitProvider for UnavailablePermits {
    fn mode(&self) -> &str {
        "fixture"
    }

    fn search_by_address_prefix(
        &self,
        _house_number: &str,
        _direction: Option<&str>,
        _street_name: &str,
        _zip_code: Option<&str>,
    ) -> Result<Vec<RawPermit>, ProviderError> {
        Err(self.error.clone())
    }

    fn get_permit(&self, _permit_number: &str) -> Result<Vec<RawPermit>, ProviderError> {
        Err(self.error.clone())
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            ss_domain::integrations::la_permits::PROVIDER,
            "fixture",
            IntegrationStatus::Blocked,
            false,
            &format!("Permit snapshot unavailable: {}", self.error.message),
        )
    }
}

/// A `/ready` reporter that asks the provider itself (`c.<provider>.status()`).
pub struct Reported {
    name: &'static str,
    status: Box<dyn Fn() -> ProviderStatusEntry + Send + Sync>,
}

impl ProviderStatus for Reported {
    fn name(&self) -> &str {
        self.name
    }

    fn status(&self, _settings: &Settings) -> ProviderStatusEntry {
        (self.status)()
    }
}

/// Put the providers on the application state and report their real status in `/ready`
/// (replacing the `NOT_STARTED` placeholders of the same names, in place).
pub fn register(app: &App, i: Arc<Integrations>) {
    let costar = i.costar.clone();
    app.providers.register(Arc::new(Reported {
        name: "costar",
        status: Box::new(move || costar.status()),
    }));
    let permits = i.permits.clone();
    app.providers.register(Arc::new(Reported {
        name: "la_open_data",
        status: Box::new(move || permits.status()),
    }));
    let matterport = i.matterport.clone();
    app.providers.register(Arc::new(Reported {
        name: "matterport",
        status: Box::new(move || matterport.status()),
    }));
    let vexcel = i.vexcel.clone();
    app.providers.register(Arc::new(Reported {
        name: "vexcel",
        status: Box::new(move || vexcel.status()),
    }));
    // `c.catalog` is always `FixtureCatalogProvider()` in Python's container: illustrative prices
    // only (the bill of materials uses its price table).
    app.providers.register(Arc::new(Reported {
        name: "product_catalog",
        status: Box::new(|| {
            use ss_domain::integrations::catalog::{
                FixtureCatalogProvider, ProductCatalogProvider,
            };
            FixtureCatalogProvider.status()
        }),
    }));
    app.extensions.insert(i);
}

impl App {
    /// The selected providers (`c.costar`, `c.permits`, ...). Tests replace them with
    /// `app.extensions.insert(Arc::new(integrations))`.
    pub fn integrations(&self) -> Arc<Integrations> {
        self.extensions
            .get::<Integrations>()
            .unwrap_or_else(|| Arc::new(Integrations::from_settings(&self.settings)))
    }
}

/// Run blocking provider work (file reads, HTTP) on tokio's blocking pool.
pub async fn blocking<T, F>(f: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::internal(format!("provider task failed: {e}")))
}
