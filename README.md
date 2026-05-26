# pangolin-envoy-controller

A small Kubernetes controller, written in Rust with [kube-rs], that reconciles
[Pangolin]'s Traefik dynamic-config output into [Gateway API] objects for
[Envoy Gateway]. It is the same idea as the upstream [pangolin-kube-controller]
(which emits Traefik `IngressRoute` CRDs), retargeted at Gateway API so a single
Envoy Gateway can serve every tunneled resource pangolin manages.

## How it works

```
                      ┌─────────────────────────────┐
                      │   pangolin                  │
                      │   GET /api/v1/traefik-config│
                      └────────────┬────────────────┘
                                   │  poll + ETag
                                   ▼
       ┌──────────────────────────────────────────────────┐
       │  pangolin-envoy-controller                       │
       │  – decode Traefik dynamic config                 │
       │  – translate routers/services/middlewares        │
       │  – Server-Side Apply with field manager          │
       │  – sweep orphans via managed-by label selector   │
       └────────────┬─────────────────────────────────────┘
                    │
                    ▼
   ┌─────────────────────────────────────────────────────────┐
   │  Kubernetes / Gateway API                               │
   │  • HTTPRoute        (one per pangolin router)           │
   │  • ListenerSet      (attached to your Envoy Gateway)    │
   │  • Service          (IP-backed pangolin targets only)   │
   │  • EndpointSlice    (IP-backed pangolin targets only)   │
   └─────────────────────────────────────────────────────────┘
```

The controller is intentionally stateless: every poll it diffs the desired set
against whatever it owns in the cluster (matched by the `managed-by` label),
applies the additions/updates with [Server-Side Apply], and deletes orphans.

## Mapping

| Pangolin / Traefik                       | Gateway API                                                                |
|------------------------------------------|----------------------------------------------------------------------------|
| `http.routers[*].rule = Host(…)`         | `HTTPRoute.spec.hostnames` + a Listener per unique host on the ListenerSet |
| `http.routers[*].rule = PathPrefix(`/x`)`| `HTTPRouteRulesMatchesPath{type=PathPrefix,value="/x"}`                    |
| `http.routers[*].rule = Path(`/x`)`      | `HTTPRouteRulesMatchesPath{type=Exact,value="/x"}`                         |
| `http.services[*].loadBalancer` (IP)     | Headless `Service` + `EndpointSlice` with the IPs                          |
| `http.services[*].loadBalancer` (cluster DNS `<svc>.<ns>.svc[.cluster.local]`) | direct backendRef to the existing Service     |
| `middlewares.redirectScheme`             | HTTPRoute filter `RequestRedirect{scheme}`                                 |
| `middlewares.headers.customRequestHeaders` | HTTPRoute filter `RequestHeaderModifier{set}`                            |
| `middlewares.headers.customResponseHeaders` | HTTPRoute filter `ResponseHeaderModifier{set}`                          |
| `middlewares.addPrefix`                  | HTTPRoute filter `URLRewrite{path.ReplacePrefixMatch}`                     |
| `middlewares.replacePath`                | HTTPRoute filter `URLRewrite{path.ReplaceFullPath}`                        |
| `middlewares.replacePathRegex`           | not in core Gateway API – logged + skipped                                 |
| `middlewares.plugin.badger`              | pangolin's auth plugin – configure via Envoy Gateway `SecurityPolicy` instead |
| `tcp.*` / `udp.*`                        | not yet handled (planned: `TCPRoute` / `UDPRoute`)                         |

Unsupported rule constructs (`||` disjunction, `!` negation, `HostRegexp`,
`Method`, `Headers`, …) cause the affected router to be **logged and skipped**
rather than silently misrouted.

## Certificate handling with cert-manager

There are two ways the controller fills in `tls.certificateRefs` on its listeners:

1.  **Predetermined Secret naming.** Set `CONFIG_TLS_SECRET_TEMPLATE` to a template
    referencing `{hostname}` or `{hostname-dashed}` and create the Secrets yourself
    (or with a tool of your choice). The controller resolves the template per host
    and wires up the `certificateRefs` for every HTTPS listener.

