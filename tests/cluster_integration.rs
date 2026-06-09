//! Real-cluster smoke tests for the full reconcile pipeline.
//!
//! Both tests are `#[ignore]`-d because they require:
//!   - A reachable Kubernetes API server (via `KUBECONFIG` or in-cluster).
//!   - Gateway API **experimental** CRDs installed (HTTPRoute, ListenerSet).
//!   - A mock pangolin endpoint reachable from this process — typically the
//!     `mock_pangolin` example running on `127.0.0.1:18080`.
//!
//! The `envoy_backend` variant additionally needs the Envoy Gateway `Backend`
//! CRD (`gateway.envoyproxy.io/v1alpha1`) installed.
//!
//! Wired up by `.github/workflows/integration.yml`. Run locally with e.g.:
//!
//! ```sh
//! MOCK_PANGOLIN_FIXTURE=tests/fixtures/integration-minimal.json \
//!     cargo run --example mock_pangolin &
//! INTEGRATION_PANGOLIN_URL=http://127.0.0.1:18080/api/v1/traefik-config \
//! INTEGRATION_NAMESPACE=pangolin-system \
//! cargo test --test cluster_integration controller_reconciles_service_backends \
//!     -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::ListenerSet;
use gateway_api::apis::experimental::tcproutes::TCPRoute;
use gateway_api::apis::experimental::udproutes::UDPRoute;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::Resource;
use kube::api::{Api, ListParams};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use url::Url;

use pangolin_gateway_controller::config::{BackendKind, Config};
use pangolin_gateway_controller::envoy_gateway::Backend as EnvoyBackend;
use pangolin_gateway_controller::health::Readiness;
use pangolin_gateway_controller::{pangolin, reconcile};

/// Per-test instance label. Keeps the two test runs from GC-deleting each
/// other's objects when they happen to share a namespace (which they do not
/// in CI, but might locally).
fn integration_config(
    endpoint: &str,
    namespace: &str,
    parent_gateway: &str,
    backend_kind: BackendKind,
    instance: &str,
) -> Config {
    Config {
        pangolin_endpoint: Url::parse(endpoint).expect("INTEGRATION_PANGOLIN_URL must be a URL"),
        auth_header: None,
        // Tight timing so the test completes quickly. Real deployments use 30s+.
        fetch_timeout: Duration::from_secs(10),
        poll_interval: Duration::from_secs(2),
        max_backoff: Duration::from_secs(5),
        max_response_body_bytes: 1 << 20,
        tls_skip_verify: false,
        ca_file: None,

        namespace: namespace.to_string(),
        parent_gateway: parent_gateway.to_string(),
        parent_gateway_namespace: Some(namespace.to_string()),
        listener_set_name: format!("pangolin-integration-{instance}"),

        http_port: 80,
        https_port: 443,
        enable_https_listeners: false,
        enable_tcp_routes: false,
        enable_udp_routes: false,
        backend_kind,
        ext_authz: None,
        allow_unauthenticated_routes: false,
        tls_secret_template: None,
        tls_secret_namespace: None,

        field_manager: format!("pangolin-gateway-controller-it-{instance}"),
        managed_label_key: "app.kubernetes.io/managed-by".into(),
        managed_label_value: "pangolin-gateway-controller".into(),
        instance_label_key: "pangolin.envisia.de/instance".into(),
        // Distinct instance label per test so they never GC each other.
        instance_label_value: format!("integration-{instance}"),
        managed_annotation_key: "pangolin.envisia.de/source".into(),
        managed_annotation_value: "pangolin-gateway-controller".into(),

        httproute_annotations: BTreeMap::new(),
        listenerset_annotations: BTreeMap::new(),

        read_only: false,
        log_traefik_config: false,
        health_listen: None,
    }
}

