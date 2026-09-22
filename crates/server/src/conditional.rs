//! Content versions, deliberately without a per-client or in-memory cache.
//! Call these helpers inside the same `with_project` closure as the read/write.
use std::path::{Path, PathBuf};

use axum::{
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use project::ProjectStore;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::ApiError;

pub struct Versioned<T>(pub HeaderMap, pub Json<T>);

impl<T: Serialize> IntoResponse for Versioned<T> {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

/// Paths are validated before they can participate in even a conditional read.
pub fn file(store: &ProjectStore, kind: &str, name: &str) -> Result<PathBuf, ApiError> {
    store.document_path(kind, name).map_err(Into::into)
}

fn version(path: &Path) -> Result<(String, bool), ApiError> {
    let mut hash = Sha256::new();
    let exists = match std::fs::read(path) {
        Ok(bytes) => {
            hash.update([1]);
            hash.update(bytes);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            hash.update([0]);
            false
        }
        Err(e) => return Err(ApiError::Internal(e.to_string())),
    };
    Ok((format!("\"{:x}\"", hash.finalize()), exists))
}

pub fn reply<T>(path: &Path, value: T) -> Result<Versioned<T>, ApiError> {
    let (etag, _) = version(path)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("hex ETag"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(Versioned(headers, Json(value)))
}

/// PUT bodies can be normalized (JSON -> TOML). RFC 9110 §9.3.4 forbids
/// returning an ETag after such a transformation. Supply the resulting
/// document version separately so clients need no racy follow-up GET.
pub fn written<T>(path: &Path, value: T) -> Result<Versioned<T>, ApiError> {
    let mut response = reply(path, value)?;
    let version = response.0.remove(header::ETAG).expect("reply sets ETag");
    response.0.insert("X-IA2-Version", version);
    Ok(response)
}

/// A filesystem writer doesn't take our lock. Do not label an older parsed
/// document with a newer file's version if one edits during this GET.
pub fn read<T>(
    path: &Path,
    read: impl FnOnce() -> Result<T, ApiError>,
) -> Result<Versioned<T>, ApiError> {
    let before = version(path)?;
    let value = read()?;
    let result = reply(path, value)?;
    if result.0[header::ETAG] != before.0 {
        return Err(ApiError::PreconditionFailed);
    }
    Ok(result)
}

pub fn check(path: &Path, headers: &HeaderMap) -> Result<(), ApiError> {
    let (current, exists) = version(path)?;
    if let Some(value) = headers.get(header::IF_MATCH) {
        let text = value
            .to_str()
            .map_err(|_| ApiError::BadRequest("invalid If-Match".into()))?;
        // Strong comparison only: weak validators must never authorize writes.
        let matches = if text.trim() == "*" {
            exists
        } else {
            text.split(',').any(|v| v.trim() == current)
        };
        if !matches {
            return Err(ApiError::PreconditionFailed);
        }
    }
    if let Some(value) = headers.get(header::IF_NONE_MATCH) {
        let text = value
            .to_str()
            .map_err(|_| ApiError::BadRequest("invalid If-None-Match".into()))?;
        let matches = if text.trim() == "*" {
            exists
        } else {
            text.split(',')
                .any(|v| v.trim().strip_prefix("W/").unwrap_or(v.trim()) == current)
        };
        if matches {
            return Err(ApiError::PreconditionFailed);
        }
    }
    Ok(())
}
