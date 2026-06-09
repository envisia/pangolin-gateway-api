# Deploying the pangolin gateway controller

This directory contains a [Kustomize](https://kustomize.io/) overlay that installs
the controller into a cluster. It is the same set of manifests referenced by the
top-level `README.md` and is consumed both directly (`kubectl apply -k deploy/`)
and as a component from sibling infrastructure repos.

## What gets installed

| File | Resource | Notes |
|---|---|---|
| `namespace.yaml` | `Namespace/pangolin-system` | Drop this if the namespace is created elsewhere — see [Embedding from another kustomization](#embedding-from-another-kustomization). |
| `badger-shim.yaml` | `Deployment` + `Service` for the badger ext-authz shim | **Not in `kustomization.yaml`** — opt in after setting `SHIM_PANGOLIN_API_BASE_URL`. Required for badger-protected routes unless you set `CONFIG_ALLOW_UNAUTHENTICATED_ROUTES=true`. |
| `rbac.yaml` | `ServiceAccount`, `ClusterRole`, `ClusterRoleBinding` | Cluster-wide read on Gateways; write on HTTPRoute / TCPRoute / UDPRoute / ListenerSet / Service / EndpointSlice / Envoy Gateway `Backend`. |
| `deployment.yaml` | `Deployment/pangolin-gateway-controller` | Single replica, read-only rootfs, all caps dropped. |

The CRDs are **not** installed here. You need:

- Gateway API v1.5+ **experimental** channel (for `ListenerSet`, and for
  `TCPRoute`/`UDPRoute` when the L4 flags are on):
  `kubectl apply -k https://github.com/kubernetes-sigs/gateway-api/config/crd/experimental?ref=v1.5.1`
- Envoy Gateway (required for the default `CONFIG_BACKEND_KIND=envoy-backend`,
  and the implementation this controller targets):
  see <https://gateway.envoyproxy.io/docs/install/>. For
  `CONFIG_ENABLE_TCP_ROUTES`/`CONFIG_ENABLE_UDP_ROUTES`, use an Envoy Gateway
  release that reconciles the graduated `ListenerSet` kind
  (`gateway.networking.k8s.io`, not the legacy `XListenerSet`).

A `Gateway` object the controller can attach its `ListenerSet` to must exist
before the controller will produce useful output. The default config points at a
`Gateway` named `eg` in `envoy-gateway-system`.

## Prerequisites

- A reachable pangolin instance exposing `/api/v1/traefik-config`. By default
  the deployment points at `http://pangolin.pangolin-system.svc.cluster.local:8081/api/v1/traefik-config`;
  change `CONFIG_ENDPOINT` if your service name, namespace, or port differ.
- A parent `Gateway` whose listeners the controller should extend via
  `ListenerSet`. Default: `eg` in `envoy-gateway-system`.

## Install

```sh
# CRDs first (one-time per cluster)
kubectl apply -k https://github.com/kubernetes-sigs/gateway-api/config/crd/experimental?ref=v1.5.1

# Controller
kubectl apply -k deploy/
```

Verify:

```sh
kubectl -n pangolin-system rollout status deploy/pangolin-gateway-controller
kubectl -n pangolin-system logs deploy/pangolin-gateway-controller -f
```

After a successful poll the controller logs the digest of the dynamic config it
received and lists the routers it materialised. Unsupported routers are logged
at `WARN` level and skipped — they are never silently misrouted.

## Configuration

Every knob is an environment variable named `CONFIG_*` and consumed by
[`src/config.rs`](../src/config.rs). The list below is the current set; check
that file for authoritative defaults.

| Env var | Default | Purpose |
|---|---|---|
| `CONFIG_ENDPOINT` | _(required)_ | URL of pangolin's `traefik-config` endpoint. |
| `CONFIG_NAMESPACE` | _(required)_ | Namespace the controller writes Gateway API + Service + EndpointSlice objects into. |
| `CONFIG_PARENT_GATEWAY` | _(required)_ | Name of the existing `Gateway` the `ListenerSet` attaches to. |
| `CONFIG_PARENT_GATEWAY_NAMESPACE` | same as `CONFIG_NAMESPACE` | Namespace of the parent `Gateway`. |
| `CONFIG_LISTENERSET_NAME` | `pangolin` | Name of the `ListenerSet` the controller manages. |
| `CONFIG_POLL_INTERVAL` | `30s` | How often to poll pangolin. ETag + sha256 short-circuit keep idle polls cheap. |
| `CONFIG_BACKEND_KIND` | `envoy-backend` | `envoy-backend` (emit Envoy Gateway `Backend` CRD) or `service` (synthesize headless `Service` + `EndpointSlice`; portable, no FQDN support). |
| `CONFIG_ENABLE_HTTPS_LISTENERS` | `false` | Whether to add HTTPS listeners to the `ListenerSet`. |
| `CONFIG_ENABLE_TCP_ROUTES` | `false` | Translate pangolin's raw TCP resources into `TCPRoute`s + `TCP` listeners. Each `tcp-<port>` entrypoint becomes a port on the Envoy LoadBalancer Service. |
| `CONFIG_ENABLE_UDP_ROUTES` | `false` | Same for raw UDP resources / `UDPRoute`. The cloud LB must support mixed TCP+UDP Services; if it doesn't, leave this off. |
| `CONFIG_EXT_AUTHZ_SERVICE` | _(unset)_ | Service name of an Envoy ext-authz endpoint verifying pangolin sessions — ship the bundled shim via `badger-shim.yaml` and set this to `pangolin-badger-shim` (port 9001, path `/verify`). When set, badger-protected routes get a `SecurityPolicy`; when unset they are **skipped**. |
| `CONFIG_ALLOW_UNAUTHENTICATED_ROUTES` | `false` | **Dangerous.** Emit badger-protected routes without authentication. |
| `CONFIG_TLS_SECRET_TEMPLATE` | _(unset)_ | Required when HTTPS listeners are on. Supports `{hostname}` and `{hostname-dashed}` placeholders. |
| `CONFIG_HTTPROUTE_ANNOTATIONS` | _(empty)_ | `k=v,k=v` annotations stamped onto every `HTTPRoute`. Typical use: cert-manager cluster-issuer. |
| `CONFIG_LISTENERSET_ANNOTATIONS` | _(empty)_ | Same for `ListenerSet`. |
| `CONFIG_FIELD_MANAGER` | `pangolin-gateway-controller` | SSA field manager. Change only if you need to migrate ownership. |
| `CONFIG_HEALTH_LISTEN` | `0.0.0.0:8081` | Liveness (`/healthz`) + readiness (`/readyz`, ready after the first successful poll cycle). The shipped Deployment wires both probes. Set `off` to disable. |
| `RUST_LOG` | `info` | Standard `tracing_subscriber::EnvFilter`. |

`Service` and `EndpointSlice` deliberately **do not** receive
`CONFIG_*_ANNOTATIONS` so cert-manager won't try to mint certs for backend stubs.

## cert-manager (optional)

If you run cert-manager with the
[gateway-shim](https://cert-manager.io/docs/usage/gateway/) enabled, set both
annotation env vars to your cluster issuer:

```yaml
- name: CONFIG_HTTPROUTE_ANNOTATIONS
  value: "cert-manager.io/cluster-issuer=letsencrypt-prod"
- name: CONFIG_LISTENERSET_ANNOTATIONS
  value: "cert-manager.io/cluster-issuer=letsencrypt-prod"
- name: CONFIG_ENABLE_HTTPS_LISTENERS
  value: "true"
- name: CONFIG_TLS_SECRET_TEMPLATE
  value: "{hostname-dashed}-tls"
```

cert-manager will see the annotated `ListenerSet`, mint a Certificate per
listener hostname, and write the Secret matching `CONFIG_TLS_SECRET_TEMPLATE`.

## Embedding from another kustomization

The deploy folder is a working stand-alone overlay. To embed it inside an
infrastructure repo that already owns the namespace:

```yaml
# components/pangolin-gateway-controller/kustomization.yaml
apiVersion: kustomize.config.k8s.io/v1alpha1
kind: Component

resources:
  - https://github.com/envisia/pangolin-gateway-api.git//deploy?ref=main

patches:
  # The parent overlay owns the namespace — drop the one shipped here.
  - target: { kind: Namespace, name: pangolin-system }
    patch: |-
      $patch: delete
      apiVersion: v1
      kind: Namespace
      metadata:
        name: pangolin-system
```

Then set `namespace:` in the consuming overlay to rewrite the ServiceAccount and
Deployment namespaces (and the ClusterRoleBinding's `subjects[].namespace`).

## Cross-namespace backends need a ReferenceGrant

This is **not** an RBAC matter, and the controller deliberately does not create
these for you. When a pangolin target resolves to a Service in *another*
namespace (`<svc>.<ns>.svc.cluster.local` pass-through), Gateway API requires a
`ReferenceGrant` in the **target** namespace before Envoy Gateway will resolve
the `backendRef`. The grant is a consent object owned by the target namespace —
auto-creating it from the controller would defeat its purpose. One grant per
backend namespace:

```yaml
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata:
  name: allow-pangolin-routes
  namespace: dummyservices        # the namespace the backend Service lives in
spec:
  from:
    - group: gateway.networking.k8s.io
      kind: HTTPRoute
      namespace: pangolin-system  # where the controller writes its routes
    - group: gateway.networking.k8s.io
      kind: TCPRoute
      namespace: pangolin-system
    - group: gateway.networking.k8s.io
      kind: UDPRoute
      namespace: pangolin-system
  to:
    - group: ""
      kind: Service
```

Routes whose backendRef is rejected for a missing grant show it in their
`status.parents[].conditions` (`ResolvedRefs: RefNotPermitted`). The same
applies to `CONFIG_EXT_AUTHZ_NAMESPACE` if the ext-authz Service lives outside
the controller namespace.

## GC behaviour worth knowing

The controller owns objects by the `app.kubernetes.io/managed-by` /
`app.kubernetes.io/instance` label pair, not by `ownerReferences`. After every
successful reconcile it lists each managed kind by label and deletes anything
not in the desired set. Implications:

- Renaming `CONFIG_LISTENERSET_NAME`, the parent gateway, or any pangolin
  resource that affects sanitised object names = delete-then-create, not in-place
  rename. Expect a brief gap.
- Manual edits to controller-managed objects survive an apply round (SSA only
  reconciles fields the controller owns) but will not survive a label change.
- If you uninstall the controller without deleting its objects, the Gateway API
  state freezes — nothing will GC stale `HTTPRoute`s until the controller comes
  back.
