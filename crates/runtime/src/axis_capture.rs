//! Edge control plane for a passive EtherCAT capture.

use std::convert::Infallible;
use std::path::PathBuf;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream;
use ironplc_bridge::axis_capture::{
    AxisCapture, CaptureError, Progress, ReplayImport, Start, Status,
};
use ironplc_bridge::axis_replay::Source;
use ironplc_bridge::ProgramHandle;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::{runtime_instance_id, AppState};

static DOWNLOAD: Semaphore = Semaphore::const_new(1);

pub(crate) async fn save_after_drain(
    handle: ProgramHandle,
    state_dir: PathBuf,
    deadline: tokio::time::Instant,
) {
    let path = state_dir.join("axis-capture").join("last-shutdown.json");
    let (send, receive) = tokio::sync::oneshot::channel();
    let worker_path = path.clone();
    // A Tokio blocking task can keep the runtime alive on drop if its I/O
    // stalls. The process can exit after the deadline with this thread detached.
    let worker = std::thread::Builder::new()
        .name("axis-capture-save".into())
        .spawn(move || {
            let save = || -> anyhow::Result<Option<String>> {
                let Some(archive) = handle.freeze_axis_capture_for_shutdown() else {
                    return Ok(None);
                };
                let bytes = serde_json::to_vec(&*archive)?;
                std::fs::create_dir_all(worker_path.parent().expect("archive directory"))?;
                project::write_atomic(&worker_path, bytes)?;
                Ok(Some(archive.capture_id.clone()))
            };
            let _ = send.send(save());
        });
    if let Err(error) = worker {
        tracing::warn!(%error, path = %path.display(), "axis capture shutdown save could not start");
        return;
    }

    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(Some(id)))) => {
            tracing::info!(%id, path = %path.display(), "axis capture saved after bridge drain")
        }
        Ok(Ok(Ok(None))) => {}
        Ok(Ok(Err(error))) => {
            tracing::warn!(%error, path = %path.display(), "axis capture shutdown save failed")
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, path = %path.display(), "axis capture shutdown save worker did not report a result")
        }
        Err(_) => tracing::warn!(path = %path.display(), "axis capture shutdown save timed out"),
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/axis-capture/{device}",
            get(status).post(start).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/axis-capture/{device}/{id}", get(archive))
        .route("/axis-capture/{device}/{id}/stop", post(stop))
        .route("/axis-capture/{device}/{id}/replay", post(replay))
}

struct Error(StatusCode, String);

impl From<CaptureError> for Error {
    fn from(error: CaptureError) -> Self {
        let status = match error {
            CaptureError::Invalid(_) => StatusCode::BAD_REQUEST,
            CaptureError::Conflict(_) => StatusCode::CONFLICT,
            CaptureError::NotFound => StatusCode::NOT_FOUND,
            CaptureError::Capacity => StatusCode::SERVICE_UNAVAILABLE,
        };
        Self(status, error.to_string())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn internal(error: impl std::fmt::Display) -> Error {
    Error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn capture(state: &AppState, device: &str) -> Result<std::sync::Arc<dyn AxisCapture>, Error> {
    state.handle.axis_capture(device).ok_or_else(|| {
        Error(
            StatusCode::NOT_FOUND,
            format!("no connected scan-synchronized EtherCAT capture for {device:?}"),
        )
    })
}

#[derive(Serialize)]
struct StatusResponse {
    runtime_id: &'static str,
    #[serde(flatten)]
    status: Status,
}

async fn status(
    State(state): State<AppState>,
    Path(device): Path<String>,
) -> Result<Json<StatusResponse>, Error> {
    Ok(Json(StatusResponse {
        runtime_id: runtime_instance_id(),
        status: capture(&state, &device)?.status(),
    }))
}

async fn start(
    State(state): State<AppState>,
    Path(device): Path<String>,
    Json(request): Json<Start>,
) -> Result<(StatusCode, Json<Progress>), Error> {
    if request.runtime_id != runtime_instance_id() {
        return Err(CaptureError::Conflict("runtime instance changed").into());
    }
    let response = capture(&state, &device)?.start(request)?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn stop(
    State(state): State<AppState>,
    Path((device, id)): Path<(String, String)>,
) -> Result<Json<Progress>, Error> {
    Ok(Json(capture(&state, &device)?.stop(&id)?))
}

async fn archive(
    State(state): State<AppState>,
    Path((device, id)): Path<(String, String)>,
) -> Result<Response, Error> {
    let capture = capture(&state, &device)?;
    download(move || {
        let frozen = capture.frozen(&id)?;
        serde_json::to_vec(&*frozen).map_err(internal)
    })
    .await
}

async fn replay(
    State(state): State<AppState>,
    Path((device, id)): Path<(String, String)>,
    Json(import): Json<ReplayImport>,
) -> Result<Response, Error> {
    let capture = capture(&state, &device)?;
    download(move || {
        let frozen = capture.frozen(&id)?;
        let mut hash = Sha256::new();
        serde_json::to_writer(&mut hash, &*frozen).map_err(internal)?;
        let origin = if frozen.simulated {
            "simulated"
        } else {
            "ethercat"
        };
        let source = Source {
            path: format!("axis-capture/{origin}/{}.json", frozen.capture_id),
            sha256: format!("{:x}", hash.finalize()),
        };
        serde_json::to_vec(&frozen.replay(import, source)?).map_err(internal)
    })
    .await
}

async fn download(
    encode: impl FnOnce() -> Result<Vec<u8>, Error> + Send + 'static,
) -> Result<Response, Error> {
    let permit = DOWNLOAD.try_acquire().map_err(|_| {
        Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "capture download in progress, retry after it finishes".into(),
        )
    })?;
    let (bytes, permit) = tokio::task::spawn_blocking(move || {
        // Retain the permit while encoding even if the request is canceled.
        encode().map(|bytes| (Bytes::from(bytes), permit))
    })
    .await
    .map_err(internal)??;
    let len = bytes.len();
    let body = Body::from_stream(stream::unfold(
        (bytes, permit),
        |(mut bytes, permit)| async {
            if bytes.is_empty() {
                return None;
            }
            let chunk = bytes.split_to(bytes.len().min(64 * 1024));
            Some((Ok::<_, Infallible>(chunk), (bytes, permit)))
        },
    ));
    Ok((
        [
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    )
        .into_response())
}
