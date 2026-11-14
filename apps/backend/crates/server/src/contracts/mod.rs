//! Contracts owned by the server track: ports of `apps/api/app/contracts/{api,approvals,tasks,
//! events}.py`. Project and business-profile contracts live in `ss-core`.

pub mod api;
pub mod approvals;
pub mod events;
pub mod tasks;

use serde::{Deserialize, Deserializer};

/// JSON object (`dict[str, Any]`).
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

/// For `X | None` fields without a default: the key must be present, `null` is allowed.
/// (serde would otherwise treat a missing `Option` as `None`.)
pub(crate) fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

/// Implement `Validate` (no field rules) and `Contract` for a plain contract struct.
macro_rules! plain_contract {
    ($($t:ident),+ $(,)?) => {
        $(
            impl ::ss_core::Validate for $t {
                fn check(&self, _c: &mut ::ss_core::Checker) {}
            }
            impl ::ss_core::Contract for $t {
                const NAME: &'static str = stringify!($t);
            }
        )+
    };
}
pub(crate) use plain_contract;

pub use api::*;
pub use approvals::*;
pub use events::*;
pub use tasks::*;
