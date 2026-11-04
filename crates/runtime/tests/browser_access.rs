//! Process-level checks that a web page in the user's browser cannot drive the
//! real `ia2-runtime` binary, and that the callers which must keep working do.
//!
//! The runtime has no authentication; it is reachable from the dev machine
//! through an SSH-forwarded loopback port, which every page open in the
//! browser can also reach. These requests are written out by hand so the
//! `Host` and `Origin` headers are exactly what a browser (or an attacker's
//! DNS name) would send.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn write_project(dir: &Path) {
    let write = |rel: &str, text: &str| {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write(
        "project.toml",
        "name = \"browser_access\"\nversion = \"0.1\"\n",
    );
    write(
        "tasks.toml",
        "[[tasks]]\nname = \"fast\"\ninterval_ms = 20\npriority = 1\n\n\
         [[programs]]\ninstance = \"main\"\nprogram = \"main\"\ntask = \"fast\"\n",
    );
    write(
        "pous/main.st",
        "PROGRAM main\nVAR\n    valve_cmd : BOOL;\nEND_VAR\nEND_PROGRAM\n",
    );
}

struct Runtime {
    child: Child,
    port: u16,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(dir: &Path) -> Runtime {
    start_with(dir, &[])
}

fn start_with(dir: &Path, extra_args: &[&str]) -> Runtime {
    start_on(dir, "127.0.0.1", extra_args)
}

fn start_on(dir: &Path, bind_ip: &str, extra_args: &[&str]) -> Runtime {
    let project = dir.join("project");
    write_project(&project);
    let port = {
        let l = TcpListener::bind((bind_ip, 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let child = Command::new(env!("CARGO_BIN_EXE_ia2-runtime"))
        .arg("--project-dir")
        .arg(&project)
        .arg("--state-dir")
        .arg(dir.join("state"))
        .arg("--bind")
        .arg(std::net::SocketAddr::new(bind_ip.parse().unwrap(), port).to_string())
        .args(extra_args)
        .env_remove("IA2_ALLOWED_ORIGINS")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start ia2-runtime");
    let runtime = Runtime { child, port };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if send(port, "GET", "/health", &[], "").is_some_and(|r| r.status == 200) {
            return runtime;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the runtime did not answer /health on port {port}");
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then_some(v.trim())
        })
    }
}

/// One request with exactly the headers given (plus `Content-Length` and
/// `Connection: close`). A streaming answer (`/events`) is cut after its head.
fn send(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &str,
) -> Option<Reply> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if path == "/events" && raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some(Reply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    })
}

fn host(port: u16) -> (&'static str, String) {
    ("host", format!("127.0.0.1:{port}"))
}

const FORCE: &str = r#"{"name":"valve_cmd","value":1}"#;

fn audit(port: u16) -> serde_json::Value {
    let reply = send(port, "GET", "/audit", &[host(port)], "").expect("audit");
    assert_eq!(reply.status, 200, "{}", reply.head);
    serde_json::from_str(&reply.body).expect("audit json")
}

#[test]
fn a_page_on_another_origin_cannot_force_write_or_stop_the_real_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start(dir.path());
    let port = rt.port;

    for origin in ["https://evil.example", "http://localhost:3002", "null"] {
        for (path, body) in [("/force", FORCE), ("/write", FORCE), ("/stop", "")] {
            let headers = [
                host(port),
                ("origin", origin.to_string()),
                ("content-type", "application/json".into()),
            ];
            let reply = send(port, "POST", path, &headers, body).expect("reply");
            assert_eq!(reply.status, 403, "{path} from {origin}: {}", reply.head);
            assert_eq!(
                reply.header("access-control-allow-origin"),
                None,
                "a refusal must be unreadable"
            );
        }
    }
    // The preflight that a permissive CORS layer used to answer "yes".
    let preflight = [
        host(port),
        ("origin", "https://evil.example".into()),
        ("access-control-request-method", "POST".into()),
        (
            "access-control-request-headers",
            "content-type,x-ia2-origin".into(),
        ),
    ];
    let reply = send(port, "OPTIONS", "/force", &preflight, "").expect("reply");
    assert_eq!(reply.status, 403);
    assert_eq!(reply.header("access-control-allow-origin"), None);

    // Nothing reached a handler: no write was audited, and the runtime is still up.
    assert_eq!(audit(port), serde_json::json!([]));
}

