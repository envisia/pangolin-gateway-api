//! Integration tests for the badger ext-authz shim: spin the shim's axum
//! router against a mock pangolin (also axum) and drive it the way Envoy's
//! HTTP ext-authz filter would — original method/Host/path preserved, the
//! configured path acting as a prefix.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;

use pangolin_gateway_controller::shim::{ShimConfig, ShimState, router};

/// Mock pangolin: records the last verify/exchange body and returns a canned
/// response per endpoint.
#[derive(Default)]
struct MockPangolin {
    last_verify: Mutex<Option<Value>>,
    verify_response: Mutex<Value>,
    last_exchange: Mutex<Option<Value>>,
    exchange_response: Mutex<Value>,
}

async fn spawn_mock(state: Arc<MockPangolin>) -> String {
    async fn verify(
        State(s): State<Arc<MockPangolin>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        *s.last_verify.lock().await = Some(body);
        axum::Json(s.verify_response.lock().await.clone())
    }
    async fn exchange(
        State(s): State<Arc<MockPangolin>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        *s.last_exchange.lock().await = Some(body);
        axum::Json(s.exchange_response.lock().await.clone())
    }
    let app = Router::new()
        .route("/api/v1/badger/verify-session", post(verify))
        .route("/api/v1/badger/exchange-session", post(exchange))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/api/v1")
}

async fn spawn_shim(pangolin_base: &str) -> String {
    let cfg = ShimConfig {
        listen: String::new(), // unused; we bind ourselves
        pangolin_api_base_url: Url::parse(pangolin_base).unwrap(),
        path_prefix: "/verify".into(),
        user_session_cookie_name: "p_session_token".into(),
        resource_session_request_param: "p_session_request".into(),
        timeout: Duration::from_secs(5),
    };
    let state = Arc::new(ShimState {
        cfg,
        http: reqwest::Client::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{addr}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn healthz_is_open() {
    let mock = Arc::new(MockPangolin::default());
    let base = spawn_mock(mock).await;
    let shim = spawn_shim(&base).await;

    let resp = client()
        .get(format!("{shim}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn valid_session_allows_with_identity_headers() {
    let mock = Arc::new(MockPangolin::default());
    *mock.verify_response.lock().await = json!({
        "data": {
            "valid": true,
            "userData": { "userId": "u1", "username": "alice", "email": "alice@example.com" }
        }
    });
    let base = spawn_mock(mock.clone()).await;
    let shim = spawn_shim(&base).await;

    // Envoy check request: original path behind the /verify prefix, original
    // Host, cookies forwarded via headersToExtAuth.
    let resp = client()
        .get(format!("{shim}/verify/dashboard?x=1"))
        .header("Host", "app.example.com")
        .header("X-Forwarded-Proto", "https")
        .header("X-Forwarded-For", "203.0.113.7")
        .header("Cookie", "p_session_token=sess123; unrelated=1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("remote-user").unwrap(), "alice");
    assert_eq!(resp.headers().get("remote-user-id").unwrap(), "u1");

    // What pangolin saw must match its zod schema and badger's semantics.
    let body = mock
        .last_verify
        .lock()
        .await
        .clone()
        .expect("verify called");
    assert_eq!(
        body["originalRequestURL"],
        "https://app.example.com/dashboard?x=1"
    );
    assert_eq!(body["host"], "app.example.com");
    assert_eq!(body["path"], "/dashboard");
    assert_eq!(body["method"], "GET");
    assert_eq!(body["scheme"], "https");
    assert_eq!(body["tls"], true);
    assert_eq!(body["requestIp"], "203.0.113.7");
    assert_eq!(body["sessions"]["p_session_token"], "sess123");
    assert!(
        body["sessions"].get("unrelated").is_none(),
        "non-session cookies must not be forwarded"
    );
    assert_eq!(body["query"]["x"], "1");
}

#[tokio::test]
async fn missing_session_redirects_to_portal() {
    let mock = Arc::new(MockPangolin::default());
    *mock.verify_response.lock().await = json!({
        "data": {
            "valid": false,
            "redirectUrl": "https://pangolin.example.com/auth/resource/1?redirect=https%3A%2F%2Fapp.example.com%2F"
        }
    });
    let base = spawn_mock(mock).await;
    let shim = spawn_shim(&base).await;

    let resp = client()
        .get(format!("{shim}/verify/"))
        .header("Host", "app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 302);
    assert!(
        resp.headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("https://pangolin.example.com/auth/")
    );
}

#[tokio::test]
async fn invalid_session_is_401() {
    let mock = Arc::new(MockPangolin::default());
    *mock.verify_response.lock().await = json!({ "data": { "valid": false } });
    let base = spawn_mock(mock).await;
    let shim = spawn_shim(&base).await;

    let resp = client()
        .get(format!("{shim}/verify/secret"))
        .header("Host", "app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn header_auth_challenge_sets_www_authenticate() {
    let mock = Arc::new(MockPangolin::default());
    *mock.verify_response.lock().await = json!({
        "data": { "valid": false, "headerAuthChallenged": true }
    });
    let base = spawn_mock(mock).await;
    let shim = spawn_shim(&base).await;

    let resp = client()
        .get(format!("{shim}/verify/api"))
        .header("Host", "app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(
        resp.headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Basic")
    );
}

#[tokio::test]
async fn session_request_token_is_exchanged_for_cookie() {
    let mock = Arc::new(MockPangolin::default());
    *mock.exchange_response.lock().await = json!({
        "data": {
            "valid": true,
            "cookie": "p_session_token=newsess; Path=/; HttpOnly; Secure"
        }
    });
    let base = spawn_mock(mock.clone()).await;
    let shim = spawn_shim(&base).await;

    let resp = client()
        .get(format!(
            "{shim}/verify/welcome?p_session_request=tok42&keep=yes"
        ))
        .header("Host", "app.example.com")
        .header("X-Forwarded-Proto", "https")
        .send()
        .await
        .unwrap();

    // 302 back to the original URL with the handoff param stripped, carrying
    // the new session cookie. Envoy forwards both headers to the client.
    assert_eq!(resp.status(), 302);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "https://app.example.com/welcome?keep=yes"
    );
    assert!(
        resp.headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("p_session_token=newsess")
    );

    let body = mock
        .last_exchange
        .lock()
        .await
        .clone()
        .expect("exchange called");
    assert_eq!(body["requestToken"], "tok42");
    assert_eq!(body["host"], "app.example.com");
}

#[tokio::test]
async fn pangolin_outage_fails_closed() {
    // Point the shim at a port nothing listens on.
    let shim = spawn_shim("http://127.0.0.1:1/api/v1").await;

    let resp = client()
        .get(format!("{shim}/verify/secret"))
        .header("Host", "app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        503,
        "unreachable pangolin must deny, not allow"
    );
}
