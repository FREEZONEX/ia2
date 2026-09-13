//! Desktop ownership and an atomic, idle-only backend shutdown.

use std::{io::Write, net::SocketAddr};

use axum::{extract::State, http::HeaderMap, http::StatusCode, Json};
use tokio::sync::{watch, Mutex, MutexGuard};

use crate::{error::ApiError, routes::RunResponse, state::AppState};

const TOKEN_HEADER: &str = "x-ia2-desktop-token";

/// An HTTP caller disappearing must not release the lifecycle gate while
/// a scan thread is still stopping. Detaching the request's JoinHandle
/// leaves the complete transition alive until its teardown finishes.
pub async fn complete_transition<T: Send + 'static>(
    operation: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::spawn(operation)
        .await
        .map_err(|error| ApiError::Internal(format!("runtime transition failed: {error}")))
}

pub struct DesktopLifecycle {
    token: Option<String>,
    // All start/stop/close/fault transitions hold this through scan-thread
    // teardown. `true` permanently refuses new starts after accepted exit.
    pub transition: Mutex<bool>,
    shutdown: watch::Sender<bool>,
}

impl DesktopLifecycle {
    pub fn new(token: Option<String>) -> Self {
        Self {
            token,
            transition: Mutex::new(false),
            shutdown: watch::channel(false).0,
        }
    }

    pub fn from_environment() -> anyhow::Result<Self> {
        let token = std::env::var("IA2_DESKTOP_TOKEN").ok();
        if token
            .as_ref()
            .is_some_and(|t| t.len() != 64 || !t.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            anyhow::bail!("IA2_DESKTOP_TOKEN must contain exactly 64 hexadecimal characters");
        }
        Ok(Self::new(token))
    }

    pub fn enabled(&self) -> bool {
        self.token.is_some()
    }

    pub fn announce_ready(&self, address: SocketAddr) -> std::io::Result<()> {
        if let Some(token) = &self.token {
            // Only the private child stdout pipe carries the ownership
            // token. Do not include it in tracing, HTTP health or URLs.
            let mut stdout = std::io::stdout().lock();
            writeln!(
                stdout,
                "IA2_READY={}",
                serde_json::json!({"url": format!("http://{address}"), "instance": token})
            )?;
            stdout.flush()?;
        }
        Ok(())
    }

    pub async fn begin_run(&self) -> Result<MutexGuard<'_, bool>, ApiError> {
        let guard = self.transition.lock().await;
        if *guard {
            return Err(ApiError::Conflict(
                "desktop backend is shutting down".into(),
            ));
        }
        Ok(guard)
    }

    pub async fn wait_for_shutdown(&self) {
        let mut receiver = self.shutdown.subscribe();
        let _ = receiver.wait_for(|shutdown| *shutdown).await;
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let expected = self.token.as_ref().ok_or_else(|| {
            ApiError::NotFound("desktop shutdown is unavailable on this backend".into())
        })?;
        let supplied = headers
            .get(TOKEN_HEADER)
            .map(|h| h.as_bytes())
            .unwrap_or(&[]);
        // Fixed-size tokens; avoid matching prefixes or exposing the value.
        let mismatch = expected
            .as_bytes()
            .iter()
            .zip(supplied)
            .fold(expected.len() ^ supplied.len(), |diff, (left, right)| {
                diff | usize::from(left ^ right)
            });
        if mismatch != 0 {
            return Err(ApiError::Forbidden(
                "desktop ownership token required".into(),
            ));
        }
        Ok(())
    }
}