async fn wait_for_at_least<T>(
    api: &Api<T>,
    selector: &str,
    min_count: usize,
    deadline: tokio::time::Instant,
    kind: &str,
) -> Vec<String>
where
    T: Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug,
{
    let lp = ListParams::default().labels(selector);
    loop {
        match api.list(&lp).await {
            Ok(list) => {
                let names: Vec<String> = list
                    .items
                    .into_iter()
                    .filter_map(|x| x.meta().name.clone())
                    .collect();
                if names.len() >= min_count {
                    return names;
                }
            }
            Err(e) => eprintln!("list {kind} failed (will retry): {e:#}"),
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for >= {min_count} {kind} objects matching {selector}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn assert_exact_count<T>(api: &Api<T>, selector: &str, expected: usize, kind: &str)
where
    T: Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug,
{
    let lp = ListParams::default().labels(selector);
    let list = api
        .list(&lp)
        .await
        .unwrap_or_else(|e| panic!("list {kind}: {e:#}"));
    let names: Vec<String> = list
        .items
        .into_iter()
        .filter_map(|x| x.meta().name.clone())
        .collect();
    assert_eq!(
        names.len(),
        expected,
        "expected exactly {expected} {kind}, got {}: {names:?}",
        names.len()
    );
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set"))
}

#[tokio::test]
#[ignore = "requires a real cluster + mock pangolin; see .github/workflows/integration.yml"]
async fn controller_reconciles_service_backends() {
    let endpoint = env("INTEGRATION_PANGOLIN_URL");
    let namespace = env("INTEGRATION_NAMESPACE");
    let parent_gateway =
        std::env::var("INTEGRATION_PARENT_GATEWAY").unwrap_or_else(|_| "eg".into());

    let kube_client = kube::Client::try_default()
        .await
        .expect("connect to Kubernetes API (is KUBECONFIG set?)");

    let cfg = integration_config(
        &endpoint,
        &namespace,
        &parent_gateway,
        BackendKind::Service,
        "svc",
    );
    let pang_client = pangolin::Client::new(&cfg).expect("build pangolin client");

    let shutdown = CancellationToken::new();
    let handle = {
        let cfg = cfg.clone();
        let kube_client = kube_client.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            reconcile::run_loop(
                cfg,
                kube_client,
                pang_client,
                shutdown,
                Readiness::default(),
            )
            .await
        })
    };

    let route_api: Api<HTTPRoute> = Api::namespaced(kube_client.clone(), &namespace);
    let ls_api: Api<ListenerSet> = Api::namespaced(kube_client.clone(), &namespace);
    let svc_api: Api<Service> = Api::namespaced(kube_client.clone(), &namespace);
    let eps_api: Api<EndpointSlice> = Api::namespaced(kube_client.clone(), &namespace);

    let selector = cfg.managed_selector();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    // Wait for the IP-backed objects to land — those are last in the apply
    // order, so once they exist the rest must too.
    let services = wait_for_at_least(&svc_api, &selector, 1, deadline, "Service").await;
    let endpoint_slices =
        wait_for_at_least(&eps_api, &selector, 1, deadline, "EndpointSlice").await;
    let routes = wait_for_at_least(&route_api, &selector, 2, deadline, "HTTPRoute").await;
    let listener_sets = wait_for_at_least(&ls_api, &selector, 1, deadline, "ListenerSet").await;

    println!("HTTPRoutes:     {routes:?}");
    println!("ListenerSets:   {listener_sets:?}");
    println!("Services:       {services:?}");
    println!("EndpointSlices: {endpoint_slices:?}");

    // Fixture has two routers; both should produce HTTPRoutes.
    assert_eq!(routes.len(), 2, "expected exactly two HTTPRoutes");
    assert_eq!(listener_sets.len(), 1);
    // IP-backed router synthesizes a Service + EndpointSlice; cluster-DNS one
    // resolves to a direct backendRef and does NOT synthesize anything.
    assert_eq!(
        services.len(),
        1,
        "expected exactly one synthesized Service"
    );
    assert_eq!(
        endpoint_slices.len(),
        1,
        "expected exactly one synthesized EndpointSlice"
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}

#[tokio::test]
#[ignore = "requires a real cluster + mock pangolin; see .github/workflows/integration.yml"]
async fn controller_reconciles_l4_routes() {
    let endpoint = env("INTEGRATION_PANGOLIN_URL");
    let namespace = env("INTEGRATION_NAMESPACE");
    let parent_gateway =
        std::env::var("INTEGRATION_PARENT_GATEWAY").unwrap_or_else(|_| "eg".into());

    let kube_client = kube::Client::try_default()
        .await
        .expect("connect to Kubernetes API (is KUBECONFIG set?)");

    let mut cfg = integration_config(
        &endpoint,
        &namespace,
        &parent_gateway,
        BackendKind::Service,
        "l4",
    );
    cfg.enable_tcp_routes = true;
    cfg.enable_udp_routes = true;
    let pang_client = pangolin::Client::new(&cfg).expect("build pangolin client");

    let shutdown = CancellationToken::new();
    let handle = {
        let cfg = cfg.clone();
        let kube_client = kube_client.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            reconcile::run_loop(
                cfg,
                kube_client,
                pang_client,
                shutdown,
                Readiness::default(),
            )
            .await
        })
    };

    let route_api: Api<HTTPRoute> = Api::namespaced(kube_client.clone(), &namespace);
    let ls_api: Api<ListenerSet> = Api::namespaced(kube_client.clone(), &namespace);
    let tcp_api: Api<TCPRoute> = Api::namespaced(kube_client.clone(), &namespace);
    let udp_api: Api<UDPRoute> = Api::namespaced(kube_client.clone(), &namespace);
    let svc_api: Api<Service> = Api::namespaced(kube_client.clone(), &namespace);
    let eps_api: Api<EndpointSlice> = Api::namespaced(kube_client.clone(), &namespace);

    let selector = cfg.managed_selector();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    // L4 routes are last in the apply order; once they land everything else did.
    let tcp_routes = wait_for_at_least(&tcp_api, &selector, 1, deadline, "TCPRoute").await;
    let udp_routes = wait_for_at_least(&udp_api, &selector, 1, deadline, "UDPRoute").await;
    let http_routes = wait_for_at_least(&route_api, &selector, 1, deadline, "HTTPRoute").await;
    let listener_sets = wait_for_at_least(&ls_api, &selector, 1, deadline, "ListenerSet").await;
    // The UDP router targets an IP, so service mode synthesizes a UDP stub.
    let services = wait_for_at_least(&svc_api, &selector, 1, deadline, "Service").await;
    let endpoint_slices =
        wait_for_at_least(&eps_api, &selector, 1, deadline, "EndpointSlice").await;

    println!("TCPRoutes:      {tcp_routes:?}");
    println!("UDPRoutes:      {udp_routes:?}");
    println!("HTTPRoutes:     {http_routes:?}");
    println!("ListenerSets:   {listener_sets:?}");
    println!("Services:       {services:?}");
    println!("EndpointSlices: {endpoint_slices:?}");

    assert_eq!(tcp_routes.len(), 1);
    assert_eq!(udp_routes.len(), 1);
    assert_eq!(http_routes.len(), 1);
    assert_eq!(listener_sets.len(), 1);

    let lp = ListParams::default().labels(&selector);

    // The API server accepted the L4 listeners — verify protocol/port landed.
    let ls = &ls_api.list(&lp).await.expect("list ListenerSets").items[0];
    let tcp_listener = ls
        .spec
        .listeners
        .iter()
        .find(|l| l.protocol == "TCP")
        .expect("TCP listener");
    assert_eq!(tcp_listener.port, 2345);
    let udp_listener = ls
        .spec
        .listeners
        .iter()
        .find(|l| l.protocol == "UDP")
        .expect("UDP listener");
    assert_eq!(udp_listener.port, 5353);

    // TCPRoute attaches to its listener by sectionName.
    let tcp = &tcp_api.list(&lp).await.expect("list TCPRoutes").items[0];
    let parents = tcp.spec.parent_refs.as_ref().expect("parentRefs");
    assert_eq!(parents[0].kind.as_deref(), Some("ListenerSet"));
    assert_eq!(parents[0].section_name.as_deref(), Some("tcp-2345"));

    // The synthesized UDP backend stub carries the UDP port protocol.
    let svc = &svc_api.list(&lp).await.expect("list Services").items[0];
    let port = &svc.spec.as_ref().unwrap().ports.as_ref().unwrap()[0];
    assert_eq!(port.protocol.as_deref(), Some("UDP"));

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}

