//! A saved source the compiler panics on must not take the listings down:
//! the project tree, the file itself and the cross-POU indexes keep
//! answering, so the file can still be opened and fixed.
//!
//! The handlers are mounted WITHOUT the panic-catching layer, so a listing
//! that unwinds fails the test instead of becoming a 500.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    routing::{get, post},
    Router,
};
use tower::ServiceExt;

use crate::{hmi_routes, routes, state::AppState};

/// A direct address with no size letter makes the vendored compiler
/// (v0.248.0) panic inside its parser. If a later vendor version parses
/// this source, the requests below succeed for the ordinary reason and
/// these tests only pin that; none of them asserts that `bad` lists nothing.
const PANICS_TODAY: &str = "PROGRAM bad\nVAR\n  x AT %I1 : BOOL;\nEND_VAR\nEND_PROGRAM\n";
const GOOD: &str = "PROGRAM good\nVAR\n  level : INT;\nEND_VAR\nEND_PROGRAM\n";

fn setup() -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let store = project::ProjectStore::create(dir.path().join("p"), "p").unwrap();
    for (path, source) in [("good", GOOD), ("bad", PANICS_TODAY)] {
        store
            .create_pou_file(path, project::PouType::Program, project::PouLanguage::St)
            .unwrap();
        store.write_pou_source(path, source).unwrap();
    }
    store.create_hmi("screen", "Screen").unwrap();
    let state = AppState::new(iomap_modbus::DemoSlave::new(), String::new(), None, None);
    state.projects.lock().insert_and_activate(store);
    let app = Router::new()
        .route("/api/project", get(routes::project_tree))
        .route("/api/pous/{path}", get(routes::get_pou))
        .route("/api/pous/{path}/variables", get(routes::pou_variables))
        .route("/api/project/pous", get(routes::project_pous))
        .route("/api/project/variables", get(routes::project_variables))
        .route("/api/symbols", post(routes::symbols))
        .route("/api/hmi/{path}/check", get(hmi_routes::check_hmi))
        .with_state(state);
    (dir, app)
}

async fn answer(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn get_json(app: &Router, url: &str) -> serde_json::Value {
    let (status, body) = answer(app, Request::get(url).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "GET {url}");
    body
}

#[tokio::test]
async fn listings_when_a_saved_source_panics_the_compiler_then_they_all_answer() {
    let (_dir, app) = setup();
    for url in [
        "/api/project",
        "/api/pous/bad",
        "/api/pous/bad/variables",
        "/api/project/pous",
        "/api/project/variables",
        "/api/hmi/screen/check",
    ] {
        get_json(&app, url).await;
    }
    let (status, _) = answer(
        &app,
        Request::post("/api/symbols")
            .body(Body::from(PANICS_TODAY))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "POST /api/symbols");
}

#[tokio::test]
async fn pou_read_when_its_source_panics_the_compiler_then_the_source_comes_back() {
    let (_dir, app) = setup();
    let pou = get_json(&app, "/api/pous/bad").await;
    assert_eq!(pou["path"], "bad");
    assert_eq!(pou["source"], PANICS_TODAY, "the file must stay readable");
}

#[tokio::test]
async fn project_tree_when_one_source_panics_the_compiler_then_every_file_is_still_listed() {
    let (_dir, app) = setup();
    let tree = get_json(&app, "/api/project").await;
    let pous = tree["pous"].as_array().expect("tree.pous");
    let file = |path: &str| {
        pous.iter()
            .find(|p| p["path"] == path)
            .unwrap_or_else(|| panic!("{path} missing from the tree: {tree}"))
    };
    file("bad");
    assert_eq!(
        file("good")["declarations"][0]["name"],
        "good",
        "a healthy file keeps its declarations"
    );
}

#[tokio::test]
async fn project_indexes_when_one_source_panics_the_compiler_then_other_files_are_indexed() {
    let (_dir, app) = setup();
    let variables = get_json(&app, "/api/project/variables").await;
    assert!(
        variables["variables"]
            .as_array()
            .expect("variables")
            .iter()
            .any(|v| v["file_path"] == "good" && v["name"] == "level"),
        "{variables}"
    );
    let pous = get_json(&app, "/api/project/pous").await;
    assert!(
        pous["pous"]
            .as_array()
            .expect("pous")
            .iter()
            .any(|p| p["file_path"] == "good" && p["name"] == "good"),
        "{pous}"
    );
}
