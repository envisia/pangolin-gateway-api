//! Real-cluster smoke test for the full reconcile pipeline.
//!
//! Marked `#[ignore]` because it requires:
//!   - A reachable Kubernetes API server (via `KUBECONFIG` or in-cluster).
//!   - Gateway API **experimental** CRDs installed (HTTPRoute, ListenerSet).
//!   - A mock pangolin endpoint reachable from this process — typically the
//!     `mock_pangolin` example running on `127.0.0.1:18080`.
//!
//! Wired up by `.github/workflows/integration.yml` (manual trigger). Run
//! locally with:
//!
//! ```sh
//! cargo run --example mock_pangolin -- &
//! INTEGRATION_PANGOLIN_URL=http://127.0.0.1:18080/api/v1/traefik-config \
//! INTEGRATION_NAMESPACE=pangolin-system \
//! cargo test --test cluster_integration -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::ListenerSet;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::Resource;
use kube::api::{Api, ListParams};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use url::Url;

use pangolin_gateway_controller::config::{BackendKind, Config};
use pangolin_gateway_controller::{pangolin, reconcile};

fn integration_config(endpoint: &str, namespace: &str, parent_gateway: &str) -> Config {
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
        listener_set_name: "pangolin-integration".into(),

        http_port: 80,
        https_port: 443,
        enable_https_listeners: false,
        backend_kind: BackendKind::Service,
        tls_secret_template: None,
        tls_secret_namespace: None,

        field_manager: "pangolin-gateway-controller-integration".into(),
        managed_label_key: "app.kubernetes.io/managed-by".into(),
        managed_label_value: "pangolin-gateway-controller".into(),
        instance_label_key: "pangolin.envisia.de/instance".into(),
        // Distinct instance label so the test never collides with a real
        // controller running in the same cluster.
        instance_label_value: "integration".into(),
        managed_annotation_key: "pangolin.envisia.de/source".into(),
        managed_annotation_value: "pangolin-gateway-controller".into(),

        httproute_annotations: BTreeMap::new(),
        listenerset_annotations: BTreeMap::new(),

        read_only: false,
        log_traefik_config: false,
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

#[tokio::test]
#[ignore = "requires a real cluster + mock pangolin; see .github/workflows/integration.yml"]
async fn controller_reconciles_into_cluster() {
    let endpoint =
        std::env::var("INTEGRATION_PANGOLIN_URL").expect("INTEGRATION_PANGOLIN_URL must be set");
    let namespace =
        std::env::var("INTEGRATION_NAMESPACE").expect("INTEGRATION_NAMESPACE must be set");
    let parent_gateway =
        std::env::var("INTEGRATION_PARENT_GATEWAY").unwrap_or_else(|_| "eg".into());

    let kube_client = kube::Client::try_default()
        .await
        .expect("connect to Kubernetes API (is KUBECONFIG set?)");

    let cfg = integration_config(&endpoint, &namespace, &parent_gateway);
    let pang_client = pangolin::Client::new(&cfg).expect("build pangolin client");

    let shutdown = CancellationToken::new();
    let handle = {
        let cfg = cfg.clone();
        let kube_client = kube_client.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(
            async move { reconcile::run_loop(cfg, kube_client, pang_client, shutdown).await },
        )
    };

    let route_api: Api<HTTPRoute> = Api::namespaced(kube_client.clone(), &namespace);
    let ls_api: Api<ListenerSet> = Api::namespaced(kube_client.clone(), &namespace);
    let svc_api: Api<Service> = Api::namespaced(kube_client.clone(), &namespace);
    let eps_api: Api<EndpointSlice> = Api::namespaced(kube_client.clone(), &namespace);

    let selector = cfg.managed_selector();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    let routes = wait_for_at_least(&route_api, &selector, 2, deadline, "HTTPRoute").await;
    let listener_sets = wait_for_at_least(&ls_api, &selector, 1, deadline, "ListenerSet").await;
    let services = wait_for_at_least(&svc_api, &selector, 1, deadline, "Service").await;
    let endpoint_slices =
        wait_for_at_least(&eps_api, &selector, 1, deadline, "EndpointSlice").await;

    println!("HTTPRoutes:     {routes:?}");
    println!("ListenerSets:   {listener_sets:?}");
    println!("Services:       {services:?}");
    println!("EndpointSlices: {endpoint_slices:?}");

    // Fixture has two routers; both should produce HTTPRoutes.
    assert_eq!(routes.len(), 2, "expected exactly two HTTPRoutes");
    // One aggregated ListenerSet for the controller instance.
    assert_eq!(listener_sets.len(), 1);
    // IP-backed router synthesizes a Service + EndpointSlice; cluster-DNS one
    // does not. So we expect exactly one of each.
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