#[tokio::test]
#[ignore = "requires a real cluster + Envoy Gateway Backend CRD + mock pangolin; \
            see .github/workflows/integration.yml"]
async fn controller_reconciles_envoy_backends() {
    let endpoint = env("INTEGRATION_PANGOLIN_URL");
    let namespace = env("INTEGRATION_NAMESPACE");
    let parent_gateway =
        std::env::var("INTEGRATION_PARENT_GATEWAY").unwrap_or_else(|_| "eg".into());

    let kube_client = kube::Client::try_default()
        .await
        .expect("connect to Kubernetes API (is KUBECONFIG set?)");

    let cfg = integration_config(
        &endpoint,
        &namespace,
        &parent_gateway,
        BackendKind::EnvoyBackend,
        "egw",
    );
    let pang_client = pangolin::Client::new(&cfg).expect("build pangolin client");

    let shutdown = CancellationToken::new();
    let handle = {
        let cfg = cfg.clone();
        let kube_client = kube_client.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            reconcile::run_loop(
                cfg,
                kube_client,
                pang_client,
                shutdown,
                Readiness::default(),
            )
            .await
        })
    };

    let route_api: Api<HTTPRoute> = Api::namespaced(kube_client.clone(), &namespace);
    let ls_api: Api<ListenerSet> = Api::namespaced(kube_client.clone(), &namespace);
    let svc_api: Api<Service> = Api::namespaced(kube_client.clone(), &namespace);
    let eps_api: Api<EndpointSlice> = Api::namespaced(kube_client.clone(), &namespace);
    let be_api: Api<EnvoyBackend> = Api::namespaced(kube_client.clone(), &namespace);

    let selector = cfg.managed_selector();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    // Fixture has three routers: IP, FQDN, cluster-DNS. IP and FQDN both
    // emit Envoy Backend objects; cluster-DNS goes direct.
    let backends = wait_for_at_least(&be_api, &selector, 2, deadline, "Backend").await;
    let routes = wait_for_at_least(&route_api, &selector, 3, deadline, "HTTPRoute").await;
    let listener_sets = wait_for_at_least(&ls_api, &selector, 1, deadline, "ListenerSet").await;

    println!("HTTPRoutes:    {routes:?}");
    println!("ListenerSets:  {listener_sets:?}");
    println!("Backends:      {backends:?}");

    assert_eq!(routes.len(), 3, "expected exactly three HTTPRoutes");
    assert_eq!(listener_sets.len(), 1);
    assert_eq!(backends.len(), 2, "expected one Backend each for IP + FQDN");

    // Envoy-backend mode must NOT synthesize Services / EndpointSlices.
    assert_exact_count(&svc_api, &selector, 0, "Service").await;
    assert_exact_count(&eps_api, &selector, 0, "EndpointSlice").await;

    // Sanity-check the Backend payloads we wrote.
    let lp = ListParams::default().labels(&selector);
    let be_list = be_api.list(&lp).await.expect("list Backends");
    let mut saw_ip = false;
    let mut saw_fqdn = false;
    for be in &be_list.items {
        for ep in &be.spec.endpoints {
            if let Some(ip) = &ep.ip {
                saw_ip = true;
                assert!(!ip.address.is_empty());
                assert!(ip.port > 0);
            }
            if let Some(fqdn) = &ep.fqdn {
                saw_fqdn = true;
                assert!(!fqdn.hostname.is_empty());
                assert!(fqdn.port > 0);
            }
        }
    }
    assert!(saw_ip, "no Backend carried an IP endpoint");
    assert!(saw_fqdn, "no Backend carried an FQDN endpoint");

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}
