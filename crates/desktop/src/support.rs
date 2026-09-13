//! Platform-independent checks for the small Windows host boundary.
#![cfg_attr(not(windows), allow(dead_code))]

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

pub const HELP: &str = "IA2 desktop\n\nIA2.exe [--port PORT]\nIA2.exe --shutdown\nIA2.exe --check-runtime\nIA2.exe --version\n\nThe default port is 3001. Closing a window keeps the controller running in the system tray. Exit refuses while a controller is running.\n";
pub const WEBVIEW_INSTALL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

#[derive(Debug, PartialEq)]
pub enum Mode {
    Launch(u16),
    Shutdown,
    CheckRuntime,
    Help,
    Version,
}

pub fn arguments(args: impl IntoIterator<Item = String>) -> Result<Mode, String> {
    let args: Vec<_> = args.into_iter().collect();
    match args.as_slice() {
        [] => Ok(Mode::Launch(3001)),
        [flag] => match flag.as_str() {
            "--shutdown" => Ok(Mode::Shutdown),
            "--check-runtime" => Ok(Mode::CheckRuntime),
            "--help" | "-h" => Ok(Mode::Help),
            "--version" | "-V" => Ok(Mode::Version),
            _ => Err(format!("Unknown argument: {flag}\n{HELP}")),
        },
        [flag, port] if flag == "--port" => port
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .map(Mode::Launch)
            .ok_or_else(|| "--port must be an integer from 1 to 65535".into()),
        _ => Err(HELP.into()),
    }
}

pub struct Layout {
    pub root: PathBuf,
    pub server: PathBuf,
    pub web: PathBuf,
    pub library: PathBuf,
}

