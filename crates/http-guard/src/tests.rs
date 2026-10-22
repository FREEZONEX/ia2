use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
    routing::get,
};
use tower::ServiceExt;

fn addr(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

// ---------------------------------------------------------------- parsing

#[test]
fn authority_accepts_the_forms_a_host_header_takes() {
    let ok = |raw: &str, host: &str, port: Option<u16>| {
        assert_eq!(
            parse_authority(raw),
            Some(Authority {
                host: host.into(),
                port
            }),
            "{raw}"
        );
    };
    ok("localhost", "localhost", None);
    ok("localhost:3001", "localhost", Some(3001));
    ok("127.0.0.1:3001", "127.0.0.1", Some(3001));
    ok("[::1]:3001", "::1", Some(3001));
    ok("[::1]", "::1", None);
    ok("[0:0:0:0:0:0:0:1]:1420", "::1", Some(1420));
    ok("[::ffff:127.0.0.1]:1420", "::ffff:127.0.0.1", Some(1420));
    ok("localhost.:80", "localhost", Some(80));
    ok("a.b-c.example", "a.b-c.example", None);
}

#[test]
fn authority_refuses_everything_else() {
    for raw in [
        "",
        ":80",
        "host:",
        "host:+80",
        "host:80a",
        "host:65536",
        "a:b:c",
        "::1",
        "[::1",
        "[::1]x",
        "[localhost]:80",
        "user@host",
        "host/path",
        "host\\x",
        "ho st",
        "host?x",
        "host#x",
        "ho%73t",
        "host\t",
        ".",
        "*.example",
    ] {
        assert_eq!(parse_authority(raw), None, "{raw:?}");
    }
}

#[test]
fn origin_is_a_scheme_and_an_authority_and_nothing_else() {
    let o = |raw: &str| parse_origin(raw);
    assert_eq!(
        o("http://localhost:5173"),
        Some(Origin {
            scheme: Scheme::Http,
            host: "localhost".into(),
            port: 5173
        })
    );
    assert_eq!(
        o("https://example.com"),
        Some(Origin {
            scheme: Scheme::Https,
            host: "example.com".into(),
            port: 443
        })
    );
    assert_eq!(o("http://example.com").unwrap().port, 80);
    for raw in [
        "null",
        "file://",
        "file:///etc/passwd",
        "ftp://host",
        "app://localhost",
        "tauri://localhost",
        "http://",
        "http://host/",
        "http://host/path",
        "http://user@host",
        "http://*.example",
        "localhost:3001",
        "*",
    ] {
        assert_eq!(o(raw), None, "{raw:?}");
    }
}

#[test]
fn loopback_names() {
    for host in [
        "localhost",
        "app.localhost",
        "127.0.0.1",
        "127.1.2.3",
        "::1",
        "::ffff:127.0.0.1",
    ] {
        assert!(is_loopback_host(host), "{host}");
    }
    for host in [
        "localhost.evil.example",
        "evil.example",
        "notlocalhost",
        "128.0.0.1",
        "0.0.0.0",
        "10.0.0.5",
        "::",
        "::2",
        "127.1",
        "2130706433",
        "[::1]",
        "",
    ] {
        assert!(!is_loopback_host(host), "{host}");
    }
}

// --------------------------------------------------------------- decision

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Kind {
    Pass,
    Cross,
    Preflight,
    Reject,
}

fn kind(v: &Verdict) -> Kind {
    match v {
        Verdict::Pass => Kind::Pass,
        Verdict::CrossOrigin(_) => Kind::Cross,
        Verdict::Preflight(_) => Kind::Preflight,
        Verdict::Reject(_) => Kind::Reject,
    }
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

fn run(guard: &Guard, method: Method, path: &str, pairs: &[(&str, &str)]) -> Verdict {
    decide(&guard.policy, &method, path, &headers(pairs), None)
}

fn reason(v: Verdict) -> String {
    match v {
        Verdict::Reject(r) => r,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_request_without_origin_passes_whatever_the_method() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
        assert_eq!(
            kind(&run(&g, method, "/api/pous", &[("host", "127.0.0.1:3001")])),
            Kind::Pass
        );
    }
    // No Host either: an in-process or HTTP/1.0 client, not a browser.
    assert_eq!(kind(&run(&g, Method::POST, "/api/run", &[])), Kind::Pass);
}

#[test]
fn same_origin_passes_on_any_host_spelling_of_loopback() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    for (host, origin) in [
        ("127.0.0.1:3001", "http://127.0.0.1:3001"),
        ("localhost:3001", "http://localhost:3001"),
        ("LOCALHOST:3001", "http://localhost:3001"),
        ("[::1]:3001", "http://[::1]:3001"),
        ("localhost:5173", "http://localhost:5173"), // the Vite dev server forwards Host unchanged
        ("localhost", "http://localhost:80"),
    ] {
        assert_eq!(
            kind(&run(
                &g,
                Method::POST,
                "/api/pous",
                &[("host", host), ("origin", origin)]
            )),
            Kind::Pass,
            "{host} / {origin}"
        );
    }
}

#[test]
fn a_page_on_another_origin_is_refused_for_every_method_even_a_read() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    for method in [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::OPTIONS,
    ] {
        for origin in [
            "https://evil.example",
            "http://localhost:3002", // same host, other port
            "http://127.0.0.1:3001", // a different spelling of the host than Host: localhost
            "http://evil.example:3001",
        ] {
            let v = run(
                &g,
                method.clone(),
                "/api/fs/browse",
                &[
                    ("host", "localhost:3001"),
                    ("origin", origin),
                    ("access-control-request-method", "POST"),
                ],
            );
            assert_eq!(kind(&v), Kind::Reject, "{method} {origin}");
        }
    }
}

