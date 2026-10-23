//! Wires the browser-access guard (`ia2-http-guard`) around the IDE server's
//! routes.
//!
//! Its own file, not `main.rs`, because `tests/api_doc_coverage.rs` reads every
//! `.route("…")` literal in `main.rs` and demands a row in `docs/api.md` for
//! each; the throwaway routes of the tests below are not API.

use std::net::SocketAddr;

use axum::Router;
use ia2_http_guard::{guarded, Guard, ALLOWED_ORIGINS_ENV};
use tower_http::catch_panic::CatchPanicLayer;

use crate::error::handler_panic_response;

/// Put every route and the static-file fallback of `app` behind the guard for
/// a server listening on `bind`, with `handler_panic_response` inside it so a
/// `500` answering an allowed cross-origin page still carries its headers.
///
/// The IDE's own pages are served by this very server (or through the Vite dev
/// proxy, which forwards `Host` and `Origin` unchanged), so they are
/// same-origin and need nothing here. `extra_origins` is for a front-end served
/// from somewhere else (the removed macOS shell loaded `http://127.0.0.1:<port>/`
/// from this server, so it never needed one; a native shell that returns should
/// do the same, or be named here).
pub fn secure<S>(
    app: Router<S>,
    bind: SocketAddr,
    extra_origins: &[String],
) -> anyhow::Result<Router<S>>
where
    S: Clone + Send + Sync + 'static,
{
    let guard = Guard::for_listener(bind)
        .allow_document_navigations(&["/", "/index.html", "/hmi.html"], &[])
        .allow_origins(extra_origins)
        .map_err(|e| anyhow::anyhow!("--allow-origin / {ALLOWED_ORIGINS_ENV}: {e}"))?;
    if !guard.is_loopback_only() {
        tracing::warn!(
            %bind,
            "the server is bound beyond loopback: Host names are not restricted and the API has no \
             authentication, so anyone who can reach this address can drive the IDE"
        );
    }
    Ok(guarded(
        app.layer(CatchPanicLayer::custom(handler_panic_response)),
        &guard,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
        routing::{get, post},
    };
    use tower::ServiceExt;

    async fn boom() -> &'static str {
        panic!("handler failed")
    }

    fn app(bind: &str, origins: &[&str]) -> Router {
        // `Router<()>`: the throwaway routes below take no state.
        let origins: Vec<String> = origins.iter().map(|s| s.to_string()).collect();
        secure(
            Router::new()
                .route("/api/projects/open", post(|| async { "opened" }))
                .route("/api/lsp", get(|| async { "upgraded" }))
                .route("/api/boom", get(boom))
                .fallback(|| async { "index.html" }),
            bind.parse().unwrap(),
            &origins,
        )
        .unwrap()
    }

    async fn status(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> StatusCode {
        let mut request = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_page_on_the_internet_cannot_open_a_project_or_upgrade_to_the_lsp_socket() {
        let app = app("127.0.0.1:3001", &[]);
        let page = [
            ("host", "127.0.0.1:3001"),
            ("origin", "https://evil.example"),
        ];
        assert_eq!(
            status(&app, "POST", "/api/projects/open", &page).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(&app, "GET", "/api/lsp", &page).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(&app, "GET", "/index.html", &page).await,
            StatusCode::FORBIDDEN
        );
        // The preflight that used to be answered "yes, anything" is refused too.
        let preflight = [
            ("host", "127.0.0.1:3001"),
            ("origin", "https://evil.example"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
        ];
        assert_eq!(
            status(&app, "OPTIONS", "/api/projects/open", &preflight).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn a_rebound_name_is_refused_even_with_a_matching_origin() {
        let app = app("127.0.0.1:3001", &[]);
        let rebound = [
            ("host", "rebind.evil.example:3001"),
            ("origin", "http://rebind.evil.example:3001"),
        ];
        assert_eq!(
            status(&app, "POST", "/api/projects/open", &rebound).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn the_ide_its_cli_and_the_vite_proxy_still_get_through() {
        let app = app("127.0.0.1:3001", &[]);
        // The CLI and tests: no Origin.
        assert_eq!(
            status(
                &app,
                "POST",
                "/api/projects/open",
                &[("host", "127.0.0.1:3001")]
            )
            .await,
            StatusCode::OK
        );
        // The UI served by this server, or through `vite dev` (Host and Origin forwarded unchanged).
        for (host, origin) in [
            ("127.0.0.1:3001", "http://127.0.0.1:3001"),
            ("localhost:5173", "http://localhost:5173"),
        ] {
            let pairs = [("host", host), ("origin", origin)];
            assert_eq!(
                status(&app, "POST", "/api/projects/open", &pairs).await,
                StatusCode::OK,
                "{origin}"
            );
            assert_eq!(
                status(&app, "GET", "/api/lsp", &pairs).await,
                StatusCode::OK,
                "{origin}"
            );
        }
    }

    #[tokio::test]
    async fn a_named_origin_is_let_in_and_gets_its_headers_even_when_the_handler_panics() {
        let app = app("127.0.0.1:3001", &["http://localhost:1420"]);
        let shell = [
            ("host", "127.0.0.1:3001"),
            ("origin", "http://localhost:1420"),
        ];
        assert_eq!(
            status(&app, "POST", "/api/projects/open", &shell).await,
            StatusCode::OK
        );

        let response = app
            .clone()
            .oneshot(
                Request::get("/api/boom")
                    .header("host", "127.0.0.1:3001")
                    .header("origin", "http://localhost:1420")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://localhost:1420"
        );
    }

    #[test]
    fn a_bad_origin_in_the_config_is_an_error_at_startup() {
        let err = secure(
            Router::<()>::new(),
            "127.0.0.1:3001".parse().unwrap(),
            &["*".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("--allow-origin"), "{err}");
    }
}
