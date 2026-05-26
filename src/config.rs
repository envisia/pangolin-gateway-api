#![allow(dead_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub pangolin_endpoint: Url,
    pub auth_header: Option<String>,
    pub fetch_timeout: Duration,
    pub poll_interval: Duration,
    pub max_backoff: Duration,
    pub max_response_body_bytes: u64,
    pub tls_skip_verify: bool,
    pub ca_file: Option<String>,
    pub allow_insecure_http: bool,

    pub namespace: String,
    pub parent_gateway: String,
    pub parent_gateway_namespace: Option<String>,
    pub listener_set_name: String,
    pub gateway_class: Option<String>,

    pub http_port: i32,
    pub https_port: i32,
    pub enable_https_listeners: bool,

    /// Which Kubernetes object kind backs an HTTPRoute's IP/FQDN targets.
    pub backend_kind: BackendKind,

    /// Optional template for the TLS secret name per hostname. Supports the placeholders
    /// `{hostname}` (dots kept) and `{hostname-dashed}` (dots → dashes).
    /// When `None`, listeners are plain HTTP only.
    pub tls_secret_template: Option<String>,
    /// Optional namespace where TLS secrets live; defaults to the controller namespace.
    pub tls_secret_namespace: Option<String>,

    pub field_manager: String,
    pub managed_label_key: String,
    pub managed_label_value: String,
    pub instance_label_key: String,
    pub instance_label_value: String,
    pub managed_annotation_key: String,
    pub managed_annotation_value: String,

    /// Annotations stamped onto every HTTPRoute the controller creates. Typical use:
    /// `cert-manager.io/cluster-issuer=letsencrypt-prod` so cert-manager can mint
    /// a Secret per hostname.
    pub httproute_annotations: BTreeMap<String, String>,
    /// Annotations stamped onto the ListenerSet. Same intent as above for
    /// implementations that watch the ListenerSet directly.
    pub listenerset_annotations: BTreeMap<String, String>,

    pub read_only: bool,
    pub log_traefik_config: bool,
}

/// Backend object kind used for IP / FQDN pangolin targets.
///
/// Cluster-DNS pass-through (`<svc>.<ns>.svc[.cluster.local]`) is independent of
/// this setting — it always resolves to a direct `Service` `backendRef` because
/// the Service already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Synthesize a headless `Service` plus an `EndpointSlice` (IPv4 only).
    /// Portable across every Gateway API implementation.
    Service,
    /// Emit a `gateway.envoyproxy.io/v1alpha1` `Backend` CRD. Envoy Gateway only,
    /// but unlocks FQDN targets as well as IPs.
    EnvoyBackend,
}

