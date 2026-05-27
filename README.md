# pangolin-gateway-controller

[![CI](https://github.com/envisia/pangolin-gateway-api/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/envisia/pangolin-gateway-api/actions/workflows/ci.yml)
[![Docker](https://github.com/envisia/pangolin-gateway-api/actions/workflows/docker.yml/badge.svg?branch=main)](https://github.com/envisia/pangolin-gateway-api/actions/workflows/docker.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](#license)

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
       │  pangolin-gateway-controller                       │
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
| `http.services[*].loadBalancer` (IP)     | _service mode (default):_ headless `Service` + `EndpointSlice` with the IPs.<br>_envoy-backend mode:_ one `gateway.envoyproxy.io/v1alpha1 Backend` with `endpoints[].ip`. |
| `http.services[*].loadBalancer` (cluster DNS `<svc>.<ns>.svc[.cluster.local]`) | direct backendRef to the existing Service (both modes)            |
| `http.services[*].loadBalancer` (other FQDN, e.g. `api.example.com`) | _service mode:_ logged and dropped (EndpointSlice can't carry hostnames).<br>_envoy-backend mode:_ `Backend.spec.endpoints[].fqdn`. |
| `middlewares.redirectScheme`             | HTTPRoute filter `RequestRedirect{scheme}`                                 |
| `middlewares.headers.customRequestHeaders` | HTTPRoute filter `RequestHeaderModifier{set}`                            |
| `middlewares.headers.customResponseHeaders` | HTTPRoute filter `ResponseHeaderModifier{set}`                          |
| `middlewares.addPrefix`                  | HTTPRoute filter `URLRewrite{path.ReplacePrefixMatch}`                     |
| `middlewares.replacePath`                | HTTPRoute filter `URLRewrite{path.ReplaceFullPath}`                        |
| `middlewares.replacePathRegex`           | not in core Gateway API – logged + skipped                                 |
| `middlewares.plugin.badger`              | when `CONFIG_BADGER_EXT_AUTH=true`: per-route Envoy Gateway `SecurityPolicy.extAuth`; otherwise logged + skipped |
| Pangolin dashboard host                  | optional static `HTTPRoute`s for Next.js (`:3002`) and API/WebSocket (`:3000`) when `CONFIG_PANGOLIN_DASHBOARD_HOST` is set |
| Gerbil UDP ports                         | optional `UDPRoute`s + UDP listeners when `CONFIG_GERBIL_UDP_ROUTE=true`    |
| `tcp.*` / other `udp.*` dynamic config   | logged + skipped                                                            |

Unsupported rule constructs (`||` disjunction, `!` negation, `HostRegexp`,
`Method`, `Headers`, …) cause the affected router to be **logged and skipped**
rather than silently misrouted.

## Backend strategy

`CONFIG_BACKEND_KIND` selects how pangolin's IP/FQDN backends are represented
in the cluster:

| Value                | Emits                                                          | Portable? | Supports FQDN? |
|----------------------|----------------------------------------------------------------|-----------|----------------|
| `service` _(default)_| headless `Service` + `EndpointSlice` per pangolin service      | yes       | no             |
| `envoy-backend`      | `gateway.envoyproxy.io/v1alpha1 Backend` per pangolin service  | **Envoy Gateway only** | yes  |

In either mode, pangolin URLs that point at a Kubernetes cluster Service
(`<name>.<namespace>.svc[.cluster.local][:port]`) are passed through as a
direct `Service` `backendRef` — the controller does not synthesize a duplicate.

Pick the default unless you specifically want FQDN backends or the `Backend`
CRD's other features (health checking via `BackendTrafficPolicy`, etc.).

## Badger external auth

Gateway API cannot express Traefik plugins directly. When
`CONFIG_BADGER_EXT_AUTH=true`, routers that reference Pangolin's `badger`
plugin get a managed Envoy Gateway `SecurityPolicy` targeting the generated
`HTTPRoute`.

The policy must point at an HTTP service that speaks Envoy's ext_authz protocol.
Pangolin's `/api/v1/badger/verify-session` endpoint expects Badger's JSON body,
so in today's Pangolin releases this normally means running a small shim service
that adapts Envoy's auth check request to Pangolin's badger verify-session API.
The controller defaults to a Service named `pangolin-badger-ext-authz` on port
`9002`; override that with the `CONFIG_BADGER_EXT_AUTH_*` settings.

## Scoped reconcile

Set `CONFIG_RECONCILE_ONLY` to a comma-separated allow-list to test a subset of
objects without touching the rest of the controller-owned state. The easiest
form is a hostname; the controller expands it to the matching `HTTPRoute`, its
badger `SecurityPolicy`, generated backend objects, and the shared
`ListenerSet`:

```sh
CONFIG_RECONCILE_ONLY="9-chris.example.com"
```

Explicit object selectors still work as `Kind/name` or `Kind:name`, for example
`HTTPRoute/hr-9-chris-connect-router`. Use `Hostname/name` if you need to force
a hostname selector that does not contain a dot.

Apply and GC both honor the scope. Objects outside the scope are left alone, and
selected objects that are no longer desired can still be garbage-collected.

## Migration config changes

For a dual-stack rollout, keep Traefik active and add the Gateway API pieces in
small steps:

1. Deploy a badger ext_auth shim Service, then set:

   ```sh
   CONFIG_BADGER_EXT_AUTH=true
   CONFIG_BADGER_EXT_AUTH_BACKEND_NAME=pangolin-badger-ext-authz
   CONFIG_BADGER_EXT_AUTH_BACKEND_PORT=9002
   ```

2. Start with a narrow reconcile scope:

   ```sh
   CONFIG_RECONCILE_ONLY="9-chris.example.com"
   ```

3. To serve Pangolin itself through Envoy Gateway, expose the Pangolin Service
   ports for API/WebSocket (`3000`) and Next.js (`3002`), then set:

   ```sh
   CONFIG_PANGOLIN_DASHBOARD_HOST=pangolin.example.com
   ```

4. To move Gerbil UDP traffic through Envoy Gateway, expose UDP ports on the
   Gerbil Service and enable:

   ```sh
   CONFIG_GERBIL_UDP_ROUTE=true
   CONFIG_GERBIL_UDP_PORTS=51820,21820
   ```

Remove `CONFIG_RECONCILE_ONLY` only after the selected route, policy, dashboard,
and UDP paths have been validated against Envoy Gateway.

For the Gerbil public-IP migration pattern, including direct old-IP traffic plus
a separate Envoy Gateway IP, see
[docs/gerbil-gateway-migration.md](docs/gerbil-gateway-migration.md).
For the shared-IP path where only HTTP/HTTPS moves from Traefik to Envoy while
Gerbil keeps the WireGuard UDP ports, see
[docs/shared-ip-http-envoy-migration.md](docs/shared-ip-http-envoy-migration.md).

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
| `CONFIG_BACKEND_KIND`              | `service`                                   | `service` (default) or `envoy-backend`. See [Backend strategy](#backend-strategy) |
| `CONFIG_HTTPROUTE_ANNOTATIONS`     | _(unset)_                                   | `k=v,k=v` annotations stamped on every HTTPRoute. Typical: `cert-manager.io/cluster-issuer=letsencrypt-prod` |
| `CONFIG_LISTENERSET_ANNOTATIONS`   | _(unset)_                                   | `k=v,k=v` annotations stamped on the ListenerSet                            |
| `CONFIG_BADGER_EXT_AUTH`           | `false`                                     | Emit Envoy Gateway `SecurityPolicy.extAuth` for routes that use badger       |
| `CONFIG_BADGER_EXT_AUTH_BACKEND_NAME` | `pangolin-badger-ext-authz`              | Service name of the Envoy ext_auth-compatible badger shim                   |
| `CONFIG_BADGER_EXT_AUTH_BACKEND_NAMESPACE` | controller namespace                 | Namespace of the ext_auth backend Service                                   |
| `CONFIG_BADGER_EXT_AUTH_BACKEND_PORT` | `9002`                                   | Service port for the ext_auth backend                                       |
| `CONFIG_BADGER_EXT_AUTH_PATH`      | _(unset)_                                   | Optional fixed auth check path                                              |
| `CONFIG_BADGER_EXT_AUTH_HEADERS_TO_EXTAUTH` | `authorization,cookie,x-forwarded-for,x-forwarded-host,x-forwarded-proto,x-real-ip,p-access-token-id,p-access-token` | Headers forwarded to the auth service |
| `CONFIG_BADGER_EXT_AUTH_HEADERS_TO_BACKEND` | `remote-user,remote-email,remote-name,remote-role` | Headers copied from auth response to upstream backends          |
| `CONFIG_BADGER_EXT_AUTH_FAIL_OPEN` | `false`                                     | Allow traffic if the auth service is unavailable                            |
| `CONFIG_RECONCILE_ONLY`            | _(unset)_                                   | Optional comma-separated allow-list of hostnames or `Kind/name` objects to apply/GC |
| `CONFIG_PANGOLIN_DASHBOARD_HOST`   | _(unset)_                                   | When set, emit static dashboard HTTPRoutes for this hostname                 |
| `CONFIG_PANGOLIN_SERVICE_NAME`     | `pangolin`                                  | Service that backs the dashboard/API routes                                 |
| `CONFIG_PANGOLIN_SERVICE_NAMESPACE`| controller namespace                        | Namespace of the Pangolin Service                                           |
| `CONFIG_PANGOLIN_API_PORT`         | `3000`                                      | Pangolin API/WebSocket Service port                                         |
| `CONFIG_PANGOLIN_NEXT_PORT`        | `3002`                                      | Pangolin Next.js Service port                                               |
| `CONFIG_PANGOLIN_REDIRECT_HTTP_TO_HTTPS` | `true`                              | Emit an HTTP-to-HTTPS redirect route when HTTPS listeners are configured    |
| `CONFIG_GERBIL_UDP_ROUTE`          | `false`                                     | Emit UDP listeners and UDPRoutes for Gerbil                                 |
| `CONFIG_GERBIL_SERVICE_NAME`       | `gerbil`                                    | Service that backs Gerbil UDP traffic                                       |
| `CONFIG_GERBIL_SERVICE_NAMESPACE`  | controller namespace                        | Namespace of the Gerbil Service                                             |
| `CONFIG_GERBIL_UDP_PORTS`          | `51820,21820`                               | UDP ports to expose through the ListenerSet                                 |
| `CONFIG_FIELD_MANAGER`             | `pangolin-gateway-controller`                 | Server-Side Apply field manager                                             |
| `CONFIG_MANAGED_LABEL_KEY`         | `app.kubernetes.io/managed-by`              | Used for GC selector                                                        |
| `CONFIG_MANAGED_LABEL_VALUE`       | `pangolin-gateway-controller`                 | Used for GC selector                                                        |
| `CONFIG_INSTANCE_LABEL_KEY`        | `pangolin.envisia.de/instance`              | Lets multiple controller instances coexist without trampling each other     |
| `CONFIG_INSTANCE_LABEL_VALUE`      | `default`                                   |                                                                             |
| `CONFIG_READ_ONLY`                 | `false`                                     | Dry-run: log what would happen, do not call apply/delete                    |
| `CONFIG_LOG_TRAEFIK_CONFIG`        | `false`                                     | Debug: log the raw pangolin response at `debug` level                       |

## Development

```sh
cargo test                  # unit + fixture tests
cargo run                   # needs CONFIG_ENDPOINT, CONFIG_PARENT_GATEWAY, kubeconfig
docker build -t pangolin-gateway-controller:dev .
```

Fixtures under `tests/fixtures/` are real pangolin Traefik provider responses
from the upstream pangolin-kube-controller test corpus.

## License

This project was AI-generated using the Go reference implementation
[pangolin-kube-controller] as the architectural model. Because that upstream is
licensed under the [GNU Affero General Public License v3.0][AGPL-3.0], and the
design and behaviour of this controller (polling model, managed-label GC,
SSA apply strategy, fixture corpus) was derived from it, the combined work
must be distributed under the AGPL-3.0:

* GNU Affero General Public License v3.0 ([LICENSE-AGPL](LICENSE-AGPL) or
  <https://www.gnu.org/licenses/agpl-3.0.html>)

Separately, the *new* code contributed in this repository — i.e. the
Rust implementation itself, exclusive of any patterns inherited from
[pangolin-kube-controller] — is *additionally* offered by its authors under
either of:

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option, so that those original portions may be reused outside the
context of this combined work. The dual MIT/Apache-2.0 grant does **not**
extend to any portion derived from [pangolin-kube-controller] or any other
AGPL-3.0 code — if you cannot satisfy the AGPL-3.0, you cannot redistribute
this repository as a whole.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be licensed under AGPL-3.0 for the combined work and additionally available
under MIT or Apache-2.0 for the original portions, as above, without any
additional terms or conditions.

[AGPL-3.0]: https://www.gnu.org/licenses/agpl-3.0.html

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
