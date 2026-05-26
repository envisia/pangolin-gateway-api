use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, header};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::config::Config;
use crate::pangolin::types::TraefikDynamicConfig;

/// Result of polling pangolin once.
pub enum FetchOutcome {
    /// Pangolin returned `304 Not Modified` — caller should skip reconciliation.
    NotModified,
    /// New configuration. `digest` is a sha256 over the raw response body so callers can
    /// short-circuit when the ETag is absent but the body is unchanged.
    Changed(Box<ChangedConfig>),
}

pub struct ChangedConfig {
    pub config: TraefikDynamicConfig,
    pub etag: Option<String>,
    pub digest: String,
    pub raw_bytes: usize,
}

pub struct Client {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    auth_header: Option<String>,
    max_body_bytes: u64,
    log_body: bool,
}

impl Client {
    pub fn new(cfg: &Config) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(cfg.fetch_timeout)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(format!(
                "pangolin-gateway-controller/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .pool_idle_timeout(Some(Duration::from_secs(90)));

        if cfg.tls_skip_verify {
            warn!("CONFIG_TLS_SKIP_VERIFY is enabled — pangolin TLS certificate is NOT verified");
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(path) = &cfg.ca_file {
            let pem =
                std::fs::read(path).with_context(|| format!("reading CONFIG_CA_FILE {path:?}"))?;
            let cert =
                reqwest::Certificate::from_pem(&pem).context("parsing CONFIG_CA_FILE as PEM")?;
            builder = builder.add_root_certificate(cert);
        }

        let http = builder.build().context("building HTTP client")?;
        // Reuse reqwest::Url to avoid mismatched url types.
        let endpoint = reqwest::Url::parse(cfg.pangolin_endpoint.as_str())
            .context("re-parsing pangolin endpoint")?;

        Ok(Self {
            http,
            endpoint,
            auth_header: cfg.auth_header.clone(),
            max_body_bytes: cfg.max_response_body_bytes,
            log_body: cfg.log_traefik_config,
        })
    }

    /// Issue a single conditional GET against pangolin's traefik-config provider.
    pub async fn fetch(&self, last_etag: Option<&str>) -> Result<FetchOutcome> {
        let mut req = self.http.get(self.endpoint.clone());
        req = req.header(header::ACCEPT, "application/json");
        if let Some(token) = &self.auth_header {
            req = req.header(header::AUTHORIZATION, token);
        }
        if let Some(tag) = last_etag {
            req = req.header(header::IF_NONE_MATCH, tag);
        }

        let resp = req.send().await.context("calling pangolin endpoint")?;
        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(FetchOutcome::NotModified);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("pangolin returned {status}: {}", truncate(&body, 512));
        }

        let etag = resp
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        if let Some(len) = resp.content_length()
            && len > self.max_body_bytes
        {
            bail!(
                "pangolin response is {len} bytes, exceeds CONFIG_MAX_RESPONSE_BODY_BYTES={}",
                self.max_body_bytes
            );
        }

        let raw = resp
            .bytes()
            .await
            .context("reading pangolin response body")?;
        if raw.len() as u64 > self.max_body_bytes {
            bail!(
                "pangolin response is {} bytes, exceeds CONFIG_MAX_RESPONSE_BODY_BYTES={}",
                raw.len(),
                self.max_body_bytes
            );
        }

        if self.log_body {
            debug!(body = %String::from_utf8_lossy(&raw), "pangolin response body");
        }

        let mut hasher = Sha256::new();
        hasher.update(&raw);
        let digest = hex::encode(hasher.finalize());

        let config: TraefikDynamicConfig =
            serde_json::from_slice(&raw).context("decoding pangolin JSON")?;

        Ok(FetchOutcome::Changed(Box::new(ChangedConfig {
            config,
            etag,
            digest,
            raw_bytes: raw.len(),
        })))
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a UTF-8 safe boundary at or before `max`.
        let mut idx = max;
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        &s[..idx]
    }
}
