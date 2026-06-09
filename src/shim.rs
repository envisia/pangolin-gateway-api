//! Ext-authz shim: bridges Envoy's HTTP external-authorization protocol to
//! pangolin's badger session verification API.
//!
//! Pangolin protects resources with its `badger` Traefik plugin. Envoy can't
//! run Traefik plugins, so the controller (when `CONFIG_EXT_AUTHZ_SERVICE` is
//! set) emits a `SecurityPolicy` per protected route that points Envoy's
//! ext-authz filter at this service. Per Envoy's HTTP ext-authz contract the
//! check request preserves the original request's **method**, **Host** and
//! **path** (the configured `path` acts as a prefix), and on a non-2xx reply
//! Envoy returns our status and headers (including `Location` / `Set-Cookie`)
//! to the client — which is exactly what badger's redirect-to-portal flow
//! needs.
//!
//! Per check request the shim mirrors `fosrl/badger`:
//!
//! 1. If the query carries the resource-session-request param (the portal's
//!    post-login handoff), exchange it via `POST {api}/badger/exchange-session`
//!    and answer `302` back to the original URL (param stripped) with the
//!    returned `Set-Cookie`.
//! 2. Otherwise `POST {api}/badger/verify-session` with the session cookies,
//!    request metadata, headers and query.
//!    * `data.redirectUrl`            → `302` with `Location`
//!    * `data.headerAuthChallenged`   → `401` + `WWW-Authenticate: Basic`
//!    * `data.valid`                  → `200` + `Remote-User`/`Remote-Email`/…
//!      response headers (forward them upstream via
//!      `CONFIG_EXT_AUTHZ_HEADERS_TO_BACKEND` if the backend wants them)
//!    * anything else                 → `401`
//! 3. Pangolin unreachable → `503`. Envoy treats any non-2xx as deny, so an
//!    outage fails **closed**.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response, StatusCode, header};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use url::Url;

/// Everything the shim binary reads from the environment. All names start
/// with `SHIM_` to keep them apart from the controller's `CONFIG_*` set.
#[derive(Debug, Clone)]
pub struct ShimConfig {
    /// `SHIM_LISTEN`, default `0.0.0.0:9001`.
    pub listen: String,
    /// `SHIM_PANGOLIN_API_BASE_URL` (required) — e.g.
    /// `http://pangolin.pangolin-system.svc.cluster.local:3001/api/v1`.
    /// Matches the `apiBaseUrl` pangolin hands to badger.
    pub pangolin_api_base_url: Url,
    /// `SHIM_PATH_PREFIX`, default `/verify`. Must equal the controller's
    /// `CONFIG_EXT_AUTHZ_PATH`; Envoy prepends it to the original path.
    pub path_prefix: String,
    /// `SHIM_USER_SESSION_COOKIE_NAME`, default `p_session_token`. Cookies
    /// whose name starts with this are forwarded as sessions (badger sends
    /// prefix matches: resource session cookies share the prefix).
    pub user_session_cookie_name: String,
    /// `SHIM_RESOURCE_SESSION_REQUEST_PARAM`, default `p_session_request`.
    pub resource_session_request_param: String,
    /// `SHIM_PANGOLIN_TIMEOUT`, default 10s.
    pub timeout: Duration,
    /// `SHIM_CA_FILE` — PEM bundle to trust for an https pangolin API.
    pub ca_file: Option<String>,
    /// `SHIM_TLS_SKIP_VERIFY` — requires
    /// `I_UNDERSTAND_SHIM_TLS_SKIP_VERIFY_IS_INSECURE=true`, mirroring the
    /// controller's guard.
    pub tls_skip_verify: bool,
}

impl ShimConfig {
    pub fn from_env() -> Result<Self> {
        let api_raw = std::env::var("SHIM_PANGOLIN_API_BASE_URL")
            .context("required environment variable SHIM_PANGOLIN_API_BASE_URL is not set")?;
        let pangolin_api_base_url = Url::parse(api_raw.trim_end_matches('/'))
            .context("invalid SHIM_PANGOLIN_API_BASE_URL")?;

        let path_prefix = optional("SHIM_PATH_PREFIX").unwrap_or_else(|| "/verify".into());
        if !path_prefix.is_empty() && !path_prefix.starts_with('/') {
            bail!("SHIM_PATH_PREFIX must start with '/' (got {path_prefix:?})");
        }

        let tls_skip_verify = optional("SHIM_TLS_SKIP_VERIFY").is_some_and(|v| {
            matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        });
        if tls_skip_verify
            && !optional("I_UNDERSTAND_SHIM_TLS_SKIP_VERIFY_IS_INSECURE")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            bail!(
                "SHIM_TLS_SKIP_VERIFY=true requires \
                 I_UNDERSTAND_SHIM_TLS_SKIP_VERIFY_IS_INSECURE=true to be explicitly set"
            );
        }

