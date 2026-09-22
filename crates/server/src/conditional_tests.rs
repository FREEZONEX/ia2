//! Exercise the mounted handlers, including headers and serialized responses.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use tower::ServiceExt;

use crate::{hmi_routes, routes, state::AppState};

fn setup() -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let store = project::ProjectStore::create(dir.path().join("p"), "p").unwrap();
    store.create_hmi("screen", "Screen").unwrap();
    store
        .create_device("device", project::Protocol::Modbus)
        .unwrap();
    store.create_edge("edge", "user@localhost").unwrap();
    let state = AppState::new(iomap_modbus::DemoSlave::new(), String::new(), None, None);
    state.projects.lock().insert_and_activate(store);
    let app = Router::new()
        .route(
            "/api/pous/{path}",
            get(routes::get_pou).put(routes::save_pou),
        )
        .route(
            "/api/devices/{name}",
            get(routes::get_device).put(routes::update_device),
        )
        .route(
            "/api/edges/{name}",
            get(routes::get_edge).put(routes::update_edge),
        )
        .route(
            "/api/hmi/{path}",
            get(hmi_routes::get_hmi).put(hmi_routes::put_hmi),
        )
        .route("/api/iomap", get(routes::get_iomap).put(routes::put_iomap))
        .route("/api/tasks", get(routes::get_tasks).put(routes::put_tasks))
        .route(
            "/api/alarms",
            get(routes::get_alarms).put(routes::put_alarms),
        )
        .route(
            "/api/northbound",
            get(routes::get_northbound).put(routes::put_northbound),
        )
        .with_state(state);
    (dir, app)
}

async fn read(app: &Router, url: &str) -> (String, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(Request::get(url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {url}");
    let etag = response.headers()["ETag"].to_str().unwrap().to_string();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (etag, serde_json::from_slice(&bytes).unwrap())
}

fn put(url: &str, body: &str, etag: Option<&str>) -> Request<Body> {
    let mut request = Request::put(url).header("Content-Type", "application/json");
    if let Some(etag) = etag {
        request = request.header("If-Match", etag);
    }
    request.body(Body::from(body.to_owned())).unwrap()
}

#[tokio::test]
async fn every_document_rejects_a_stale_copy_and_accepts_its_own_version() {
    let (dir, app) = setup();
    for (url, file) in [
        ("/api/pous/main", "pous/main.st"),
        ("/api/devices/device", "devices/device.toml"),
        ("/api/edges/edge", "edges/edge.toml"),
        ("/api/hmi/screen", "hmi/screen.hmi.json"),
        ("/api/iomap", "iomap.toml"),
        ("/api/tasks", "tasks.toml"),
        ("/api/alarms", "alarms.toml"),
        ("/api/northbound", "northbound.toml"),
    ] {
        let (_, body) = read(&app, url).await;
        let body = if url.contains("/pous/") {
            body["source"].as_str().unwrap().to_owned()
        } else {
            body.to_string()
        };
        // First materialize optional config files. Raw clients remain compatible.
        let response = app.clone().oneshot(put(url, &body, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "unconditional PUT {url}");
        let (original, _) = read(&app, url).await;
        let path = dir.path().join("p").join(file);
        let mut other = std::fs::read_to_string(&path).unwrap();
        other.push('\n'); // valid external filesystem edit, no server counter involved
        std::fs::write(&path, &other).unwrap();
        let stale = app
            .clone()
            .oneshot(put(url, &body, Some(&original)))
            .await
            .unwrap();
        assert_eq!(
            stale.status(),
            StatusCode::PRECONDITION_FAILED,
            "stale PUT {url}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            other,
            "must preserve the other writer: {url}"
        );
        let (fresh, _) = read(&app, url).await;
        assert_ne!(fresh, original);
        let weak = format!("W/{fresh}");
        let response = app
            .clone()
            .oneshot(put(url, &body, Some(&weak)))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::PRECONDITION_FAILED,
            "weak If-Match {url}"
        );
        let response = app
            .clone()
            .oneshot(put(url, &body, Some(&fresh)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "fresh PUT {url}");
        assert!(response.headers().contains_key("X-IA2-Version"));
        assert!(!response.headers().contains_key("ETag"));
        let next = response.headers()["X-IA2-Version"].to_str().unwrap();
        assert_eq!(
            read(&app, url).await.0,
            next,
            "written version must match the next GET: {url}"
        );
    }
}

#[tokio::test]
async fn simultaneous_writers_cannot_both_replace_the_same_version() {
    let (_dir, app) = setup();
    let (etag, _) = read(&app, "/api/pous/main").await;
    let a = app
        .clone()
        .oneshot(put("/api/pous/main", "PROGRAM a\nEND_PROGRAM", Some(&etag)));
    let b = app
        .clone()
        .oneshot(put("/api/pous/main", "PROGRAM b\nEND_PROGRAM", Some(&etag)));
    let (a, b) = tokio::join!(a, b);
    let mut statuses = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 412]);
}

#[tokio::test]
async fn if_none_match_prevents_create_races_and_deleted_versions_do_not_match() {
    let (dir, app) = setup();
    let (etag, value) = read(&app, "/api/hmi/screen").await;
    let request = || {
        Request::put("/api/hmi/new")
            .header("If-None-Match", "*")
            .header("Content-Type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::PRECONDITION_FAILED
    );
    std::fs::remove_file(dir.path().join("p/hmi/screen.hmi.json")).unwrap();
    let result = app
        .oneshot(put("/api/hmi/screen", &value.to_string(), Some(&etag)))
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn absent_config_defaults_have_a_version_and_raw_pou_creation_still_works() {
    let (_dir, app) = setup();
    let (etag, doc) = read(&app, "/api/northbound").await;
    let result = app
        .clone()
        .oneshot(put("/api/northbound", &doc.to_string(), Some(&etag)))
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::OK);
    let result = app
        .clone()
        .oneshot(put("/api/northbound", &doc.to_string(), Some(&etag)))
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::PRECONDITION_FAILED);
    let result = app
        .oneshot(put("/api/pous/new", "PROGRAM new\nEND_PROGRAM", None))
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::OK);
}
