# Project guide for Claude

## What this is

A Kubernetes controller, in Rust + [kube-rs], that polls
[Pangolin](https://github.com/fosrl/pangolin)'s Traefik dynamic-config endpoint and
reconciles each entry into [Gateway API] objects (`HTTPRoute`, `ListenerSet`,
`Service`, `EndpointSlice`) sized for [Envoy Gateway].

It mirrors the upstream Go controller
[`fosrl/pangolin-kube-controller`](https://github.com/fosrl/pangolin-kube-controller)
at the architectural level — same polling model, same managed-label-based GC, same
SSA apply strategy — but emits Gateway API objects instead of Traefik IngressRoute
CRDs. Treat the Go repo (also vendored as a submodule of the parent repo) as the
reference implementation when behaviour is ambiguous.

## Submodules

The parent repo (`envisia/pangolin-gateway-api`) tracks two submodules:

- `pangolin/` → `github.com/fosrl/pangolin` — Traefik provider source.
- `pangolin-kube-controller/` → `github.com/fosrl/pangolin-kube-controller` — Go
  reference implementation. Useful for fixtures (`test/testdata/traefik-configs/`)
  and for understanding edge-case decisions.

This worktree's `.gitmodules` may be a step behind the parent — `pangolin-kube-controller`
is referenced from `main`. Read from the parent checkout at
`/Users/schmitch/projects/envisia/incubator/traefik-gateway-api-proxy/pangolin-kube-controller/`
if it isn't initialized here.

## Crate/version pins worth knowing

- `gateway-api = "0.21"` with the **`experimental` feature** — required for
  `ListenerSet`, which lives in `gateway_api::apis::experimental::listenersets`.
  Tracks Gateway API v1.5.1 CRDs.
- `kube = "3.1"` with `runtime, derive, client, rustls-tls, ring`.
- `k8s-openapi = "0.27"` pinned to feature `v1_32`.
- Rust edition **2024** (let-chains used in a few places).

### Naming quirks in the gateway-api crate

The crate's struct names are *not* uniformly cased to match the K8s `kind`:

| Conceptually          | Rust import                                           |
|-----------------------|-------------------------------------------------------|
| `HTTPRoute` (top CR)  | `gateway_api::apis::experimental::httproutes::HTTPRoute` |
| HTTPRoute spec fields | `HttpRouteSpec`, `HttpRouteRulesFilters`, …            |
| `ListenerSet` (top CR)| `gateway_api::apis::experimental::listenersets::ListenerSet` |
| ListenerSet listener  | `ListenerSetListeners`, `ListenerSetListenersTls`, …   |

Top-level CR types are derived by `kube::CustomResource` from the `kind = "..."`
attribute, hence the uppercase. Inner types come from `kopium` and use
PascalCase from the field name. Don't waste time reverse-engineering this — open
the file in `/tmp/gateway-api-rs/gateway-api/src/apis/experimental/` (or
`~/.cargo/registry/.../gateway-api-0.21.0/src/apis/experimental/`) and grep.

### `ListenerSet`, not `XListenerSet`

Gateway API v1.5 promoted ListenerSet out of `gateway.networking.x-k8s.io/v1alpha1`
into `gateway.networking.k8s.io/v1` under the experimental channel. We use the
new kind. Don't reintroduce the `X` prefix.

## Architecture cheat sheet

```
pangolin /api/v1/traefik-config
        │ poll + If-None-Match (ETag) + sha256 short-circuit
        ▼
TraefikDynamicConfig (src/pangolin/types.rs)
        │ transform pipeline
        ▼
Desired { http_routes, listener_sets, services, endpoint_slices }
        │ Server-Side Apply with field_manager = pangolin-gateway-controller
        ▼
Cluster state
        │ sweep: list by managed-by label, delete anything not in Desired
        ▼
GC complete
```

Each `Desired.*` map is keyed by the K8s object name. GC enumerates by label
selector and deletes names not present in the corresponding desired map.

## Module map

- `src/main.rs` — tracing init, signal handlers, kube client bootstrap.
- `src/config.rs` — every env var the controller reads. **All names start
  with `CONFIG_`** to mirror the Go controller. New options should follow that
  convention.
- `src/pangolin/` — HTTP client (ETag/conditional-GET) and Traefik dynamic-config
  serde types. TCP/UDP blocks are typed (`L4Config`); their fields are defaulted
  so malformed routers degrade to warn+skip instead of failing the whole parse.
- `src/transform/` — pure functions. **Adding behaviour usually means editing
  here.**
  - `rule.rs` — Traefik rule parser. Only `Host(...)`, `PathPrefix(...)`,
    `Path(...)`, `PathRegexp(...)` are usable. Anything else (`||`, `!`,
    `HostRegexp`, `Method`, `Headers`, `Query`) marks the router as unusable and
    it's logged + skipped — never silently misrouted.
  - `backend.rs` — four-way classifier, dispatched on `CONFIG_BACKEND_KIND`:
    1. IP literal → service mode synthesizes headless `Service` +
       `EndpointSlice`; envoy-backend mode emits an `gateway.envoyproxy.io`
       `Backend` with `endpoints[].ip`.
    2. `<svc>.<ns>.svc[.cluster.local]` → direct cross-ns `backendRef` in
       **both** modes. The real Service already exists; don't duplicate it.
    3. Bare FQDN (e.g. `api.example.com`) → service mode logs and skips
       (EndpointSlice can't carry hostnames); envoy-backend mode emits a
       `Backend` with `endpoints[].fqdn`.
    4. Anything else → logged and skipped.

    The dispatch is the only place that knows about `BackendKind` (default:
    `EnvoyBackend`). Don't leak that enum into `route.rs` — the route builder
    reads `ResolvedBackend { group, kind, name, namespace, port }` and is
    mode-agnostic. `build_l4_backends` is the L4 twin: same classifier fed from
    `address` (`host:port`, stray scheme tolerated) instead of `url`;
    synthesized names get a `-tcp-`/`-udp-` infix and the right port protocol.
  - `middleware.rs` — small allow-list mapping `redirectScheme`, `headers`,
    `addPrefix`, `replacePath`, `stripPrefix` to Gateway API filters.
    `replacePathRegex` is intentionally skipped. `requires_badger_auth` detects
    pangolin's `badger` auth plugin — it is never a filter; auth is decided
    per-route (see below).
  - `route.rs` — one `HTTPRoute` per pangolin router, parentRef = our
    `ListenerSet`. Badger-protected routers (pangolin attaches badger to every
    non-redirect router) are: wired to a `SecurityPolicy` when
    `CONFIG_EXT_AUTHZ_SERVICE` is set, emitted bare when
    `CONFIG_ALLOW_UNAUTHENTICATED_ROUTES=true`, and **skipped otherwise** —
    never silently exposed without auth.
  - `ext_authz.rs` — builds the Envoy Gateway `SecurityPolicy` (extAuth.http,
    `failOpen: false`) per protected route; the policy name derives from the
    route name so their GC stays in lockstep.
  - `l4.rs` — raw TCP/UDP resources → `TCPRoute`/`UDPRoute` plus `TCP`/`UDP`
    listeners (gated on `CONFIG_ENABLE_TCP_ROUTES`/`CONFIG_ENABLE_UDP_ROUTES`,
    both off by default). Port comes from the entrypoint name (`tcp-234`). Only
    `HostSNI(\`*\`)` TCP rules are usable; concrete SNI / `tls` options need
    TLSRoute and are logged + skipped. One listener per (protocol, port);
    duplicate claims and collisions with the HTTP/HTTPS ports are skipped.
  - `listener.rs` — one `ListenerSet` aggregating every host. Optional HTTPS
    listeners only when `CONFIG_TLS_SECRET_TEMPLATE` is set; appends whatever
    L4 listeners `l4.rs` produced.
  - `naming.rs` — DNS-1123 sanitization with a deterministic 32-bit hash
    suffix when truncating. **Don't rename objects without thinking about GC** —
    a name change means delete-then-create, not in-place update.
- `src/apply.rs` — `managed_metadata{,_with}` builders plus the SSA helper.
  Every applied object gets the managed/instance labels and managed annotation;
  HTTPRoute and ListenerSet additionally get any user-configured annotations.
- `src/envoy_gateway.rs` — hand-rolled bindings for Envoy Gateway's
  `Backend` and `SecurityPolicy` CRDs (`gateway.envoyproxy.io/v1alpha1`). The
  `gateway-api` crate doesn't ship these. Add new Envoy Gateway-specific CRDs
  here too.
- `src/gc.rs` — generic sweep over `Api<T>` by the managed-by selector.
- `src/reconcile.rs` — outer loop: fetch → if changed transform+apply+gc → wait.
  Apply order is `Service → EndpointSlice → Backend → ListenerSet → HTTPRoute →
  TCPRoute → UDPRoute → SecurityPolicy`. GC happens after every successful
  apply round. The `Backend` sweep is gated on `cfg.backend_kind ==
  EnvoyBackend`, the TCPRoute/UDPRoute sweeps on their enable flags, and the
  SecurityPolicy sweep on `cfg.ext_authz.is_some()` — the CRDs may not even be
  installed otherwise, so we don't list them.

## Configurable annotation hook

`CONFIG_HTTPROUTE_ANNOTATIONS` and `CONFIG_LISTENERSET_ANNOTATIONS` accept
`key=value,key=value` and are stamped onto every HTTPRoute/ListenerSet the
controller creates. Primary intended use: cert-manager (
`cert-manager.io/cluster-issuer=letsencrypt-prod`). **`Service` and
`EndpointSlice` are deliberately excluded** so cert-manager doesn't try to mint
certs for backend stubs.

## Testing

- `cargo test` runs everything; integration coverage lives inline in
  `src/transform/mod.rs::e2e_tests` and loads JSON fixtures from
  `tests/fixtures/`. The fixtures are real pangolin responses lifted verbatim
  from `pangolin-kube-controller/test/testdata/traefik-configs/v3.5.0/`. If
  upstream updates those, refresh ours and keep the filenames.
- There is no Kubernetes integration test yet. Adding one means either spinning
  up envtest-like fake APIserver bindings (not currently in kube-rs) or shelling
  out to `kind`. Don't block PRs on this absent a real need.

## Build & run

```sh
cargo test
cargo build --release
docker build -t pangolin-gateway-controller:dev .
kubectl apply -k deploy/
```

The deployment manifest assumes the controller writes into
`pangolin-system` and attaches to a Gateway named `eg` in
`envoy-gateway-system`. Change via `CONFIG_*` env vars, not by editing the
controller code.

## Design rules

- **Never silently downgrade.** Anything we can't translate (regex rewrites,
  unsupported predicates, exotic backends) is `tracing::warn!`ed and dropped at
  the affected router/service level. Don't pretend partial config worked.
- **Idempotent applies.** Every write goes through SSA with
  `field_manager = pangolin-gateway-controller` (configurable via
  `CONFIG_FIELD_MANAGER`). Never use update-with-resourceVersion.
- **GC by label, not owner reference.** Pangolin objects are spread across
  kinds; we don't have a single root to attach `ownerReferences` to. The
  managed-by + instance labels are the contract — preserve them.
- **No state on disk.** The controller restarts must be cheap. Anything that
  needs to survive (ETag, last digest) is a single in-memory string; if you
  need more, ask first.

[kube-rs]: https://github.com/kube-rs/kube
[Gateway API]: https://gateway-api.sigs.k8s.io/
[Envoy Gateway]: https://gateway.envoyproxy.io/
