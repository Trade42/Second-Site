//! Port of `apps/api/app/core/errors.py` and `apps/api/app/api/errors.py`: one [`AppError`] enum,
//! rendered as the `ProblemDetail` body with the same status codes and `error_code` strings.
//!
//! | variant (Python class)               | status | default `error_code`        | retryable |
//! |--------------------------------------|--------|-----------------------------|-----------|
//! | `BadRequest` (`AppError`)            | 400    | `BAD_REQUEST`               | no        |
//! | `NotFound` (`NotFoundError`)         | 404    | `NOT_FOUND`                 | no        |
//! | `Forbidden` (`ForbiddenError`)       | 403    | `FORBIDDEN`                 | no        |
//! | `Unauthorized` (`UnauthorizedError`) | 401    | `UNAUTHORIZED`              | no        |
//! | `Conflict` (`ConflictError`)         | 409    | `CONFLICT`                  | no        |
//! | `ApprovalRequired`                   | 409    | `APPROVAL_REQUIRED`         | no        |
//! | `ProviderUnavailable`                | 503    | `PROVIDER_UNAVAILABLE`      | yes       |
//! | `InvalidSignature`                   | 400    | `INVALID_SIGNATURE`         | no        |
//! | `InvalidStateTransition`             | 409    | `INVALID_STATE_TRANSITION`  | no        |
//! | `RequestValidation` (FastAPI)        | 422    | `VALIDATION_ERROR`          | no        |
//! | `ContractValidation` (Pydantic)      | 422    | `CONTRACT_VALIDATION_ERROR` | no        |
//! | `Internal` (any other exception)     | 500    | `INTERNAL_ERROR`            | yes       |
//!
//! `Internal` never sends its cause to the client; the cause is logged.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value};
use ss_core::{InvalidStateTransition, ValidationError};

use crate::logging;
use crate::store::StoreError;

/// Message, optional `error_code` override and `details` of an application error.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub message: String,
    pub error_code: Option<String>,
    pub details: Map<String, Value>,
    /// Overrides the variant's default `retryable` (a provider error's kind decides it).
    pub retryable: Option<bool>,
    /// Overrides the Python class name recorded as a failed task's `error_code`.
    pub class_name: Option<&'static str>,
}

impl Problem {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_code: None,
            details: Map::new(),
            retryable: None,
            class_name: None,
        }
    }
}

/// The cause of an unexpected error. Logged, never returned to the client.
#[derive(Debug, Clone, PartialEq)]
pub struct Internal {
    /// Python exception class analogue, used as `TaskRecord.error_code` (e.g. `RuntimeError`).
    pub kind: &'static str,
    pub cause: String,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AppError {
    #[error("{}", .0.message)]
    BadRequest(Problem),
    #[error("{}", .0.message)]
    NotFound(Problem),
    #[error("{}", .0.message)]
    Forbidden(Problem),
    #[error("{}", .0.message)]
    Unauthorized(Problem),
    #[error("{}", .0.message)]
    Conflict(Problem),
    #[error("{}", .0.message)]
    ApprovalRequired(Problem),
    #[error("{}", .0.message)]
    ProviderUnavailable(Problem),
    #[error("{}", .0.message)]
    InvalidSignature(Problem),
    #[error("{0}")]
    InvalidStateTransition(InvalidStateTransition),
    /// FastAPI `RequestValidationError`: `errors` in FastAPI's shape (`loc` starts with
    /// `body` / `path` / `query`).
    #[error("request validation failed")]
    RequestValidation(Vec<Value>),
    /// Pydantic `ValidationError` raised by a contract inside a handler or service.
    #[error("{0}")]
    ContractValidation(ValidationError),
    #[error("unexpected server error")]
    Internal(Internal),
}

/// `ProblemDetail` (`apps/api/app/contracts/api.py`).
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProblemDetail {
    pub error_code: String,
    pub message: String,
    #[serde(default)]
    pub details: Map<String, Value>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}

impl ss_core::Validate for ProblemDetail {
    fn check(&self, _c: &mut ss_core::Checker) {}
}

impl ss_core::Contract for ProblemDetail {
    const NAME: &'static str = "ProblemDetail";
}

fn problem(message: impl Into<String>) -> Problem {
    Problem::new(message)
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        AppError::BadRequest(problem(msg))
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        AppError::NotFound(problem(msg))
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        AppError::Forbidden(problem(msg))
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        AppError::Unauthorized(problem(msg))
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        AppError::Conflict(problem(msg))
    }
    pub fn approval_required(msg: impl Into<String>) -> Self {
        AppError::ApprovalRequired(problem(msg))
    }
    pub fn provider_unavailable(msg: impl Into<String>) -> Self {
        AppError::ProviderUnavailable(problem(msg))
    }
    pub fn invalid_signature(msg: impl Into<String>) -> Self {
        AppError::InvalidSignature(problem(msg))
    }
    /// An unexpected failure; `cause` is logged and never sent to the client.
    pub fn internal(cause: impl Into<String>) -> Self {
        AppError::Internal(Internal {
            kind: "InternalError",
            cause: cause.into(),
        })
    }
    /// An unexpected failure with a Python-style kind (used as a failed task's `error_code`).
    pub fn internal_kind(kind: &'static str, cause: impl Into<String>) -> Self {
        AppError::Internal(Internal {
            kind,
            cause: cause.into(),
        })
    }
    /// A single request-validation error, FastAPI shape.
    pub fn request_validation(kind: &str, loc: Vec<Value>, msg: impl Into<String>) -> Self {
        AppError::RequestValidation(vec![serde_json::json!({
            "type": kind,
            "loc": loc,
            "msg": msg.into(),
        })])
    }

