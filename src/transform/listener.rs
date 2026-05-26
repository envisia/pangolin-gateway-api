//! Aggregate every hostname seen in routes into a single ListenerSet attached to the
//! configured parent Gateway. One HTTP listener per host, plus an HTTPS listener when
//! a TLS secret template is configured.

use gateway_api::apis::experimental::listenersets::{
    ListenerSet, ListenerSetListeners, ListenerSetListenersTls,
    ListenerSetListenersTlsCertificateRefs, ListenerSetListenersTlsMode, ListenerSetParentRef,
    ListenerSetSpec,
};

use crate::apply::{managed_metadata_with, owner_labels};
use crate::config::Config;
use crate::transform::Desired;
use crate::transform::naming::dns_label;
use crate::transform::route::RouteIndex;

pub fn build_listener_set(cfg: &Config, routes: &RouteIndex, desired: &mut Desired) {
    let mut listeners: Vec<ListenerSetListeners> = Vec::new();

    for host in &routes.hostnames {
        listeners.push(ListenerSetListeners {
            name: dns_label(&format!("http-{host}")),
            hostname: Some(host.clone()),
            port: cfg.http_port,
            protocol: "HTTP".into(),
            tls: None,
            allowed_routes: None,
        });
    }

    if cfg.enable_https_listeners && cfg.tls_secret_template.is_some() {
        for host in &routes.https_hosts {
            let secret = render_secret_name(cfg.tls_secret_template.as_deref().unwrap(), host);
            listeners.push(ListenerSetListeners {
                name: dns_label(&format!("https-{host}")),
                hostname: Some(host.clone()),
                port: cfg.https_port,
                protocol: "HTTPS".into(),
                tls: Some(ListenerSetListenersTls {
                    mode: Some(ListenerSetListenersTlsMode::Terminate),
                    certificate_refs: Some(vec![ListenerSetListenersTlsCertificateRefs {
                        group: Some(String::new()),
                        kind: Some("Secret".into()),
                        name: secret,
                        namespace: cfg.tls_secret_namespace.clone(),
                    }]),
                    ..Default::default()
                }),
                allowed_routes: None,
            });
        }
    }

    let name = dns_label(&cfg.listener_set_name);
    let labels = owner_labels(cfg, &name);

    let parent_ref = ListenerSetParentRef {
        group: Some("gateway.networking.k8s.io".into()),
        kind: Some("Gateway".into()),
        name: cfg.parent_gateway.clone(),
        namespace: cfg.parent_gateway_namespace.clone(),
    };

    let ls = ListenerSet {
        metadata: managed_metadata_with(cfg, &name, labels, &cfg.listenerset_annotations),
        spec: ListenerSetSpec {
            parent_ref,
            listeners,
        },
        status: None,
    };

    desired.listener_sets.insert(name, ls);
}

fn render_secret_name(template: &str, host: &str) -> String {
    template
        .replace("{hostname}", host)
        .replace("{hostname-dashed}", &host.replace('.', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_substitution() {
        assert_eq!(
            render_secret_name("{hostname-dashed}-tls", "api.example.com"),
            "api-example-com-tls"
        );
        assert_eq!(
            render_secret_name("{hostname}", "api.example.com"),
            "api.example.com"
        );
    }
}