impl Layout {
    pub fn from_executable(executable: &Path) -> Result<Self, String> {
        let bin = executable
            .parent()
            .ok_or("IA2 executable has no parent directory")?;
        let root = bin
            .parent()
            .ok_or("IA2 install directory is missing")?
            .to_path_buf();
        Ok(Self {
            server: bin.join("ia2-server.exe"),
            web: root.join("web"),
            library: root.join("library"),
            root,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        for file in [
            &self.server,
            &self.web.join("index.html"),
            &self.web.join("hmi.html"),
        ] {
            if !file.is_file() {
                return Err(format!(
                    "安装文件缺失：{}。请重新安装 IA2。",
                    file.display()
                ));
            }
        }
        if !self.library.is_dir() {
            return Err(format!("功能块库目录缺失：{}", self.library.display()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Origin {
    port: u16,
    url: String,
}

impl Origin {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            url: format!("http://127.0.0.1:{port}"),
        }
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn allows(&self, candidate: &str) -> bool {
        let Ok(url) = Url::parse(candidate) else {
            return false;
        };
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port_or_known_default() == Some(self.port)
            && url.username().is_empty()
            && url.password().is_none()
    }
    pub fn address(&self) -> SocketAddr {
        (Ipv4Addr::LOCALHOST, self.port).into()
    }
}

pub fn external_url(candidate: &str) -> bool {
    Url::parse(candidate).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

#[derive(Deserialize)]
struct Ready {
    url: String,
    instance: String,
}

pub fn parse_ready(line: &str, origin: &Origin, token: &str) -> Result<bool, String> {
    let Some(json) = line.strip_prefix("IA2_READY=") else {
        return Ok(false);
    };
    let ready: Ready =
        serde_json::from_str(json.trim()).map_err(|e| format!("Invalid backend readiness: {e}"))?;
    if ready.instance != token || !origin.allows(&ready.url) {
        return Err(
            "Backend readiness identity/origin does not match this desktop instance".into(),
        );
    }
    Ok(true)
}

/// Only numeric loopback addresses are accepted. Never follows redirects,
/// applies proxy settings, or sends the desktop secret to another origin.
pub fn request(
    origin: &Origin,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> Result<(u16, String), String> {
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&origin.address(), timeout)
        .map_err(|e| format!("连接后台失败：{e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let mut headers = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: 0\r\n", origin.port);
    if let Some(token) = token {
        if !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Invalid desktop token".into());
        }
        headers.push_str(&format!("X-IA2-Desktop-Token: {token}\r\n"));
    }
    headers.push_str("\r\n");
    stream
        .write_all(headers.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    stream
        .take(65536)
        .read_to_string(&mut reply)
        .map_err(|e| e.to_string())?;
    let (head, body) = reply
        .split_once("\r\n\r\n")
        .ok_or("后台返回了无效 HTTP 响应")?;
    let code = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("后台返回了无效 HTTP 状态")?;
    Ok((code, body.to_string()))
}

pub fn escaped(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn status_page(title: &str, detail: &str) -> String {
    format!("<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>IA2</title><style>body{{margin:0;background:#14181c;color:#e8eeeb;font:16px 'Segoe UI',sans-serif;display:grid;min-height:100vh;place-content:center}}main{{max-width:680px;padding:48px}}b{{font-size:48px;color:#1fbe6c}}h1{{font-size:24px}}p{{line-height:1.7;white-space:pre-wrap;overflow-wrap:anywhere;color:#b8c8bf}}</style><main><b>IA2</b><h1>{}</h1><p>{}</p></main></html>", escaped(title), escaped(detail))
}

#[derive(Serialize, Deserialize)]
pub struct ShutdownRequest {
    pub id: String,
}
#[derive(Serialize, Deserialize)]
pub struct ShutdownResponse {
    pub id: String,
    pub ok: bool,
    pub detail: String,
}

fn shutdown_request_id(filename: &str) -> Option<&str> {
    let id = filename
        .strip_prefix("shutdown-request-")?
        .strip_suffix(".json")?;
    (id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(id)
}

/// A client owns only its request. Publish a completed JSON file atomically:
/// another client's signal can make the host scan while this client writes.
pub struct PendingShutdownRequest {
    request: PathBuf,
    staged: PathBuf,
    published: bool,
    staged_owned: bool,
}

impl PendingShutdownRequest {
    pub fn publish(directory: &Path, id: &str) -> std::io::Result<Self> {
        let filename = format!("shutdown-request-{id}.json");
        if shutdown_request_id(&filename).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid shutdown request id",
            ));
        }
        let mut pending = Self {
            request: directory.join(&filename),
            staged: directory.join(format!("shutdown-request-{id}.pending")),
            published: false,
            staged_owned: false,
        };
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&pending.staged)?;
            pending.staged_owned = true;
            serde_json::to_writer(&mut file, &ShutdownRequest { id: id.into() })?;
            file.flush()?;
        }
        if pending.request.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "shutdown request already exists",
            ));
        }
        std::fs::rename(&pending.staged, &pending.request)?;
        pending.published = true;
        pending.staged_owned = false;
        Ok(pending)
    }
}

impl Drop for PendingShutdownRequest {
    fn drop(&mut self) {
        if self.published {
            let _ = std::fs::remove_file(&self.request);
        }
        if self.staged_owned {
            let _ = std::fs::remove_file(&self.staged);
        }
    }
}

/// Drain every published request because Windows auto-reset events coalesce.
/// Removing each file before delivery makes repeated events harmless.
pub fn take_shutdown_requests(directory: &Path) -> std::io::Result<Vec<ShutdownRequest>> {
    let mut requests = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let filename = entry.file_name();
        let Some(id) = filename.to_str().and_then(shutdown_request_id) else {
            continue;
        };
        if !entry
            .metadata()
            .is_ok_and(|metadata| metadata.len() <= 4096)
        {
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(bytes) => bytes,
            // One expired/locked request must not lose requests already
            // removed from the directory during this scan.
            Err(_) => continue,
        };
        let request = serde_json::from_slice::<ShutdownRequest>(&bytes)
            .ok()
            .filter(|request| request.id == id);
        // A client may remove its own request after its timeout.
        if std::fs::remove_file(entry.path()).is_ok() {
            if let Some(request) = request {
                requests.push(request);
            }
        }
    }
    Ok(requests)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_shutdown_requests_are_drained_once_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let _first = PendingShutdownRequest::publish(directory.path(), &first).unwrap();
        let _second = PendingShutdownRequest::publish(directory.path(), &second).unwrap();
        let mut ids: Vec<_> = take_shutdown_requests(directory.path())
            .unwrap()
            .into_iter()
            .map(|request| request.id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec![first, second]);
        assert!(take_shutdown_requests(directory.path()).unwrap().is_empty());
    }

    #[test]
    fn shutdown_scan_ignores_staged_invalid_and_mismatched_requests() {
        let directory = tempfile::tempdir().unwrap();
        let id = "a".repeat(64);
        let json = serde_json::to_vec(&ShutdownRequest { id: id.clone() }).unwrap();
        for filename in [
            "shutdown-request.json".to_string(),
            "shutdown-request-short.json".to_string(),
            format!("shutdown-request-{}.json", "g".repeat(64)),
            format!("shutdown-request-{id}.pending"),
            format!("shutdown-request-{id}.json.extra"),
            format!("shutdown-request-{}.json", "b".repeat(64)),
        ] {
            std::fs::write(directory.path().join(filename), &json).unwrap();
        }
        assert!(take_shutdown_requests(directory.path()).unwrap().is_empty());
        assert!(take_shutdown_requests(directory.path()).unwrap().is_empty());
        assert!(directory
            .path()
            .join(format!("shutdown-request-{id}.pending"))
            .is_file());
    }

