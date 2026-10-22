//! Keeps web pages in the user's browser from driving the IA2 HTTP servers.
//!
//! The IDE server and the edge runtime have no authentication: they rely on
//! being reachable only from the machine that runs them (or, for the runtime,
//! through an SSH forward the developer opened). That holds against the
//! network but not against a browser on that machine, where any page the user
//! has open can send requests to `127.0.0.1` and, if the server allows it,
//! read the answers. Two things make that possible and this crate closes both:
//!
//! * **A permissive CORS policy** lets a page on any origin send JSON `POST`s
//!   (the preflight is answered "yes") and read the responses. Every request
//!   that carries an `Origin` header must therefore be same-origin, or come
//!   from an origin that was named explicitly ([`Guard::allow_origin`]).
//!   WebSocket handshakes are not covered by CORS at all but carry the same
//!   `Origin` header, so the same rule is what protects `/api/lsp`.
//! * **DNS rebinding** makes a page on `http://attacker.example:3001` reach
//!   the loopback server while its `Origin` and `Host` agree. A server bound
//!   to a loopback address therefore answers only requests whose `Host` is a
//!   loopback name.
//!
//! A request without an `Origin` header (curl, the `cs` CLI, the IDE server
//! talking to a runtime, a browser fetching a page it navigated to) is not
//! affected by the first rule: browsers always send `Origin` on the requests
//! that can change state.
//!
//! One narrow door stays open for the edge runtime: the web IDE reads
//! `GET /events` from the runtime through an SSH-forwarded loopback port, which
//! is cross-origin by construction. [`Guard::allow_loopback_reads`] lets a
//! `GET`/`HEAD` of a named path through for a page whose own origin is a
//! loopback name, and for nothing else. Those answers are readable by any
//! page served from a loopback origin; that is the residual exposure.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

/// Environment variable that names extra allowed origins, comma separated. A
/// `--allow-origin` flag wins when both are given.
pub const ALLOWED_ORIGINS_ENV: &str = "IA2_ALLOWED_ORIGINS";