#[test]
fn the_scheme_is_not_compared_because_the_server_cannot_know_if_a_proxy_terminates_tls() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    let pairs = [
        ("host", "localhost:3001"),
        ("origin", "https://localhost:3001"),
    ];
    assert_eq!(kind(&run(&g, Method::POST, "/api/x", &pairs)), Kind::Pass);
}

#[test]
fn opaque_and_malformed_origins_are_refused() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    for origin in [
        "null",
        "file://",
        "tauri://localhost",
        "http://localhost:3001/x",
        "garbage",
        "",
    ] {
        let v = run(
            &g,
            Method::POST,
            "/api/x",
            &[("host", "localhost:3001"), ("origin", origin)],
        );
        assert_eq!(kind(&v), Kind::Reject, "{origin:?}");
    }
    // Even when `null` is what the (rejected) config would have said.
    assert!(Guard::for_listener(addr("127.0.0.1:1"))
        .allow_origin("null")
        .is_err());
}

#[test]
fn dns_rebinding_is_refused_by_the_host_check_even_though_origin_matches_host() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    let v = run(
        &g,
        Method::POST,
        "/api/projects/open",
        &[
            ("host", "rebind.evil.example:3001"),
            ("origin", "http://rebind.evil.example:3001"),
        ],
    );
    assert!(reason(v).contains("not a loopback name"));
    // A rebinding page that only reads (no Origin on a plain navigation or <img>) is refused too.
    assert_eq!(
        kind(&run(
            &g,
            Method::GET,
            "/api/fs/browse",
            &[("host", "rebind.evil.example:3001")]
        )),
        Kind::Reject
    );
    for host in [
        "localhost.evil.example",
        "127.0.0.1.evil.example",
        "0.0.0.0:3001",
        "192.168.1.5:3001",
    ] {
        assert_eq!(
            kind(&run(&g, Method::GET, "/health", &[("host", host)])),
            Kind::Reject,
            "{host}"
        );
    }
}