        Ok(Self {
            listen: optional("SHIM_LISTEN").unwrap_or_else(|| "0.0.0.0:9001".into()),
            pangolin_api_base_url,
            path_prefix,
            user_session_cookie_name: optional("SHIM_USER_SESSION_COOKIE_NAME")
                .unwrap_or_else(|| "p_session_token".into()),
            resource_session_request_param: optional("SHIM_RESOURCE_SESSION_REQUEST_PARAM")
                .unwrap_or_else(|| "p_session_request".into()),
            timeout: optional("SHIM_PANGOLIN_TIMEOUT")
                .map(|v| humantime::parse_duration(&v))
                .transpose()
                .context("invalid SHIM_PANGOLIN_TIMEOUT")?
                .unwrap_or(Duration::from_secs(10)),
            ca_file: optional("SHIM_CA_FILE"),
            tls_skip_verify,
        })
    }
}

/// HTTP client honoring the shim's TLS settings towards pangolin.
pub fn build_http_client(cfg: &ShimConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(path) = &cfg.ca_file {
        let pem = std::fs::read(path).with_context(|| format!("reading SHIM_CA_FILE {path}"))?;
        let cert = reqwest::Certificate::from_pem_bundle(&pem)
            .with_context(|| format!("parsing SHIM_CA_FILE {path}"))?;
        for c in cert {
            builder = builder.add_root_certificate(c);
        }
    }
    if cfg.tls_skip_verify {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("building HTTP client")
}

fn optional(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

pub struct ShimState {
    pub cfg: ShimConfig,
    pub http: reqwest::Client,
}

pub fn router(state: Arc<ShimState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback(check)
        .with_state(state)
}

// ---- wire types -----------------------------------------------------------
// Field names mirror pangolin's zod schemas in `server/routers/badger/`
// (`verifySession.ts`, `exchangeSession.ts`) — the authoritative contract.

#[derive(Debug, Serialize)]
struct VerifyBody {
    sessions: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    query: BTreeMap<String, String>,
    #[serde(rename = "originalRequestURL")]
    original_request_url: String,
    scheme: String,
    host: String,
    path: String,
    method: String,
    tls: bool,
    #[serde(rename = "requestIp", skip_serializing_if = "Option::is_none")]
    request_ip: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VerifyResponse {
    #[serde(default)]
    data: VerifyData,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyData {
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    redirect_url: Option<String>,
    #[serde(default)]
    header_auth_challenged: Option<bool>,
    #[serde(default)]
    user_data: Option<UserData>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserData {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExchangeBody {
    #[serde(rename = "requestToken")]
    request_token: String,
    host: String,
    #[serde(rename = "requestIp", skip_serializing_if = "Option::is_none")]
    request_ip: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    #[serde(default)]
    data: ExchangeData,
}

#[derive(Debug, Default, Deserialize)]
struct ExchangeData {
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    cookie: Option<String>,
}

// ---- request handling -----------------------------------------------------

async fn check(State(state): State<Arc<ShimState>>, req: Request<Body>) -> Response<Body> {
    let cfg = &state.cfg;

    let full_path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    // Envoy prepends the configured path prefix to the original path. Tolerate
    // a missing prefix (e.g. probes or a differently-configured policy) by
    // treating the whole path as the original one.
    let original_path_q = strip_path_prefix(&cfg.path_prefix, &full_path).unwrap_or_else(|| {
        debug!(path = %full_path, prefix = %cfg.path_prefix, "path does not carry the configured prefix");
        full_path.clone()
    });

    let host = header_str(&req, header::HOST).unwrap_or_default();
    if host.is_empty() {
        return plain(StatusCode::BAD_REQUEST, "missing Host header");
    }
    let scheme = header_str(&req, "x-forwarded-proto").unwrap_or_else(|| "https".into());
    let request_ip = header_str(&req, "x-forwarded-for")
        .and_then(|v| v.split(',').next().map(|ip| ip.trim().to_string()))
        .filter(|ip| !ip.is_empty());
    let method = req.method().as_str().to_string();

    let (path_only, query_str) = match original_path_q.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (original_path_q.clone(), String::new()),
    };
    let query = parse_query(&query_str);
    let sessions = collect_sessions(
        header_str(&req, header::COOKIE).as_deref().unwrap_or(""),
        &cfg.user_session_cookie_name,
    );
    let headers = collect_headers(&req);

    // Post-login handoff: the auth portal redirects back to the resource with
    // a one-time session-request token in the query. Exchange it for the real
    // session cookie and bounce the client to the cleaned-up URL.
    if let Some(token) = query.get(&cfg.resource_session_request_param) {
        match exchange_session(&state, token, &host, request_ip.clone()).await {
            Ok(ExchangeData {
                valid: true,
                cookie: Some(cookie),
            }) if !cookie.is_empty() => {
                let location = format!(
                    "{scheme}://{host}{}",
                    strip_query_param(&original_path_q, &cfg.resource_session_request_param)
                );
                let mut resp = Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::LOCATION, location);
                match header::HeaderValue::from_str(&cookie) {
                    Ok(v) => resp = resp.header(header::SET_COOKIE, v),
                    Err(_) => {
                        warn!(
                            "exchange-session returned a cookie that is not a valid header value"
                        );
                        return plain(StatusCode::SERVICE_UNAVAILABLE, "bad exchange cookie");
                    }
                }
                return resp.body(Body::empty()).expect("static response");
            }
            Ok(_) => {
                // Invalid/expired token — fall through to a normal verify,
                // which will redirect back to the portal.
                debug!("exchange-session token rejected; falling back to verify-session");
            }
            Err(e) => {
                warn!(error = ?e, "exchange-session call failed");
                return plain(StatusCode::SERVICE_UNAVAILABLE, "auth backend unavailable");
            }
        }
    }

    let body = VerifyBody {
        sessions,
        headers,
        query,
        original_request_url: format!("{scheme}://{host}{original_path_q}"),
        scheme: scheme.clone(),
        host: host.clone(),
        path: path_only,
        method,
        tls: scheme == "https",
        request_ip,
    };

    let data = match verify_session(&state, &body).await {
        Ok(d) => d,
        Err(e) => {
            warn!(error = ?e, "verify-session call failed");
            return plain(StatusCode::SERVICE_UNAVAILABLE, "auth backend unavailable");
        }
    };

    if let Some(redirect) = data.redirect_url.as_deref().filter(|r| !r.is_empty()) {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, redirect)
            .body(Body::empty())
            .unwrap_or_else(|_| plain(StatusCode::SERVICE_UNAVAILABLE, "bad redirect"));
    }
    if data.header_auth_challenged.unwrap_or(false) {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Basic realm=\"pangolin\"")
            .body(Body::empty())
            .expect("static response");
    }
    if data.valid {
        let mut resp = Response::builder().status(StatusCode::OK);
        if let Some(user) = &data.user_data {
            for (name, value) in [
                ("remote-user-id", &user.user_id),
                ("remote-user", &user.username),
                ("remote-email", &user.email),
                ("remote-name", &user.name),
                ("remote-role", &user.role),
            ] {
                if let Some(v) = value
                    && let Ok(hv) = header::HeaderValue::from_str(v)
                {
                    resp = resp.header(name, hv);
                }
            }
        }
        return resp.body(Body::empty()).expect("static response");
    }
    plain(StatusCode::UNAUTHORIZED, "unauthorized")
}

async fn verify_session(state: &ShimState, body: &VerifyBody) -> Result<VerifyData> {
    let url = endpoint(&state.cfg.pangolin_api_base_url, "badger/verify-session")?;
    let resp = state
        .http
        .post(url)
        .timeout(state.cfg.timeout)
        .json(body)
        .send()
        .await
        .context("sending verify-session")?;
    // Pangolin answers 401-ish statuses with a JSON body that still carries
    // data.valid/redirectUrl — parse the body regardless of status.
    let status = resp.status();
    let parsed: VerifyResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) if status.is_success() => return Err(e).context("decoding verify-session response"),
        Err(_) => {
            // Non-2xx without a parseable body: deny without portal redirect.
            return Ok(VerifyData::default());
        }
    };
    Ok(parsed.data)
}