2.  **Annotation-driven (e.g. cert-manager).** Set
    `CONFIG_HTTPROUTE_ANNOTATIONS` and/or `CONFIG_LISTENERSET_ANNOTATIONS` to a
    comma-separated `key=value` list and the controller stamps those annotations
    onto every HTTPRoute / ListenerSet it creates. Combined with cert-manager's
    [gateway-shim], cert-manager mints a Secret per hostname automatically and the
    template above just refers to that Secret. For example:

    ```sh
    CONFIG_HTTPROUTE_ANNOTATIONS="cert-manager.io/cluster-issuer=letsencrypt-prod"
    CONFIG_LISTENERSET_ANNOTATIONS="cert-manager.io/cluster-issuer=letsencrypt-prod"
    CONFIG_TLS_SECRET_TEMPLATE="{hostname-dashed}-tls"
    ```

    Annotations apply only to the kind they're configured for; the synthesized
    `Service` and `EndpointSlice` objects are never tagged, so cert-manager (or any
    other consumer that filters by annotation) won't accidentally target them.

[gateway-shim]: https://cert-manager.io/docs/usage/gateway/

> **Note on the kind name.** This controller emits `ListenerSet`
> (`gateway.networking.k8s.io/v1`, experimental channel) — not the older
> `XListenerSet` from `gateway.networking.x-k8s.io/v1alpha1`. Make sure your
> installed Gateway API CRDs match (v1.5.1+).

## Quickstart

1.  Install Gateway API CRDs (experimental channel — needed for `ListenerSet`):

    ```sh
    kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.5.1/experimental-install.yaml
    ```

2.  Install [Envoy Gateway] and create a `Gateway` named `eg` (or whatever you
    configure via `CONFIG_PARENT_GATEWAY`). The `Gateway` should permit attachment
    from `ListenerSet`s in your controller namespace:

    ```yaml
    apiVersion: gateway.networking.k8s.io/v1
    kind: Gateway
    metadata:
      name: eg
      namespace: envoy-gateway-system
    spec:
      gatewayClassName: eg
      allowedListeners:
        namespaces:
          from: All
      listeners:
        - name: placeholder
          protocol: HTTP
          port: 8080
    ```

3.  Apply the controller manifests:

    ```sh
    kubectl apply -k deploy/
    ```

4.  Point `CONFIG_ENDPOINT` at your pangolin's internal Traefik provider URL
    (`http://pangolin:8081/api/v1/traefik-config` by default) and watch new
    HTTPRoutes appear as you add resources in pangolin.

## Configuration

Everything is configured via environment variables. The naming follows the
upstream Go controller (`CONFIG_*`) where the concepts overlap.

