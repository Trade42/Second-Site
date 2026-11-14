//! Port of `apps/api/app/services/authorization.py`: project-scoped access, tool permission
//! categories, and the reusable "record belongs to this project or 404" check.

use ss_core::{string_enum, ProjectContext};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::store::repo::{ProjectScoped, PROJECTS};
use crate::store::Tx;

string_enum! {
    pub enum ToolCategory {
        ReadOnly = "READ_ONLY",
        InternalWrite = "INTERNAL_WRITE",
        ExternalCommunication = "EXTERNAL_COMMUNICATION",
        Consequential = "CONSEQUENTIAL",
    }
}

/// `APPROVAL_REQUIRED`.
pub const APPROVAL_REQUIRED: [ToolCategory; 2] = [
    ToolCategory::ExternalCommunication,
    ToolCategory::Consequential,
];

pub fn requires_approval(c: ToolCategory) -> bool {
    APPROVAL_REQUIRED.contains(&c)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AuthorizationService;

impl AuthorizationService {
    /// `project_for_user`: 404 (not 403) for a missing or foreign project, so other owners'
    /// project ids are not discoverable.
    pub async fn project_for_user(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        user_id: Uuid,
    ) -> AppResult<ProjectContext> {
        match PROJECTS.get(tx, project_id).await? {
            Some(p) if p.owner_id == user_id => Ok(p),
            _ => Err(AppError::not_found("project not found")),
        }
    }
}

/// The record if it exists and belongs to `project_id`, else a 404 with `message`. Use this for
/// every record loaded by id below a project route.
pub fn ensure_in_project<T: ProjectScoped>(
    record: Option<T>,
    project_id: Uuid,
    message: &str,
) -> AppResult<T> {
    match record {
        Some(r) if r.project_id() == project_id => Ok(r),
        _ => Err(AppError::not_found(message)),
    }
}

/// The same check for records whose project is known only by id (e.g. a joined parent).
pub fn ensure_project_id(found: Option<Uuid>, project_id: Uuid, message: &str) -> AppResult<()> {
    match found {
        Some(p) if p == project_id => Ok(()),
        _ => Err(AppError::not_found(message)),
    }
}