async fn exchange_session(
    state: &ShimState,
    token: &str,
    host: &str,
    request_ip: Option<String>,
) -> Result<ExchangeData> {
    let url = endpoint(&state.cfg.pangolin_api_base_url, "badger/exchange-session")?;
    let resp = state
        .http
        .post(url)
        .timeout(state.cfg.timeout)
        .json(&ExchangeBody {
            request_token: token.to_string(),
            // Pangolin strips a port itself, but be tidy about it.
            host: host.split(':').next().unwrap_or(host).to_string(),
            request_ip,
        })
        .send()
        .await
        .context("sending exchange-session")?;
    let status = resp.status();
    let parsed: ExchangeResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) if status.is_success() => {
            return Err(e).context("decoding exchange-session response");
        }
        Err(_) => return Ok(ExchangeData::default()),
    };
    Ok(parsed.data)
}

fn endpoint(base: &Url, suffix: &str) -> Result<Url> {
    let mut s = base.as_str().trim_end_matches('/').to_string();
    s.push('/');
    s.push_str(suffix);
    Url::parse(&s).context("building pangolin endpoint URL")
}

fn plain(status: StatusCode, msg: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(msg))
        .expect("static response")
}

fn header_str<K>(req: &Request<Body>, key: K) -> Option<String>
where
    K: header::AsHeaderName,
{
    req.headers()
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

// ---- pure helpers (unit-tested) -------------------------------------------

/// Strip the configured prefix from `path_and_query`. `Some` when the prefix
/// matched on a segment boundary, `None` otherwise.
pub fn strip_path_prefix(prefix: &str, path_and_query: &str) -> Option<String> {
    if prefix.is_empty() || prefix == "/" {
        return Some(path_and_query.to_string());
    }
    let prefix = prefix.trim_end_matches('/');
    let rest = path_and_query.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some("/".into());
    }
    // Reject partial-segment matches like prefix `/verify` on `/verifyx`.
    if rest.starts_with('/') || rest.starts_with('?') {
        Some(if rest.starts_with('?') {
            format!("/{rest}")
        } else {
            rest.to_string()
        })
    } else {
        None
    }
}