    fn problem_mut(&mut self) -> Option<&mut Problem> {
        match self {
            AppError::BadRequest(p)
            | AppError::NotFound(p)
            | AppError::Forbidden(p)
            | AppError::Unauthorized(p)
            | AppError::Conflict(p)
            | AppError::ApprovalRequired(p)
            | AppError::ProviderUnavailable(p)
            | AppError::InvalidSignature(p) => Some(p),
            _ => None,
        }
    }

    /// Override the `error_code` (Python's `error_code=` keyword).
    pub fn code(mut self, code: impl Into<String>) -> Self {
        if let Some(p) = self.problem_mut() {
            p.error_code = Some(code.into());
        }
        self
    }

    /// Set `details` (Python's `details=` keyword). Non-object values are ignored.
    pub fn details(mut self, details: Value) -> Self {
        if let (Some(p), Value::Object(map)) = (self.problem_mut(), details) {
            p.details = map;
        }
        self
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) | AppError::InvalidSignature(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Conflict(_)
            | AppError::ApprovalRequired(_)
            | AppError::InvalidStateTransition(_) => StatusCode::CONFLICT,
            AppError::ProviderUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::RequestValidation(_) | AppError::ContractValidation(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn error_code(&self) -> String {
        let default = match self {
            AppError::BadRequest(_) => "BAD_REQUEST",
            AppError::NotFound(_) => "NOT_FOUND",
            AppError::Forbidden(_) => "FORBIDDEN",
            AppError::Unauthorized(_) => "UNAUTHORIZED",
            AppError::Conflict(_) => "CONFLICT",
            AppError::ApprovalRequired(_) => "APPROVAL_REQUIRED",
            AppError::ProviderUnavailable(_) => "PROVIDER_UNAVAILABLE",
            AppError::InvalidSignature(_) => "INVALID_SIGNATURE",
            AppError::InvalidStateTransition(_) => "INVALID_STATE_TRANSITION",
            AppError::RequestValidation(_) => "VALIDATION_ERROR",
            AppError::ContractValidation(_) => "CONTRACT_VALIDATION_ERROR",
            AppError::Internal(_) => "INTERNAL_ERROR",
        };
        self.problem_ref()
            .and_then(|p| p.error_code.clone())
            .unwrap_or_else(|| default.to_owned())
    }

    fn problem_ref(&self) -> Option<&Problem> {
        match self {
            AppError::BadRequest(p)
            | AppError::NotFound(p)
            | AppError::Forbidden(p)
            | AppError::Unauthorized(p)
            | AppError::Conflict(p)
            | AppError::ApprovalRequired(p)
            | AppError::ProviderUnavailable(p)
            | AppError::InvalidSignature(p) => Some(p),
            _ => None,
        }
    }

    pub fn retryable(&self) -> bool {
        if let Some(r) = self.problem_ref().and_then(|p| p.retryable) {
            return r;
        }
        matches!(
            self,
            AppError::ProviderUnavailable(_) | AppError::Internal(_)
        )
    }

    /// Override `retryable` (Python's class attribute, decided per error here).
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        if let Some(p) = self.problem_mut() {
            p.retryable = Some(retryable);
        }
        self
    }

    /// Override the Python class name a failed task records as `error_code`.
    pub fn with_class_name(mut self, name: &'static str) -> Self {
        if let Some(p) = self.problem_mut() {
            p.class_name = Some(name);
        }
        self
    }

    /// A provider failure (`integrations.base.ProviderError`) as Python's
    /// `ProviderUnavailableError` would answer it: 503 `PROVIDER_UNAVAILABLE`, `retryable` from
    /// the error kind, `details` with the provider and the kind. The message is `str(exc)`;
    /// provider messages never contain credentials. A failed task records `error_code`
    /// `ProviderError`, as Python's `TaskService` records `type(exc).__name__`.
    pub fn provider(e: &ss_domain::integrations::base::ProviderError) -> Self {
        AppError::provider_unavailable(e.to_string())
            .details(serde_json::json!({"provider": e.provider, "kind": e.kind.as_str()}))
            .with_retryable(e.retryable())
            .with_class_name("ProviderError")
    }

    /// The message sent to the client.
    pub fn public_message(&self) -> String {
        match self {
            AppError::RequestValidation(_) => "request validation failed".into(),
            AppError::ContractValidation(_) => "contract validation failed".into(),
            AppError::Internal(_) => "unexpected server error".into(),
            other => other.to_string(),
        }
    }

    pub fn public_details(&self) -> Map<String, Value> {
        match self {
            AppError::InvalidStateTransition(t) => {
                let mut m = Map::new();
                m.insert("current".into(), Value::String(t.current.clone()));
                m.insert("target".into(), Value::String(t.target.clone()));
                m
            }
            AppError::RequestValidation(errors) => {
                let mut m = Map::new();
                m.insert("errors".into(), Value::Array(errors.clone()));
                m
            }
            AppError::ContractValidation(v) => {
                let mut m = Map::new();
                m.insert("errors".into(), v.errors_json());
                m
            }
            AppError::Internal(_) => Map::new(),
            other => other
                .problem_ref()
                .map(|p| p.details.clone())
                .unwrap_or_default(),
        }
    }

    /// The Python exception class name (`type(exc).__name__`), used as a failed task's
    /// `error_code` exactly as `TaskService.run` records it.
    pub fn python_class_name(&self) -> &'static str {
        if let Some(name) = self.problem_ref().and_then(|p| p.class_name) {
            return name;
        }
        match self {
            AppError::BadRequest(_) => "AppError",
            AppError::NotFound(_) => "NotFoundError",
            AppError::Forbidden(_) => "ForbiddenError",
            AppError::Unauthorized(_) => "UnauthorizedError",
            AppError::Conflict(_) => "ConflictError",
            AppError::ApprovalRequired(_) => "ApprovalRequiredError",
            AppError::ProviderUnavailable(_) => "ProviderUnavailableError",
            AppError::InvalidSignature(_) => "InvalidSignatureError",
            AppError::InvalidStateTransition(_) => "InvalidStateTransition",
            AppError::RequestValidation(_) => "RequestValidationError",
            AppError::ContractValidation(_) => "ValidationError",
            AppError::Internal(i) => i.kind,
        }
    }