impl BackendKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "service" | "service-endpointslice" | "" => Ok(BackendKind::Service),
            "envoy-backend" | "envoybackend" | "backend" => Ok(BackendKind::EnvoyBackend),
            other => bail!(
                "invalid CONFIG_BACKEND_KIND {other:?}; expected `service` or `envoy-backend`"
            ),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let endpoint_raw = required_env("CONFIG_ENDPOINT")?;
        let pangolin_endpoint =
            Url::parse(&endpoint_raw).context("CONFIG_ENDPOINT must be a valid URL")?;

        let tls_skip_verify = bool_env("CONFIG_TLS_SKIP_VERIFY", false)?;
        if tls_skip_verify && !bool_env("I_UNDERSTAND_CONFIG_TLS_SKIP_VERIFY_IS_INSECURE", false)? {
            bail!(
                "CONFIG_TLS_SKIP_VERIFY=true requires \
                 I_UNDERSTAND_CONFIG_TLS_SKIP_VERIFY_IS_INSECURE=true to be explicitly set"
            );
        }
        let allow_insecure_http = bool_env("CONFIG_ALLOW_INSECURE_HTTP", false)?;
        if pangolin_endpoint.scheme() == "http" && !allow_insecure_http {
            bail!(
                "CONFIG_ENDPOINT uses http://. Set CONFIG_ALLOW_INSECURE_HTTP=true to permit it."
            );
        }

        Ok(Self {
            pangolin_endpoint,
            auth_header: optional_env("CONFIG_AUTH_HEADER"),
            fetch_timeout: duration_env("CONFIG_FETCH_TIMEOUT", Duration::from_secs(30))?,
            poll_interval: duration_env("CONFIG_POLL_INTERVAL", Duration::from_secs(30))?,
            max_backoff: duration_env("CONFIG_MAX_BACKOFF", Duration::from_secs(300))?,
            max_response_body_bytes: u64_env("CONFIG_MAX_RESPONSE_BODY_BYTES", 16 * 1024 * 1024)?,
            tls_skip_verify,
            ca_file: optional_env("CONFIG_CA_FILE"),
            allow_insecure_http,

            namespace: optional_env("CONFIG_NAMESPACE").unwrap_or_else(|| "default".into()),
            parent_gateway: required_env("CONFIG_PARENT_GATEWAY")?,
            parent_gateway_namespace: optional_env("CONFIG_PARENT_GATEWAY_NAMESPACE"),
            listener_set_name: optional_env("CONFIG_LISTENERSET_NAME")
                .unwrap_or_else(|| "pangolin".into()),
            gateway_class: optional_env("CONFIG_GATEWAY_CLASS"),

            http_port: i32_env("CONFIG_HTTP_PORT", 80)?,
            https_port: i32_env("CONFIG_HTTPS_PORT", 443)?,
            enable_https_listeners: bool_env("CONFIG_ENABLE_HTTPS_LISTENERS", true)?,
            backend_kind: match optional_env("CONFIG_BACKEND_KIND") {
                Some(raw) => BackendKind::parse(&raw)?,
                None => BackendKind::Service,
            },
            tls_secret_template: optional_env("CONFIG_TLS_SECRET_TEMPLATE"),
            tls_secret_namespace: optional_env("CONFIG_TLS_SECRET_NAMESPACE"),

            field_manager: optional_env("CONFIG_FIELD_MANAGER")
                .unwrap_or_else(|| "pangolin-gateway-controller".into()),
            managed_label_key: optional_env("CONFIG_MANAGED_LABEL_KEY")
                .unwrap_or_else(|| "app.kubernetes.io/managed-by".into()),
            managed_label_value: optional_env("CONFIG_MANAGED_LABEL_VALUE")
                .unwrap_or_else(|| "pangolin-gateway-controller".into()),
            instance_label_key: optional_env("CONFIG_INSTANCE_LABEL_KEY")
                .unwrap_or_else(|| "pangolin.envisia.de/instance".into()),
            instance_label_value: optional_env("CONFIG_INSTANCE_LABEL_VALUE")
                .unwrap_or_else(|| "default".into()),
            managed_annotation_key: optional_env("CONFIG_MANAGED_ANNOTATION_KEY")
                .unwrap_or_else(|| "pangolin.envisia.de/source".into()),
            managed_annotation_value: optional_env("CONFIG_MANAGED_ANNOTATION_VALUE")
                .unwrap_or_else(|| "pangolin-gateway-controller".into()),

            httproute_annotations: parse_kv_env("CONFIG_HTTPROUTE_ANNOTATIONS")?,
            listenerset_annotations: parse_kv_env("CONFIG_LISTENERSET_ANNOTATIONS")?,

            read_only: bool_env("CONFIG_READ_ONLY", false)?,
            log_traefik_config: bool_env("CONFIG_LOG_TRAEFIK_CONFIG", false)?,
        })
    }

    /// Selector used by GC to enumerate every object the controller has created.
    pub fn managed_selector(&self) -> String {
        format!(
            "{}={},{}={}",
            self.managed_label_key,
            self.managed_label_value,
            self.instance_label_key,
            self.instance_label_value,
        )
    }
}

fn required_env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("required environment variable {key} is not set"))
}

fn optional_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn bool_env(key: &str, default: bool) -> Result<bool> {
    match optional_env(key) {
        None => Ok(default),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => bail!("invalid boolean for {key}: {other:?}"),
        },
    }
}

fn duration_env(key: &str, default: Duration) -> Result<Duration> {
    match optional_env(key) {
        None => Ok(default),
        Some(v) => humantime::parse_duration(&v)
            .with_context(|| format!("invalid duration for {key}: {v:?}")),
    }
}

fn u64_env(key: &str, default: u64) -> Result<u64> {
    match optional_env(key) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .with_context(|| format!("invalid u64 for {key}: {v:?}")),
    }
}

fn i32_env(key: &str, default: i32) -> Result<i32> {
    match optional_env(key) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .with_context(|| format!("invalid i32 for {key}: {v:?}")),
    }
}

/// Parse a `key=value,key=value,...` list. Empty entries are skipped.
fn parse_kv_env(key: &str) -> Result<BTreeMap<String, String>> {
    let Some(raw) = optional_env(key) else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (k, v) = entry
            .split_once('=')
            .with_context(|| format!("{key}: entry {entry:?} is not `key=value`"))?;
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() {
            bail!("{key}: empty annotation key in entry {entry:?}");
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_handles_empty_and_spaces() {
        // SAFETY: tests run single-threaded for env mutation here.
        unsafe {
            std::env::set_var(
                "TEST_KV_ANNOS",
                " cert-manager.io/cluster-issuer = letsencrypt-prod , foo=bar ",
            );
        }
        let parsed = parse_kv_env("TEST_KV_ANNOS").unwrap();
        unsafe { std::env::remove_var("TEST_KV_ANNOS") };
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get("cert-manager.io/cluster-issuer"),
            Some(&"letsencrypt-prod".to_string())
        );
        assert_eq!(parsed.get("foo"), Some(&"bar".to_string()));
    }

    #[test]
    fn parse_kv_rejects_missing_equals() {
        unsafe { std::env::set_var("TEST_KV_BAD", "this-has-no-equals") };
        let err = parse_kv_env("TEST_KV_BAD").unwrap_err();
        unsafe { std::env::remove_var("TEST_KV_BAD") };
        assert!(err.to_string().contains("not `key=value`"));
    }
}
