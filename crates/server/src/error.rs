use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use ironplc_bridge::BridgeError;
use project::StoreError;

/// Unified API error → HTTP response.
#[derive(Debug)]
pub enum ApiError {
    NoProject,
    NotFound(String),
    Conflict(String),
    PreconditionFailed,
    BadRequest(String),
    /// 422 — the request was well-formed but the resulting state fails
    /// validation (e.g. an HMI ops batch whose result has structural
    /// errors).
    Unprocessable(String),
    /// 403 — the request was understood but project policy forbids it
    /// (write governance denied the write).
    Forbidden(String),
    Internal(String),
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(p) => Self::NotFound(format!("project not found: {p}")),
            StoreError::AlreadyExists(p) => Self::Conflict(format!("already exists: {p}")),
            StoreError::InvalidName(n) => Self::BadRequest(format!("invalid name: {n}")),
            StoreError::PouNotFound(n) => Self::NotFound(format!("POU '{n}' not found")),
            StoreError::HmiNotFound(n) => Self::NotFound(format!("HMI screen '{n}' not found")),
            e @ StoreError::HmiCorrupt(..) => Self::BadRequest(e.to_string()),
            StoreError::DeviceNotFound(n) => Self::NotFound(format!("device '{n}' not found")),
            StoreError::EdgeNotFound(n) => Self::NotFound(format!("edge '{n}' not found")),
            StoreError::FolderNotFound(n) => Self::NotFound(format!("folder '{n}' not found")),
            StoreError::FolderNotEmpty(n) => Self::Conflict(format!("folder '{n}' not empty")),
            other => Self::Internal(other.to_string()),
        }
    }
}

/// Wrap a StoreError as ApiError. Tiny helper for places where the
/// `.map_err(Into::into)` form is awkward — e.g. when not inside
/// `with_project`'s closure.
pub fn project_err(e: StoreError) -> ApiError {
    ApiError::from(e)
}

impl From<BridgeError> for ApiError {
    fn from(e: BridgeError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

/// One canonical mapping for the runtime write/force family — the same
/// three-way split that write / force / unforce each hand-rolled before:
/// unknown variable → 404, scan loop gone → 409, VM trap → 500.
impl From<ironplc_bridge::RuntimeWriteError> for ApiError {
    fn from(e: ironplc_bridge::RuntimeWriteError) -> Self {
        use ironplc_bridge::RuntimeWriteError as E;
        match e {
            E::UnknownVariable(n) => Self::NotFound(format!("variable '{n}' not declared")),
            E::Disconnected => Self::Conflict("scan loop has stopped".into()),
            E::Vm(msg) => Self::Internal(msg),
            E::GovernanceDenied(n) => {
                Self::Forbidden(format!("write to '{n}' rejected by project governance"))
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::NoProject => (StatusCode::CONFLICT, "no project open".to_string()),
            Self::NotFound(s) => (StatusCode::NOT_FOUND, s),
            Self::Conflict(s) => (StatusCode::CONFLICT, s),
            Self::PreconditionFailed => (StatusCode::PRECONDITION_FAILED,
                "document changed since it was read; re-read it and reapply your edit (local edits have not been saved)".into()),
            Self::BadRequest(s) => (StatusCode::BAD_REQUEST, s),
            Self::Unprocessable(s) => (StatusCode::UNPROCESSABLE_ENTITY, s),
            Self::Forbidden(s) => (StatusCode::FORBIDDEN, s),
            Self::Internal(s) => (StatusCode::INTERNAL_SERVER_ERROR, s),
        };
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The negative path the governance feature exists for: a policy
    /// denial is a 403 with an explanatory body — not a 404, not a
    /// 500 — matching the edge runtime's own mapping so all four
    /// write paths report denials identically (ADR-0002).
    #[test]
    fn governance_denied_maps_to_forbidden_403() {
        let api: ApiError = ironplc_bridge::RuntimeWriteError::GovernanceDenied(
            "x (write_mode = allowlist, no rule allows it)".into(),
        )
        .into();
        assert!(
            matches!(&api, ApiError::Forbidden(msg) if msg.contains("governance")),
            "expected Forbidden with a governance message, got {api:?}"
        );
        assert_eq!(api.into_response().status(), StatusCode::FORBIDDEN);
    }
}
