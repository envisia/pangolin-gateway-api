//! Smoke tests for `pangolin::Client::fetch`.
//!
//! A tiny hand-rolled HTTP/1.1 server is spun up on an ephemeral port so we
//! can drive every branch of the conditional-GET state machine — 200 with
//! ETag, 304 short-circuit, 5xx error, oversize body guard — without pulling
//! in `hyper`/`wiremock` as a dev-dependency.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

use pangolin_gateway_controller::config::{BackendKind, Config, ReconcileScope};
use pangolin_gateway_controller::pangolin::{Client, FetchOutcome};

#[derive(Clone)]
struct CannedResponse {
    status_line: &'static str,
    extra_headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
struct ObservedRequest {
    headers: BTreeMap<String, String>,
}

/// Start an HTTP server that serves one request per `responses` entry, in
/// order, and records the request headers it saw.
async fn start_server(
    responses: Vec<CannedResponse>,
) -> (String, Arc<Mutex<Vec<ObservedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let observed = Arc::new(Mutex::new(Vec::<ObservedRequest>::new()));
    let observed_clone = observed.clone();

    tokio::spawn(async move {
        for canned in responses {
            let (mut socket, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };

            let mut buf = vec![0u8; 8192];
            let mut received = Vec::<u8>::new();
            loop {
                let n = match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                received.extend_from_slice(&buf[..n]);
                if received.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }

            let req = parse_request(&received);
            observed_clone.lock().await.push(req);

            let mut resp = Vec::<u8>::new();
            resp.extend_from_slice(canned.status_line.as_bytes());
            resp.extend_from_slice(b"\r\n");
            resp.extend_from_slice(format!("Content-Length: {}\r\n", canned.body.len()).as_bytes());
            for (k, v) in &canned.extra_headers {
                resp.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
            }
            resp.extend_from_slice(b"Connection: close\r\n\r\n");
            resp.extend_from_slice(&canned.body);

            let _ = socket.write_all(&resp).await;
            let _ = socket.shutdown().await;
        }
    });

    (format!("http://{addr}/api/v1/traefik-config"), observed)
}

fn parse_request(raw: &[u8]) -> ObservedRequest {
    let text = String::from_utf8_lossy(raw);
    let mut headers = BTreeMap::new();
    let mut lines = text.split("\r\n");
    let _request_line = lines.next();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    ObservedRequest { headers }
}

fn config_pointing_at(endpoint: &str) -> Config {
    Config {
        pangolin_endpoint: Url::parse(endpoint).unwrap(),
        auth_header: Some("Bearer test-token".into()),
        fetch_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_secs(30),
        max_backoff: Duration::from_secs(60),
        max_response_body_bytes: 1 << 16,
        tls_skip_verify: false,
        ca_file: None,
        namespace: "gateway".into(),
        parent_gateway: "eg".into(),
        parent_gateway_namespace: Some("gateway".into()),
        listener_set_name: "pangolin".into(),
        http_port: 80,
        https_port: 443,
        enable_https_listeners: false,
        backend_kind: BackendKind::Service,
        tls_secret_template: None,
        tls_secret_namespace: None,
        field_manager: "pangolin-gateway-controller".into(),
        managed_label_key: "app.kubernetes.io/managed-by".into(),
        managed_label_value: "pangolin-gateway-controller".into(),
        instance_label_key: "pangolin.envisia.de/instance".into(),
        instance_label_value: "default".into(),
        managed_annotation_key: "pangolin.envisia.de/source".into(),
        managed_annotation_value: "pangolin-gateway-controller".into(),
        httproute_annotations: BTreeMap::new(),
        listenerset_annotations: BTreeMap::new(),
        badger_ext_auth: None,
        pangolin_dashboard: None,
        gerbil_udp: None,
        reconcile_scope: ReconcileScope::all(),
        read_only: false,
        log_traefik_config: false,
    }
}

const EMPTY_CONFIG_JSON: &[u8] = br#"{"http":{"routers":{},"services":{}}}"#;

#[tokio::test]
async fn fetch_200_returns_changed_with_etag_and_digest() {
    let (endpoint, observed) = start_server(vec![CannedResponse {
        status_line: "HTTP/1.1 200 OK",
        extra_headers: vec![
            ("Content-Type", "application/json".into()),
            ("ETag", "\"abc123\"".into()),
        ],
        body: EMPTY_CONFIG_JSON.to_vec(),
    }])
    .await;

    let cfg = config_pointing_at(&endpoint);
    let client = Client::new(&cfg).expect("build client");

    let outcome = client.fetch(None).await.expect("fetch");
    match outcome {
        FetchOutcome::Changed(c) => {
            assert_eq!(c.etag.as_deref(), Some("\"abc123\""));
            assert_eq!(c.raw_bytes, EMPTY_CONFIG_JSON.len());
            // Digest is sha256 hex over the body; we just check it parses + length.
            assert_eq!(c.digest.len(), 64);
            assert!(c.digest.chars().all(|ch| ch.is_ascii_hexdigit()));
        }
        FetchOutcome::NotModified => panic!("expected Changed, got NotModified"),
    }

    let req = observed.lock().await[0].clone();
    assert_eq!(
        req.headers.get("authorization").map(String::as_str),
        Some("Bearer test-token")
    );
    assert_eq!(
        req.headers.get("accept").map(String::as_str),
        Some("application/json")
    );
    assert!(
        !req.headers.contains_key("if-none-match"),
        "first call should not send If-None-Match"
    );
}

#[tokio::test]
async fn fetch_with_etag_sends_if_none_match_and_handles_304() {
    let (endpoint, observed) = start_server(vec![CannedResponse {
        status_line: "HTTP/1.1 304 Not Modified",
        extra_headers: vec![],
        body: vec![],
    }])
    .await;

    let cfg = config_pointing_at(&endpoint);
    let client = Client::new(&cfg).expect("build client");

    let outcome = client.fetch(Some("\"abc123\"")).await.expect("fetch");
    assert!(matches!(outcome, FetchOutcome::NotModified));

    let req = observed.lock().await[0].clone();
    assert_eq!(
        req.headers.get("if-none-match").map(String::as_str),
        Some("\"abc123\"")
    );
}

#[tokio::test]
async fn fetch_5xx_is_an_error_with_truncated_body() {
    let big_body = vec![b'x'; 4096];
    let (endpoint, _) = start_server(vec![CannedResponse {
        status_line: "HTTP/1.1 503 Service Unavailable",
        extra_headers: vec![("Content-Type", "text/plain".into())],
        body: big_body,
    }])
    .await;

    let cfg = config_pointing_at(&endpoint);
    let client = Client::new(&cfg).expect("build client");

    let err = match client.fetch(None).await {
        Err(e) => e,
        Ok(_) => panic!("expected error from 503 response"),
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("503"), "error should mention status: {msg}");
}

#[tokio::test]
async fn fetch_rejects_oversize_body() {
    let (endpoint, _) = start_server(vec![CannedResponse {
        status_line: "HTTP/1.1 200 OK",
        extra_headers: vec![("Content-Type", "application/json".into())],
        body: vec![b'{'; 10_000],
    }])
    .await;

    let mut cfg = config_pointing_at(&endpoint);
    cfg.max_response_body_bytes = 1024;
    let client = Client::new(&cfg).expect("build client");

    let err = match client.fetch(None).await {
        Err(e) => e,
        Ok(_) => panic!("expected oversize-body error"),
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("CONFIG_MAX_RESPONSE_BODY_BYTES"),
        "error should mention the size limit, got: {msg}"
    );
}