#[test]
fn a_server_bound_beyond_loopback_serves_any_host_but_still_checks_origin() {
    let g = Guard::for_listener(addr("0.0.0.0:13001"));
    assert!(!g.is_loopback_only());
    assert_eq!(
        kind(&run(
            &g,
            Method::GET,
            "/hmi/panel",
            &[("host", "edge-1.plant.local:13001")]
        )),
        Kind::Pass
    );
    // The operator panel served by this very server: same origin.
    assert_eq!(
        kind(&run(
            &g,
            Method::POST,
            "/write",
            &[
                ("host", "edge-1.plant.local:13001"),
                ("origin", "http://edge-1.plant.local:13001")
            ]
        )),
        Kind::Pass
    );
    assert_eq!(
        kind(&run(
            &g,
            Method::POST,
            "/write",
            &[
                ("host", "edge-1.plant.local:13001"),
                ("origin", "https://evil.example")
            ]
        )),
        Kind::Reject
    );
}

#[test]
fn malformed_and_repeated_host_headers_are_refused() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    for pairs in [
        vec![("host", "localhost:3001"), ("host", "evil.example")],
        vec![("host", "local host")],
        vec![("host", "localhost:99999")],
        vec![("host", "user@localhost")],
    ] {
        assert_eq!(
            kind(&run(&g, Method::GET, "/health", &pairs)),
            Kind::Reject,
            "{pairs:?}"
        );
    }
    let doubled = run(
        &g,
        Method::POST,
        "/x",
        &[
            ("host", "localhost:3001"),
            ("origin", "http://localhost:3001"),
            ("origin", "https://evil.example"),
        ],
    );
    assert!(reason(doubled).contains("more than one Origin"));
}

#[test]
fn an_http2_style_authority_stands_in_for_the_host_header() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    let v = decide(
        &g.policy,
        &Method::POST,
        "/x",
        &headers(&[("origin", "http://localhost:3001")]),
        Some("localhost:3001"),
    );
    assert_eq!(kind(&v), Kind::Pass);
    let v = decide(
        &g.policy,
        &Method::GET,
        "/x",
        &HeaderMap::new(),
        Some("evil.example:3001"),
    );
    assert_eq!(kind(&v), Kind::Reject);
}

#[test]
fn a_named_origin_may_call_and_is_answered_with_its_own_origin() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"))
        .allow_origin("http://localhost:1420/")
        .unwrap()
        .allow_origin("HTTPS://Shell.Example")
        .unwrap();
    let base = [("host", "127.0.0.1:3001")];
    let with = |origin: &'static str| [base[0], ("origin", origin)];

    assert_eq!(
        kind(&run(
            &g,
            Method::POST,
            "/api/run",
            &with("http://localhost:1420")
        )),
        Kind::Cross
    );
    assert_eq!(
        kind(&run(
            &g,
            Method::GET,
            "/api/lsp",
            &with("https://shell.example")
        )),
        Kind::Cross
    );
    // Not the same origin as a named one: scheme, port and host all count.
    for origin in [
        "http://localhost:1421",
        "https://localhost:1420",
        "http://shell.example",
        "https://evil.example",
    ] {
        assert_eq!(
            kind(&run(&g, Method::POST, "/api/run", &with(origin))),
            Kind::Reject,
            "{origin}"
        );
    }
    // Preflight only for a named origin, and only when it announces a method.
    let pre = [
        ("host", "127.0.0.1:3001"),
        ("origin", "http://localhost:1420"),
        ("access-control-request-method", "PUT"),
    ];
    assert_eq!(
        kind(&run(&g, Method::OPTIONS, "/api/pous/x", &pre)),
        Kind::Preflight
    );
    let foreign = [
        ("host", "127.0.0.1:3001"),
        ("origin", "https://evil.example"),
        ("access-control-request-method", "PUT"),
    ];
    assert_eq!(
        kind(&run(&g, Method::OPTIONS, "/api/pous/x", &foreign)),
        Kind::Reject
    );
    // A named origin does not switch the Host check off.
    assert_eq!(
        kind(&run(
            &g,
            Method::POST,
            "/api/run",
            &[
                ("host", "evil.example:3001"),
                ("origin", "http://localhost:1420")
            ]
        )),
        Kind::Reject
    );
}

