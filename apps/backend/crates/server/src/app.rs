//! Shared application state (Python's `Container`): settings, the store, the event subscriber
//! registry, the provider status registry and a typed extension map for provider handles.
//!
//! Extension points for later commissions (one line each):
//! - subscribers: `app.subscribe(EventType::X, "name", Arc::new(Handler))` in
//!   [`crate::register_subscribers`];
//! - providers: `app.providers.register(Arc::new(MyProvider))` (replaces the `NOT_STARTED`
//!   placeholder of the same name in `/ready`) and `app.extensions.insert(Arc::new(handle))` in
//!   [`crate::register_providers`];
//! - routes: `.merge(my_routes::router())` in [`crate::http::routes::api_routes`].

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;

use crate::config::{ProviderMode, Settings};
use crate::contracts::{ActivityEvent, EventType, ProviderStatusEntry};
use crate::error::AppResult;
use crate::services::{
    approvals::ApprovalService, authorization::AuthorizationService, events::EventService,
    tasks::TaskService,
};
use crate::store::{Store, Tx};
use ss_core::IntegrationStatus;

/// The router state.
pub type AppState = Arc<App>;

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// An event subscriber (`@subscribe(event_type, name)` in Python). Runs inline, inside the
/// producer's transaction.
#[async_trait]
pub trait EventHandler: Send + Sync {
    async fn handle(&self, app: &App, tx: &mut dyn Tx, event: &ActivityEvent) -> AppResult<()>;
}

type Subscribers = HashMap<EventType, Vec<(String, Arc<dyn EventHandler>)>>;

/// Something `/ready` reports on.
pub trait ProviderStatus: Send + Sync {
    fn name(&self) -> &str;
    fn status(&self, settings: &Settings) -> ProviderStatusEntry;
}

/// A provider that exists in the Python backend but not (yet) in this one. Reported as
/// `NOT_STARTED`, never `LIVE` or `MOCK`.
pub struct NotPorted {
    pub name: &'static str,
    pub mode: fn(&Settings) -> String,
    pub credentials: fn(&Settings) -> bool,
    pub note: &'static str,
}

impl ProviderStatus for NotPorted {
    fn name(&self) -> &str {
        self.name
    }

    fn status(&self, settings: &Settings) -> ProviderStatusEntry {
        ProviderStatusEntry {
            provider: self.name.to_owned(),
            mode: (self.mode)(settings),
            status: IntegrationStatus::NotStarted,
            credentials_present: (self.credentials)(settings),
            note: self.note.to_owned(),
        }
    }
}

const NOT_PORTED: &str = "Not ported to the Rust backend yet; nothing is called.";

/// The providers Python's `/ready` lists, in its order, as `NOT_STARTED` placeholders.
pub fn default_providers() -> Vec<Arc<dyn ProviderStatus>> {
    fn mode(m: ProviderMode) -> String {
        m.as_str().to_owned()
    }
    vec![
        Arc::new(NotPorted {
            name: "costar",
            mode: |s| mode(s.property_provider_mode),
            credentials: |s| std::path::Path::new(&s.costar_kit_dir).is_dir(),
            note: NOT_PORTED,
        }),
        Arc::new(NotPorted {
            name: "la_open_data",
            mode: |s| mode(s.permit_provider_mode),
            credentials: |s| s.has("LA_OPEN_DATA_APP_TOKEN"),
            note: NOT_PORTED,
        }),
        Arc::new(NotPorted {
            name: "matterport",
            mode: |s| mode(s.matterport_provider_mode),
            credentials: |s| s.has("MATTERPORT_API_KEY") && s.has("MATTERPORT_API_TOKEN"),
            note: NOT_PORTED,
        }),
        Arc::new(NotPorted {
            name: "vexcel",
            mode: |s| mode(s.vexcel_provider_mode),
            credentials: |s| s.has("VEXCEL_API_KEY"),
            note: NOT_PORTED,
        }),
        Arc::new(NotPorted {
            name: "photon",
            mode: |s| mode(s.messaging_provider_mode),
            credentials: |s| s.has("PHOTON_API_KEY"),
            note: "Photon stays in the Python service; the Rust backend does not send messages.",
        }),
        Arc::new(NotPorted {
            name: "telnyx",
            mode: |s| mode(s.voice_provider_mode),
            credentials: |s| s.has("TELNYX_API_KEY"),
            note: NOT_PORTED,
        }),
        Arc::new(NotPorted {
            name: "xai_voice",
            mode: |s| mode(s.voice_provider_mode),
            credentials: |s| s.has("XAI_API_KEY"),
            note: NOT_PORTED,
        }),
        Arc::new(NotPorted {
            name: "product_catalog",
            mode: |s| mode(s.procurement_provider_mode),
            credentials: |_| false,
            note: NOT_PORTED,
        }),
        Arc::new(NotPorted {
            name: "langchain_deep_agents",
            mode: |s| {
                if s.has("SUPERVISOR_MODEL") {
                    "live".into()
                } else {
                    "deterministic".into()
                }
            },
            credentials: |s| s.has("ANTHROPIC_API_KEY"),
            note: "The supervisor stays in the Python service; the project chat is forwarded to \
                   it (see python_service).",
        }),
    ]
}