    pub fn to_problem_detail(&self) -> ProblemDetail {
        ProblemDetail {
            error_code: self.error_code(),
            message: self.public_message(),
            details: self.public_details(),
            request_id: logging::current_request_id(),
            retryable: self.retryable(),
        }
    }
}

impl From<InvalidStateTransition> for AppError {
    fn from(e: InvalidStateTransition) -> Self {
        AppError::InvalidStateTransition(e)
    }
}

impl From<ValidationError> for AppError {
    fn from(e: ValidationError) -> Self {
        AppError::ContractValidation(e)
    }
}

impl From<ss_domain::integrations::base::ProviderError> for AppError {
    fn from(e: ss_domain::integrations::base::ProviderError) -> Self {
        AppError::provider(&e)
    }
}

/// Domain failures (`ss-domain`), mapped as Python's handlers answer the same exceptions:
/// a Pydantic `ValidationError` is 422 `CONTRACT_VALIDATION_ERROR`; a provider error is 503
/// `PROVIDER_UNAVAILABLE` (Python: an accidental 500 outside tasks); everything else
/// (`ValueError`, `TypeError`, unbounded-int overflow) is a 500 whose cause is logged, with the
/// Python exception class as a failed task's `error_code`.
impl From<ss_domain::DomainError> for AppError {
    fn from(e: ss_domain::DomainError) -> Self {
        use ss_domain::DomainError as D;
        match e {
            D::Provider(p) => AppError::provider(&p),
            D::Validation(v) => AppError::ContractValidation(v),
            D::Address(a) => AppError::internal_kind("ValueError", a.to_string()),
            D::Extract(x) => AppError::internal_kind("ValueError", x.to_string()),
            D::BadRecord(m) => AppError::internal_kind("TypeError", m),
            D::Value(m) => AppError::internal_kind("ValueError", m),
            D::Overflow(m) => AppError::internal_kind("OverflowError", m),
        }
    }
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::UniqueViolation { table, constraint } => {
                AppError::conflict("a record with the same unique key already exists")
                    .details(serde_json::json!({"table": table, "constraint": constraint}))
            }
            other => AppError::internal_kind(other.kind(), other.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::Internal(i) = &self {
            logging::error(
                "app.api.errors",
                "unhandled error",
                &[
                    ("error_kind", Value::String(i.kind.into())),
                    ("exc", Value::String(i.cause.clone())),
                ],
            );
        }
        let status = self.status();
        let body = self.to_problem_detail();
        (status, axum::Json(body)).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_statuses_match_python() {
        let cases: Vec<(AppError, u16, &str, bool)> = vec![
            (AppError::bad_request("x"), 400, "BAD_REQUEST", false),
            (AppError::not_found("x"), 404, "NOT_FOUND", false),
            (AppError::forbidden("x"), 403, "FORBIDDEN", false),
            (AppError::unauthorized("x"), 401, "UNAUTHORIZED", false),
            (AppError::conflict("x"), 409, "CONFLICT", false),
            (
                AppError::approval_required("x"),
                409,
                "APPROVAL_REQUIRED",
                false,
            ),
            (
                AppError::provider_unavailable("x"),
                503,
                "PROVIDER_UNAVAILABLE",
                true,
            ),
            (
                AppError::invalid_signature("x"),
                400,
                "INVALID_SIGNATURE",
                false,
            ),
            (
                AppError::internal("db exploded: password=hunter2"),
                500,
                "INTERNAL_ERROR",
                true,
            ),
            (
                AppError::conflict("stale").code("STALE_VERSION"),
                409,
                "STALE_VERSION",
                false,
            ),
        ];
        for (e, status, code, retry) in cases {
            assert_eq!(e.status().as_u16(), status);
            assert_eq!(e.error_code(), code);
            assert_eq!(e.retryable(), retry);
        }
        let internal = AppError::internal("db exploded: password=hunter2").to_problem_detail();
        assert_eq!(internal.message, "unexpected server error");
        assert!(internal.details.is_empty());
        assert!(!serde_json::to_string(&internal)
            .unwrap()
            .contains("hunter2"));
    }

    #[test]
    fn invalid_transition_details() {
        let e: AppError = InvalidStateTransition {
            machine: "approval".into(),
            current: "APPROVED".into(),
            target: "REJECTED".into(),
        }
        .into();
        let p = e.to_problem_detail();
        assert_eq!(p.error_code, "INVALID_STATE_TRANSITION");
        assert_eq!(
            p.message,
            "approval: transition APPROVED -> REJECTED is not allowed"
        );
        assert_eq!(p.details["current"], "APPROVED");
        assert_eq!(p.details["target"], "REJECTED");
    }
}
