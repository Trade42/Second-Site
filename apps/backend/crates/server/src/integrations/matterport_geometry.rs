//! `floor_geometry_from_model` (Matterport wall graph -> `SiteGeometry` fields).
//! PLACEHOLDER while the port is in progress: answers an explicit provider error.

use serde_json::{Map, Value};
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};

pub fn floor_geometry_from_model(_model: &Map<String, Value>) -> Result<Option<Value>, ProviderError> {
    Err(ProviderError::new(
        super::matterport::PROVIDER,
        ProviderErrorKind::NotConfigured,
        "floor geometry conversion is not ported yet",
    ))
}
