//! The guards every edge API route sits under: a handler panic answers `500`
//! instead of closing the connection, and a web page in a browser cannot drive
//! the runtime.
//!
//! Its own file, not `main.rs`, because `crates/server/tests/api_doc_coverage.rs`
//! reads every `.route("…")` literal in `main.rs` and demands a row in
//! `docs/api.md` for each; the throwaway routes of the tests below are not API.

use std::net::SocketAddr;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};
use ia2_http_guard::{guarded, Guard};
use ironplc_bridge::monitor;
use tower_http::catch_panic::CatchPanicLayer;

/// The only paths a web page on another origin may read: the web IDE, served
/// from the dev machine, streams `/events` through the SSH-forwarded loopback
/// port (`apps/web/src/state/runtime.tsx`). Everything else the IDE does to a
/// runtime goes through the IDE server, which sends no `Origin`.
const CROSS_ORIGIN_READS: &[&str] = &["/events"];

/// What a request gets when its handler panics: a `500` with the panic text,
/// like any other server bug. Without this the connection just closes with no
/// response, which a client cannot tell from a network fault. The body is the
/// one the IDE server answers with too (`monitor::handler_panic_body`).
fn handler_panic_response(payload: Box<dyn std::any::Any + Send + 'static>) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        monitor::handler_panic_body(payload.as_ref()),
    )
        .into_response()
}

/// The layer every API route sits under: a handler panic answers `500`
/// ([`handler_panic_response`]).
///
/// The browser guard ([`guard_browsers`]) goes OUTSIDE this one, around the
/// finished router, so the `500` it produces still carries the headers a
/// permitted cross-origin page needs to read it; without them a browser
/// client on the other end of the tunnel would see an opaque network error.
pub(crate) fn with_api_layers<S>(routes: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    routes.layer(CatchPanicLayer::custom(handler_panic_response))
}

/// Keep web pages in the user's browser from driving this runtime, whichever
/// way they reach it (the SSH forward's loopback port, DNS rebinding): a
/// request that names an `Origin` must be same-origin, loopback `Host` names
/// only when `bind` is a loopback address, and the one cross-origin door is a
/// loopback page reading [`CROSS_ORIGIN_READS`]. `extra_origins` names pages
/// that may call it in full (a panel behind a reverse proxy). Requests with no
/// `Origin` (curl, `cs`, the IDE server's proxy) are untouched.
pub(crate) fn guard_browsers<S>(
    app: Router<S>,
    bind: SocketAddr,
    extra_origins: &[String],
) -> anyhow::Result<Router<S>>
where
    S: Clone + Send + Sync + 'static,
{
    let guard = browser_guard(bind)
        .allow_origins(extra_origins)
        .map_err(|e| {
            anyhow::anyhow!(
                "--allow-origin / {}: {e}",
                ia2_http_guard::ALLOWED_ORIGINS_ENV
            )
        })?;
    if !guard.is_loopback_only() {
        tracing::warn!(
            %bind,
            "bound beyond loopback: Host names are not restricted, and /write, /force and /stop \
             have no authentication — anyone who can reach this address can drive the plant"
        );
    }
    Ok(guarded(app, &guard))
}

