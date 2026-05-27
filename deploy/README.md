# Deploying the pangolin gateway controller

This directory contains a [Kustomize](https://kustomize.io/) overlay that installs
the controller into a cluster. It is the same set of manifests referenced by the
top-level `README.md` and is consumed both directly (`kubectl apply -k deploy/`)
and as a component from sibling infrastructure repos.

## What gets installed

| File | Resource | Notes |
|---|---|---|
| `namespace.yaml` | `Namespace/pangolin-system` | Drop this if the namespace is created elsewhere — see [Embedding from another kustomization](#embedding-from-another-kustomization). |
| `rbac.yaml` | `ServiceAccount`, `ClusterRole`, `ClusterRoleBinding` | Cluster-wide read on Gateways; write on HTTPRoute / UDPRoute / ListenerSet / Service / EndpointSlice / Envoy Gateway `Backend` and `SecurityPolicy`. |
| `deployment.yaml` | `Deployment/pangolin-gateway-controller` | Single replica, read-only rootfs, all caps dropped. |

The CRDs are **not** installed here. You need:

- Gateway API v1.5+ **experimental** channel (for `ListenerSet`):
  `kubectl apply -k https://github.com/kubernetes-sigs/gateway-api/config/crd/experimental?ref=v1.5.1`
- Envoy Gateway, if you set `CONFIG_BACKEND_KIND=envoy-backend` or
  `CONFIG_BADGER_EXT_AUTH=true`:
  see <https://gateway.envoyproxy.io/docs/install/>.

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
| `CONFIG_BACKEND_KIND` | `service` | `service` (synthesize headless `Service` + `EndpointSlice`) or `envoy-backend` (emit Envoy Gateway `Backend` CRD). |
| `CONFIG_ENABLE_HTTPS_LISTENERS` | `false` | Whether to add HTTPS listeners to the `ListenerSet`. |
| `CONFIG_TLS_SECRET_TEMPLATE` | _(unset)_ | Required when HTTPS listeners are on. Supports `{hostname}` and `{hostname-dashed}` placeholders. |
| `CONFIG_HTTPROUTE_ANNOTATIONS` | _(empty)_ | `k=v,k=v` annotations stamped onto every `HTTPRoute`. Typical use: cert-manager cluster-issuer. |
| `CONFIG_LISTENERSET_ANNOTATIONS` | _(empty)_ | Same for `ListenerSet`. |
| `CONFIG_BADGER_EXT_AUTH` | `false` | Emit Envoy Gateway `SecurityPolicy.extAuth` for routes that use Pangolin's badger plugin. Requires an Envoy ext_auth-compatible shim service. |
| `CONFIG_BADGER_EXT_AUTH_BACKEND_NAME` | `pangolin-badger-ext-authz` | Auth shim Service name. |
| `CONFIG_BADGER_EXT_AUTH_BACKEND_PORT` | `9002` | Auth shim Service port. |
| `CONFIG_RECONCILE_ONLY` | _(empty)_ | Optional comma-separated allow-list of hostnames or object selectors such as `9-chris.example.com` or `HTTPRoute/hr-9-chris-connect-router`. |
| `CONFIG_PANGOLIN_DASHBOARD_HOST` | _(empty)_ | Emit static dashboard/API/WebSocket HTTPRoutes for this host. |
| `CONFIG_GERBIL_UDP_ROUTE` | `false` | Emit Gerbil UDP listeners and UDPRoutes; requires UDPRoute support in your Gateway implementation. |
| `CONFIG_GERBIL_UDP_PORTS` | `51820,21820` | UDP ports to route to the Gerbil Service. |
| `CONFIG_FIELD_MANAGER` | `pangolin-gateway-controller` | SSA field manager. Change only if you need to migrate ownership. |
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

## Badger ext_authz (optional)

The controller can close the badger gap from the Traefik plugin side by emitting
Envoy Gateway `SecurityPolicy` objects, but it does not make Pangolin's badger
endpoint speak Envoy's auth protocol by itself. Run a shim Service that accepts
Envoy HTTP ext_auth checks and calls
`http://pangolin:3001/api/v1/badger/verify-session`, then enable:

```yaml
- name: CONFIG_BADGER_EXT_AUTH
  value: "true"
- name: CONFIG_BADGER_EXT_AUTH_BACKEND_NAME
  value: "pangolin-badger-ext-authz"
- name: CONFIG_BADGER_EXT_AUTH_BACKEND_PORT
  value: "9002"
```

During migration, combine this with `CONFIG_RECONCILE_ONLY` to apply a single
hostname first. Hostname scopes expand to the matching `HTTPRoute`, badger
`SecurityPolicy`, generated backend objects, and the shared `ListenerSet`.

## Configuration changes for migration

When enabling the new Gateway API path in an existing dual-stack deployment,
change configuration in this order:

1. Keep Traefik serving production traffic.
2. Add or deploy a Service that fronts your badger ext_auth shim. The Service
   name and port must match `CONFIG_BADGER_EXT_AUTH_BACKEND_NAME` and
   `CONFIG_BADGER_EXT_AUTH_BACKEND_PORT`.
3. Make sure the Pangolin Service exposes the API/WebSocket port (`3000` by
   default) and the Next.js port (`3002` by default) if you want
   `CONFIG_PANGOLIN_DASHBOARD_HOST`.
4. Make sure the Gerbil Service exposes UDP `51820` and `21820`, or override
   `CONFIG_GERBIL_UDP_PORTS`.
5. Patch the controller Deployment env vars.

Example kustomize patch:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pangolin-gateway-controller
  namespace: pangolin-system
spec:
  template:
    spec:
      containers:
        - name: controller
          env:
            - name: CONFIG_BADGER_EXT_AUTH
              value: "true"
            - name: CONFIG_BADGER_EXT_AUTH_BACKEND_NAME
              value: "pangolin-badger-ext-authz"
            - name: CONFIG_BADGER_EXT_AUTH_BACKEND_PORT
              value: "9002"
            - name: CONFIG_RECONCILE_ONLY
              value: "9-chris.example.com"
            - name: CONFIG_PANGOLIN_DASHBOARD_HOST
              value: "pangolin.example.com"
            - name: CONFIG_GERBIL_UDP_ROUTE
              value: "true"
```

For the first test, keep `CONFIG_RECONCILE_ONLY` narrow. Remove it only after the
selected hostname's `HTTPRoute` and `SecurityPolicy` are accepted by Envoy
Gateway and the auth shim returns the expected allow/redirect/deny responses.
For Gerbil's old-IP plus Envoy-IP migration pattern, see
[`docs/gerbil-gateway-migration.md`](../docs/gerbil-gateway-migration.md).
For a shared public IP where only TCP `80`/`443` moves from Traefik to Envoy,
see
[`docs/shared-ip-http-envoy-migration.md`](../docs/shared-ip-http-envoy-migration.md).

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
