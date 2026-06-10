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
        enable_tcp_routes: true,
        enable_udp_routes: true,
        backend_kind: BackendKind::Service,
        ext_authz: None,
        // The upstream fixtures attach badger to every non-redirect router;
        // keep them flowing through the pipeline. Auth-gating behaviour has
        // its own dedicated tests below.
        allow_unauthenticated_routes: true,
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
        health_listen: None,
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
    // Service object. Redirect-only routers have no backendRefs by design
    // (Gateway API CEL: RequestRedirect + backendRefs are mutually exclusive).
    for route in desired.http_routes.values() {
        for rule in route.spec.rules.as_ref().unwrap() {
            let Some(refs) = rule.backend_refs.as_ref() else {
                continue;
            };
            for r in refs {
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

#[test]
fn extended_fixture_emits_l4_routes() {
    let cfg = test_config(); // enable_tcp_routes/enable_udp_routes are on
    let dyn_config = load_fixture("pangolin-traefik-v3.5.0-extended.json");
    let desired = build_desired(&cfg, &dyn_config);

    // Fixture carries one raw TCP router (entrypoint tcp-234) and one raw UDP
    // router (entrypoint udp-345), both targeting cluster-DNS services.
    assert_eq!(desired.tcp_routes.len(), 1, "expected one TCPRoute");
    assert_eq!(desired.udp_routes.len(), 1, "expected one UDPRoute");

    let ls = desired.listener_sets.values().next().expect("listener set");
    let tcp_listener = ls
        .spec
        .listeners
        .iter()
        .find(|l| l.protocol == "TCP")
        .expect("TCP listener");
    assert_eq!(tcp_listener.port, 234);
    assert_eq!(tcp_listener.name, "tcp-234");
    assert!(tcp_listener.hostname.is_none());
    assert!(tcp_listener.tls.is_none());

    let udp_listener = ls
        .spec
        .listeners
        .iter()
        .find(|l| l.protocol == "UDP")
        .expect("UDP listener");
    assert_eq!(udp_listener.port, 345);
    assert_eq!(udp_listener.name, "udp-345");

    let tcp_route = desired.tcp_routes.values().next().unwrap();
    let parents = tcp_route.spec.parent_refs.as_ref().expect("parentRefs");
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].kind.as_deref(), Some("ListenerSet"));
    assert_eq!(parents[0].name, cfg.listener_set_name);
    assert_eq!(parents[0].section_name.as_deref(), Some("tcp-234"));
    let tcp_backend = &tcp_route.spec.rules[0].backend_refs[0];
    assert_eq!(tcp_backend.kind.as_deref(), Some("Service"));
    assert_eq!(tcp_backend.name, "show");
    assert_eq!(tcp_backend.namespace.as_deref(), Some("dummyservices"));
    assert_eq!(tcp_backend.port, Some(8000));

    // The UDP fixture address carries a stray `http://` scheme — it must still
    // classify as the cluster-DNS service behind it.
    let udp_route = desired.udp_routes.values().next().unwrap();
    let udp_backend = &udp_route.spec.rules[0].backend_refs[0];
    assert_eq!(udp_backend.kind.as_deref(), Some("Service"));
    assert_eq!(udp_backend.name, "itsnew");
    assert_eq!(udp_backend.namespace.as_deref(), Some("dummyservices"));
    assert_eq!(udp_backend.port, Some(9090));
}

#[test]
fn l4_disabled_flags_drop_l4_blocks() {
    let mut cfg = test_config();
    cfg.enable_tcp_routes = false;
    cfg.enable_udp_routes = false;
    let dyn_config = load_fixture("pangolin-traefik-v3.5.0-extended.json");
    let desired = build_desired(&cfg, &dyn_config);

    assert!(desired.tcp_routes.is_empty());
    assert!(desired.udp_routes.is_empty());
    let ls = desired.listener_sets.values().next().expect("listener set");
    assert!(
        ls.spec
            .listeners
            .iter()
            .all(|l| l.protocol == "HTTP" || l.protocol == "HTTPS"),
        "no L4 listeners may be emitted when the flags are off"
    );
}

#[test]
fn l4_ip_backend_in_envoy_mode_emits_backend_crd() {
    let mut cfg = test_config();
    cfg.backend_kind = BackendKind::EnvoyBackend;

    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "tcp": {
            "routers": {
                "db-router": {
                    "entryPoints": ["tcp-5432"],
                    "service": "db-service",
                    "rule": "HostSNI(`*`)"
                }
            },
            "services": {
                "db-service": {
                    "loadBalancer": { "servers": [{ "address": "10.0.0.9:5432" }] }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    assert!(desired.services.is_empty());
    assert!(desired.endpoint_slices.is_empty());
    assert_eq!(desired.envoy_backends.len(), 1);
    let (be_name, be) = desired.envoy_backends.iter().next().unwrap();
    assert!(
        be_name.starts_with("be-tcp-"),
        "L4 Backend names carry the protocol infix, got {be_name}"
    );
    let ip = be.spec.endpoints[0].ip.as_ref().expect("ip endpoint");
    assert_eq!(ip.address, "10.0.0.9");
    assert_eq!(ip.port, 5432);

    let route = desired.tcp_routes.values().next().expect("TCPRoute");
    let backend_ref = &route.spec.rules[0].backend_refs[0];
    assert_eq!(backend_ref.group.as_deref(), Some("gateway.envoyproxy.io"));
    assert_eq!(backend_ref.kind.as_deref(), Some("Backend"));
    assert_eq!(backend_ref.name, *be_name);
}

#[test]
fn l4_udp_ip_backend_in_service_mode_uses_udp_protocol() {
    let cfg = test_config(); // service mode

    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "udp": {
            "routers": {
                "dns-router": {
                    "entryPoints": ["udp-53"],
                    "service": "dns-service"
                }
            },
            "services": {
                "dns-service": {
                    "loadBalancer": { "servers": [{ "address": "10.0.0.53:53" }] }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    assert_eq!(desired.udp_routes.len(), 1);
    assert_eq!(desired.services.len(), 1);
    let svc = desired.services.values().next().unwrap();
    let port = &svc.spec.as_ref().unwrap().ports.as_ref().unwrap()[0];
    assert_eq!(port.protocol.as_deref(), Some("UDP"));

    let eps = desired.endpoint_slices.values().next().unwrap();
    let eps_port = &eps.ports.as_ref().unwrap()[0];
    assert_eq!(eps_port.protocol.as_deref(), Some("UDP"));
}

#[test]
fn l4_concrete_sni_router_is_skipped() {
    let cfg = test_config();
    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "tcp": {
            "routers": {
                "sni-router": {
                    "entryPoints": ["tcp-8883"],
                    "service": "mqtt-service",
                    "rule": "HostSNI(`mqtt.example.com`)"
                }
            },
            "services": {
                "mqtt-service": {
                    "loadBalancer": { "servers": [{ "address": "10.0.0.4:8883" }] }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    assert!(
        desired.tcp_routes.is_empty(),
        "concrete SNI needs TLSRoute and must be skipped, not misrouted"
    );
    let ls = desired.listener_sets.values().next().expect("listener set");
    assert!(ls.spec.listeners.iter().all(|l| l.protocol != "TCP"));
}

#[test]
fn https_ip_target_gets_backend_tls_in_envoy_mode() {
    let mut cfg = test_config();
    cfg.backend_kind = BackendKind::EnvoyBackend;

    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": {
                "r1": { "rule": "Host(`a.example.com`)", "service": "verified" },
                "r2": { "rule": "Host(`b.example.com`)", "service": "skipped" }
            },
            "services": {
                "verified": {
                    "loadBalancer": { "servers": [{ "url": "https://10.0.0.8:8443" }] }
                },
                "skipped": {
                    "loadBalancer": {
                        "servers": [{ "url": "https://10.0.0.9:8443" }],
                        "serversTransport": "skip-tls"
                    }
                }
            },
            "serversTransports": {
                "skip-tls": { "insecureSkipVerify": true }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    assert_eq!(desired.envoy_backends.len(), 2);
    let verified = desired
        .envoy_backends
        .values()
        .find(|b| b.spec.endpoints[0].ip.as_ref().unwrap().address == "10.0.0.8")
        .expect("verified backend");
    let tls = verified.spec.tls.as_ref().expect("tls settings");
    assert_eq!(tls.well_known_ca_certificates.as_deref(), Some("System"));
    assert_eq!(tls.insecure_skip_verify, None);

    let skipped = desired
        .envoy_backends
        .values()
        .find(|b| b.spec.endpoints[0].ip.as_ref().unwrap().address == "10.0.0.9")
        .expect("skip-verify backend");
    let tls = skipped.spec.tls.as_ref().expect("tls settings");
    assert_eq!(tls.insecure_skip_verify, Some(true));
    assert_eq!(tls.well_known_ca_certificates, None);
}

#[test]
fn plain_http_targets_get_no_backend_tls() {
    let mut cfg = test_config();
    cfg.backend_kind = BackendKind::EnvoyBackend;
    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": { "r1": { "rule": "Host(`a.example.com`)", "service": "s1" } },
            "services": {
                "s1": { "loadBalancer": { "servers": [{ "url": "http://10.0.0.8:8080" }] } }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);
    assert!(
        desired
            .envoy_backends
            .values()
            .next()
            .unwrap()
            .spec
            .tls
            .is_none()
    );
    assert!(desired.backend_tls_policies.is_empty());
}

#[test]
fn https_cluster_dns_in_envoy_mode_wraps_in_backend() {
    let mut cfg = test_config();
    cfg.backend_kind = BackendKind::EnvoyBackend;
    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": { "r1": { "rule": "Host(`a.example.com`)", "service": "s1" } },
            "services": {
                "s1": {
                    "loadBalancer": {
                        "servers": [{ "url": "https://echo.other-ns.svc.cluster.local:8443" }]
                    }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    // TLS needs a Backend wrapper instead of the direct Service backendRef.
    assert_eq!(desired.envoy_backends.len(), 1);
    let be = desired.envoy_backends.values().next().unwrap();
    let fqdn = be.spec.endpoints[0].fqdn.as_ref().expect("fqdn endpoint");
    assert_eq!(fqdn.hostname, "echo.other-ns.svc.cluster.local");
    let tls = be.spec.tls.as_ref().expect("tls settings");
    assert_eq!(tls.sni.as_deref(), Some("echo.other-ns.svc.cluster.local"));

    let route = desired.http_routes.values().next().unwrap();
    let backend_ref = &route.spec.rules.as_ref().unwrap()[0]
        .backend_refs
        .as_ref()
        .unwrap()[0];
    assert_eq!(backend_ref.kind.as_deref(), Some("Backend"));
}

#[test]
fn https_cluster_dns_in_service_mode_emits_backend_tls_policy() {
    let cfg = test_config(); // service mode; cfg.namespace == "gateway"
    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": {
                "same-ns": { "rule": "Host(`a.example.com`)", "service": "same-ns-svc" },
                "cross-ns": { "rule": "Host(`b.example.com`)", "service": "cross-ns-svc" }
            },
            "services": {
                "same-ns-svc": {
                    "loadBalancer": { "servers": [{ "url": "https://echo.gateway.svc.cluster.local:8443" }] }
                },
                "cross-ns-svc": {
                    "loadBalancer": { "servers": [{ "url": "https://echo.elsewhere.svc.cluster.local:8443" }] }
                }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    // Policies are local objects: only the same-namespace target can get one.
    assert_eq!(desired.backend_tls_policies.len(), 1);
    let btp = desired.backend_tls_policies.values().next().unwrap();
    assert_eq!(btp.spec.target_refs[0].kind, "Service");
    assert_eq!(btp.spec.target_refs[0].name, "echo");
    assert_eq!(
        btp.spec.validation.hostname,
        "echo.gateway.svc.cluster.local"
    );
    assert_eq!(
        btp.spec.validation.well_known_ca_certificates.as_deref(),
        Some("System")
    );
    // Both routes still emit — the cross-ns one is warned about, not dropped.
    assert_eq!(desired.http_routes.len(), 2);
}

#[test]
fn custom_host_header_becomes_url_rewrite() {
    let cfg = test_config();
    // The real-fixture shape: headers middleware with Host + another header,
    // alongside a path-rewriting middleware on the same router.
    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": {
                "r1": {
                    "rule": "Host(`a.example.com`)",
                    "service": "s1",
                    "middlewares": ["hostset", "prefix"]
                }
            },
            "services": {
                "s1": { "loadBalancer": { "servers": [{ "url": "http://10.0.0.8:8080" }] } }
            },
            "middlewares": {
                "hostset": {
                    "headers": {
                        "customRequestHeaders": { "Host": "internal.example.com", "X-Extra": "1" },
                        "customResponseHeaders": { "X-Resp": "2" }
                    }
                },
                "prefix": { "addPrefix": { "prefix": "/api" } }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    let route = desired.http_routes.values().next().expect("route");
    let filters = route.spec.rules.as_ref().unwrap()[0]
        .filters
        .as_ref()
        .expect("filters");

    // Exactly one URLRewrite carrying BOTH the hostname (from the Host header)
    // and the path rewrite (from addPrefix) — Gateway API allows the filter
    // only once per rule.
    let rewrites: Vec<_> = filters
        .iter()
        .filter_map(|f| f.url_rewrite.as_ref())
        .collect();
    assert_eq!(
        rewrites.len(),
        1,
        "URLRewrite must be merged into one filter"
    );
    assert_eq!(
        rewrites[0].hostname.as_deref(),
        Some("internal.example.com")
    );
    assert!(
        rewrites[0].path.is_some(),
        "path rewrite must survive the merge"
    );

    // Host must NOT appear as a header modification…
    let req_mod = filters
        .iter()
        .find_map(|f| f.request_header_modifier.as_ref())
        .expect("request header modifier");
    let names: Vec<_> = req_mod
        .set
        .as_ref()
        .unwrap()
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(!names.iter().any(|n| n.eq_ignore_ascii_case("host")));
    assert!(names.contains(&"X-Extra"));

    // …and the response headers must survive alongside the request headers.
    let resp_mod = filters
        .iter()
        .find_map(|f| f.response_header_modifier.as_ref())
        .expect("response header modifier");
    assert_eq!(resp_mod.set.as_ref().unwrap()[0].name, "X-Resp");
}

/// Minimal config with one badger-protected router and one redirect router —
/// the shape real pangolin emits for an HTTP resource.
fn badger_fixture() -> TraefikDynamicConfig {
    serde_json::from_value(json!({
        "http": {
            "routers": {
                "1-app-router": {
                    "rule": "Host(`app.example.com`)",
                    "service": "app-service",
                    "entryPoints": ["websecure"],
                    "middlewares": ["badger"]
                },
                "1-app-router-redirect": {
                    "rule": "Host(`app.example.com`)",
                    "service": "app-service",
                    "entryPoints": ["web"],
                    "middlewares": ["redirect-to-https"]
                }
            },
            "services": {
                "app-service": {
                    "loadBalancer": { "servers": [{ "url": "http://10.0.0.7:8080" }] }
                }
            },
            "middlewares": {
                "redirect-to-https": { "redirectScheme": { "scheme": "https" } },
                "badger": {
                    "plugin": {
                        "badger": {
                            "apiBaseUrl": "http://pangolin.pangolin-system.svc.cluster.local:3001/api/v1",
                            "userSessionCookieName": "p_session_token"
                        }
                    }
                }
            }
        }
    }))
    .unwrap()
}

#[test]
fn protected_router_skipped_without_ext_authz() {
    let mut cfg = test_config();
    cfg.allow_unauthenticated_routes = false; // the production default
    let desired = build_desired(&cfg, &badger_fixture());

    // The badger-protected router must be dropped; the redirect router (no
    // auth — it only bounces to HTTPS) still emits.
    assert_eq!(
        desired.http_routes.len(),
        1,
        "only the redirect route may be emitted: {:?}",
        desired.http_routes.keys().collect::<Vec<_>>()
    );
    assert!(
        desired
            .http_routes
            .keys()
            .all(|name| name.contains("redirect")),
        "the protected route leaked through"
    );
    assert!(desired.security_policies.is_empty());
}

#[test]
fn protected_router_emitted_with_override() {
    let mut cfg = test_config();
    cfg.allow_unauthenticated_routes = true;
    let desired = build_desired(&cfg, &badger_fixture());

    assert_eq!(desired.http_routes.len(), 2);
    assert!(desired.security_policies.is_empty());
}

#[test]
fn ext_authz_emits_security_policy_for_protected_route() {
    use pangolin_gateway_controller::config::ExtAuthzConfig;

    let mut cfg = test_config();
    cfg.allow_unauthenticated_routes = false;
    cfg.ext_authz = Some(ExtAuthzConfig {
        service: "badger-shim".into(),
        namespace: None,
        port: 9001,
        path: Some("/verify".into()),
        headers_to_ext_auth: vec!["cookie".into(), "authorization".into()],
        headers_to_backend: vec![],
    });
    let desired = build_desired(&cfg, &badger_fixture());

    // Both routes emit; only the protected one gets a SecurityPolicy.
    assert_eq!(desired.http_routes.len(), 2);
    assert_eq!(desired.security_policies.len(), 1);

    let sp = desired.security_policies.values().next().unwrap();
    let target = &sp.spec.target_refs[0];
    assert_eq!(target.kind, "HTTPRoute");
    assert!(
        desired.http_routes.contains_key(&target.name),
        "policy must target an emitted route, got {}",
        target.name
    );
    assert!(
        !target.name.contains("redirect"),
        "policy must target the protected route, not the redirect"
    );

    let ext = sp.spec.ext_auth.as_ref().expect("extAuth");
    assert_eq!(ext.fail_open, Some(false), "auth outages must fail closed");
    let http = ext.http.as_ref().expect("http ext auth");
    assert_eq!(http.backend_refs[0].name, "badger-shim");
    assert_eq!(http.backend_refs[0].port, Some(9001));
    assert_eq!(http.path.as_deref(), Some("/verify"));
    assert_eq!(
        ext.headers_to_ext_auth.as_deref(),
        Some(&["cookie".to_string(), "authorization".to_string()][..])
    );
}

#[test]
fn real_fixtures_protected_routes_gated_by_default() {
    // The upstream fixtures carry badger on every non-redirect router. With
    // production defaults (no ext-authz, no override) only redirect routers
    // survive — nothing protected may leak.
    let mut cfg = test_config();
    cfg.allow_unauthenticated_routes = false;

    for fixture in [
        "pangolin-traefik-v3.5.0-extended.json",
        "pangolin-traefik-v3.5.0-older.json",
    ] {
        let desired = build_desired(&cfg, &load_fixture(fixture));
        for name in desired.http_routes.keys() {
            assert!(
                name.contains("redirect"),
                "{fixture}: protected route {name} emitted without auth"
            );
        }
    }
}

#[test]
fn redirect_router_drops_backend_refs() {
    // Gateway API CEL forbids combining a RequestRedirect filter with
    // backendRefs. Pangolin emits exactly this shape for its
    // `redirect-to-https` router, so the controller has to suppress the
    // backendRef on the resulting rule or admission rejects the HTTPRoute.
    let cfg = test_config();
    let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
        "http": {
            "routers": {
                "plain-router": {
                    "rule": "Host(`web.example.com`)",
                    "service": "web-service",
                    "entryPoints": ["websecure"]
                },
                "redirect-router": {
                    "rule": "Host(`web.example.com`)",
                    "service": "web-service",
                    "entryPoints": ["web"],
                    "middlewares": ["to-https"]
                }
            },
            "services": {
                "web-service": {
                    "loadBalancer": { "servers": [{ "url": "http://10.0.0.7:8080" }] }
                }
            },
            "middlewares": {
                "to-https": { "redirectScheme": { "scheme": "https" } }
            }
        }
    }))
    .unwrap();
    let desired = build_desired(&cfg, &dyn_config);

    assert_eq!(desired.http_routes.len(), 2, "both routers should emit");

    let plain = desired
        .http_routes
        .values()
        .find(|r| r.metadata.name.as_deref().unwrap().contains("plain-router"))
        .expect("plain router");
    let redirect = desired
        .http_routes
        .values()
        .find(|r| {
            r.metadata
                .name
                .as_deref()
                .unwrap()
                .contains("redirect-router")
        })
        .expect("redirect router");

    let plain_rule = &plain.spec.rules.as_ref().unwrap()[0];
    let redirect_rule = &redirect.spec.rules.as_ref().unwrap()[0];

    // Plain router proxies to backend → backendRefs present.
    assert!(
        plain_rule
            .backend_refs
            .as_ref()
            .is_some_and(|refs| !refs.is_empty()),
        "plain HTTPRoute must keep its backendRefs"
    );
    assert!(
        plain_rule.filters.as_ref().is_none_or(Vec::is_empty),
        "plain HTTPRoute should have no filters"
    );

    // Redirect router terminates → backendRefs must be absent.
    assert!(
        redirect_rule.backend_refs.is_none(),
        "redirect HTTPRoute must NOT carry backendRefs (Gateway API CEL forbids it)"
    );
    let filters = redirect_rule
        .filters
        .as_ref()
        .expect("redirect HTTPRoute must carry filters");
    assert_eq!(filters.len(), 1);
    assert!(filters[0].request_redirect.is_some());
}
