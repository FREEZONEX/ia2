//! The guard every edge API route sits under: a handler panic answers `500`
//! instead of closing the connection.
//!
//! Its own file, not `main.rs`, because `crates/server/tests/api_doc_coverage.rs`
//! reads every `.route("…")` literal in `main.rs` and demands a row in
//! `docs/api.md` for each; the throwaway routes of the test below are not API.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};
use ironplc_bridge::monitor;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;

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

/// The layers every API route sits under: a handler panic answers `500`
/// ([`handler_panic_response`]), and CORS is permissive.
///
/// The panic layer goes INSIDE the CORS layer so the `500` it produces still
/// carries the CORS headers; a browser client on the other end of the tunnel
/// cannot read a response without them, which would turn a legible `500` back
/// into an opaque network error.
pub(crate) fn with_api_layers<S>(routes: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    routes
        .layer(CatchPanicLayer::custom(handler_panic_response))
        .layer(CorsLayer::permissive())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic inside a handler used to close the connection with no answer.
    /// Mounted through the same function `main` uses, it is a `500` with the
    /// panic text — and it still carries the CORS headers, which only holds
    /// while the panic layer sits inside the CORS layer.
    #[tokio::test]
    async fn a_handler_panic_is_a_500_with_the_message_and_cors_headers() {
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        async fn literal() -> &'static str {
            panic!("no group at index")
        }
        let app = with_api_layers(
            axum::Router::new()
                .route("/ok", get(|| async { "fine" }))
                .route("/boom", get(literal)),
        );
        let ask = |uri: &'static str| {
            let app = app.clone();
            async move {
                let request = Request::get(uri)
                    .header("origin", "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap();
                let response = app.oneshot(request).await.unwrap();
                let status = response.status();
                let cors = response
                    .headers()
                    .contains_key("access-control-allow-origin");
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, cors, String::from_utf8(body.to_vec()).unwrap())
            }
        };

        let (status, cors, body) = ask("/boom").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("no group at index"), "{body}");
        assert!(body.contains("re-read the resource"), "{body}");
        assert!(cors, "the 500 must still carry the CORS headers");

        let (status, cors, body) = ask("/ok").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "fine"));
        assert!(cors);
    }
}
