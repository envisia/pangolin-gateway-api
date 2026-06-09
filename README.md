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
| `middlewares.plugin.badger`              | pangolin's auth plugin – `SecurityPolicy` (ext-authz) when `CONFIG_EXT_AUTHZ_SERVICE` is set; otherwise the router is **skipped** (see [Authentication](#authentication-badger-protected-resources)) |
| `tcp.routers[*]` (entrypoint `tcp-<port>`, rule `HostSNI(`*`)`) | `TCPRoute` + a `TCP` listener on port `<port>` (requires `CONFIG_ENABLE_TCP_ROUTES=true`) |
| `tcp.routers[*]` with a concrete SNI or `tls` options | needs `TLSRoute` passthrough – logged + skipped              |
| `udp.routers[*]` (entrypoint `udp-<port>`) | `UDPRoute` + a `UDP` listener on port `<port>` (requires `CONFIG_ENABLE_UDP_ROUTES=true`) |
| `tcp.services` / `udp.services` `loadBalancer.servers[].address` | same IP / cluster-DNS / FQDN classification as HTTP backends (synthesized objects get a `-tcp-`/`-udp-` name infix and the right port protocol) |

Unsupported rule constructs (`||` disjunction, `!` negation, `HostRegexp`,
`Method`, `Headers`, …) cause the affected router to be **logged and skipped**
rather than silently misrouted.

## Backend strategy

`CONFIG_BACKEND_KIND` selects how pangolin's IP/FQDN backends are represented
in the cluster:

| Value                       | Emits                                                          | Portable? | Supports FQDN? |
|-----------------------------|----------------------------------------------------------------|-----------|----------------|
| `envoy-backend` _(default)_ | `gateway.envoyproxy.io/v1alpha1 Backend` per pangolin service  | **Envoy Gateway only** | yes  |
| `service`                   | headless `Service` + `EndpointSlice` per pangolin service      | yes       | no             |

In either mode, pangolin URLs that point at a Kubernetes cluster Service
(`<name>.<namespace>.svc[.cluster.local][:port]`) are passed through as a
direct `Service` `backendRef` — the controller does not synthesize a duplicate.

Pick the default (`envoy-backend`) unless you need portability to a non-Envoy
Gateway API implementation — `Backend` supports FQDN targets and unlocks the
CRD's other features (health checking via `BackendTrafficPolicy`, etc.), at
the cost of being Envoy Gateway-specific.

## Raw TCP/UDP resources

Pangolin's "raw" resources arrive as `tcp`/`udp` blocks whose entrypoint names
encode the public port (`tcp-234`, `udp-345`). With
`CONFIG_ENABLE_TCP_ROUTES` / `CONFIG_ENABLE_UDP_ROUTES` set, each router
becomes a `TCPRoute`/`UDPRoute` (experimental channel, `v1alpha2`) attached via
`sectionName` to a `TCP`/`UDP` listener the controller adds to its
ListenerSet. Envoy Gateway merges those listeners into the Gateway's
LoadBalancer Service, so the ports surface on the existing Envoy LB.

Both flags default to **off** because they have cluster-level prerequisites:

