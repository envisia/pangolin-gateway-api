//! Map a subset of Traefik middlewares into Gateway API HTTPRoute filters.
//!
//! Only filters that are part of standard Gateway API are emitted. Anything else
//! (pangolin's `badger` auth plugin, regex rewrites, retry/ratelimit) is logged
//! and dropped; users are expected to layer Envoy Gateway's own policy CRDs on
//! top if they need those behaviors.

use std::collections::BTreeMap;

use gateway_api::apis::experimental::httproutes::{
    HttpRouteRulesFilters, HttpRouteRulesFiltersRequestHeaderModifier,
    HttpRouteRulesFiltersRequestHeaderModifierSet, HttpRouteRulesFiltersRequestRedirect,
    HttpRouteRulesFiltersRequestRedirectScheme, HttpRouteRulesFiltersResponseHeaderModifier,
    HttpRouteRulesFiltersResponseHeaderModifierSet, HttpRouteRulesFiltersType,
    HttpRouteRulesFiltersUrlRewrite, HttpRouteRulesFiltersUrlRewritePath,
    HttpRouteRulesFiltersUrlRewritePathType,
};
use serde_json::Value;
use tracing::warn;

use crate::pangolin::types::Middleware;

/// True when the router references pangolin's `badger` auth plugin — i.e. the
/// resource is (potentially) protected by pangolin's SSO/password/PIN auth.
/// A referenced-but-missing middleware literally named `badger` is treated as
/// protected too: better to skip a route than to expose a protected resource.
pub fn requires_badger_auth(
    middleware_names: &[String],
    middlewares: &BTreeMap<String, Middleware>,
) -> bool {
    middleware_names
        .iter()
        .any(|name| match middlewares.get(name) {
            Some(mw) => is_badger(mw),
            None => name == "badger",
        })
}

fn is_badger(mw: &Middleware) -> bool {
    mw.as_object()
        .and_then(|obj| obj.get("plugin"))
        .and_then(Value::as_object)
        .is_some_and(|plugin| plugin.contains_key("badger"))
}

pub fn build_filters(
    router: &str,
    middleware_names: &[String],
    middlewares: &BTreeMap<String, Middleware>,
) -> Vec<HttpRouteRulesFilters> {
    let mut filters = Vec::new();
    for name in middleware_names {
        let Some(mw) = middlewares.get(name) else {
            warn!(router, middleware = %name, "router references missing middleware");
            continue;
        };
        filters.extend(translate(router, name, mw));
    }
    merge_filters(router, filters)
}

