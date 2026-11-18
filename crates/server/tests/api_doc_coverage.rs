//! The agent contract's drift guard: every route the two routers mount
//! must have a row in `docs/api.md`.
//!
//! api.md is the HTTP contract agents read; it has drifted before
//! (routes shipped without rows, an edge table frozen at 4 of 14
//! routes) precisely because nothing checked it. This test walks the
//! actual `.route("…")` strings in `crates/server/src/main.rs` and
//! `crates/runtime/src/main.rs` and requires each path to appear in the
//! doc, so "add a route" and "document the route" become one change —
//! the local gate fails otherwise. Coverage is by path string, not
//! prose quality; keeping the row accurate is still on the author.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // crates/server -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/server has a repo root two levels up")
        .to_path_buf()
}

/// Every literal in `.route("<path>", …)` calls in `src`, deduplicated.
fn mounted_routes(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (idx, _) in src.match_indices(".route(") {
        let rest = &src[idx + ".route(".len()..];
        let Some(open) = rest.find('"') else { continue };
        let Some(close) = rest[open + 1..].find('"') else {
            continue;
        };
        out.insert(rest[open + 1..open + 1 + close].to_string());
    }
    out
}

/// axum writes `{name}` and `{*name}` placeholders; api.md writes
/// `{name}`. Normalise the wildcard marker away before comparing.
fn doc_form(route: &str) -> String {
    route.replace("{*", "{")
}

fn assert_documented(router_src_path: &Path, doc: &str, surface: &str) {
    let src = std::fs::read_to_string(router_src_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", router_src_path.display()));
    let routes = mounted_routes(&src);
    assert!(
        !routes.is_empty(),
        "no .route() literals found in {} — extractor broken?",
        router_src_path.display()
    );
    let missing: Vec<String> = routes
        .iter()
        .map(|r| doc_form(r))
        .filter(|r| !doc.contains(r.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "{surface}: routes mounted in {} but absent from docs/api.md: {missing:?}\n\
         Add a row for each (same change as the route — that's the contract).",
        router_src_path.display()
    );
}

#[test]
fn every_mounted_route_has_a_row_in_api_md() {
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("docs/api.md")).expect("docs/api.md readable");
    assert_documented(&root.join("crates/server/src/main.rs"), &doc, "IDE server");
    assert_documented(
        &root.join("crates/runtime/src/main.rs"),
        &doc,
        "edge runtime",
    );
    assert_documented(
        &root.join("crates/runtime/src/axis_capture.rs"),
        &doc,
        "edge axis capture",
    );
}