    #[test]
    fn shutdown_client_cleanup_removes_only_its_own_request() {
        let directory = tempfile::tempdir().unwrap();
        let first = PendingShutdownRequest::publish(directory.path(), &"a".repeat(64)).unwrap();
        let _second = PendingShutdownRequest::publish(directory.path(), &"b".repeat(64)).unwrap();
        drop(first);
        let requests = take_shutdown_requests(directory.path()).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].id, "b".repeat(64));
    }

    #[test]
    fn failed_duplicate_publish_does_not_delete_existing_request() {
        let directory = tempfile::tempdir().unwrap();
        let id = "a".repeat(64);
        let _first = PendingShutdownRequest::publish(directory.path(), &id).unwrap();
        assert!(PendingShutdownRequest::publish(directory.path(), &id).is_err());
        assert_eq!(take_shutdown_requests(directory.path()).unwrap().len(), 1);
    }
    #[test]
    fn navigation_rejects_spoofed_origins_and_privileged_schemes() {
        let origin = Origin::new(3001);
        for good in [
            "http://127.0.0.1:3001/",
            "http://127.0.0.1:3001/hmi.html?project=%E4%B8%AD",
        ] {
            assert!(origin.allows(good));
        }
        for bad in [
            "http://127.0.0.1:30010",
            "http://127.0.0.1:3001.evil.test",
            "http://127.0.0.1:3001@evil.test",
            "http://user@127.0.0.1:3001",
            "http://localhost:3001",
            "https://127.0.0.1:3001",
            "file:///C:/secret",
            "javascript:alert(1)",
        ] {
            assert!(!origin.allows(bad), "{bad}");
        }
        for bad in [
            "file:///C:/Windows",
            "javascript:alert(1)",
            "ms-settings:display",
            "https://user:pass@example.com",
        ] {
            assert!(!external_url(bad));
        }
    }
    #[test]
    fn readiness_requires_own_token_and_requested_port() {
        let origin = Origin::new(3301);
        assert!(parse_ready(
            "IA2_READY={\"url\":\"http://127.0.0.1:3301\",\"instance\":\"abcd\"}",
            &origin,
            "abcd"
        )
        .unwrap());
        assert!(parse_ready(
            "IA2_READY={\"url\":\"http://127.0.0.1:3301\",\"instance\":\"wrong\"}",
            &origin,
            "abcd"
        )
        .is_err());
        assert!(parse_ready(
            "IA2_READY={\"url\":\"http://127.0.0.1:3001\",\"instance\":\"abcd\"}",
            &origin,
            "abcd"
        )
        .is_err());
        assert!(!parse_ready("ordinary log", &origin, "abcd").unwrap());
    }
    #[test]
    fn flags_keep_cli_port_and_shutdown_explicit() {
        assert_eq!(arguments(Vec::new()).unwrap(), Mode::Launch(3001));
        assert_eq!(
            arguments(["--port".into(), "3301".into()]).unwrap(),
            Mode::Launch(3301)
        );
        assert!(arguments(["--port".into(), "0".into()]).is_err());
        assert!(arguments(["--shutdown".into(), "--port".into(), "3001".into()]).is_err());
    }
    #[test]
    fn layout_keeps_unicode_paths_and_never_searches_path() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("中文 安装/bin/IA2.exe");
        let layout = Layout::from_executable(&exe).unwrap();
        assert_eq!(layout.server, exe.parent().unwrap().join("ia2-server.exe"));
        assert!(layout.validate().is_err());
    }
    #[test]
    fn startup_errors_are_html_escaped() {
        let html = status_page("<bad>", "<script>alert('x')</script>");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
    #[test]
    fn shutdown_request_does_not_follow_redirects() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let origin = Origin::new(listener.local_addr().unwrap().port());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let len = stream.read(&mut bytes).unwrap();
            let request = std::str::from_utf8(&bytes[..len]).unwrap();
            assert!(request.contains("X-IA2-Desktop-Token: abcd"));
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        assert_eq!(
            request(&origin, "POST", "/api/desktop/shutdown", Some("abcd"))
                .unwrap()
                .0,
            302
        );
        worker.join().unwrap();
    }
}
