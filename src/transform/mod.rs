//! Transform pangolin's Traefik dynamic config into a desired set of Gateway API objects.

pub mod backend;
pub mod listener;
pub mod middleware;
pub mod naming;
pub mod route;
pub mod rule;

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use tracing::warn;

#[cfg(test)]
use crate::config::BackendKind;
use crate::config::Config;
use crate::envoy_gateway::Backend;
use crate::pangolin::TraefikDynamicConfig;
use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::ListenerSet;

/// Everything the controller wants to exist in the cluster, keyed by resource name.
/// Names are guaranteed DNS-label safe.
#[derive(Debug, Default)]
pub struct Desired {
    pub http_routes: BTreeMap<String, HTTPRoute>,
    pub listener_sets: BTreeMap<String, ListenerSet>,
    pub services: BTreeMap<String, Service>,
    pub endpoint_slices: BTreeMap<String, EndpointSlice>,
    /// Populated only when `CONFIG_BACKEND_KIND=envoy-backend`. Empty otherwise.
    pub envoy_backends: BTreeMap<String, Backend>,
}

pub fn build_desired(cfg: &Config, dyn_config: &TraefikDynamicConfig) -> Desired {
    let mut desired = Desired::default();

    // 1. Backends: pangolin services -> Service + EndpointSlice
    let backend_index = backend::build_backends(cfg, &dyn_config.http.services, &mut desired);

    // 2. Routes: pangolin routers -> HTTPRoute
    let route_index = route::build_routes(cfg, dyn_config, &backend_index, &mut desired);

    // 3. Listeners: aggregate unique hostnames into one ListenerSet
    listener::build_listener_set(cfg, &route_index, &mut desired);

    if let (Some(_), Some(_)) = (&dyn_config.tcp, &dyn_config.udp) {
        warn!("pangolin response contains tcp/udp blocks; this controller only handles HTTP");
    }

    desired
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::config::Config;
    use std::time::Duration;
    use url::Url;

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
            allow_insecure_http: true,
            namespace: "gateway".into(),
            parent_gateway: "eg".into(),
            parent_gateway_namespace: Some("gateway".into()),
            listener_set_name: "pangolin".into(),
            gateway_class: None,
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
            httproute_annotations: std::collections::BTreeMap::new(),
            listenerset_annotations: std::collections::BTreeMap::new(),
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
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!("reading fixture {path}: {e}");
        });
        serde_json::from_slice(&bytes).expect("valid pangolin JSON")
    }

    #[test]
    fn extended_fixture_produces_routes_and_listeners() {
        let cfg = test_config();
        let dyn_config = load_fixture("pangolin-traefik-v3.5.0-extended.json");
        let desired = build_desired(&cfg, &dyn_config);

        // Every router with a usable rule should become an HTTPRoute.
        assert!(!desired.http_routes.is_empty(), "produced no HTTPRoutes");

        // Exactly one ListenerSet aggregating every host.
        assert_eq!(desired.listener_sets.len(), 1);
        let ls = desired.listener_sets.values().next().unwrap();
        assert!(!ls.spec.listeners.is_empty());

        // Every Service has a matching EndpointSlice (1:1).
        for svc in desired.services.keys() {
            // service name is `svc-...`, slice name is `eps-...` with same suffix.
            let suffix = svc.strip_prefix("svc-").unwrap();
            let eps_name = format!("eps-{suffix}");
            assert!(
                desired.endpoint_slices.contains_key(&eps_name),
                "missing EndpointSlice for {svc} (expected {eps_name})"
            );
        }

        // All HTTPRoutes parent-ref our ListenerSet by name.
        for route in desired.http_routes.values() {
            let parents = route.spec.parent_refs.as_ref().expect("parentRefs");
            assert!(
                parents.iter().any(|p| p.name == cfg.listener_set_name),
                "HTTPRoute does not reference configured listener set"
            );
        }
    }

    #[test]
    fn envoy_backend_mode_emits_backend_crds() {
        let mut cfg = test_config();
        cfg.backend_kind = BackendKind::EnvoyBackend;
        let dyn_config = load_fixture("pangolin-traefik-v3.5.0-older.json");
        let desired = build_desired(&cfg, &dyn_config);

        // In envoy-backend mode, the IP-backed pangolin services that would have
        // become Service+EndpointSlice in service mode become Backend CRDs instead.
        // The fixtures from upstream only use cluster-DNS targets, so no Backends
        // are emitted — but more importantly, no Services/EndpointSlices are
        // synthesized for them either.
        assert!(
            desired.services.is_empty(),
            "envoy-backend mode must not synthesize Services"
        );
        assert!(
            desired.endpoint_slices.is_empty(),
            "envoy-backend mode must not synthesize EndpointSlices"
        );

        // Cluster-DNS pass-through still produces real Service backendRefs:
        for route in desired.http_routes.values() {
            let rules = route.spec.rules.as_ref().unwrap();
            for rule in rules {
                for r in rule.backend_refs.as_ref().unwrap() {
                    // For these fixtures everything resolves to cluster Services.
                    assert_eq!(r.kind.as_deref(), Some("Service"));
                    assert_eq!(r.group.as_deref(), Some(""));
                }
            }
        }
    }

    #[test]
    fn envoy_backend_mode_on_ip_fixture() {
        use serde_json::json;
        // Hand-build a minimal pangolin response where the service has both IP
        // and FQDN endpoints — service mode would drop the FQDN, but
        // envoy-backend mode keeps it. IP wins for classification (single kind
        // per pangolin service), so we exercise the two cases in separate
        // fixtures.
        let mut cfg = test_config();
        cfg.backend_kind = BackendKind::EnvoyBackend;

        let ip_only_json = json!({
            "http": {
                "routers": {
                    "r1": {
                        "rule": "Host(`api.example.com`)",
                        "service": "s1",
                    }
                },
                "services": {
                    "s1": {
                        "loadBalancer": {
                            "servers": [{"url": "http://10.0.0.7:8080"}]
                        }
                    }
                }
            }
        });
        let dyn_config: TraefikDynamicConfig = serde_json::from_value(ip_only_json).unwrap();
        let desired = build_desired(&cfg, &dyn_config);

        assert!(desired.services.is_empty());
        assert!(desired.endpoint_slices.is_empty());
        assert_eq!(desired.envoy_backends.len(), 1);

        let be = desired.envoy_backends.values().next().unwrap();
        let ep = &be.spec.endpoints[0];
        assert_eq!(ep.ip.as_ref().unwrap().address, "10.0.0.7");
        assert_eq!(ep.ip.as_ref().unwrap().port, 8080);
        assert!(ep.fqdn.is_none());

        // HTTPRoute backendRef points at the Envoy Backend CRD.
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
        use serde_json::json;
        let mut cfg = test_config();
        cfg.backend_kind = BackendKind::EnvoyBackend;
        let fqdn_json = json!({
            "http": {
                "routers": {
                    "r1": {
                        "rule": "Host(`pangolin.example.com`)",
                        "service": "s1",
                    }
                },
                "services": {
                    "s1": {
                        "loadBalancer": {
                            "servers": [{"url": "https://upstream.example.com"}]
                        }
                    }
                }
            }
        });
        let dyn_config: TraefikDynamicConfig = serde_json::from_value(fqdn_json).unwrap();
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
        use serde_json::json;
        let cfg = test_config(); // service mode
        let fqdn_json = json!({
            "http": {
                "routers": {
                    "r1": {
                        "rule": "Host(`pangolin.example.com`)",
                        "service": "s1",
                    }
                },
                "services": {
                    "s1": {
                        "loadBalancer": {
                            "servers": [{"url": "https://upstream.example.com"}]
                        }
                    }
                }
            }
        });
        let dyn_config: TraefikDynamicConfig = serde_json::from_value(fqdn_json).unwrap();
        let desired = build_desired(&cfg, &dyn_config);

        // Router referenced a service that couldn't be resolved -> no HTTPRoute.
        assert!(desired.http_routes.is_empty());
        assert!(desired.services.is_empty());
        assert!(desired.envoy_backends.is_empty());
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
            // managed annotation must still be present alongside.
            assert!(annos.contains_key(&cfg.managed_annotation_key));
        }

        let ls = desired.listener_sets.values().next().expect("listener set");
        let annos = ls.metadata.annotations.as_ref().expect("annotations");
        assert_eq!(
            annos.get("cert-manager.io/cluster-issuer"),
            Some(&"letsencrypt-prod".to_string()),
        );

        // Service/EndpointSlice should NOT get the cert annotation.
        if let Some(svc) = desired.services.values().next() {
            let annos = svc.metadata.annotations.as_ref();
            if let Some(a) = annos {
                assert!(!a.contains_key("cert-manager.io/cluster-issuer"));
            }
        }
    }
}