/// Provider status reporters, keyed by name, in registration order.
#[derive(Default)]
pub struct ProviderRegistry {
    entries: RwLock<Vec<Arc<dyn ProviderStatus>>>,
}

impl ProviderRegistry {
    pub fn with_defaults() -> Self {
        Self {
            entries: RwLock::new(default_providers()),
        }
    }

    /// Add a provider, replacing an existing one with the same name in place.
    pub fn register(&self, provider: Arc<dyn ProviderStatus>) {
        let mut entries = write(&self.entries);
        match entries.iter().position(|p| p.name() == provider.name()) {
            Some(i) => entries[i] = provider,
            None => entries.push(provider),
        }
    }

    pub fn statuses(&self, settings: &Settings) -> Vec<ProviderStatusEntry> {
        read(&self.entries)
            .iter()
            .map(|p| p.status(settings))
            .collect()
    }
}

/// A typed map of shared handles (provider clients and the like).
#[derive(Default)]
pub struct Extensions {
    map: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Extensions {
    pub fn insert<T: Any + Send + Sync>(&self, value: Arc<T>) {
        write(&self.map).insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        read(&self.map)
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|a| a.downcast::<T>().ok())
    }
}

pub struct App {
    pub settings: Settings,
    pub store: Arc<dyn Store>,
    pub providers: ProviderRegistry,
    pub extensions: Extensions,
    subscribers: RwLock<Subscribers>,
}

impl App {
    pub fn new(settings: Settings, store: Arc<dyn Store>) -> Self {
        Self {
            settings,
            store,
            providers: ProviderRegistry::with_defaults(),
            extensions: Extensions::default(),
            subscribers: RwLock::new(HashMap::new()),
        }
    }

    /// `subscribe(event_type, name)`: registering the same name twice for an event type is a
    /// no-op, as in Python.
    pub fn subscribe(&self, event_type: EventType, name: &str, handler: Arc<dyn EventHandler>) {
        let mut subs = write(&self.subscribers);
        let list = subs.entry(event_type).or_default();
        if list.iter().all(|(n, _)| n != name) {
            list.push((name.to_owned(), handler));
        }
    }

    /// `subscribers(event_type)`.
    pub fn subscribers(&self, event_type: EventType) -> Vec<(String, Arc<dyn EventHandler>)> {
        read(&self.subscribers)
            .get(&event_type)
            .cloned()
            .unwrap_or_default()
    }

    pub fn events(&self) -> EventService<'_> {
        EventService::new(self)
    }

    pub fn tasks(&self) -> TaskService<'_> {
        TaskService::new(self)
    }

    pub fn approvals(&self) -> ApprovalService<'_> {
        ApprovalService::new(self)
    }

    pub fn authz(&self) -> AuthorizationService {
        AuthorizationService
    }
}
