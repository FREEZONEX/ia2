//! Exercise the public routes against an isolated registry and device-free projects.
use std::fs;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    routing::{delete, get, post},
    Router,
};
use serde_json::{json, Value};
use tokio::sync::broadcast::{error::TryRecvError, Receiver};
use tower::ServiceExt;

use crate::{events::AppEvent, routes, state::AppState};

const FIRST: &str = "(* First block *)\nFUNCTION_BLOCK FB_First\nEND_FUNCTION_BLOCK\n";
const SECOND: &str = "FUNCTION_BLOCK FB_Second\nEND_FUNCTION_BLOCK\n";

struct Fixture {
    dir: tempfile::TempDir,
    state: AppState,
    app: Router,
    events: Receiver<AppEvent>,
}

impl Fixture {
    fn new(projects: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("registry");
        fs::create_dir_all(registry.join("controls/pous")).unwrap();
        fs::write(
            registry.join("controls/library.toml"),
            "name = 'controls'\nversion = '1.2.3'\ndescription = 'Test blocks'\n",
        )
        .unwrap();
        fs::write(registry.join("controls/pous/first.st"), FIRST).unwrap();
        fs::write(registry.join("controls/pous/second.st"), SECOND).unwrap();
        fs::create_dir_all(registry.join("devices")).unwrap();
        fs::write(
            registry.join("devices/test-drive.toml"),
            "name = 'Test drive'\nprotocol = 'ethercat'\nvendor_id = 17\nproduct_id = 23\n\
             requires_dc_sync = 'sync0'\nrecommended_cycle_us = 1000\n\
             [[channels]]\nname = 'position'\ndirection = 'tx_pdo'\npdo_index = 24576\n\
             bit_length = 32\ndata_type = 'DINT'\npdi_byte_offset = 4\n",
        )
        .unwrap();
        let state = AppState::new(
            iomap_modbus::DemoSlave::new(),
            String::new(),
            Some(registry),
            None,
        );
        for name in projects {
            let store = project::ProjectStore::create(dir.path().join(name), name).unwrap();
            state.projects.lock().insert_and_activate(store);
        }
        let events = state.event_tx.subscribe();
        let app = Router::new()
            .route("/api/library", get(routes::list_libraries))
            .route("/api/library/import", post(routes::import_library))
            .route("/api/library/{name}", delete(routes::remove_library))
            .route("/api/device-catalog", get(routes::list_device_catalog))
            .route(
                "/api/device-catalog/match",
                get(routes::match_device_catalog),
            )
            .route("/api/pous", post(routes::create_pou))
            .route("/api/pous/folders", post(routes::create_pou_folder))
            .route(
                "/api/pous/folders/{path}",
                delete(routes::delete_pou_folder),
            )
            .route(
                "/api/pous/{path}",
                get(routes::get_pou)
                    .put(routes::save_pou)
                    .delete(routes::delete_pou),
            )
            .with_state(state.clone());
        Self {
            dir,
            state,
            app,
            events,
        }
    }