#[test]
fn a_rebound_dns_name_is_refused_even_when_origin_and_host_agree() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start(dir.path());
    let port = rt.port;
    let name = format!("rebind.evil.example:{port}");
    for (method, path, body) in [
        ("POST", "/force", FORCE),
        ("GET", "/audit", ""),
        ("GET", "/status", ""),
    ] {
        let headers = [
            ("host", name.clone()),
            ("origin", format!("http://{name}")),
            ("content-type", "application/json".into()),
        ];
        let reply = send(port, method, path, &headers, body).expect("reply");
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.head);
    }
    assert_eq!(audit(port), serde_json::json!([]));
}

#[test]
fn cli_style_and_same_origin_callers_still_work_and_the_audit_tells_them_apart() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start(dir.path());
    let port = rt.port;

    // `cs`, curl, the IDE server's proxy: no Origin.
    let cli = [
        host(port),
        ("x-ia2-origin", "cs".into()),
        ("content-type", "application/json".into()),
    ];
    let reply = send(port, "POST", "/force", &cli, FORCE).expect("reply");
    assert_eq!(reply.status, 200, "{} {}", reply.head, reply.body);

    // The operator panel served by this very runtime: same origin.
    let panel_origin = format!("http://127.0.0.1:{port}");
    let panel = [
        host(port),
        ("origin", panel_origin.clone()),
        ("x-ia2-origin", "hmi".into()),
        ("content-type", "application/json".into()),
    ];
    let reply = send(port, "POST", "/write", &panel, FORCE).expect("reply");
    assert_eq!(reply.status, 200, "{} {}", reply.head, reply.body);

    let entries = audit(port);
    assert_eq!(entries[0]["origin"], "cs");
    assert!(entries[0].get("http_origin").is_none(), "{}", entries[0]);
    assert_eq!(entries[1]["origin"], "hmi");
    assert_eq!(entries[1]["http_origin"], panel_origin.as_str());
}

#[test]
fn the_web_ide_on_a_loopback_origin_may_stream_events_and_only_that() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start(dir.path());
    let port = rt.port;
    let ide = "http://127.0.0.1:3001";

    let reply = send(
        port,
        "GET",
        "/events",
        &[host(port), ("origin", ide.into())],
        "",
    )
    .expect("reply");
    assert_eq!(reply.status, 200, "{}", reply.head);
    assert_eq!(reply.header("access-control-allow-origin"), Some(ide));

    for path in ["/status", "/audit", "/logs"] {
        let reply =
            send(port, "GET", path, &[host(port), ("origin", ide.into())], "").expect("reply");
        assert_eq!(reply.status, 403, "{path}");
    }
    let reply = send(
        port,
        "GET",
        "/events",
        &[host(port), ("origin", "https://evil.example".into())],
        "",
    )
    .expect("reply");
    assert_eq!(reply.status, 403);
}

#[test]
fn a_panel_behind_a_proxy_works_once_its_origin_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start_with(dir.path(), &["--allow-origin", "https://hmi.plant.example"]);
    let port = rt.port;
    let json = ("content-type", "application/json".to_string());
    // The proxy forwards Host: 127.0.0.1:<port>; the browser's Origin is the public name.
    let proxied = [
        host(port),
        ("origin", "https://hmi.plant.example".to_string()),
        json.clone(),
    ];
    let reply = send(port, "POST", "/write", &proxied, FORCE).expect("reply");
    assert_eq!(reply.status, 200, "{} {}", reply.head, reply.body);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://hmi.plant.example")
    );
    let other = [
        host(port),
        ("origin", "https://evil.example".to_string()),
        json,
    ];
    assert_eq!(
        send(port, "POST", "/write", &other, FORCE)
            .expect("reply")
            .status,
        403
    );
}

#[test]
fn a_malformed_origin_is_refused_before_project_loading_or_scan_startup() {
    let dir = tempfile::tempdir().unwrap();
    write_project(&dir.path().join("project"));
    for by_env in [false, true] {
        for project in [
            dir.path().join("project"),
            dir.path().join("missing-project"),
        ] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_ia2-runtime"));
            command
                .arg("--project-dir")
                .arg(project)
                .arg("--state-dir")
                .arg(dir.path().join("state"))
                .arg("--bind")
                .arg("127.0.0.1:0")
                .env_remove("IA2_ALLOWED_ORIGINS")
                .env("HOME", dir.path())
                .env("XDG_CONFIG_HOME", dir.path().join("config"))
                .env("RUST_LOG", "info")
                .current_dir(dir.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if by_env {
                command.env("IA2_ALLOWED_ORIGINS", "*");
            } else {
                command.args(["--allow-origin", "*"]);
            }
            let mut child = command.spawn().expect("start ia2-runtime");
            // A runtime that accepted the wildcard would serve forever: bound the wait
            // so that failure is a test failure, not a hang.
            let deadline = Instant::now() + Duration::from_secs(20);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            if status.is_none() {
                let _ = child.kill();
            }
            let output = child.wait_with_output().unwrap();
            let status = status.expect("the runtime kept running with a wildcard --allow-origin");
            assert!(!status.success(), "a wildcard origin must be refused");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                text.contains("--allow-origin / IA2_ALLOWED_ORIGINS"),
                "{text}"
            );
            assert!(
                !text.contains("compiled"),
                "invalid config must fail before compiling: {text}"
            );
            assert!(
                !text.contains("scan loop ready"),
                "invalid config must never start scanning: {text}"
            );
            assert!(
                !dir.path().join("state").exists(),
                "invalid config must not start persistence"
            );
        }
    }
}

