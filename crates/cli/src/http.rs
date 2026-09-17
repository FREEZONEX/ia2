//! HTTP plumbing shared by every online subcommand: a no-proxy `ureq`
//! agent, JSON/text verb helpers that PRESERVE the server's error body,
//! the `--project` routing header, and exit-code mapping.
//!
//! The error contract (see `MEMORY/principles.md`): the server writes
//! human-actionable error bodies ("missing field `application`", …).
//! Every helper here surfaces that body verbatim and maps the HTTP
//! status onto the CLI's exit-code convention:
//!   * 4xx — the request was wrong (fixable by the caller) → exit 2
//!   * 5xx — the server failed → exit 3
//!   * transport (refused / timeout / DNS) → exit 3

use anyhow::{Context, Result};

/// Base URL + routing context for one CLI invocation. Built once in
/// `main` from the global `--server` / `--project` flags and threaded
/// through every command so there is exactly one place that speaks HTTP.
#[derive(Clone, Debug)]
pub(crate) struct Client {
    pub server: String,
    pub project: Option<String>,
}

/// A failed HTTP call, with everything the agent needs to fix it.
#[derive(Debug)]
pub(crate) enum ApiError {
    /// The server answered with a non-2xx status. `body` is the
    /// server's error text (its whole point is to be shown).
    Status {
        method: &'static str,
        url: String,
        code: u16,
        body: String,
    },
    /// No HTTP conversation happened (connection refused, timeout…).
    Transport {
        method: &'static str,
        url: String,
        detail: String,
    },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Status {
                method,
                url,
                code,
                body,
            } => {
                let body = body.trim();
                if body.is_empty() {
                    write!(f, "{method} {url}: HTTP {code}")
                } else {
                    write!(f, "{method} {url}: HTTP {code}\n{body}")
                }
            }
            ApiError::Transport {
                method,
                url,
                detail,
            } => write!(
                f,
                "{method} {url}: {detail}\n(is the server running? start it with `ia2-server` \
                 or `cargo run -p server`)"
            ),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// CLI exit code for this failure: 2 when the caller can fix the
    /// request (4xx), 3 when the infrastructure is at fault.
    pub fn exit_code(&self) -> i32 {
        match self {
            ApiError::Status { code, .. } if (400..500).contains(code) => 2,
            _ => 3,
        }
    }
}

/// A caller-fixable input problem (bad resource path, unreadable
/// `--from` file, malformed JSON body). Exits 2, like clap usage
/// errors — distinct from infrastructure failures (exit 3).
#[derive(Debug)]
pub(crate) struct UsageError(pub anyhow::Error);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for UsageError {}

impl UsageError {
    pub fn wrap(e: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(UsageError(e))
    }
}

/// Request body variants the IA2 API actually uses.
pub(crate) enum Body<'a> {
    None,
    Json(&'a serde_json::Value),
    /// POU source — `save_pou` takes text/plain, not JSON.
    Text(&'a str),
}

impl Client {
    pub fn new(server: String, project: Option<String>) -> Self {
        Self { server, project }
    }

    /// Absolute URL for an API path (`/api/...`).
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.server, path)
    }

    pub fn get(&self, path: &str) -> Result<serde_json::Value> {
        self.request("GET", path, Body::None, None)
    }

    pub fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.request("POST", path, Body::Json(body), None)
    }

    pub fn put(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.request("PUT", path, Body::Json(body), None)
    }

    pub fn put_text(&self, path: &str, body: &str) -> Result<serde_json::Value> {
        self.request("PUT", path, Body::Text(body), None)
    }

    pub fn delete(&self, path: &str) -> Result<serde_json::Value> {
        self.request("DELETE", path, Body::None, None)
    }

    /// Does the resource answer 2xx to GET? 404 → Ok(false); any other
    /// failure (transport, 5xx, 4xx≠404) propagates, so "server down"
    /// never masquerades as "doesn't exist yet".
    pub fn exists(&self, path: &str) -> Result<bool> {
        match self.get(path) {
            Ok(_) => Ok(true),
            Err(e) => match e.downcast_ref::<ApiError>() {
                Some(ApiError::Status { code: 404, .. }) => Ok(false),
                _ => Err(e),
            },
        }
    }

    /// One HTTP round-trip. `timeout` overrides the default 30 s (the
    /// deploy path passes 600 s — tar+ssh can take minutes).
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Body<'_>,
        timeout: Option<std::time::Duration>,
    ) -> Result<serde_json::Value> {
        let url = self.url(path);
        // `method` lives for the whole error's lifetime; leak-free
        // static mapping for the common verbs.
        let method_static: &'static str = match method.to_ascii_uppercase().as_str() {
            "GET" => "GET",
            "POST" => "POST",
            "PUT" => "PUT",
            "DELETE" => "DELETE",
            "PATCH" => "PATCH",
            other => anyhow::bail!("unsupported HTTP method `{other}`"),
        };

        let mut req = http_agent().request(method_static, &url);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        if let Some(p) = &self.project {
            let encoded =
                percent_encoding::utf8_percent_encode(p, percent_encoding::NON_ALPHANUMERIC);
            req = req
                .set("X-IA2-Project", &encoded.to_string())
                .set("X-IA2-Project-Encoding", "percent");
        }
        // Attribution convention (ADR-0002): mutating requests carry
        // their operator's origin so the server's takeover overlay and
        // the edge's audit ring can tell `cs` apart from unattributed
        // writers. Inside `cs agent run`, a cs-origin mutating request
        // also refreshes the session's liveness and the banner stays on
        // the session label; outside a session the server labels the
        // action "… — cs (no session)" rather than suppressing it.
        req = req.set("X-IA2-Origin", "cs");

        let outcome = match body {
            Body::None => req.call(),
            Body::Json(v) => req.set("Content-Type", "application/json").send_json(v),
            Body::Text(s) => req.set("Content-Type", "text/plain").send_string(s),
        };

        let resp = match outcome {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, resp)) => {
                // THE load-bearing line: read the server's error body
                // instead of discarding it. This is what turns
                // "HTTP 422" into "missing field `application`".
                let body = resp.into_string().unwrap_or_default();
                return Err(ApiError::Status {
                    method: method_static,
                    url,
                    code,
                    body,
                }
                .into());
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(ApiError::Transport {
                    method: method_static,
                    url,
                    detail: t.to_string(),
                }
                .into());
            }
        };

        // 2xx. Most endpoints answer JSON; tolerate empty bodies.
        let text = resp
            .into_string()
            .with_context(|| format!("reading response from {url}"))?;
        if text.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&text)
            .with_context(|| format!("decoding JSON from {url} (got: {})", truncate(&text, 200)))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Pretty-print a serialisable value as JSON on stdout and return the
