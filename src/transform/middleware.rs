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
        if let Some(filter) = translate(router, name, mw) {
            filters.push(filter);
        }
    }
    filters
}

fn translate(router: &str, name: &str, mw: &Middleware) -> Option<HttpRouteRulesFilters> {
    let obj = mw.as_object()?;
    // Pangolin emits one top-level key per middleware kind.
    let (kind, body) = obj.iter().next()?;
    match kind.as_str() {
        "redirectScheme" => translate_redirect_scheme(body),
        "headers" => translate_headers(body),
        "addPrefix" => translate_add_prefix(body),
        "replacePath" => translate_replace_path(body),
        "replacePathRegex" => {
            warn!(router, middleware = %name, "replacePathRegex is not supported by core Gateway API; skipping");
            None
        }
        "stripPrefix" => translate_strip_prefix(body),
        "plugin" => {
            warn!(router, middleware = %name, "plugin middlewares (e.g. badger) must be configured via Envoy Gateway policies; skipping");
            None
        }
        other => {
            warn!(router, middleware = %name, kind = %other, "unsupported middleware kind; skipping");
            None
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

fn translate_headers(body: &Value) -> Option<HttpRouteRulesFilters> {
    // Traefik's headers middleware has many flavors; we map the two most common ones.
    let mut req_set: Vec<HttpRouteRulesFiltersRequestHeaderModifierSet> = Vec::new();
    if let Some(map) = body.get("customRequestHeaders").and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(val) = v.as_str() {
                req_set.push(HttpRouteRulesFiltersRequestHeaderModifierSet {
                    name: k.clone(),
                    value: val.to_string(),
                });
            }
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
        return Some(HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
            request_header_modifier: Some(HttpRouteRulesFiltersRequestHeaderModifier {
                set: Some(req_set),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    if !resp_set.is_empty() {
        return Some(HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::ResponseHeaderModifier,
            response_header_modifier: Some(HttpRouteRulesFiltersResponseHeaderModifier {
                set: Some(resp_set),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    None
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