#[test]
fn a_mapped_ipv6_loopback_listener_still_rejects_rebinding() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start_on(dir.path(), "::ffff:127.0.0.1", &[]);
    let name = format!("rebind.example:{}", rt.port);
    let reply = send(
        rt.port,
        "POST",
        "/force",
        &[
            ("host", name.clone()),
            ("origin", format!("http://{name}")),
            ("content-type", "application/json".into()),
        ],
        FORCE,
    )
    .expect("reply");
    assert_eq!(reply.status, 403, "{} {}", reply.head, reply.body);
    assert_eq!(audit(rt.port), serde_json::json!([]));
}

#[test]
fn originless_browser_requests_are_blocked_but_same_origin_reads_work() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start(dir.path());
    for site in ["cross-site", "same-site"] {
        for (method, path, body) in [
            ("GET", "/status", ""),
            ("HEAD", "/status", ""),
            ("GET", "/events", ""),
            ("POST", "/force", FORCE),
        ] {
            let reply = send(
                rt.port,
                method,
                path,
                &[
                    host(rt.port),
                    ("sec-fetch-site", site.into()),
                    ("sec-fetch-mode", "navigate".into()),
                    ("sec-fetch-dest", "document".into()),
                    ("content-type", "application/json".into()),
                ],
                body,
            )
            .expect("reply");
            assert_eq!(reply.status, 403, "{site} {method} {path}");
            assert_eq!(reply.header("access-control-allow-origin"), None);
        }
    }
    for site in ["same-origin", "none"] {
        assert_eq!(
            send(
                rt.port,
                "GET",
                "/status",
                &[host(rt.port), ("sec-fetch-site", site.into()),],
                ""
            )
            .expect("reply")
            .status,
            200
        );
    }
    assert_eq!(audit(rt.port), serde_json::json!([]));
}

#[test]
fn an_ipv6_origin_config_matches_the_browser_canonical_origin() {
    let dir = tempfile::tempdir().unwrap();
    let rt = start_with(
        dir.path(),
        &["--allow-origin", "http://[0:0:0:0:0:0:0:1]:1420"],
    );
    let origin = "http://[::1]:1420";
    let reply = send(
        rt.port,
        "POST",
        "/write",
        &[
            host(rt.port),
            ("origin", origin.into()),
            ("sec-fetch-site", "cross-site".into()),
            ("content-type", "application/json".into()),
        ],
        FORCE,
    )
    .expect("reply");
    assert_eq!(reply.status, 200, "{} {}", reply.head, reply.body);
    assert_eq!(reply.header("access-control-allow-origin"), Some(origin));
}

#[test]
fn only_top_level_navigations_can_open_the_standalone_panel() {
    let dir = tempfile::tempdir().unwrap();
    let web = dir.path().join("web");
    fs::create_dir(&web).unwrap();
    fs::write(
        web.join("hmi.html"),
        "<!doctype html><title>fixture panel</title>",
    )
    .unwrap();
    let rt = start_with(dir.path(), &["--static-dir", web.to_str().unwrap()]);
    for path in ["/hmi", "/hmi/operator", "/hmi.html"] {
        let navigation = [
            host(rt.port),
            ("sec-fetch-site", "cross-site".into()),
            ("sec-fetch-mode", "navigate".into()),
            ("sec-fetch-dest", "document".into()),
        ];
        let reply = send(rt.port, "GET", path, &navigation, "").expect("reply");
        assert_eq!(reply.status, 200, "{path}: {} {}", reply.head, reply.body);
        assert!(reply.body.contains("fixture panel"));
        for (mode, dest) in [("navigate", "iframe"), ("no-cors", "image")] {
            let reply = send(
                rt.port,
                "GET",
                path,
                &[
                    host(rt.port),
                    ("sec-fetch-site", "cross-site".into()),
                    ("sec-fetch-mode", mode.into()),
                    ("sec-fetch-dest", dest.into()),
                ],
                "",
            )
            .expect("reply");
            assert_eq!(reply.status, 403, "{path} {dest}");
        }
    }
}