| Env var                            | Default                                     | Notes                                                                       |
|------------------------------------|---------------------------------------------|-----------------------------------------------------------------------------|
| `CONFIG_ENDPOINT`                  | _(required)_                                | Pangolin Traefik provider URL                                               |
| `CONFIG_PARENT_GATEWAY`            | _(required)_                                | Name of the Envoy `Gateway` the ListenerSet attaches to                     |
| `CONFIG_PARENT_GATEWAY_NAMESPACE`  | controller namespace                        | Namespace of the parent Gateway                                             |
| `CONFIG_NAMESPACE`                 | `default`                                   | Namespace where HTTPRoute/ListenerSet/etc. are written                      |
| `CONFIG_LISTENERSET_NAME`          | `pangolin`                                  | Name of the single ListenerSet object                                       |
| `CONFIG_POLL_INTERVAL`             | `30s`                                       | How often to poll pangolin                                                  |
| `CONFIG_FETCH_TIMEOUT`             | `30s`                                       | Per-request HTTP timeout                                                    |
| `CONFIG_MAX_BACKOFF`               | `5m`                                        | Upper bound on exponential backoff for failed polls                         |
| `CONFIG_MAX_RESPONSE_BODY_BYTES`   | `16777216`                                  | Cap on pangolin response size                                               |
| `CONFIG_AUTH_HEADER`               | _(unset)_                                   | Sent as `Authorization:` if pangolin is protected                           |
| `CONFIG_CA_FILE`                   | _(unset)_                                   | PEM root CA bundle for pangolin TLS                                         |
| `CONFIG_TLS_SKIP_VERIFY`           | `false`                                     | Requires `I_UNDERSTAND_CONFIG_TLS_SKIP_VERIFY_IS_INSECURE=true` to enable   |
| `CONFIG_ALLOW_INSECURE_HTTP`       | `false`                                     | Allow `http://` pangolin endpoints                                          |
| `CONFIG_HTTP_PORT`                 | `80`                                        | Port for HTTP listeners on the ListenerSet                                  |
| `CONFIG_HTTPS_PORT`                | `443`                                       | Port for HTTPS listeners on the ListenerSet                                 |
| `CONFIG_ENABLE_HTTPS_LISTENERS`    | `true`                                      | Add an HTTPS listener per host when TLS is configured                       |
| `CONFIG_TLS_SECRET_TEMPLATE`       | _(unset)_                                   | Template for cert secret name. `{hostname}` and `{hostname-dashed}` placeholders. When unset, no HTTPS listener is created. |
| `CONFIG_TLS_SECRET_NAMESPACE`      | controller namespace                        | Namespace where TLS Secrets live                                            |
| `CONFIG_HTTPROUTE_ANNOTATIONS`     | _(unset)_                                   | `k=v,k=v` annotations stamped on every HTTPRoute. Typical: `cert-manager.io/cluster-issuer=letsencrypt-prod` |
| `CONFIG_LISTENERSET_ANNOTATIONS`   | _(unset)_                                   | `k=v,k=v` annotations stamped on the ListenerSet                            |
| `CONFIG_FIELD_MANAGER`             | `pangolin-envoy-controller`                 | Server-Side Apply field manager                                             |
| `CONFIG_MANAGED_LABEL_KEY`         | `app.kubernetes.io/managed-by`              | Used for GC selector                                                        |
| `CONFIG_MANAGED_LABEL_VALUE`       | `pangolin-envoy-controller`                 | Used for GC selector                                                        |
| `CONFIG_INSTANCE_LABEL_KEY`        | `pangolin.envisia.de/instance`              | Lets multiple controller instances coexist without trampling each other     |
| `CONFIG_INSTANCE_LABEL_VALUE`      | `default`                                   |                                                                             |
| `CONFIG_READ_ONLY`                 | `false`                                     | Dry-run: log what would happen, do not call apply/delete                    |
| `CONFIG_LOG_TRAEFIK_CONFIG`        | `false`                                     | Debug: log the raw pangolin response at `debug` level                       |

## Development

```sh
cargo test                  # unit + fixture tests
cargo run                   # needs CONFIG_ENDPOINT, CONFIG_PARENT_GATEWAY, kubeconfig
docker build -t pangolin-envoy-controller:dev .
```

Fixtures under `tests/fixtures/` are real pangolin Traefik provider responses
from the upstream pangolin-kube-controller test corpus.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

## Acknowledgements

* [pangolin] for the upstream tunneled reverse-proxy server.
* [pangolin-kube-controller] for the Go-based Traefik-CRD prior art that this
  project mirrors at a high level.
* [kube-rs] and [gateway-api-rs] for making Rust controllers pleasant.

[Pangolin]: https://github.com/fosrl/pangolin
[pangolin]: https://github.com/fosrl/pangolin
[pangolin-kube-controller]: https://github.com/fosrl/pangolin-kube-controller
[Envoy Gateway]: https://gateway.envoyproxy.io/
[Gateway API]: https://gateway-api.sigs.k8s.io/
[kube-rs]: https://github.com/kube-rs/kube
[gateway-api-rs]: https://github.com/kube-rs/gateway-api-rs
[Server-Side Apply]: https://kubernetes.io/docs/reference/using-api/server-side-apply/