#[test]
fn allow_origin_refuses_what_is_not_an_origin() {
    let g = || Guard::for_listener(addr("127.0.0.1:3001"));
    for bad in [
        "*",
        "null",
        "localhost:3000",
        "http://host/path",
        "ftp://host",
        "",
        "http://",
    ] {
        assert!(g().allow_origin(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn loopback_reads_open_exactly_the_named_get_for_loopback_pages() {
    let g = Guard::for_listener(addr("127.0.0.1:13001")).allow_loopback_reads(&["/events"]);
    let host = ("host", "127.0.0.1:13001");
    let page = |origin: &'static str| [host, ("origin", origin)];

    for origin in [
        "http://127.0.0.1:3001",
        "http://localhost:5173",
        "http://[::1]:3001",
    ] {
        assert_eq!(
            kind(&run(&g, Method::GET, "/events", &page(origin))),
            Kind::Cross,
            "{origin}"
        );
    }
    assert_eq!(
        kind(&run(
            &g,
            Method::HEAD,
            "/events",
            &page("http://localhost:5173")
        )),
        Kind::Cross
    );
    // A page on the internet, or on a name that merely contains localhost: no.
    for origin in [
        "https://evil.example",
        "http://localhost.evil.example:3001",
        "http://10.0.0.5:3001",
    ] {
        assert_eq!(
            kind(&run(&g, Method::GET, "/events", &page(origin))),
            Kind::Reject,
            "{origin}"
        );
    }
    // Any other path or method from a loopback page: no.
    for (method, path) in [
        (Method::GET, "/status"),
        (Method::GET, "/audit"),
        (Method::GET, "/events/extra"),
        (Method::POST, "/events"),
        (Method::POST, "/write"),
        (Method::POST, "/force"),
        (Method::POST, "/stop"),
        (Method::OPTIONS, "/events"),
    ] {
        let v = run(&g, method.clone(), path, &page("http://localhost:5173"));
        assert_eq!(kind(&v), Kind::Reject, "{method} {path}");
    }
}

// ----------------------------------------------------------------- layer

fn app(guard: &Guard) -> Router {
    guarded(
        Router::new()
            .route(
                "/api/x",
                get(|| async { "read" }).post(|| async { "wrote" }),
            )
            .route("/events", get(|| async { "stream" }))
            .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .fallback(|| async { "static file" }),
        guard,
    )
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    pairs: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in pairs {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

#[tokio::test]
async fn the_layer_serves_same_origin_and_adds_no_cors_headers() {
    let app = app(&Guard::for_listener(addr("127.0.0.1:3001")));
    let (status, headers, body) = call(
        &app,
        "POST",
        "/api/x",
        &[
            ("host", "localhost:3001"),
            ("origin", "http://localhost:3001"),
        ],
    )
    .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "wrote"));
    assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
}

#[tokio::test]
async fn the_layer_refuses_a_foreign_page_with_a_403_that_says_why_and_no_cors_headers() {
    let app = app(&Guard::for_listener(addr("127.0.0.1:3001")));
    for (method, path) in [
        ("POST", "/api/x"),
        ("GET", "/api/x"),
        ("GET", "/some/static/file.js"),
    ] {
        let (status, headers, body) = call(
            &app,
            method,
            path,
            &[
                ("host", "localhost:3001"),
                ("origin", "https://evil.example"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(
            body.contains("https://evil.example") && body.contains("--allow-origin"),
            "{body}"
        );
        assert!(
            !headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            "a refusal must be unreadable to the page"
        );
    }
}

#[tokio::test]
async fn the_layer_covers_the_fallback_and_a_websocket_style_upgrade() {
    let app = app(&Guard::for_listener(addr("127.0.0.1:3001")));
    let (status, _, _) = call(
        &app,
        "GET",
        "/index.html",
        &[("host", "rebind.evil.example:3001")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the static UI is behind the Host check too"
    );
    let (status, _, _) = call(
        &app,
        "GET",
        "/api/x",
        &[
            ("host", "localhost:3001"),
            ("origin", "https://evil.example"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a cross-site WebSocket handshake never reaches the route"
    );
}

#[tokio::test]
async fn the_layer_answers_a_named_origin_with_its_headers_even_on_a_500() {
    let guard = Guard::for_listener(addr("127.0.0.1:3001"))
        .allow_origin("http://localhost:1420")
        .unwrap();
    let app = app(&guard);
    let pairs = [
        ("host", "127.0.0.1:3001"),
        ("origin", "http://localhost:1420"),
    ];
    for (path, expect) in [
        ("/api/x", StatusCode::OK),
        ("/boom", StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let (status, headers, _) = call(&app, "GET", path, &pairs).await;
        assert_eq!(status, expect);
        assert_eq!(
            headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://localhost:1420",
            "{path}"
        );
        assert!(headers.get_all(header::VARY).iter().any(|v| v == "origin"));
    }
    let (status, headers, body) = call(
        &app,
        "OPTIONS",
        "/api/x",
        &[
            ("host", "127.0.0.1:3001"),
            ("origin", "http://localhost:1420"),
            ("access-control-request-method", "POST"),
            (
                "access-control-request-headers",
                "content-type,x-ia2-project",
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    assert_eq!(
        headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "http://localhost:1420"
    );
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_METHODS], "POST");
    assert_eq!(
        headers[header::ACCESS_CONTROL_ALLOW_HEADERS],
        "content-type,x-ia2-project"
    );
}

#[tokio::test]
async fn the_layer_lets_the_web_ide_read_events_from_a_forwarded_port_and_nothing_else() {
    let guard = Guard::for_listener(addr("127.0.0.1:13001")).allow_loopback_reads(&["/events"]);
    let app = app(&guard);
    let ide = [
        ("host", "127.0.0.1:41234"),
        ("origin", "http://127.0.0.1:3001"),
    ];
    let (status, headers, body) = call(&app, "GET", "/events", &ide).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "stream"));
    assert_eq!(
        headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "http://127.0.0.1:3001"
    );
    let (status, _, _) = call(&app, "POST", "/api/x", &ide).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the same page may not write");
    let (status, _, _) = call(&app, "GET", "/api/x", &ide).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[test]
fn flags_win_over_the_environment_which_is_comma_separated() {
    let flags = vec!["http://a:1".to_string()];
    assert_eq!(configured_origins(&flags, Some("http://b:2")), flags);
    assert_eq!(
        configured_origins(&[], Some(" http://b:2 , ,http://c:3 ")),
        vec!["http://b:2".to_string(), "http://c:3".to_string()]
    );
    assert!(configured_origins(&[], None).is_empty());
    assert!(configured_origins(&[], Some("")).is_empty());
}

#[test]
fn allow_origins_stops_at_the_first_bad_entry() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    let ok = g
        .clone()
        .allow_origins(&["http://a:1".to_string(), "https://b".to_string()])
        .unwrap();
    assert_eq!(ok.policy.allowed.len(), 2);
    let err = g
        .allow_origins(&["http://a:1".to_string(), "*".to_string()])
        .unwrap_err();
    assert!(err.contains('*'), "{err}");
}

#[test]
fn mapped_loopback_listeners_reject_rebinding_just_like_ipv4() {
    for bind in ["127.0.0.1:3001", "[::1]:3001", "[::ffff:127.0.0.1]:3001"] {
        let g = Guard::for_listener(addr(bind));
        assert!(g.is_loopback_only(), "{bind}");
        for origin in [None, Some("http://rebind.example:3001")] {
            let mut pairs = vec![("host", "rebind.example:3001")];
            if let Some(origin) = origin {
                pairs.push(("origin", origin));
            }
            assert_eq!(kind(&run(&g, Method::POST, "/force", &pairs)), Kind::Reject);
        }
    }
    // Mapped routable addresses are still not loopback.
    assert!(!Guard::for_listener(addr("[::ffff:192.0.2.1]:3001")).is_loopback_only());
}

#[test]
fn an_allowed_ipv6_origin_matches_the_browser_serialization() {
    for (configured, wire) in [
        ("http://[0:0:0:0:0:0:0:1]:1420", "http://[::1]:1420"),
        (
            "http://[::ffff:127.0.0.1]:1420",
            "http://[::ffff:7f00:1]:1420",
        ),
    ] {
        let g = Guard::for_listener(addr("127.0.0.1:3001"))
            .allow_origin(configured)
            .unwrap();
        let pairs = [("host", "127.0.0.1:3001"), ("origin", wire)];
        assert_eq!(
            kind(&run(&g, Method::GET, "/api/project", &pairs)),
            Kind::Cross
        );
        let mut preflight = pairs.to_vec();
        preflight.push(("access-control-request-method", "POST"));
        assert_eq!(
            kind(&run(&g, Method::OPTIONS, "/api/check", &preflight)),
            Kind::Preflight
        );
        let wrong_port = wire.replace(":1420", ":1421");
        assert_eq!(
            kind(&run(
                &g,
                Method::GET,
                "/api/project",
                &[pairs[0], ("origin", wrong_port.as_str()),]
            )),
            Kind::Reject
        );
    }
    let g = Guard::for_listener(addr("[::1]:3001"));
    assert_eq!(
        kind(&run(
            &g,
            Method::POST,
            "/write",
            &[
                ("host", "[0:0:0:0:0:0:0:1]:3001"),
                ("origin", "http://[::1]:3001"),
            ]
        )),
        Kind::Pass
    );
}

#[test]
fn originless_browser_requests_cannot_call_apis_from_other_origins() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"))
        .allow_document_navigations(&["/", "/index.html"], &["/hmi/"]);
    for site in ["cross-site", "same-site"] {
        for method in [Method::GET, Method::HEAD, Method::POST, Method::OPTIONS] {
            for path in [
                "/api/edges/dummy/probe",
                "/api/fs/browse",
                "/events",
                "/health",
            ] {
                // Navigating to an API is not an HTML-page exception.
                let pairs = [
                    ("host", "127.0.0.1:3001"),
                    ("sec-fetch-site", site),
                    ("sec-fetch-mode", "navigate"),
                    ("sec-fetch-dest", "document"),
                ];
                assert_eq!(
                    kind(&run(&g, method.clone(), path, &pairs)),
                    Kind::Reject,
                    "{site} {method} {path}"
                );
            }
        }
    }
}

#[test]
fn only_top_level_read_navigations_may_open_named_html_entries() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"))
        .allow_document_navigations(&["/", "/index.html", "/hmi"], &["/hmi/"]);
    for site in ["cross-site", "same-site"] {
        for path in ["/", "/index.html", "/hmi", "/hmi/operator"] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    kind(&run(
                        &g,
                        method,
                        path,
                        &[
                            ("sec-fetch-site", site),
                            ("sec-fetch-mode", "navigate"),
                            ("sec-fetch-dest", "document"),
                        ]
                    )),
                    Kind::Pass,
                    "{site} {path}"
                );
            }
            for (mode, dest) in [
                ("no-cors", "image"),
                ("no-cors", "script"),
                ("navigate", "iframe"),
                ("navigate", "frame"),
                ("cors", "empty"),
            ] {
                assert_eq!(
                    kind(&run(
                        &g,
                        Method::GET,
                        path,
                        &[
                            ("sec-fetch-site", site),
                            ("sec-fetch-mode", mode),
                            ("sec-fetch-dest", dest),
                        ]
                    )),
                    Kind::Reject,
                    "{path} {mode} {dest}"
                );
            }
            assert_eq!(
                kind(&run(
                    &g,
                    Method::POST,
                    path,
                    &[
                        ("sec-fetch-site", site),
                        ("sec-fetch-mode", "navigate"),
                        ("sec-fetch-dest", "document"),
                    ]
                )),
                Kind::Reject
            );
        }
    }
    assert_eq!(
        kind(&run(
            &g,
            Method::GET,
            "/hmi-other",
            &[
                ("sec-fetch-site", "cross-site"),
                ("sec-fetch-mode", "navigate"),
                ("sec-fetch-dest", "document"),
            ]
        )),
        Kind::Reject
    );
    // A policy without HTML exceptions never enables navigation by itself.
    let strict = Guard::for_listener(addr("127.0.0.1:3001"));
    assert_eq!(
        kind(&run(
            &strict,
            Method::GET,
            "/",
            &[
                ("sec-fetch-site", "cross-site"),
                ("sec-fetch-mode", "navigate"),
                ("sec-fetch-dest", "document"),
            ]
        )),
        Kind::Reject
    );
}

#[test]
fn originless_tools_same_origin_ui_and_address_bar_navigation_still_work() {
    let g = Guard::for_listener(addr("127.0.0.1:3001"));
    for site in [None, Some("same-origin"), Some("none")] {
        let mut pairs = vec![("host", "127.0.0.1:3001")];
        if let Some(site) = site {
            pairs.push(("sec-fetch-site", site));
        }
        for method in [Method::GET, Method::POST] {
            assert_eq!(kind(&run(&g, method, "/api/project", &pairs)), Kind::Pass);
        }
    }
}

#[test]
fn malformed_metadata_cannot_claim_a_navigation_exception() {
    let g = Guard::for_listener(addr("127.0.0.1:3001")).allow_document_navigations(&["/"], &[]);
    for pairs in [
        vec![("sec-fetch-site", "")],
        vec![("sec-fetch-site", "unknown")],
        vec![("sec-fetch-site", "cross-site, same-origin")],
        vec![
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-site", "cross-site"),
        ],
        vec![("sec-fetch-site", "cross-site")],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "navigate"),
        ],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-mode", "no-cors"),
            ("sec-fetch-dest", "document"),
        ],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "document"),
            ("sec-fetch-dest", "iframe"),
        ],
    ] {
        assert_eq!(
            kind(&run(&g, Method::GET, "/", &pairs)),
            Kind::Reject,
            "{pairs:?}"
        );
    }
}