/// The origins an operator configured: the flag's values when there are any,
/// else the comma-separated environment variable's.
pub fn configured_origins(flags: &[String], env: Option<&str>) -> Vec<String> {
    if !flags.is_empty() {
        return flags.to_vec();
    }
    env.map(|text| {
        text.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// What to enforce. Build one with [`Guard::for_listener`], add exceptions,
/// then wrap a router with [`guarded`].
#[derive(Clone, Debug)]
pub struct Guard {
    policy: Arc<Policy>,
}

#[derive(Debug, Clone)]
struct Policy {
    /// The listener is bound to a loopback address: only loopback `Host`s are served.
    loopback_only: bool,
    /// Origins that may call cross-origin, in their canonical form.
    allowed: Vec<Origin>,
    /// Paths a loopback origin may `GET` cross-origin.
    loopback_reads: Vec<&'static str>,
}

impl Guard {
    /// The strictest policy for a server listening on `addr`: same-origin
    /// requests only, and, when `addr` is a loopback address, loopback `Host`
    /// names only.
    pub fn for_listener(addr: SocketAddr) -> Self {
        Self {
            policy: Arc::new(Policy {
                loopback_only: addr.ip().is_loopback(),
                allowed: Vec::new(),
                loopback_reads: Vec::new(),
            }),
        }
    }

    /// Also let pages served from `origin` (`http://host[:port]`) call this
    /// server, preflights included. Each origin is named in full: there is no
    /// wildcard, and `null` (the origin of a sandboxed frame or a `file:` page)
    /// cannot be allowed.
    pub fn allow_origin(self, origin: &str) -> Result<Self, String> {
        let parsed = parse_origin(origin.trim().to_ascii_lowercase().trim_end_matches('/'))
            .ok_or_else(|| {
                format!(
                    "`{origin}` is not an origin: expected http://host[:port] or https://host[:port] \
                     (no path, no wildcard, not `null`)"
                )
            })?;
        let mut policy = (*self.policy).clone();
        if !policy.allowed.contains(&parsed) {
            policy.allowed.push(parsed);
        }
        Ok(Self {
            policy: Arc::new(policy),
        })
    }

    /// [`Guard::allow_origin`] for each of `origins`; the first that is not an
    /// origin is the error.
    pub fn allow_origins(self, origins: &[String]) -> Result<Self, String> {
        origins
            .iter()
            .try_fold(self, |guard, origin| guard.allow_origin(origin))
    }

    /// Let a page served from a loopback origin `GET` (or `HEAD`) exactly these
    /// paths cross-origin, and read the answer.
    pub fn allow_loopback_reads(self, paths: &[&'static str]) -> Self {
        let mut policy = (*self.policy).clone();
        policy.loopback_reads.extend_from_slice(paths);
        Self {
            policy: Arc::new(policy),
        }
    }

    /// Whether `Host` names are being restricted to loopback ones.
    pub fn is_loopback_only(&self) -> bool {
        self.policy.loopback_only
    }
}

/// Wrap `router` (its routes and its fallback) in the guard.
///
/// Put it OUTSIDE the layers that turn a handler failure into a response (the
/// panic catcher), so that a `500` answering an allowed cross-origin request
/// still carries the headers the page needs to read it.
pub fn guarded<S>(router: Router<S>, guard: &Guard) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(middleware::from_fn_with_state(
        guard.policy.clone(),
        enforce,
    ))
}

async fn enforce(State(policy): State<Arc<Policy>>, request: Request, next: Next) -> Response {
    let verdict = decide(
        &policy,
        request.method(),
        request.uri().path(),
        request.headers(),
        request.uri().authority().map(|a| a.as_str()),
    );
    match verdict {
        Verdict::Pass => next.run(request).await,
        Verdict::CrossOrigin(origin) => {
            let mut response = next.run(request).await;
            add_cors_headers(response.headers_mut(), &origin);
            response
        }
        Verdict::Preflight(origin) => preflight_response(&origin, request.headers()),
        Verdict::Reject(reason) => {
            tracing::warn!(
                method = %request.method(),
                path = %request.uri().path(),
                %reason,
                "request refused by the browser-access guard"
            );
            (
                StatusCode::FORBIDDEN,
                format!("request refused: {reason}\n"),
            )
                .into_response()
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Serve it, add nothing.
    Pass,
    /// Serve it and let the page at this `Origin` read the answer.
    CrossOrigin(HeaderValue),
    /// Answer a CORS preflight for this `Origin` without routing it.
    Preflight(HeaderValue),
    Reject(String),
}

fn decide(
    policy: &Policy,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Verdict {
    // --- Host -------------------------------------------------------------
    // HTTP/1.1 always sends one; HTTP/2 carries the authority in the URI. A
    // request with neither (HTTP/1.0 tools, in-process test requests) cannot be
    // a browser, which is who rebinding and cross-origin attacks need.
    let mut hosts = headers.get_all(header::HOST).iter();
    let host_raw: Option<String> = match (hosts.next(), hosts.next()) {
        (Some(_), Some(_)) => return Verdict::Reject("more than one Host header".into()),
        (Some(value), None) => match value.to_str() {
            Ok(text) => Some(text.to_string()),
            Err(_) => return Verdict::Reject("the Host header is not plain text".into()),
        },
        (None, _) => uri_authority.map(str::to_string),
    };
    let host = match &host_raw {
        None => None,
        Some(raw) => match parse_authority(&raw.to_ascii_lowercase()) {
            Some(authority) => Some(authority),
            None => return Verdict::Reject(format!("malformed Host `{}`", shown(raw))),
        },
    };
    if policy.loopback_only {
        if let (Some(authority), Some(raw)) = (&host, &host_raw) {
            if !is_loopback_host(&authority.host) {
                return Verdict::Reject(format!(
                    "Host `{}` is not a loopback name; this server is bound to a loopback \
                     address and answers only to localhost, 127.0.0.0/8 and [::1]",
                    shown(raw)
                ));
            }
        }
    }

    // --- Origin -----------------------------------------------------------
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let origin_value = match (origins.next(), origins.next()) {
        (None, _) => return Verdict::Pass,
        (Some(_), Some(_)) => return Verdict::Reject("more than one Origin header".into()),
        (Some(value), None) => value,
    };
    let origin_raw = match origin_value.to_str() {
        Ok(text) => text,
        Err(_) => return Verdict::Reject("the Origin header is not plain text".into()),
    };
    let Some(origin) = parse_origin(&origin_raw.to_ascii_lowercase()) else {
        return Verdict::Reject(format!(
            "Origin `{}` is opaque or malformed (a sandboxed frame, a file: page or a data: page)",
            shown(origin_raw)
        ));
    };

    // Same origin = same host and port as the request was addressed to. The
    // scheme is not compared: the server cannot tell whether a TLS-terminating
    // proxy sits in front of it, and a page cannot claim an `https` origin on a
    // port this plain-HTTP server holds.
    if let Some(authority) = &host {
        let host_port = authority.port.unwrap_or(origin.scheme.default_port());
        if authority.host == origin.host && host_port == origin.port {
            return Verdict::Pass;
        }
    }
    if policy.allowed.contains(&origin) {
        let preflight = method == Method::OPTIONS
            && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
        return if preflight {
            Verdict::Preflight(origin_value.clone())
        } else {
            Verdict::CrossOrigin(origin_value.clone())
        };
    }
    let is_read = method == Method::GET || method == Method::HEAD;
    if is_read && policy.loopback_reads.contains(&path) && is_loopback_host(&origin.host) {
        return Verdict::CrossOrigin(origin_value.clone());
    }
    Verdict::Reject(format!(
        "Origin `{}` is not allowed to call this server: it answers same-origin requests only \
         (start it with --allow-origin to name another page)",
        shown(origin_raw)
    ))
}

fn add_cors_headers(headers: &mut HeaderMap, origin: &HeaderValue) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("*"),
    );
    headers.append(header::VARY, HeaderValue::from_static("origin"));
}

fn preflight_response(origin: &HeaderValue, request: &HeaderMap) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    if let Some(method) = request.get(header::ACCESS_CONTROL_REQUEST_METHOD) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(requested) = request.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
    }
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    headers.append(
        header::VARY,
        HeaderValue::from_static(
            "origin, access-control-request-method, access-control-request-headers",
        ),
    );
    response
}

/// A header value, bounded and with anything unusual replaced, for a message.
fn shown(raw: &str) -> String {
    raw.chars()
        .take(100)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// A scheme, host and port: what a browser compares to decide "same origin".
#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    scheme: Scheme,
    host: String,
    port: u16,
}

#[derive(Debug, PartialEq, Eq)]
struct Authority {
    /// Lower case, no brackets around an IPv6 literal, no trailing dot.
    host: String,
    port: Option<u16>,
}

/// `Origin: http://host[:port]` (already lower-cased). `null`, other schemes
/// and anything with a path, user info or query are not origins.
fn parse_origin(raw: &str) -> Option<Origin> {
    let (scheme, rest) = if let Some(rest) = raw.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else {
        (Scheme::Https, raw.strip_prefix("https://")?)
    };
    let authority = parse_authority(rest)?;
    Some(Origin {
        scheme,
        host: authority.host,
        port: authority.port.unwrap_or(scheme.default_port()),
    })
}

/// `host`, `host:port`, `[v6]` or `[v6]:port` (already lower-cased).
fn parse_authority(raw: &str) -> Option<Authority> {
    if raw.is_empty()
        || raw
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b"/\\@?#%".contains(&b))
    {
        return None;
    }
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        host.parse::<std::net::Ipv6Addr>().ok()?;
        let tail = &rest[end + 1..];
        let port = match tail {
            "" => None,
            tail => Some(tail.strip_prefix(':')?),
        };
        (host, port)
    } else {
        match raw.rsplit_once(':') {
            Some((host, _)) if host.contains(':') => return None,
            Some((host, port)) => (host, Some(port)),
            None => (raw, None),
        }
    };
    let port = match port {
        None => None,
        Some(digits) => {
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(digits.parse::<u16>().ok()?)
        }
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(Authority {
        host: host.to_string(),
        port,
    })
}

/// `localhost`, `*.localhost` (which browsers resolve to loopback themselves,
/// so a name under it cannot be pointed elsewhere), `127.0.0.0/8` and `::1`.
fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests;
