//! Bootstrap a fresh pangolin instance for the e2e test.
//!
//! Reads connection details + provisioning data from env vars, then walks
//! pangolin's REST API to:
//!   1. set the server admin (using `PANGOLIN_SETUP_TOKEN`),
//!   2. log in to obtain a session cookie,
//!   3. create an org, site, HTTP resource, and IP target,
//!   4. verify the internal `/api/v1/traefik-config` endpoint reports the
//!      provisioned resource.
//!
//! Idempotent on second runs: each step swallows the "already exists" error
//! pangolin returns so the binary can be safely re-invoked.
//!
//! Required env vars:
//!   PANGOLIN_EXTERNAL_URL    e.g. http://127.0.0.1:13000
//!   PANGOLIN_INTERNAL_URL    e.g. http://127.0.0.1:13001
//!   PANGOLIN_SETUP_TOKEN     32-char [a-z0-9] string passed to the server
//!   PANGOLIN_ADMIN_EMAIL     e.g. admin@integration.local
//!   PANGOLIN_ADMIN_PASSWORD  must satisfy pangolin's password policy
//!
//! Optional:
//!   PANGOLIN_ORG_ID          default: "e2e"
//!   PANGOLIN_RESOURCE_HOST   subdomain of the resource, default: "web"
//!   PANGOLIN_TARGET_IP       default: "10.0.0.42"
//!   PANGOLIN_TARGET_PORT     default: "8080"
//!   PANGOLIN_DOMAIN_ID       default: "domain1" (matches config.yml)

use std::env;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderMap, SET_COOKIE};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

const CSRF_HEADER: &str = "X-CSRF-Token";
const CSRF_VALUE: &str = "x-csrf-protection";
const SESSION_COOKIE: &str = "p_session_token";

struct Pangolin {
    http: Client,
    external: String,
    internal: String,
    session: Option<String>,
}

impl Pangolin {
    fn new(external: String, internal: String) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            external,
            internal,
            session: None,
        })
    }

    fn ext(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.external.trim_end_matches('/'), path)
    }

    async fn wait_ready(&self, attempts: u32) -> Result<()> {
        for i in 1..=attempts {
            let resp = self
                .http
                .get(format!("{}/api/v1/", self.internal.trim_end_matches('/')))
                .send()
                .await;
            if let Ok(r) = resp
                && r.status().is_success()
            {
                eprintln!("pangolin reachable on attempt {i}");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("pangolin not reachable after {attempts} attempts")
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value, HeaderMap)> {
        let mut req = self
            .http
            .request(method, self.ext(path))
            .header(CSRF_HEADER, CSRF_VALUE);
        if let Some(token) = &self.session {
            req = req.header(COOKIE, format!("{SESSION_COOKIE}={token}"));
        }
        if let Some(b) = body {
            let bytes = serde_json::to_vec(b).context("encoding body")?;
            req = req.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("calling {path}"))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::String(text.clone()));
        Ok((status, json, headers))
    }

    /// Try to log in; returns true if successful, false if credentials don't
    /// authenticate (i.e. the admin hasn't been bootstrapped yet).
    async fn try_login(&mut self, email: &str, password: &str) -> Result<bool> {
        let body = json!({"email": email, "password": password});
        let (status, body, headers) = self.call(Method::POST, "/auth/login", Some(&body)).await?;
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::BAD_REQUEST {
            eprintln!("login refused ({status}): {body}");
            return Ok(false);
        }
        if !status.is_success() {
            bail!("login failed ({status}): {body}");
        }
        let token = extract_session_cookie(&headers)
            .ok_or_else(|| anyhow!("login response had no {SESSION_COOKIE} cookie"))?;
        self.session = Some(token);
        eprintln!("logged in");
        Ok(true)
    }

    async fn set_admin(&self, email: &str, password: &str, setup_token: &str) -> Result<()> {
        let body = json!({
            "email": email,
            "password": password,
            "setupToken": setup_token,
        });
        let (status, body, _) = self
            .call(Method::PUT, "/auth/set-server-admin", Some(&body))
            .await?;
        if status.is_success() {
            eprintln!("admin created");
            return Ok(());
        }
        bail!("set-server-admin failed ({status}): {body}");
    }

    async fn ensure_org(&self, org_id: &str, name: &str) -> Result<()> {
        let (_, defaults, _) = self.call(Method::GET, "/pick-org-defaults", None).await?;
        let subnet = defaults["data"]["subnet"]
            .as_str()
            .ok_or_else(|| anyhow!("no subnet in pick-org-defaults: {defaults}"))?;
        let utility = defaults["data"]["utilitySubnet"]
            .as_str()
            .ok_or_else(|| anyhow!("no utilitySubnet in pick-org-defaults: {defaults}"))?;
        let body = json!({
            "orgId": org_id,
            "name": name,
            "subnet": subnet,
            "utilitySubnet": utility,
        });
        let (status, body, _) = self.call(Method::PUT, "/org", Some(&body)).await?;
        if status.is_success() {
            eprintln!("org {org_id} created");
            return Ok(());
        }
        if message_contains(&body, "already exists") || status == StatusCode::CONFLICT {
            eprintln!("org {org_id} already exists, skipping");
            return Ok(());
        }
        bail!("create org failed ({status}): {body}");
    }

    /// Returns the siteId (pangolin assigns it).
    async fn ensure_local_site(&self, org_id: &str, name: &str) -> Result<i64> {
        let body = json!({"name": name, "type": "local"});
        let (status, body, _) = self
            .call(Method::PUT, &format!("/org/{org_id}/site"), Some(&body))
            .await?;
        if status.is_success()
            && let Some(id) = body["data"]["siteId"].as_i64()
        {
            eprintln!("site {name} (id={id}) created");
            return Ok(id);
        }
        // Idempotent path: list sites and find the existing one.
        let (status, list, _) = self
            .call(Method::GET, &format!("/org/{org_id}/sites"), None)
            .await?;
        if !status.is_success() {
            bail!("list sites failed ({status}): {list}");
        }
        let id = list["data"]["sites"]
            .as_array()
            .and_then(|arr| arr.iter().find(|s| s["name"] == name))
            .and_then(|s| s["siteId"].as_i64())
            .ok_or_else(|| anyhow!("site {name} not found in list: {list}"))?;
        eprintln!("site {name} (id={id}) already exists, reusing");
        Ok(id)
    }

    /// Returns the resourceId.
    async fn ensure_http_resource(
        &self,
        org_id: &str,
        name: &str,
        subdomain: &str,
        domain_id: &str,
    ) -> Result<i64> {
        let body = json!({
            "name": name,
            "subdomain": subdomain,
            "http": true,
            "protocol": "tcp",
            "domainId": domain_id,
        });
        let (status, body, _) = self
            .call(Method::PUT, &format!("/org/{org_id}/resource"), Some(&body))
            .await?;
        if status.is_success()
            && let Some(id) = body["data"]["resourceId"].as_i64()
        {
            eprintln!("resource {name} (id={id}) created");
            return Ok(id);
        }
        // Find the existing resource by name.
        let (status, list, _) = self
            .call(Method::GET, &format!("/org/{org_id}/resources"), None)
            .await?;
        if !status.is_success() {
            bail!("list resources failed ({status}): {list}");
        }
        let id = list["data"]["resources"]
            .as_array()
            .and_then(|arr| arr.iter().find(|r| r["name"] == name))
            .and_then(|r| r["resourceId"].as_i64())
            .ok_or_else(|| anyhow!("resource {name} not found in list: {list}"))?;
        eprintln!("resource {name} (id={id}) already exists, reusing");
        Ok(id)
    }

    async fn ensure_target(
        &self,
        resource_id: i64,
        site_id: i64,
        ip: &str,
        port: u16,
    ) -> Result<()> {
        let body = json!({
            "siteId": site_id,
            "ip": ip,
            "port": port,
            "method": "http",
        });
        let (status, body, _) = self
            .call(
                Method::PUT,
                &format!("/resource/{resource_id}/target"),
                Some(&body),
            )
            .await?;
        if status.is_success() {
            eprintln!("target {ip}:{port} created");
            return Ok(());
        }
        if message_contains(&body, "already") {
            eprintln!("target {ip}:{port} already exists");
            return Ok(());
        }
        bail!("create target failed ({status}): {body}");
    }

    async fn verify_traefik_config(&self) -> Result<()> {
        let resp = self
            .http
            .get(format!(
                "{}/api/v1/traefik-config",
                self.internal.trim_end_matches('/')
            ))
            .send()
            .await
            .context("calling internal /traefik-config")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("traefik-config returned {status}: {text}");
        }
        let json: Value = serde_json::from_str(&text)
            .with_context(|| format!("decoding traefik-config: {text}"))?;
        let servers = json["http"]["services"]
            .as_object()
            .ok_or_else(|| anyhow!("no http.services in config: {json}"))?
            .values()
            .filter_map(|s| s["loadBalancer"]["servers"].as_array())
            .flatten()
            .count();
        if servers == 0 {
            bail!("traefik-config has zero server URLs: {json}");
        }
        eprintln!("traefik-config OK ({servers} server URLs)");
        Ok(())
    }
}

fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(SET_COOKIE).iter() {
        let s = value.to_str().ok()?;
        if let Some(rest) = s.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            let token = rest.split(';').next()?.to_string();
            return Some(token);
        }
    }
    None
}

fn message_contains(body: &Value, needle: &str) -> bool {
    body["message"].as_str().is_some_and(|m| m.contains(needle))
}

fn required(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("{key} must be set"))
}

fn optional(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let external = required("PANGOLIN_EXTERNAL_URL")?;
    let internal = required("PANGOLIN_INTERNAL_URL")?;
    let setup_token = required("PANGOLIN_SETUP_TOKEN")?;
    let admin_email = required("PANGOLIN_ADMIN_EMAIL")?;
    let admin_password = required("PANGOLIN_ADMIN_PASSWORD")?;
    let org_id = optional("PANGOLIN_ORG_ID", "e2e");
    let subdomain = optional("PANGOLIN_RESOURCE_HOST", "web");
    let target_ip = optional("PANGOLIN_TARGET_IP", "10.0.0.42");
    let target_port: u16 = optional("PANGOLIN_TARGET_PORT", "8080").parse()?;
    let domain_id = optional("PANGOLIN_DOMAIN_ID", "domain1");

    let mut p = Pangolin::new(external, internal)?;
    p.wait_ready(30).await?;
    // First-run: admin doesn't exist yet, login fails → set admin → login.
    // Re-run: login succeeds and we skip set-server-admin (the setup token has
    // already been consumed and is now invalid, so calling it again would 400).
    if !p.try_login(&admin_email, &admin_password).await? {
        p.set_admin(&admin_email, &admin_password, &setup_token)
            .await?;
        if !p.try_login(&admin_email, &admin_password).await? {
            bail!("login still fails after setting admin");
        }
    }
    p.ensure_org(&org_id, "E2E Org").await?;
    let site_id = p.ensure_local_site(&org_id, "e2e-site").await?;
    let resource_id = p
        .ensure_http_resource(&org_id, &subdomain, &subdomain, &domain_id)
        .await?;
    p.ensure_target(resource_id, site_id, &target_ip, target_port)
        .await?;
    p.verify_traefik_config().await?;

    println!("pangolin bootstrap complete");
    Ok(())
}