fn browser_guard(bind: SocketAddr) -> Guard {
    Guard::for_listener(bind).allow_loopback_reads(CROSS_ORIGIN_READS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    async fn literal() -> &'static str {
        panic!("no group at index")
    }

    /// Mounted the way `main` does it: panic catcher on the routes, the browser
    /// guard around the whole router (static files and fallback included).
    fn app(bind: &str) -> Router {
        app_with(bind, &[])
    }

    fn app_with(bind: &str, origins: &[&str]) -> Router {
        let origins: Vec<String> = origins.iter().map(|s| s.to_string()).collect();
        guard_browsers(
            with_api_layers(
                Router::new()
                    .route("/ok", get(|| async { "fine" }))
                    .route("/events", get(literal))
                    .route("/write", post(|| async { "wrote" }))
                    .route("/force", post(|| async { "forced" }))
                    .route("/stop", post(|| async { "stopping" })),
            )
            .fallback(|| async { "panel" }),
            bind.parse().unwrap(),
            &origins,
        )
        .unwrap()
    }

    async fn ask(
        app: &Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Option<String>, String) {
        let mut request = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let allow = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|v| v.to_str().unwrap().to_string());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, allow, String::from_utf8(body.to_vec()).unwrap())
    }

    /// A panic inside a handler used to close the connection with no answer.
    /// Mounted through the same functions `main` uses, it is a `500` with the
    /// panic text — and for the one page that may read it, it still carries the
    /// permission to, which only holds while the panic catcher sits inside the
    /// browser guard.
    #[tokio::test]
    async fn a_handler_panic_is_a_500_with_the_message_and_the_headers_the_ide_needs() {
        let app = app("127.0.0.1:13001");
        let ide = [
            ("host", "127.0.0.1:41234"),
            ("origin", "http://127.0.0.1:3001"),
        ];
        let (status, allow, body) = ask(&app, "GET", "/events", &ide).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("no group at index"), "{body}");
        assert!(body.contains("re-read the resource"), "{body}");
        assert_eq!(allow.as_deref(), Some("http://127.0.0.1:3001"));

        let (status, _, body) = ask(&app, "GET", "/ok", &[("host", "127.0.0.1:13001")]).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "fine"));
    }

    /// The finding this module exists for: every state-changing endpoint of an
    /// unauthenticated runtime used to answer a page on any origin (CORS
    /// permissive), including from a forwarded port on the dev machine.
    #[tokio::test]
    async fn a_page_on_another_origin_cannot_write_force_or_stop() {
        let app = app("127.0.0.1:13001");
        for origin in ["https://evil.example", "http://localhost:3002", "null"] {
            for (uri, what) in [("/write", "write"), ("/force", "force"), ("/stop", "stop")] {
                let pairs = [("host", "127.0.0.1:13001"), ("origin", origin)];
                let (status, allow, _) = ask(&app, "POST", uri, &pairs).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{what} from {origin}");
                assert_eq!(allow, None, "a refusal must be unreadable to the page");
            }
            // The preflight that used to be answered "yes, anything".
            let preflight = [
                ("host", "127.0.0.1:13001"),
                ("origin", origin),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "content-type,x-ia2-origin",
                ),
            ];
            let (status, allow, _) = ask(&app, "OPTIONS", "/force", &preflight).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "preflight from {origin}");
            assert_eq!(allow, None);
        }
    }

    /// The IDE's own page (loopback origin) may stream `/events` and do nothing
    /// else to the runtime directly; a page on the internet may not even read.
    #[tokio::test]
    async fn only_a_loopback_page_may_read_events_and_nothing_else_is_cross_origin() {
        let app = app("127.0.0.1:13001");
        let from = |origin: &'static str| [("host", "127.0.0.1:41234"), ("origin", origin)];
        let (status, _, _) = ask(&app, "GET", "/ok", &from("http://127.0.0.1:3001")).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "only the listed paths are readable"
        );
        let (status, _, _) = ask(&app, "POST", "/write", &from("http://127.0.0.1:3001")).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the IDE page writes through the IDE server"
        );
        let (status, _, _) = ask(&app, "GET", "/events", &from("https://evil.example")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            CROSS_ORIGIN_READS,
            ["/events"],
            "widening this list widens what any local page can read"
        );
    }

    /// DNS rebinding: the page and the `Host` agree, but the name is not a
    /// loopback one.
    #[tokio::test]
    async fn a_rebound_name_is_refused_for_the_api_and_for_the_panel() {
        let app = app("127.0.0.1:13001");
        let rebound = [
            ("host", "rebind.evil.example:13001"),
            ("origin", "http://rebind.evil.example:13001"),
        ];
        for (method, uri) in [
            ("POST", "/force"),
            ("GET", "/events"),
            ("GET", "/hmi/panel"),
        ] {
            let (status, _, _) = ask(&app, method, uri, &rebound).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        }
    }

    /// What must keep working: `cs`, curl and the IDE server's proxy (no
    /// `Origin`), and the operator panel served by this runtime (same origin),
    /// including when the runtime is bound beyond loopback for a kiosk.
    #[tokio::test]
    async fn cli_proxy_and_the_served_operator_panel_still_work() {
        let loopback = app("127.0.0.1:13001");
        let (status, _, body) =
            ask(&loopback, "POST", "/write", &[("host", "127.0.0.1:13001")]).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "wrote"));
        let panel = [
            ("host", "localhost:13001"),
            ("origin", "http://localhost:13001"),
        ];
        let (status, _, _) = ask(&loopback, "POST", "/write", &panel).await;
        assert_eq!(status, StatusCode::OK);

        let kiosk = app("0.0.0.0:13001");
        let panel = [
            ("host", "edge-1.plant.local:13001"),
            ("origin", "http://edge-1.plant.local:13001"),
        ];
        let (status, _, _) = ask(&kiosk, "POST", "/force", &panel).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, body) = ask(
            &kiosk,
            "GET",
            "/hmi/panel",
            &[("host", "edge-1.plant.local:13001")],
        )
        .await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "panel"));
        let foreign = [
            ("host", "edge-1.plant.local:13001"),
            ("origin", "https://evil.example"),
        ];
        let (status, _, _) = ask(&kiosk, "POST", "/force", &foreign).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a kiosk bind still refuses a foreign page"
        );
    }

    /// A panel published through a reverse proxy has a public name the runtime
    /// never sees (the proxy sends `Host: 127.0.0.1:13001`): it works once that
    /// origin is named, and only that origin.
    #[tokio::test]
    async fn a_named_origin_may_use_the_panel_behind_a_proxy() {
        let app = app_with("127.0.0.1:13001", &["https://hmi.plant.example"]);
        let proxied = [
            ("host", "127.0.0.1:13001"),
            ("origin", "https://hmi.plant.example"),
        ];
        let (status, allow, _) = ask(&app, "POST", "/write", &proxied).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(allow.as_deref(), Some("https://hmi.plant.example"));
        let other = [
            ("host", "127.0.0.1:13001"),
            ("origin", "https://evil.example"),
        ];
        let (status, _, _) = ask(&app, "POST", "/write", &other).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_bad_origin_in_the_config_stops_the_runtime_at_startup() {
        let err = guard_browsers(
            Router::<()>::new(),
            "127.0.0.1:13001".parse().unwrap(),
            &["*".to_string()],
        )
        .map(|_| ())
        .unwrap_err();
        assert!(err.to_string().contains("--allow-origin"), "{err}");
    }
}