    async fn request(
        &self,
        method: &str,
        url: &str,
        project: Option<&str>,
        body: Value,
    ) -> (StatusCode, String) {
        let mut request = Request::builder().method(method).uri(url);
        if let Some(project) = project {
            request = request.header(routes::PROJECT_HEADER, project);
        }
        let response = self
            .app
            .clone()
            .oneshot(
                request
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn ok(&self, method: &str, url: &str, project: Option<&str>, body: Value) -> Value {
        let (status, body) = self.request(method, url, project, body).await;
        assert_eq!(status, StatusCode::OK, "{method} {url}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    fn libraries(&self, name: &str) -> std::collections::BTreeMap<String, String> {
        project::ProjectStore::open(self.dir.path().join(name))
            .unwrap()
            .read_libraries()
            .unwrap()
    }

    fn assert_event(&mut self, project: &str, kind: &str) {
        assert_eq!(
            serde_json::to_value(self.events.try_recv().unwrap()).unwrap(),
            json!({
                "type": "mutation", "data": {
                    "project": project, "topic": "project", "detail": {"kind": kind, "name": "controls"}
                }
            })
        );
        self.assert_quiet();
    }

    fn assert_quiet(&mut self) {
        assert!(matches!(self.events.try_recv(), Err(TryRecvError::Empty)));
    }
}

#[tokio::test]
async fn library_browse_without_a_project_preserves_plain_listing() {
    let mut f = Fixture::new(&[]);
    {
        assert_eq!(
            f.ok("GET", "/api/library", None, Value::Null).await,
            json!([{
                "name": "controls", "version": "1.2.3", "description": "Test blocks",
                "blocks": [
                    {"name": "FB_First", "file": "first.st", "summary": "First block"},
                    {"name": "FB_Second", "file": "second.st", "summary": ""}
                ], "imported_version": null, "imported_files": []
            }])
        );
    }
    assert_eq!(
        f.request("GET", "/api/library", Some("missing"), Value::Null)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for (method, url, body) in [
        (
            "POST",
            "/api/library/import",
            json!({"library": "controls"}),
        ),
        ("DELETE", "/api/library/controls", Value::Null),
    ] {
        assert_eq!(
            f.request(method, url, None, body).await,
            (StatusCode::CONFLICT, "no project open".into())
        );
    }
    f.assert_quiet();
}

#[tokio::test]
async fn library_import_defaults_overwrite_and_selected_blocks_keep_projects_isolated() {
    let mut f = Fixture::new(&["A", "B"]);
    // Omitted blocks means all; no selector uses the active project B.
    let all =
        json!({"library":"controls", "version":"1.2.3", "imported":["first.st", "second.st"]});
    assert_eq!(
        f.ok(
            "POST",
            "/api/library/import",
            None,
            json!({"library":"controls"})
        )
        .await,
        all
    );
    f.assert_event("B", "library_imported");
    assert!(f.libraries("A").is_empty());
    assert_eq!(f.libraries("B")["controls"], "1.2.3");
    for (file, source) in [("first.st", FIRST), ("second.st", SECOND)] {
        assert_eq!(
            fs::read_to_string(f.dir.path().join("B/pous/lib/controls").join(file)).unwrap(),
            source
        );
    }
    // A selected import changes only A and makes A the active fallback.
    assert_eq!(
        f.ok(
            "POST",
            "/api/library/import",
            Some("A"),
            json!({"library":"controls", "blocks":["second.st"]})
        )
        .await,
        json!({"library":"controls", "version":"1.2.3", "imported":["second.st"]})
    );
    f.assert_event("A", "library_imported");
    assert!(!f.dir.path().join("A/pous/lib/controls/first.st").exists());
    let a = f.ok("GET", "/api/library", None, Value::Null).await;
    assert_eq!(a[0]["imported_files"], json!(["second"]));
    assert_eq!(a[0]["imported_version"], "1.2.3");
    let b = f.ok("GET", "/api/library", Some("B"), Value::Null).await;
    assert_eq!(b[0]["imported_files"], json!(["first", "second"]));
    let updated = "FUNCTION_BLOCK FB_Second\nVAR value : INT; END_VAR\nEND_FUNCTION_BLOCK\n";
    fs::write(
        f.dir.path().join("registry/controls/pous/second.st"),
        updated,
    )
    .unwrap();
    fs::write(
        f.dir.path().join("registry/controls/library.toml"),
        "version = '2.0.0'\n",
    )
    .unwrap();
    assert_eq!(
        f.ok(
            "POST",
            "/api/library/import",
            Some("A"),
            json!({"library":"controls", "blocks":[]})
        )
        .await,
        json!({"library":"controls", "version":"2.0.0", "imported":["first.st", "second.st"]})
    );
    f.assert_event("A", "library_imported");
    assert_eq!(f.libraries("A")["controls"], "2.0.0");
    assert_eq!(f.libraries("B")["controls"], "1.2.3");
    assert_eq!(
        fs::read_to_string(f.dir.path().join("A/pous/lib/controls/second.st")).unwrap(),
        updated
    );
    assert_eq!(
        fs::read_to_string(f.dir.path().join("B/pous/lib/controls/second.st")).unwrap(),
        SECOND
    );
}

#[tokio::test]
async fn library_import_keeps_requested_order_and_duplicates() {
    let mut f = Fixture::new(&["A"]);
    let files = json!(["second.st", "first.st", "second.st"]);
    let response = f
        .ok(
            "POST",
            "/api/library/import",
            Some("A"),
            json!({"library":"controls", "blocks": files}),
        )
        .await;
    assert_eq!(response["imported"], files);
    f.assert_event("A", "library_imported");
}

#[tokio::test]
async fn library_preflight_errors_preserve_bodies_files_and_notifications() {
    let mut f = Fixture::new(&["A", "B"]);
    let manifest = fs::read(f.dir.path().join("A/project.toml")).unwrap();
    for (selector, body, status, message) in [
        (
            "A",
            json!({"library":"missing"}),
            StatusCode::NOT_FOUND,
            "library 'missing' not in registry",
        ),
        (
            "A",
            json!({"library":"controls", "blocks":["first.st", "missing.st"]}),
            StatusCode::BAD_REQUEST,
            "library 'controls' has no block file 'missing.st'",
        ),
        (
            "missing",
            json!({"library":"controls"}),
            StatusCode::BAD_REQUEST,
            "project 'missing' is not open on this server",
        ),
    ] {
        assert_eq!(
            f.request("POST", "/api/library/import", Some(selector), body)
                .await,
            (status, message.into())
        );
        f.assert_quiet();
    }
    assert_eq!(
        f.request(
            "DELETE",
            "/api/library/controls",
            Some("missing"),
            Value::Null
        )
        .await,
        (
            StatusCode::BAD_REQUEST,
            "project 'missing' is not open on this server".into()
        )
    );
    // A read_to_string failure during registry scanning omits that block.
    fs::write(
        f.dir.path().join("registry/controls/pous/second.st"),
        [0xff],
    )
    .unwrap();
    assert_eq!(
        f.request(
            "POST",
            "/api/library/import",
            Some("A"),
            json!({"library":"controls", "blocks":["first.st", "second.st"]})
        )
        .await,
        (
            StatusCode::BAD_REQUEST,
            "library 'controls' has no block file 'second.st'".into()
        )
    );
    assert_eq!(
        fs::read(f.dir.path().join("A/project.toml")).unwrap(),
        manifest
    );
    for name in ["A", "B"] {
        assert!(!f.dir.path().join(name).join("pous/lib").exists());
        assert!(f.libraries(name).is_empty());
    }
    f.assert_quiet();
}

#[tokio::test]
async fn library_remove_is_scoped_idempotent_and_rejects_path_traversal() {
    let mut f = Fixture::new(&["A", "B"]);
    for name in ["A", "B"] {
        f.ok(
            "POST",
            "/api/library/import",
            Some(name),
            json!({"library":"controls"}),
        )
        .await;
        f.assert_event(name, "library_imported");
    }
    let a_root = f.dir.path().join("A");
    fs::write(a_root.join("pous/keep.st"), FIRST).unwrap();
    for _ in 0..2 {
        assert_eq!(
            f.ok("DELETE", "/api/library/controls", Some("A"), Value::Null)
                .await,
            json!({"ok":true})
        );
        f.assert_event("A", "library_removed");
        assert!(!a_root.join("pous/lib").exists());
        assert!(f.libraries("A").is_empty());
        assert_eq!(
            fs::read_to_string(a_root.join("pous/keep.st")).unwrap(),
            FIRST
        );
        assert_eq!(f.libraries("B")["controls"], "1.2.3");
    }
    for (path, name) in [("..", ".."), ("..%2F..", "../..")] {
        assert_eq!(
            f.request(
                "DELETE",
                &format!("/api/library/{path}"),
                Some("A"),
                Value::Null
            )
            .await,
            (StatusCode::BAD_REQUEST, format!("invalid name: {name}"))
        );
    }
    f.assert_quiet();
}

#[tokio::test]
async fn library_generic_pou_writes_stay_read_only_while_reads_work() {
    let mut f = Fixture::new(&["A"]);
    f.ok(
        "POST",
        "/api/library/import",
        None,
        json!({"library":"controls"}),
    )
    .await;
    f.assert_event("A", "library_imported");
    let original = f
        .ok("GET", "/api/pous/lib%2Fcontrols%2Ffirst", None, Value::Null)
        .await;
    assert_eq!(original["source"], FIRST);
    for (method, path, body) in [
        (
            "POST",
            "/api/pous",
            json!({"path":"lib/controls/new", "type":"function_block", "language":"st"}),
        ),
        ("PUT", "/api/pous/lib%2Fcontrols%2Ffirst", json!("changed")),
        ("DELETE", "/api/pous/lib%2Fcontrols%2Ffirst", Value::Null),
        ("POST", "/api/pous/folders", json!({"path":"lib/new"})),
        ("DELETE", "/api/pous/folders/lib%2Fcontrols", Value::Null),
    ] {
        assert_eq!(f.request(method, path, None, body).await,
            (StatusCode::CONFLICT, "library files are read-only — import / update / remove them via /api/library, or copy the POU out of lib/ to make it editable".into()));
    }
    assert_eq!(
        fs::read_to_string(f.dir.path().join("A/pous/lib/controls/first.st")).unwrap(),
        FIRST
    );
    f.assert_quiet();
}

#[tokio::test]
async fn catalog_routes_preserve_json_exact_identity_matching_and_404_body() {
    let mut f = Fixture::new(&[]);
    let entry = json!({
        "id":"test-drive", "name":"Test drive", "protocol":"ethercat", "description":"",
        "vendor_id":17, "product_id":23, "requires_dc_sync":"sync0", "recommended_cycle_us":1000,
        "channels":[{"name":"position", "direction":"tx_pdo", "pdo_index":24576, "sub_index":0,
            "bit_length":32, "data_type":"DINT", "pdi_byte_offset":4, "pdi_bit_offset":0}]
    });
    assert_eq!(
        f.ok("GET", "/api/device-catalog", None, Value::Null).await,
        json!([entry])
    );
    assert_eq!(
        f.ok(
            "GET",
            "/api/device-catalog/match?vendor_id=17&product_id=23",
            None,
            Value::Null
        )
        .await,
        entry
    );
    for (vendor, product) in [(18, 23), (17, 24), (0, 23)] {
        assert_eq!(
            f.request(
                "GET",
                &format!("/api/device-catalog/match?vendor_id={vendor}&product_id={product}"),
                None,
                Value::Null
            )
            .await,
            (
                StatusCode::NOT_FOUND,
                format!("no catalog entry for vendor_id={vendor} product_id={product}")
            )
        );
    }
    f.assert_quiet();
}

#[tokio::test]
async fn library_malformed_project_encoding_is_rejected_before_any_write() {
    let mut f = Fixture::new(&["A"]);
    let response = f
        .app
        .clone()
        .oneshot(
            Request::post("/api/library/import")
                .header("X-IA2-Project", "%zz")
                .header("X-IA2-Project-Encoding", "percent")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"library":"controls"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "invalid X-IA2-Project selector or encoding"
    );
    assert!(f.libraries("A").is_empty());
    assert_eq!(f.state.projects.lock().active_name(), Some("A"));
    f.assert_quiet();
}

#[tokio::test]
async fn library_pending_recovery_is_visible_and_emits_no_success_mutation() {
    let mut f = Fixture::new(&["A", "B"]);
    let journal = f.dir.path().join("A/.ia2-library-import");
    fs::create_dir(&journal).unwrap();
    fs::write(journal.join("prepared.toml"), "invalid journal").unwrap();
    for (method, path, body) in [
        ("GET", "/api/library", Value::Null),
        ("POST", "/api/library/import", json!({"library":"controls"})),
        ("DELETE", "/api/library/controls", Value::Null),
    ] {
        let (status, body) = f.request(method, path, Some("A"), body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("library import recovery required"), "{body}");
        f.assert_quiet();
    }
    assert!(journal.join("prepared.toml").is_file());
    assert!(f
        .ok("GET", "/api/library", Some("B"), Value::Null)
        .await
        .is_array());
}
