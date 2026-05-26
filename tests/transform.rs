//! End-to-end integration tests for the transform pipeline.
//!
//! Lives outside `src/` so it consumes only the library's public API — same
//! surface a downstream crate or operator-runner would see. Fixtures under
//! `tests/fixtures/` are real pangolin Traefik provider responses lifted from
//! the upstream `fosrl/pangolin-kube-controller` test corpus.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use url::Url;

use pangolin_gateway_controller::config::{BackendKind, Config};
use pangolin_gateway_controller::pangolin::TraefikDynamicConfig;
use pangolin_gateway_controller::transform::build_desired;

fn test_config() -> Config {
    Config {
        pangolin_endpoint: Url::parse("http://pangolin.local/api/v1/traefik-config").unwrap(),
        auth_header: None,
        fetch_timeout: Duration::from_secs(10),
        poll_interval: Duration::from_secs(30),
        max_backoff: Duration::from_secs(60),
        max_response_body_bytes: 1 << 20,
        tls_skip_verify: false,
        ca_file: None,
        namespace: "gateway".into(),
        parent_gateway: "eg".into(),
        parent_gateway_namespace: Some("gateway".into()),
        listener_set_name: "pangolin".into(),
        http_port: 80,
        https_port: 443,
        enable_https_listeners: true,
        backend_kind: BackendKind::Service,
        tls_secret_template: Some("{hostname-dashed}-tls".into()),
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
        read_only: false,
        log_traefik_config: false,
    }
}

fn cert_manager_config() -> Config {
    let mut cfg = test_config();
    cfg.httproute_annotations.insert(
        "cert-manager.io/cluster-issuer".into(),
        "letsencrypt-prod".into(),
    );
    cfg.listenerset_annotations.insert(
        "cert-manager.io/cluster-issuer".into(),
        "letsencrypt-prod".into(),
    );
    cfg
}

fn load_fixture(name: &str) -> TraefikDynamicConfig {
    let path = format!("tests/fixtures/{name}");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
    serde_json::from_slice(&bytes).expect("valid pangolin JSON")
}

#[test]
fn extended_fixture_produces_routes_and_listeners() {
    let cfg = test_config();
    let dyn_config = load_fixture("pangolin-traefik-v3.5.0-extended.json");
    let desired = build_desired(&cfg, &dyn_config);

    assert!(!desired.http_routes.is_empty(), "produced no HTTPRoutes");
    assert_eq!(desired.listener_sets.len(), 1);
    let ls = desired.listener_sets.values().next().unwrap();
    assert!(!ls.spec.listeners.is_empty());

    for svc in desired.services.keys() {
        let suffix = svc.strip_prefix("svc-").unwrap();
        let eps_name = format!("eps-{suffix}");
        assert!(
            desired.endpoint_slices.contains_key(&eps_name),
            "missing EndpointSlice for {svc} (expected {eps_name})"
        );
    }

    for route in desired.http_routes.values() {
        let parents = route.spec.parent_refs.as_ref().expect("parentRefs");
        assert!(
            parents.iter().any(|p| p.name == cfg.listener_set_name),
            "HTTPRoute does not reference configured listener set"
        );
    }
}

#[test]
fn older_fixture_round_trips() {
    let cfg = test_config();
    let dyn_config = load_fixture("pangolin-traefik-v3.5.0-older.json");
    let desired = build_desired(&cfg, &dyn_config);
    assert!(!desired.http_routes.is_empty());
    assert_eq!(desired.listener_sets.len(), 1);
}

#[test]
fn configured_annotations_are_stamped() {
    let cfg = cert_manager_config();
    let dyn_config = load_fixture("pangolin-traefik-v3.5.0-extended.json");
    let desired = build_desired(&cfg, &dyn_config);

    for route in desired.http_routes.values() {
        let annos = route.metadata.annotations.as_ref().expect("annotations");
        assert_eq!(
            annos.get("cert-manager.io/cluster-issuer"),
            Some(&"letsencrypt-prod".to_string()),
            "HTTPRoute is missing cert-manager annotation"
        );
        assert!(annos.contains_key(&cfg.managed_annotation_key));
    }

    let ls = desired.listener_sets.values().next().expect("listener set");
    let annos = ls.metadata.annotations.as_ref().expect("annotations");
    assert_eq!(
        annos.get("cert-manager.io/cluster-issuer"),
        Some(&"letsencrypt-prod".to_string()),
    );

    if let Some(svc) = desired.services.values().next()
        && let Some(a) = svc.metadata.annotations.as_ref()
    {
        assert!(!a.contains_key("cert-manager.io/cluster-issuer"));
    }
}

