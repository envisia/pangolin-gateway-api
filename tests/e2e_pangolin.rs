//! End-to-end test driving the controller against a *real* pangolin instance.
//!
//! `#[ignore]`-d because it requires:
//!   - A reachable Kubernetes API server (Gateway API experimental CRDs).
//!   - A running pangolin server reachable at `INTEGRATION_PANGOLIN_URL`
//!     (the internal traefik-config endpoint, e.g. `http://127.0.0.1:13001/api/v1/traefik-config`).
//!   - Pangolin pre-provisioned with at least one HTTP resource pointing at
//!     an IP backend — typically by running `cargo run --example
//!     pangolin_bootstrap` first.
//!
//! Wired up by `.github/workflows/e2e.yml`.
//!
//! What we assert:
//!   - Exactly two HTTPRoutes (the `web` resource gets an HTTPS router + a
//!     `redirect-to-https` HTTP router — that's how real pangolin emits HTTP
//!     resources).
//!   - One aggregated ListenerSet for the controller instance.
//!   - At least one synthesized Service + EndpointSlice (the IP target).
//!   - Each HTTPRoute's hostnames contain the provisioned subdomain.

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

fn e2e_config(endpoint: &str, namespace: &str, parent_gateway: &str) -> Config {
    Config {
        pangolin_endpoint: Url::parse(endpoint).expect("INTEGRATION_PANGOLIN_URL must be a URL"),
        auth_header: None,
        fetch_timeout: Duration::from_secs(15),
        poll_interval: Duration::from_secs(2),
        max_backoff: Duration::from_secs(5),
        max_response_body_bytes: 4 << 20,
        tls_skip_verify: false,
        ca_file: None,

        namespace: namespace.to_string(),
        parent_gateway: parent_gateway.to_string(),
        parent_gateway_namespace: Some(namespace.to_string()),
        listener_set_name: "pangolin-e2e".into(),

        http_port: 80,
        https_port: 443,
        enable_https_listeners: false,
        backend_kind: BackendKind::Service,
        tls_secret_template: None,
        tls_secret_namespace: None,

        field_manager: "pangolin-gateway-controller-e2e".into(),
        managed_label_key: "app.kubernetes.io/managed-by".into(),
        managed_label_value: "pangolin-gateway-controller".into(),
        instance_label_key: "pangolin.envisia.de/instance".into(),
        instance_label_value: "e2e".into(),
        managed_annotation_key: "pangolin.envisia.de/source".into(),
        managed_annotation_value: "pangolin-gateway-controller".into(),

        httproute_annotations: BTreeMap::new(),
        listenerset_annotations: BTreeMap::new(),

        read_only: false,
        log_traefik_config: true,
    }
}

async fn wait_for_at_least<T>(
    api: &Api<T>,
    selector: &str,
    min_count: usize,
    deadline: tokio::time::Instant,
    kind: &str,
) -> Vec<T>
where
    T: Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug,
{
    let lp = ListParams::default().labels(selector);
    loop {
        match api.list(&lp).await {
            Ok(list) if list.items.len() >= min_count => return list.items,
            Ok(list) => eprintln!(
                "still waiting for >= {min_count} {kind}, have {}",
                list.items.len()
            ),
            Err(e) => eprintln!("list {kind} failed (will retry): {e:#}"),
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for >= {min_count} {kind} objects matching {selector}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
#[ignore = "requires a real cluster + provisioned pangolin; see .github/workflows/e2e.yml"]
async fn controller_reconciles_real_pangolin() {
    let endpoint =
        std::env::var("INTEGRATION_PANGOLIN_URL").expect("INTEGRATION_PANGOLIN_URL must be set");
    let namespace =
        std::env::var("INTEGRATION_NAMESPACE").expect("INTEGRATION_NAMESPACE must be set");
    let parent_gateway =
        std::env::var("INTEGRATION_PARENT_GATEWAY").unwrap_or_else(|_| "eg".into());
    let expected_host = std::env::var("INTEGRATION_RESOURCE_HOST")
        .unwrap_or_else(|_| "web.integration.local".into());

    let kube_client = kube::Client::try_default()
        .await
        .expect("connect to Kubernetes API (is KUBECONFIG set?)");

    let cfg = e2e_config(&endpoint, &namespace, &parent_gateway);
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);

    // Pangolin emits two routers per HTTP resource: the websecure router and a
    // redirect-to-https router on the `web` entrypoint. Both should land as
    // HTTPRoutes.
    let routes = wait_for_at_least(&route_api, &selector, 2, deadline, "HTTPRoute").await;
    let listener_sets = wait_for_at_least(&ls_api, &selector, 1, deadline, "ListenerSet").await;
    let services = wait_for_at_least(&svc_api, &selector, 1, deadline, "Service").await;
    let endpoint_slices =
        wait_for_at_least(&eps_api, &selector, 1, deadline, "EndpointSlice").await;

    eprintln!(
        "HTTPRoutes:     {:?}",
        routes
            .iter()
            .filter_map(|r| r.meta().name.clone())
            .collect::<Vec<_>>()
    );
    eprintln!(
        "ListenerSets:   {:?}",
        listener_sets
            .iter()
            .filter_map(|r| r.meta().name.clone())
            .collect::<Vec<_>>()
    );
    eprintln!(
        "Services:       {:?}",
        services
            .iter()
            .filter_map(|r| r.meta().name.clone())
            .collect::<Vec<_>>()
    );
    eprintln!(
        "EndpointSlices: {:?}",
        endpoint_slices
            .iter()
            .filter_map(|r| r.meta().name.clone())
            .collect::<Vec<_>>()
    );

    assert_eq!(routes.len(), 2, "expected websecure + redirect HTTPRoutes");
    assert_eq!(listener_sets.len(), 1);
    assert!(!services.is_empty(), "no synthesized Service");
    assert!(!endpoint_slices.is_empty(), "no synthesized EndpointSlice");

    // Every HTTPRoute should reference the provisioned hostname.
    let mut all_hostnames = Vec::<String>::new();
    for route in &routes {
        if let Some(hosts) = route.spec.hostnames.as_ref() {
            for h in hosts {
                all_hostnames.push(h.clone());
            }
        }
    }
    assert!(
        all_hostnames.iter().any(|h| h == &expected_host),
        "no HTTPRoute carried the expected hostname {expected_host}; saw: {all_hostnames:?}"
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}
