//! `ProviderStatusEntry` from `apps/api/app/contracts/api.py` (needed by provider `status()`).

use serde::{Deserialize, Serialize};
use ss_core::{Checker, Contract, IntegrationStatus, Validate};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderStatusEntry {
    pub provider: String,
    pub mode: String,
    pub status: IntegrationStatus,
    pub credentials_present: bool,
    pub note: String,
}

impl Validate for ProviderStatusEntry {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for ProviderStatusEntry {
    const NAME: &'static str = "ProviderStatusEntry";
}
