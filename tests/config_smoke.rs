//! Smoke tests for `Config::from_env` and the small pure helpers on `Config`.
//!
//! All env-mutation cases live in one `#[test]` so they execute sequentially
//! within this binary — `std::env::set_var` is process-global and racy with
//! parallel tests.

use pangolin_gateway_controller::config::{BackendKind, Config, ReconcileKind, ReconcileScope};

const CONFIG_ENV_KEYS: &[&str] = &[
    "CONFIG_ENDPOINT",
    "CONFIG_AUTH_HEADER",
    "CONFIG_FETCH_TIMEOUT",
    "CONFIG_POLL_INTERVAL",
    "CONFIG_MAX_BACKOFF",
    "CONFIG_MAX_RESPONSE_BODY_BYTES",
    "CONFIG_TLS_SKIP_VERIFY",
    "I_UNDERSTAND_CONFIG_TLS_SKIP_VERIFY_IS_INSECURE",
    "CONFIG_ALLOW_INSECURE_HTTP",
    "CONFIG_CA_FILE",
    "CONFIG_NAMESPACE",
    "CONFIG_PARENT_GATEWAY",
    "CONFIG_PARENT_GATEWAY_NAMESPACE",
    "CONFIG_LISTENERSET_NAME",
    "CONFIG_HTTP_PORT",
    "CONFIG_HTTPS_PORT",
    "CONFIG_ENABLE_HTTPS_LISTENERS",
    "CONFIG_BACKEND_KIND",
    "CONFIG_TLS_SECRET_TEMPLATE",
    "CONFIG_TLS_SECRET_NAMESPACE",
    "CONFIG_FIELD_MANAGER",
    "CONFIG_MANAGED_LABEL_KEY",
    "CONFIG_MANAGED_LABEL_VALUE",
    "CONFIG_INSTANCE_LABEL_KEY",
    "CONFIG_INSTANCE_LABEL_VALUE",
    "CONFIG_MANAGED_ANNOTATION_KEY",
    "CONFIG_MANAGED_ANNOTATION_VALUE",
    "CONFIG_HTTPROUTE_ANNOTATIONS",
    "CONFIG_LISTENERSET_ANNOTATIONS",
    "CONFIG_BADGER_EXT_AUTH",
    "CONFIG_BADGER_EXT_AUTH_BACKEND_NAME",
    "CONFIG_BADGER_EXT_AUTH_BACKEND_NAMESPACE",
    "CONFIG_BADGER_EXT_AUTH_BACKEND_PORT",
    "CONFIG_BADGER_EXT_AUTH_PATH",
    "CONFIG_BADGER_EXT_AUTH_HEADERS_TO_EXTAUTH",
    "CONFIG_BADGER_EXT_AUTH_HEADERS_TO_BACKEND",
    "CONFIG_BADGER_EXT_AUTH_FAIL_OPEN",
    "CONFIG_PANGOLIN_DASHBOARD_HOST",
    "CONFIG_PANGOLIN_SERVICE_NAME",
    "CONFIG_PANGOLIN_SERVICE_NAMESPACE",
    "CONFIG_PANGOLIN_API_PORT",
    "CONFIG_PANGOLIN_NEXT_PORT",
    "CONFIG_PANGOLIN_REDIRECT_HTTP_TO_HTTPS",
    "CONFIG_GERBIL_UDP_ROUTE",
    "CONFIG_GERBIL_SERVICE_NAME",
    "CONFIG_GERBIL_SERVICE_NAMESPACE",
    "CONFIG_GERBIL_UDP_PORTS",
    "CONFIG_RECONCILE_ONLY",
    "CONFIG_READ_ONLY",
    "CONFIG_LOG_TRAEFIK_CONFIG",
];

fn clear_all() {
    for k in CONFIG_ENV_KEYS {
        // SAFETY: tests in this binary run sequentially — see module doc.
        unsafe { std::env::remove_var(k) };
    }
}

fn set(k: &str, v: &str) {
    unsafe { std::env::set_var(k, v) };
}

#[test]
fn backend_kind_parses_known_spellings() {
    assert_eq!(BackendKind::parse("service").unwrap(), BackendKind::Service);
    assert_eq!(BackendKind::parse("").unwrap(), BackendKind::Service);
    assert_eq!(
        BackendKind::parse("service-endpointslice").unwrap(),
        BackendKind::Service
    );
    assert_eq!(
        BackendKind::parse("envoy-backend").unwrap(),
        BackendKind::EnvoyBackend
    );
    assert_eq!(
        BackendKind::parse("EnvoyBackend").unwrap(),
        BackendKind::EnvoyBackend
    );
    assert_eq!(
        BackendKind::parse("backend").unwrap(),
        BackendKind::EnvoyBackend
    );

    let err = BackendKind::parse("traefik").unwrap_err().to_string();
    assert!(err.contains("CONFIG_BACKEND_KIND"));
}