/// Cookies whose name starts with `name_prefix`, mirroring badger's
/// `strings.HasPrefix(cookie.Name, userSessionCookieName)` collection.
pub fn collect_sessions(cookie_header: &str, name_prefix: &str) -> BTreeMap<String, String> {
    let mut sessions = BTreeMap::new();
    for pair in cookie_header.split(';') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() && name.starts_with(name_prefix) {
            sessions.insert(name.to_string(), value.trim().to_string());
        }
    }
    sessions
}

/// First value per key, like badger's `queryValues` flattening.
pub fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        out.entry(k.into_owned()).or_insert_with(|| v.into_owned());
    }
    out
}

/// Remove one query parameter from a `path?query` string, dropping the `?`
/// entirely when nothing remains.
pub fn strip_query_param(path_and_query: &str, param: &str) -> String {
    let Some((path, query)) = path_and_query.split_once('?') else {
        return path_and_query.to_string();
    };
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    let mut kept_any = false;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        if k != param {
            serializer.append_pair(&k, &v);
            kept_any = true;
        }
    }
    if kept_any {
        format!("{path}?{}", serializer.finish())
    } else {
        path.to_string()
    }
}

/// First value per header in Go's canonical MIME casing (what badger sends and
/// therefore what pangolin sees today). Cookies are omitted — sessions carry them.
fn collect_headers(req: &Request<Body>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in req.headers() {
        if name == header::COOKIE {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.entry(canonical_header(name.as_str()))
                .or_insert_with(|| v.to_string());
        }
    }
    out
}

/// `x-forwarded-proto` -> `X-Forwarded-Proto`, matching Go's
/// `textproto.CanonicalMIMEHeaderKey`.
pub fn canonical_header(name: &str) -> String {
    name.split('-')
        .map(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(first) => {
                    first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_stripping() {
        assert_eq!(
            strip_path_prefix("/verify", "/verify/app?x=1").as_deref(),
            Some("/app?x=1")
        );
        assert_eq!(
            strip_path_prefix("/verify", "/verify").as_deref(),
            Some("/")
        );
        assert_eq!(
            strip_path_prefix("/verify", "/verify?x=1").as_deref(),
            Some("/?x=1")
        );
        assert_eq!(strip_path_prefix("/verify", "/verifyx/app"), None);
        assert_eq!(strip_path_prefix("", "/app").as_deref(), Some("/app"));
    }

    #[test]
    fn session_cookie_prefix_filter() {
        let sessions = collect_sessions(
            "p_session_token=abc; p_session_token_42=def; other=zzz",
            "p_session_token",
        );
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions["p_session_token"], "abc");
        assert_eq!(sessions["p_session_token_42"], "def");
    }

    #[test]
    fn query_param_stripping() {
        assert_eq!(
            strip_query_param("/app?p_session_request=tok&keep=1", "p_session_request"),
            "/app?keep=1"
        );
        assert_eq!(
            strip_query_param("/app?p_session_request=tok", "p_session_request"),
            "/app"
        );
        assert_eq!(strip_query_param("/app", "p_session_request"), "/app");
    }

    #[test]
    fn canonical_header_casing() {
        assert_eq!(canonical_header("x-forwarded-proto"), "X-Forwarded-Proto");
        assert_eq!(canonical_header("authorization"), "Authorization");
        assert_eq!(canonical_header("HOST"), "Host");
    }

    #[test]
    fn verify_body_serializes_with_pangolin_field_names() {
        let body = VerifyBody {
            sessions: BTreeMap::from([("p_session_token".into(), "abc".into())]),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            original_request_url: "https://app.example.com/x".into(),
            scheme: "https".into(),
            host: "app.example.com".into(),
            path: "/x".into(),
            method: "GET".into(),
            tls: true,
            request_ip: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("originalRequestURL").is_some());
        assert!(json.get("sessions").is_some());
        assert!(json.get("tls").is_some());
        // requestIp omitted when None.
        assert!(json.get("requestIp").is_none());
    }
}