/// Gateway API allows each of URLRewrite / RequestHeaderModifier /
/// ResponseHeaderModifier **at most once per rule**, but several pangolin
/// middlewares can map onto the same filter type (e.g. a custom Host header →
/// URLRewrite.hostname next to an addPrefix → URLRewrite.path). Merge them.
fn merge_filters(router: &str, filters: Vec<HttpRouteRulesFilters>) -> Vec<HttpRouteRulesFilters> {
    let mut out: Vec<HttpRouteRulesFilters> = Vec::new();
    let mut rewrite: Option<HttpRouteRulesFiltersUrlRewrite> = None;
    let mut req_set: Vec<HttpRouteRulesFiltersRequestHeaderModifierSet> = Vec::new();
    let mut resp_set: Vec<HttpRouteRulesFiltersResponseHeaderModifierSet> = Vec::new();

    for f in filters {
        match f.r#type {
            HttpRouteRulesFiltersType::UrlRewrite => {
                let Some(incoming) = f.url_rewrite else {
                    continue;
                };
                let merged = rewrite.get_or_insert_with(Default::default);
                if let Some(hostname) = incoming.hostname {
                    if merged.hostname.replace(hostname).is_some() {
                        warn!(
                            router,
                            "multiple hostname rewrites on one router; using the last"
                        );
                    }
                }
                if let Some(path) = incoming.path {
                    if merged.path.is_some() {
                        warn!(
                            router,
                            "multiple path rewrites on one router; keeping the first"
                        );
                    } else {
                        merged.path = Some(path);
                    }
                }
            }
            HttpRouteRulesFiltersType::RequestHeaderModifier => {
                if let Some(m) = f.request_header_modifier
                    && let Some(set) = m.set
                {
                    req_set.extend(set);
                }
            }
            HttpRouteRulesFiltersType::ResponseHeaderModifier => {
                if let Some(m) = f.response_header_modifier
                    && let Some(set) = m.set
                {
                    resp_set.extend(set);
                }
            }
            _ => out.push(f),
        }
    }

    if !req_set.is_empty() {
        out.push(HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
            request_header_modifier: Some(HttpRouteRulesFiltersRequestHeaderModifier {
                set: Some(req_set),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if !resp_set.is_empty() {
        out.push(HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::ResponseHeaderModifier,
            response_header_modifier: Some(HttpRouteRulesFiltersResponseHeaderModifier {
                set: Some(resp_set),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if let Some(rw) = rewrite {
        out.push(HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::UrlRewrite,
            url_rewrite: Some(rw),
            ..Default::default()
        });
    }
    out
}

fn translate(router: &str, name: &str, mw: &Middleware) -> Vec<HttpRouteRulesFilters> {
    let Some(obj) = mw.as_object() else {
        return Vec::new();
    };
    // Pangolin emits one top-level key per middleware kind.
    let Some((kind, body)) = obj.iter().next() else {
        return Vec::new();
    };
    let single = |f: Option<HttpRouteRulesFilters>| f.into_iter().collect::<Vec<_>>();
    match kind.as_str() {
        "redirectScheme" => single(translate_redirect_scheme(body)),
        "headers" => translate_headers(body),
        "addPrefix" => single(translate_add_prefix(body)),
        "replacePath" => single(translate_replace_path(body)),
        "replacePathRegex" => {
            warn!(router, middleware = %name, "replacePathRegex is not supported by core Gateway API; skipping");
            Vec::new()
        }
        "stripPrefix" => single(translate_strip_prefix(body)),
        "plugin" if is_badger(mw) => {
            // Auth handling is decided per-route in route.rs (ext-authz
            // SecurityPolicy, explicit unauthenticated override, or skip) —
            // it is never a per-rule filter, so nothing to emit here.
            Vec::new()
        }
        "plugin" => {
            warn!(router, middleware = %name, "plugin middlewares must be configured via Envoy Gateway policies; skipping");
            Vec::new()
        }
        other => {
            warn!(router, middleware = %name, kind = %other, "unsupported middleware kind; skipping");
            Vec::new()
        }
    }
}

fn translate_redirect_scheme(body: &Value) -> Option<HttpRouteRulesFilters> {
    let scheme = body.get("scheme")?.as_str()?;
    let port = body
        .get("port")
        .and_then(Value::as_str)
        .and_then(|p| p.parse().ok());
    let scheme_enum = match scheme {
        "http" => HttpRouteRulesFiltersRequestRedirectScheme::Http,
        "https" => HttpRouteRulesFiltersRequestRedirectScheme::Https,
        other => {
            warn!(scheme = other, "unknown redirect scheme");
            return None;
        }
    };
    Some(HttpRouteRulesFilters {
        r#type: HttpRouteRulesFiltersType::RequestRedirect,
        request_redirect: Some(HttpRouteRulesFiltersRequestRedirect {
            scheme: Some(scheme_enum),
            port,
            status_code: None,
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn translate_headers(body: &Value) -> Vec<HttpRouteRulesFilters> {
    // Traefik's headers middleware has many flavors; we map the two most common ones.
    let mut filters = Vec::new();
    let mut req_set: Vec<HttpRouteRulesFiltersRequestHeaderModifierSet> = Vec::new();
    if let Some(map) = body.get("customRequestHeaders").and_then(Value::as_object) {
        for (k, v) in map {
            let Some(val) = v.as_str() else { continue };
            // Pangolin's "custom Host header" resource option arrives as a
            // customRequestHeaders entry, but Gateway API forbids touching
            // Host via RequestHeaderModifier — it is a URLRewrite concern.
            if k.eq_ignore_ascii_case("host") {
                filters.push(HttpRouteRulesFilters {
                    r#type: HttpRouteRulesFiltersType::UrlRewrite,
                    url_rewrite: Some(HttpRouteRulesFiltersUrlRewrite {
                        hostname: Some(val.to_string()),
                        path: None,
                    }),
                    ..Default::default()
                });
                continue;
            }
            req_set.push(HttpRouteRulesFiltersRequestHeaderModifierSet {
                name: k.clone(),
                value: val.to_string(),
            });
        }
    }
    let mut resp_set: Vec<HttpRouteRulesFiltersResponseHeaderModifierSet> = Vec::new();
    if let Some(map) = body.get("customResponseHeaders").and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(val) = v.as_str() {
                resp_set.push(HttpRouteRulesFiltersResponseHeaderModifierSet {
                    name: k.clone(),
                    value: val.to_string(),
                });
            }
        }
    }

    if !req_set.is_empty() {
        filters.push(HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
            request_header_modifier: Some(HttpRouteRulesFiltersRequestHeaderModifier {
                set: Some(req_set),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if !resp_set.is_empty() {
        filters.push(HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::ResponseHeaderModifier,
            response_header_modifier: Some(HttpRouteRulesFiltersResponseHeaderModifier {
                set: Some(resp_set),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    filters
}

fn translate_add_prefix(body: &Value) -> Option<HttpRouteRulesFilters> {
    let prefix = body.get("prefix").and_then(Value::as_str)?;
    Some(HttpRouteRulesFilters {
        r#type: HttpRouteRulesFiltersType::UrlRewrite,
        url_rewrite: Some(HttpRouteRulesFiltersUrlRewrite {
            path: Some(HttpRouteRulesFiltersUrlRewritePath {
                r#type: HttpRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch,
                replace_prefix_match: Some(prefix.to_string()),
                replace_full_path: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn translate_replace_path(body: &Value) -> Option<HttpRouteRulesFilters> {
    let path = body.get("path").and_then(Value::as_str)?;
    Some(HttpRouteRulesFilters {
        r#type: HttpRouteRulesFiltersType::UrlRewrite,
        url_rewrite: Some(HttpRouteRulesFiltersUrlRewrite {
            path: Some(HttpRouteRulesFiltersUrlRewritePath {
                r#type: HttpRouteRulesFiltersUrlRewritePathType::ReplaceFullPath,
                replace_full_path: Some(path.to_string()),
                replace_prefix_match: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn translate_strip_prefix(body: &Value) -> Option<HttpRouteRulesFilters> {
    // Strip the first prefix by rewriting to "/". Gateway API doesn't have a true
    // "strip prefix" filter; ReplacePrefixMatch with "" replaces the matched prefix.
    let prefixes = body.get("prefixes").and_then(Value::as_array)?;
    let _ = prefixes.first()?;
    Some(HttpRouteRulesFilters {
        r#type: HttpRouteRulesFiltersType::UrlRewrite,
        url_rewrite: Some(HttpRouteRulesFiltersUrlRewrite {
            path: Some(HttpRouteRulesFiltersUrlRewritePath {
                r#type: HttpRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch,
                replace_prefix_match: Some(String::new()),
                replace_full_path: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}