pub async fn shutdown(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<RunResponse>), ApiError> {
    state.desktop.authorize(&headers)?;
    let mut shutting_down = state.desktop.transition.lock().await;
    if state.program.lock().is_some() {
        return Err(ApiError::Conflict(
            "A PLC program is still running. Stop it in IA2 before exiting.".into(),
        ));
    }
    *shutting_down = true;
    state.desktop.shutdown.send_replace(true);
    Ok((StatusCode::ACCEPTED, Json(RunResponse { ok: true })))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::response::IntoResponse;

    use super::*;
    use crate::{routes, state::RunningProgram};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn state(enabled: bool) -> AppState {
        let mut state = AppState::new(iomap_modbus::DemoSlave::new(), String::new(), None, None);
        state.desktop = Arc::new(DesktopLifecycle::new(enabled.then(|| TOKEN.into())));
        state
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(TOKEN_HEADER, TOKEN.parse().unwrap());
        headers
    }

    fn start_counter(state: &AppState) -> ironplc_bridge::ProgramHandle {
        let container = ironplc_bridge::compile(
            "PROGRAM main VAR n : DINT := 0; END_VAR n := n + 1; END_PROGRAM",
        )
        .unwrap();
        let handle = ironplc_bridge::spawn_units(
            vec![ironplc_bridge::ProgramUnit {
                instance: "main".into(),
                task_name: "scan".into(),
                interval_ms: 10,
                priority: 1,
                container,
                retain_vars: vec![],
            }],
            vec![],
            vec![],
            None,
            Default::default(),
        );
        state.program.lock().replace(RunningProgram {
            project_name: "desktop test".into(),
            handle: handle.clone(),
        });
        handle
    }

    #[tokio::test]
    async fn ordinary_backend_and_wrong_owner_cannot_shutdown() {
        let disabled = state(false);
        assert_eq!(
            shutdown(State(disabled.clone()), headers())
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        let enabled = state(true);
        for supplied in [None, Some("wrong"), Some(&TOKEN[..63])] {
            let mut h = HeaderMap::new();
            if let Some(token) = supplied {
                h.insert(TOKEN_HEADER, token.parse().unwrap());
            }
            assert_eq!(
                shutdown(State(enabled.clone()), h)
                    .await
                    .into_response()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        assert!(!*disabled.desktop.transition.lock().await);
        assert!(!*enabled.desktop.transition.lock().await);
    }

    #[tokio::test]
    async fn running_plc_survives_exit_refusal_then_stop_allows_exit() {
        let state = state(true);
        let handle = start_counter(&state);
        let mut snapshots = handle.subscribe();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), snapshots.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            shutdown(State(state.clone()), headers())
                .await
                .into_response()
                .status(),
            StatusCode::CONFLICT
        );
        assert!(state.program.lock().is_some());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), snapshots.recv())
            .await
            .unwrap()
            .unwrap();
        let _ = routes::stop(State(state.clone()), HeaderMap::new()).await;
        assert_eq!(
            shutdown(State(state.clone()), headers())
                .await
                .into_response()
                .status(),
            StatusCode::ACCEPTED
        );
        state.desktop.wait_for_shutdown().await;
        // Exercise the actual Run route: exit refusal precedes project
        // lookup or compilation, even for a request already in flight.
        let response = routes::run(
            State(state),
            routes::ProjectName::default(),
            HeaderMap::new(),
            None,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"desktop backend is shutting down");
    }

    #[tokio::test]
    async fn exit_waits_for_a_start_in_progress_and_cannot_miss_it() {
        let state = state(true);
        let transition = state.desktop.begin_run().await.unwrap();
        let request_state = state.clone();
        let request = tokio::spawn(async move { shutdown(State(request_state), headers()).await });
        tokio::task::yield_now().await;
        assert!(!request.is_finished());
        let handle = start_counter(&state);
        drop(transition);
        assert_eq!(
            request.await.unwrap().into_response().status(),
            StatusCode::CONFLICT
        );
        let _ = routes::stop(State(state.clone()), HeaderMap::new()).await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn delayed_old_fault_cannot_report_a_new_run_as_stopped() {
        let state = state(true);
        let old = start_counter(&state);
        let new = start_counter(&state);
        let mut events = state.event_tx.subscribe();
        routes::finish_faulted_run(&state, &old, "old run fault".into()).await;
        assert!(state.program.lock().as_ref().unwrap().handle.same_run(&new));
        assert!(state.last_error.lock().is_none());
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let _ = routes::stop(State(state), HeaderMap::new()).await;
    }

    #[tokio::test]
    async fn cancelled_http_caller_cannot_release_an_unfinished_transition() {
        let state = state(true);
        let (entered, is_entered) = tokio::sync::oneshot::channel();
        let (release, wait_release) = tokio::sync::oneshot::channel();
        let operation_state = state.clone();
        let caller = tokio::spawn(async move {
            complete_transition(async move {
                let _guard = operation_state.desktop.begin_run().await.unwrap();
                entered.send(()).unwrap();
                wait_release.await.unwrap();
            })
            .await
        });
        is_entered.await.unwrap();
        caller.abort();
        let _ = caller.await;
        assert!(state.desktop.transition.try_lock().is_err());
        let exit_state = state.clone();
        let exit = tokio::spawn(async move { shutdown(State(exit_state), headers()).await });
        tokio::task::yield_now().await;
        assert!(!exit.is_finished());
        release.send(()).unwrap();
        assert_eq!(
            exit.await.unwrap().into_response().status(),
            StatusCode::ACCEPTED
        );
    }
}
