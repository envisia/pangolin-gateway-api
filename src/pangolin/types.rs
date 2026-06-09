//! Strongly-typed view of the subset of Traefik's dynamic configuration that pangolin emits.
//!
//! Pangolin builds this structure in
//! `server/lib/traefik/getTraefikConfig.ts` (see the upstream pangolin repo). The HTTP fields are
//! exhaustive for what pangolin produces; we keep TCP/UDP as opaque JSON for now since this
//! controller targets Envoy Gateway's L7 surface only.
//!
//! Some fields are deserialized but not yet consumed — they're kept so the schema is
//! self-documenting and so we don't silently drop information when downstream features land.
//! Because this crate is a library, the `pub` fields are part of its public API and
//! the dead-code lint doesn't fire for them.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Default, Clone, Deserialize)]
pub struct TraefikDynamicConfig {
    #[serde(default)]
    pub http: HttpConfig,
    /// Raw TCP resources. Translated to Gateway API `TCPRoute` when
    /// `CONFIG_ENABLE_TCP_ROUTES=true`.
    #[serde(default)]
    pub tcp: Option<L4Config>,
    /// Raw UDP resources. Translated to Gateway API `UDPRoute` when
    /// `CONFIG_ENABLE_UDP_ROUTES=true`.
    #[serde(default)]
    pub udp: Option<L4Config>,
}

/// Shared shape of Traefik's `tcp` and `udp` dynamic-config blocks as pangolin
/// emits them for "raw" resources (`server/lib/traefik/getTraefikConfig.ts`).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct L4Config {
    #[serde(default)]
    pub routers: BTreeMap<String, L4Router>,
    #[serde(default)]
    pub services: BTreeMap<String, L4Service>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L4Router {
    /// TCP routers carry `HostSNI(`*`)`; UDP routers have no rule at all.
    #[serde(default)]
    pub rule: Option<String>,
    /// Defaulted (not required) so a malformed router degrades to a warn+skip
    /// instead of failing deserialization of the whole config.
    #[serde(default)]
    pub service: String,
    /// Pangolin encodes the public port in the entrypoint name: `tcp-<port>` / `udp-<port>`.
    #[serde(default)]
    pub entry_points: Vec<String>,
    /// TLS passthrough options. Presence means the router needs TLSRoute
    /// semantics, which we don't support yet — kept opaque.
    #[serde(default)]
    pub tls: Option<Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct L4Service {
    #[serde(default, rename = "loadBalancer")]
    pub load_balancer: Option<L4LoadBalancer>,
    /// Unhandled variants, kept so we can warn instead of silently dropping.
    #[serde(default)]
    pub weighted: Option<Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct L4LoadBalancer {
    #[serde(default)]
    pub servers: Vec<L4Server>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct L4Server {
    /// Traefik L4 servers use `address` (`host:port`), not the HTTP `url` field.
    /// Pangolin has been seen emitting a stray scheme prefix here — strip it
    /// before parsing.
    #[serde(default)]
    pub address: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default)]
    pub routers: BTreeMap<String, Router>,
    #[serde(default)]
    pub services: BTreeMap<String, Service>,
    #[serde(default)]
    pub middlewares: BTreeMap<String, Middleware>,
    #[serde(default, rename = "serversTransports")]
    pub servers_transports: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Router {
    pub rule: String,
    pub service: String,
    #[serde(default)]
    pub middlewares: Vec<String>,
    #[serde(default)]
    pub entry_points: Vec<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub tls: Option<RouterTls>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterTls {
    #[serde(default)]
    pub cert_resolver: Option<String>,
    #[serde(default)]
    pub domains: Vec<TlsDomain>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct TlsDomain {
    #[serde(default)]
    pub main: Option<String>,
    #[serde(default)]
    pub sans: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Service {
    #[serde(default, rename = "loadBalancer")]
    pub load_balancer: Option<LoadBalancer>,
    /// Pangolin may also use `weighted` / `mirroring` services; we capture
    /// them as opaque JSON to avoid silent drops. Currently unhandled.
    #[serde(default)]
    pub weighted: Option<Value>,
    #[serde(default)]
    pub mirroring: Option<Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancer {
    #[serde(default)]
    pub servers: Vec<LoadBalancerServer>,
    #[serde(default)]
    pub sticky: Option<Value>,
    #[serde(default)]
    pub servers_transport: Option<String>,
    #[serde(default)]
    pub pass_host_header: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct LoadBalancerServer {
    /// Pangolin emits servers as `{ url: "http://10.0.0.5:8080" }`.
    pub url: String,
}

/// Each middleware is one of many disjoint variants in Traefik. We keep the
/// untyped JSON body so the transform layer can switch on whichever key is present.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct Middleware(pub Value);

impl Middleware {
    pub fn as_object(&self) -> Option<&serde_json::Map<String, Value>> {
        self.0.as_object()
    }
}