- the Gateway API **experimental channel** CRDs (TCPRoute/UDPRoute),
- an Envoy Gateway release that reconciles the graduated `ListenerSet` kind,
- for UDP: a cloud LoadBalancer that accepts mixed TCP/UDP protocol Services
  (otherwise leave UDP to gerbil's own Service and enable TCP only),
- every raw-resource port added in pangolin mutates the cloud LB's port list —
  expect slower propagation than in-cluster changes.

Only `HostSNI(`*`)` TCP rules are translated; a concrete SNI (or `tls`
options on the router) would need TLSRoute passthrough semantics and is logged
and skipped instead.

## Authentication (badger-protected resources)

Pangolin enforces resource auth (SSO, password, PIN, access rules) through its
`badger` Traefik plugin, which it attaches to **every** resource router. Envoy
can't run Traefik plugins, so the controller treats badger-protected routers
as follows:

1. **`CONFIG_EXT_AUTHZ_SERVICE` set (recommended):** every protected
   `HTTPRoute` gets an Envoy Gateway `SecurityPolicy` whose `extAuth.http`
   points at that service. This repo ships exactly that service: the
   **badger ext-authz shim** (`badger-ext-authz-shim`, second binary in the
   controller image — see [The badger ext-authz shim](#the-badger-ext-authz-shim)).
   Policies are emitted with `failOpen: false`: if the auth service is down,
   protected resources stay closed. The session cookie and `Authorization`
   header are forwarded by default (`CONFIG_EXT_AUTHZ_HEADERS_TO_EXT_AUTH`).
2. **Nothing configured (the default):** protected routers are **skipped**
   with a warning. This is deliberate — emitting them would silently expose
   SSO/password/PIN-protected resources to the internet. Redirect-only routers
   (pangolin's `redirect-to-https`) carry no auth and are unaffected.
3. **`CONFIG_ALLOW_UNAUTHENTICATED_ROUTES=true`:** escape hatch that emits
   protected routers *without* any auth filter. Only sane when every pangolin
   resource is intentionally public.

Raw TCP/UDP resources are not affected — pangolin does not apply badger at L4.

### The badger ext-authz shim

`badger-ext-authz-shim` bridges Envoy's HTTP external-authorization protocol
to pangolin's badger session API (the same endpoints the upstream Traefik
plugin uses). Per Envoy's contract the check request preserves the original
request's method, `Host` and path (prefixed with `CONFIG_EXT_AUTHZ_PATH`), and
on a non-2xx answer Envoy forwards the shim's status and headers — including
`Location` and `Set-Cookie` — to the client. That gives the full badger flow:

| Situation | Shim answer | Client sees |
|---|---|---|
| valid session (`verify-session`) | `200` + `Remote-User`/`Remote-Email`/… | request reaches the backend |
| no/invalid session, portal known | `302` + `Location` | redirect to the pangolin auth portal |
| post-login handoff (`?p_session_request=…`) | exchange via `exchange-session`, then `302` back to the cleaned URL + `Set-Cookie` | logged-in session on the resource domain |
| header-auth challenge | `401` + `WWW-Authenticate: Basic` | basic-auth prompt |
| pangolin unreachable | `503` | denied — auth outages **fail closed** |

Configuration (env, all prefixed `SHIM_`): `SHIM_PANGOLIN_API_BASE_URL`
(required — the `apiBaseUrl` pangolin hands to badger),
`SHIM_LISTEN` (`0.0.0.0:9001`), `SHIM_PATH_PREFIX` (`/verify`, must equal
`CONFIG_EXT_AUTHZ_PATH`), `SHIM_USER_SESSION_COOKIE_NAME` (`p_session_token`),
`SHIM_RESOURCE_SESSION_REQUEST_PARAM` (`p_session_request`),
`SHIM_PANGOLIN_TIMEOUT` (`10s`). A ready-to-adapt Deployment + Service lives
in [`deploy/badger-shim.yaml`](deploy/badger-shim.yaml).

To pass the verified identity on to backends, list the shim's response
headers in `CONFIG_EXT_AUTHZ_HEADERS_TO_BACKEND`
(e.g. `remote-user,remote-email`).

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
| `CONFIG_ENABLE_TCP_ROUTES`         | `false`                                     | Translate pangolin's `tcp` block into TCPRoutes + TCP listeners. See [Raw TCP/UDP resources](#raw-tcpudp-resources) |
| `CONFIG_ENABLE_UDP_ROUTES`         | `false`                                     | Translate pangolin's `udp` block into UDPRoutes + UDP listeners. See [Raw TCP/UDP resources](#raw-tcpudp-resources) |
| `CONFIG_TLS_SECRET_TEMPLATE`       | _(unset)_                                   | Template for cert secret name. `{hostname}` and `{hostname-dashed}` placeholders. When unset, no HTTPS listener is created. |
| `CONFIG_TLS_SECRET_NAMESPACE`      | controller namespace                        | Namespace where TLS Secrets live                                            |
| `CONFIG_BACKEND_KIND`              | `envoy-backend`                             | `envoy-backend` (default) or `service`. See [Backend strategy](#backend-strategy) |
| `CONFIG_EXT_AUTHZ_SERVICE`         | _(unset)_                                   | Service name of the ext-authz endpoint for badger-protected routes. See [Authentication](#authentication-badger-protected-resources) |
| `CONFIG_EXT_AUTHZ_NAMESPACE`       | controller namespace                        | Namespace of the ext-authz Service (cross-ns needs a ReferenceGrant)        |
| `CONFIG_EXT_AUTHZ_PORT`            | `80`                                        | Port of the ext-authz Service                                               |
| `CONFIG_EXT_AUTHZ_PATH`            | _(unset)_                                   | Optional path prefix for check requests                                     |
| `CONFIG_EXT_AUTHZ_HEADERS_TO_EXT_AUTH` | `cookie,authorization`                  | Client headers forwarded to the ext-authz service                           |
| `CONFIG_EXT_AUTHZ_HEADERS_TO_BACKEND` | _(empty)_                                | Auth-response headers copied onto the upstream request                      |
| `CONFIG_ALLOW_UNAUTHENTICATED_ROUTES` | `false`                                  | **Dangerous.** Emit badger-protected routes without auth                    |
| `CONFIG_HTTPROUTE_ANNOTATIONS`     | _(unset)_                                   | `k=v,k=v` annotations stamped on every HTTPRoute. Typical: `cert-manager.io/cluster-issuer=letsencrypt-prod` |
| `CONFIG_LISTENERSET_ANNOTATIONS`   | _(unset)_                                   | `k=v,k=v` annotations stamped on the ListenerSet                            |
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