/// clean-success exit code.
pub(crate) fn print_json<T: serde::Serialize>(v: &T) -> Result<i32> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(0)
}

/// Read a body from a file path, or from stdin if `from == "-"`.
pub(crate) fn read_blob(from: &str) -> Result<String> {
    use std::io::Read;
    if from == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(from).with_context(|| format!("reading {from}"))
    }
}

/// Read + parse a JSON body from a file path or stdin (`-`).
pub(crate) fn read_json_blob(from: &str) -> Result<serde_json::Value> {
    let text = read_blob(from)?;
    serde_json::from_str(&text).with_context(|| format!("parsing JSON from {from}"))
}

/// Tiny URL-component escaper. Encodes everything outside
/// `[A-Za-z0-9_.~-]` — notably `/`, so nested resource names travel as
/// `%2F` the way the API expects.
pub(crate) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Percent-encoding is defined over BYTES. Encoding `char`s instead
    // emitted the code point: `泵` (U+6CF5) became `%6CF5`, which a server
    // reads as `%6C` + the literal `F51` — so `泵1` arrived as `lF51`, and
    // `café` as `caf%E9`, which is not valid UTF-8 at all. Silent, and only
    // for non-ASCII names, which nothing in the project store forbids.
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build a no-proxy ureq Agent. ureq 2.x auto-picks up `HTTP_PROXY` /
/// `HTTPS_PROXY` env vars at request time, which routes our localhost
/// API traffic through the user's developer proxy (Clash etc.). An
/// explicit Agent with no proxy keeps API calls direct.
pub(crate) fn http_agent() -> &'static ureq::Agent {
    use std::sync::OnceLock;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build()
    })
}

#[cfg(test)]
mod tests {
    use super::url_encode;

    #[test]
    fn unreserved_characters_pass_through() {
        assert_eq!(url_encode("pump1"), "pump1");
        assert_eq!(url_encode("a_b.c-d~e"), "a_b.c-d~e");
    }

    /// The defect: percent-encoding is over bytes, not chars. Encoding the
    /// code point produced `%6CF5` for `泵` — a server decodes `%6C` and
    /// leaves `F5` literal, so the name it looks up is not the name asked
    /// for. Nothing in the project store restricts names to ASCII.
    #[test]
    fn non_ascii_names_encode_as_utf8_bytes() {
        assert_eq!(url_encode("泵1"), "%E6%B3%B51");
        assert_eq!(url_encode("café"), "caf%C3%A9");
        // Round-trips through a standard decoder back to the original.
        assert_eq!(
            String::from_utf8(percent_decode(&url_encode("总览"))).unwrap(),
            "总览"
        );
    }

    #[test]
    fn reserved_and_space_are_escaped() {
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("a?b#c"), "a%3Fb%23c");
    }

    /// Minimal decoder — deliberately not the encoder's inverse by
    /// construction, so the round-trip above is a real check.
    fn percent_decode(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).expect("ascii hex");
                out.push(u8::from_str_radix(hex, 16).expect("hex pair"));
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        out
    }
}