#[test]
fn explicit_origins_keep_their_rules_even_with_fetch_metadata() {
    let g = Guard::for_listener(addr("127.0.0.1:13001"))
        .allow_origin("https://hmi.example")
        .unwrap()
        .allow_loopback_reads(&["/events"]);
    for site in ["cross-site", "same-site", "same-origin"] {
        let base = [("host", "127.0.0.1:13001"), ("sec-fetch-site", site)];
        for (origin, path, expect) in [
            ("https://hmi.example", "/write", Kind::Cross),
            ("http://localhost:3001", "/events", Kind::Cross),
            ("http://127.0.0.1:13001", "/status", Kind::Pass),
            ("https://evil.example", "/status", Kind::Reject),
        ] {
            let mut pairs = base.to_vec();
            pairs.push(("origin", origin));
            assert_eq!(kind(&run(&g, Method::GET, path, &pairs)), expect);
        }
    }
}

#[tokio::test]
async fn metadata_refusals_are_403_without_cors_and_responses_vary_by_context() {
    let router = app(&Guard::for_listener(addr("127.0.0.1:3001")));
    for site in ["cross-site", "same-origin"] {
        let (status, headers, _) = call(
            &router,
            "GET",
            "/api/x",
            &[
                ("host", "127.0.0.1:3001"),
                ("sec-fetch-site", site),
                ("sec-fetch-mode", "no-cors"),
                ("sec-fetch-dest", "image"),
            ],
        )
        .await;
        assert_eq!(
            status,
            if site == "cross-site" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            }
        );
        assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        let vary = headers
            .get_all(header::VARY)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect::<Vec<_>>()
            .join(",");
        for field in ["sec-fetch-site", "sec-fetch-mode", "sec-fetch-dest"] {
            assert!(vary.split(',').any(|v| v.trim() == field), "{vary}");
        }
    }
}