#[test]
fn reconcile_scope_parses_selectors() {
    let scope =
        ReconcileScope::parse("HTTPRoute/hr-a,SecurityPolicy/sp-a,Service:svc-a,shared-name")
            .unwrap();
    assert!(scope.includes(ReconcileKind::HttpRoute, "hr-a"));
    assert!(scope.includes(ReconcileKind::SecurityPolicy, "sp-a"));
    assert!(scope.includes(ReconcileKind::Service, "svc-a"));
    assert!(scope.includes(ReconcileKind::ListenerSet, "shared-name"));
    assert!(!scope.includes(ReconcileKind::HttpRoute, "sp-a"));
}

#[test]
fn from_env_smoke() {
    // Step 1: missing CONFIG_ENDPOINT → required-env error.
    clear_all();
    let err = Config::from_env().expect_err("should fail without CONFIG_ENDPOINT");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("CONFIG_ENDPOINT"),
        "expected error to name CONFIG_ENDPOINT, got: {msg}"
    );

    // Step 2: http endpoint requires CONFIG_ALLOW_INSECURE_HTTP.
    clear_all();
    set(
        "CONFIG_ENDPOINT",
        "http://pangolin.local/api/v1/traefik-config",
    );
    set("CONFIG_PARENT_GATEWAY", "eg");
    let err = Config::from_env().expect_err("plain http should be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("CONFIG_ALLOW_INSECURE_HTTP"),
        "expected error to mention CONFIG_ALLOW_INSECURE_HTTP, got: {msg}"
    );

    // Step 3: CONFIG_TLS_SKIP_VERIFY without the acknowledgement var is rejected.
    clear_all();
    set(
        "CONFIG_ENDPOINT",
        "https://pangolin.local/api/v1/traefik-config",
    );
    set("CONFIG_PARENT_GATEWAY", "eg");
    set("CONFIG_TLS_SKIP_VERIFY", "true");
    let err = Config::from_env().expect_err("skip-verify without ack should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("I_UNDERSTAND_CONFIG_TLS_SKIP_VERIFY_IS_INSECURE"),
        "expected error to mention the acknowledgement env, got: {msg}"
    );

    // Step 4: happy path with defaults.
    clear_all();
    set(
        "CONFIG_ENDPOINT",
        "https://pangolin.local/api/v1/traefik-config",
    );
    set("CONFIG_PARENT_GATEWAY", "eg");
    let cfg = Config::from_env().expect("happy path");

    assert_eq!(
        cfg.pangolin_endpoint.as_str(),
        "https://pangolin.local/api/v1/traefik-config"
    );
    assert_eq!(cfg.parent_gateway, "eg");
    assert_eq!(cfg.namespace, "default");
    assert_eq!(cfg.listener_set_name, "pangolin");
    assert_eq!(cfg.field_manager, "pangolin-gateway-controller");
    assert_eq!(cfg.backend_kind, BackendKind::Service);
    assert_eq!(cfg.http_port, 80);
    assert_eq!(cfg.https_port, 443);
    assert!(cfg.enable_https_listeners);
    assert!(!cfg.tls_skip_verify);
    assert!(!cfg.read_only);
    assert!(cfg.httproute_annotations.is_empty());
    assert!(cfg.listenerset_annotations.is_empty());
    assert!(cfg.badger_ext_auth.is_none());
    assert!(cfg.pangolin_dashboard.is_none());
    assert!(cfg.gerbil_udp.is_none());
    assert!(cfg.reconcile_scope.is_all());

    // Step 5: full env override exercises every parser branch.
    clear_all();
    set(
        "CONFIG_ENDPOINT",
        "https://pangolin.local/api/v1/traefik-config",
    );
    set("CONFIG_PARENT_GATEWAY", "eg");
    set("CONFIG_AUTH_HEADER", "Bearer xyz");
    set("CONFIG_FETCH_TIMEOUT", "15s");
    set("CONFIG_POLL_INTERVAL", "45s");
    set("CONFIG_MAX_BACKOFF", "10m");
    set("CONFIG_MAX_RESPONSE_BODY_BYTES", "1048576");
    set("CONFIG_NAMESPACE", "pangolin-system");
    set("CONFIG_PARENT_GATEWAY_NAMESPACE", "envoy-gateway-system");
    set("CONFIG_LISTENERSET_NAME", "pangolin-set");
    set("CONFIG_HTTP_PORT", "8080");
    set("CONFIG_HTTPS_PORT", "8443");
    set("CONFIG_ENABLE_HTTPS_LISTENERS", "false");
    set("CONFIG_BACKEND_KIND", "envoy-backend");
    set("CONFIG_TLS_SECRET_TEMPLATE", "{hostname-dashed}-tls");
    set("CONFIG_TLS_SECRET_NAMESPACE", "certs");
    set(
        "CONFIG_HTTPROUTE_ANNOTATIONS",
        "cert-manager.io/cluster-issuer=letsencrypt-prod, foo=bar",
    );
    set(
        "CONFIG_LISTENERSET_ANNOTATIONS",
        "cert-manager.io/cluster-issuer=letsencrypt-prod",
    );
    set("CONFIG_BADGER_EXT_AUTH", "true");
    set("CONFIG_BADGER_EXT_AUTH_BACKEND_NAME", "badger-shim");
    set("CONFIG_BADGER_EXT_AUTH_BACKEND_NAMESPACE", "auth");
    set("CONFIG_BADGER_EXT_AUTH_BACKEND_PORT", "9003");
    set("CONFIG_BADGER_EXT_AUTH_PATH", "/authorize");
    set(
        "CONFIG_BADGER_EXT_AUTH_HEADERS_TO_EXTAUTH",
        "cookie,authorization,x-forwarded-for",
    );
    set(
        "CONFIG_BADGER_EXT_AUTH_HEADERS_TO_BACKEND",
        "remote-user,remote-email",
    );
    set("CONFIG_BADGER_EXT_AUTH_FAIL_OPEN", "true");
    set("CONFIG_PANGOLIN_DASHBOARD_HOST", "pangolin.example.com");
    set("CONFIG_PANGOLIN_SERVICE_NAME", "pangolin-api");
    set("CONFIG_PANGOLIN_SERVICE_NAMESPACE", "pangolin-system");
    set("CONFIG_PANGOLIN_API_PORT", "3001");
    set("CONFIG_PANGOLIN_NEXT_PORT", "3002");
    set("CONFIG_PANGOLIN_REDIRECT_HTTP_TO_HTTPS", "false");
    set("CONFIG_GERBIL_UDP_ROUTE", "true");
    set("CONFIG_GERBIL_SERVICE_NAME", "gerbil-edge");
    set("CONFIG_GERBIL_SERVICE_NAMESPACE", "pangolin-system");
    set("CONFIG_GERBIL_UDP_PORTS", "51820,21820");
    set(
        "CONFIG_RECONCILE_ONLY",
        "HTTPRoute/hr-one,SecurityPolicy/sp-one",
    );
    set("CONFIG_READ_ONLY", "true");
    let cfg = Config::from_env().expect("full override");

    assert_eq!(cfg.auth_header.as_deref(), Some("Bearer xyz"));
    assert_eq!(cfg.fetch_timeout, std::time::Duration::from_secs(15));
    assert_eq!(cfg.poll_interval, std::time::Duration::from_secs(45));
    assert_eq!(cfg.max_backoff, std::time::Duration::from_secs(600));
    assert_eq!(cfg.max_response_body_bytes, 1_048_576);
    assert_eq!(cfg.namespace, "pangolin-system");
    assert_eq!(
        cfg.parent_gateway_namespace.as_deref(),
        Some("envoy-gateway-system")
    );
    assert_eq!(cfg.listener_set_name, "pangolin-set");
    assert_eq!(cfg.http_port, 8080);
    assert_eq!(cfg.https_port, 8443);
    assert!(!cfg.enable_https_listeners);
    assert_eq!(cfg.backend_kind, BackendKind::EnvoyBackend);
    assert_eq!(
        cfg.tls_secret_template.as_deref(),
        Some("{hostname-dashed}-tls")
    );
    assert_eq!(cfg.tls_secret_namespace.as_deref(), Some("certs"));
    assert_eq!(cfg.httproute_annotations.len(), 2);
    assert_eq!(
        cfg.httproute_annotations
            .get("cert-manager.io/cluster-issuer")
            .map(String::as_str),
        Some("letsencrypt-prod"),
    );
    assert_eq!(cfg.listenerset_annotations.len(), 1);
    let ext_auth = cfg.badger_ext_auth.as_ref().expect("badger ext auth");
    assert_eq!(ext_auth.backend_name, "badger-shim");
    assert_eq!(ext_auth.backend_namespace.as_deref(), Some("auth"));
    assert_eq!(ext_auth.backend_port, 9003);
    assert_eq!(ext_auth.path.as_deref(), Some("/authorize"));
    assert_eq!(ext_auth.headers_to_ext_auth.len(), 3);
    assert_eq!(ext_auth.headers_to_backend.len(), 2);
    assert!(ext_auth.fail_open);
    let dashboard = cfg.pangolin_dashboard.as_ref().expect("dashboard");
    assert_eq!(dashboard.hostname, "pangolin.example.com");
    assert_eq!(dashboard.service_name, "pangolin-api");
    assert_eq!(dashboard.api_port, 3001);
    assert!(!dashboard.redirect_http_to_https);
    let gerbil = cfg.gerbil_udp.as_ref().expect("gerbil");
    assert_eq!(gerbil.service_name, "gerbil-edge");
    assert_eq!(gerbil.ports, vec![51820, 21820]);
    assert!(
        cfg.reconcile_scope
            .includes(ReconcileKind::HttpRoute, "hr-one")
    );
    assert!(
        !cfg.reconcile_scope
            .includes(ReconcileKind::HttpRoute, "sp-one")
    );
    assert!(cfg.read_only);

    // managed_selector is the contract GC relies on — verify the format.
    assert_eq!(
        cfg.managed_selector(),
        "app.kubernetes.io/managed-by=pangolin-gateway-controller,\
         pangolin.envisia.de/instance=default"
            .replace(' ', "")
    );

    clear_all();
}
