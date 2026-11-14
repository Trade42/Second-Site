//! Ports of `apps/api/app/services/{authorization,events,tasks,approvals}.py`.
//!
//! Services are cheap views over [`crate::app::App`] (`app.events()`, `app.tasks()`, ...); every
//! operation takes the caller's transaction (`&mut dyn Tx`), as the Python services take the
//! request's `Session`.

pub mod approvals;
pub mod authorization;
pub mod events;
pub mod tasks;