#[test]
fn envoy_backend_mode_does_not_synthesize_services() {
    let mut cfg = test_config();
    cfg.backend_kind = BackendKind::EnvoyBackend;
    let dyn_config = load_fixture("pangolin-traefik-v3.5.0-older.json");
    let desired = build_desired(&cfg, &dyn_config);

    assert!(
        desired.services.is_empty(),
        "envoy-backend mode must not synthesize Services"
    );
    assert!(
        desired.endpoint_slices.is_empty(),
        "envoy-backend mode must not synthesize EndpointSlices"
    );

    // The upstream fixtures only carry cluster-DNS targets, which still resolve
    // to a direct Service backendRef in both modes — but never to a synthesized
    // Service object.
    for route in desired.http_routes.values() {
        for rule in route.spec.rules.as_ref().unwrap() {
            for r in rule.backend_refs.as_ref().unwrap() {
                assert_eq!(r.kind.as_deref(), Some("Service"));
                assert_eq!(r.group.as_deref(), Some(""));
            }
        }
    }
}

#[test]
fn envoy_backend_mode_emits_ip_backend() {
    let mut cfg = test_config();
    cfg.backend_kind = BackendKind::EnvoyBackend;

    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": {
                "r1": { "rule": "Host(`api.example.com`)", "service": "s1" }
            },
            "services": {
                "s1": {
                    "loadBalancer": { "servers": [{"url": "http://10.0.0.7:8080"}] }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    assert!(desired.services.is_empty());
    assert!(desired.endpoint_slices.is_empty());
    assert_eq!(desired.envoy_backends.len(), 1);

    let be = desired.envoy_backends.values().next().unwrap();
    let ep = &be.spec.endpoints[0];
    assert_eq!(ep.ip.as_ref().unwrap().address, "10.0.0.7");
    assert_eq!(ep.ip.as_ref().unwrap().port, 8080);
    assert!(ep.fqdn.is_none());

    let route = desired.http_routes.values().next().unwrap();
    let backend_ref = &route.spec.rules.as_ref().unwrap()[0]
        .backend_refs
        .as_ref()
        .unwrap()[0];
    assert_eq!(backend_ref.group.as_deref(), Some("gateway.envoyproxy.io"));
    assert_eq!(backend_ref.kind.as_deref(), Some("Backend"));
}

#[test]
fn envoy_backend_mode_emits_fqdn_endpoint() {
    let mut cfg = test_config();
    cfg.backend_kind = BackendKind::EnvoyBackend;

    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": {
                "r1": { "rule": "Host(`pangolin.example.com`)", "service": "s1" }
            },
            "services": {
                "s1": {
                    "loadBalancer": { "servers": [{"url": "https://upstream.example.com"}] }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    assert_eq!(desired.envoy_backends.len(), 1);
    let be = desired.envoy_backends.values().next().unwrap();
    let ep = &be.spec.endpoints[0];
    assert!(ep.ip.is_none());
    let fqdn = ep.fqdn.as_ref().expect("fqdn");
    assert_eq!(fqdn.hostname, "upstream.example.com");
    assert_eq!(fqdn.port, 443);
}

#[test]
fn service_mode_drops_bare_fqdn() {
    let cfg = test_config(); // service mode

    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": {
                "r1": { "rule": "Host(`pangolin.example.com`)", "service": "s1" }
            },
            "services": {
                "s1": {
                    "loadBalancer": { "servers": [{"url": "https://upstream.example.com"}] }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    // Router referenced a service that couldn't be resolved -> no HTTPRoute.
    assert!(desired.http_routes.is_empty());
    assert!(desired.services.is_empty());
    assert!(desired.envoy_backends.is_empty());
}
