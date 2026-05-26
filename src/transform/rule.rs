//! Minimal parser for the Traefik v3 router rule language.
//!
//! Pangolin emits expressions like:
//!
//! ```text
//! Host(`example.com`) && PathPrefix(`/api`)
//! Host(`a.com`, `b.com`)
//! Host(`example.com`) && Path(`/exact`)
//! ```
//!
//! We only need to extract hostnames + a single path matcher per rule. Anything else
//! (Method, Headers, Query, regex hosts, `!`-negation, `||`) we surface to the caller
//! so they can warn and drop the router rather than silently misroute traffic.

use std::fmt;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedRule {
    pub hosts: Vec<String>,
    pub path: Option<PathMatch>,
    pub host_regexp: bool,
    pub unsupported_predicates: Vec<String>,
    pub has_disjunction: bool,
    pub has_negation: bool,
}

impl ParsedRule {
    pub fn is_usable(&self) -> bool {
        !self.has_disjunction
            && !self.has_negation
            && !self.host_regexp
            && self.unsupported_predicates.is_empty()
            && !self.hosts.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMatch {
    pub kind: PathMatchKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMatchKind {
    Exact,
    Prefix,
    Regex,
}

impl fmt::Display for PathMatchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathMatchKind::Exact => f.write_str("Exact"),
            PathMatchKind::Prefix => f.write_str("PathPrefix"),
            PathMatchKind::Regex => f.write_str("Regex"),
        }
    }
}

pub fn parse(rule: &str) -> ParsedRule {
    let mut out = ParsedRule::default();

    // Lowercase scan for `||` / `!` outside backtick strings (both unsupported).
    let mut in_backtick = false;
    let bytes = rule.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '`' {
            in_backtick = !in_backtick;
        } else if !in_backtick {
            if c == '|' && bytes.get(i + 1) == Some(&b'|') {
                out.has_disjunction = true;
            } else if c == '!' {
                // ignore `!=` inside Headers() etc. — but since Headers is unsupported anyway,
                // any `!` triggers the warning.
                out.has_negation = true;
            }
        }
        i += 1;
    }

    // Walk predicates: `Ident(...args...)` repeated, ignoring &&.
    let mut idx = 0;
    let chars: Vec<char> = rule.chars().collect();
    while idx < chars.len() {
        // skip whitespace and connector tokens (&&, ||).
        while idx < chars.len()
            && (chars[idx].is_whitespace() || chars[idx] == '&' || chars[idx] == '|')
        {
            idx += 1;
        }
        if idx >= chars.len() {
            break;
        }
        if !chars[idx].is_ascii_alphabetic() {
            // Bail: unrecognized character at this position — surface as unsupported.
            out.unsupported_predicates.push(format!(
                "unparseable token at offset {idx}: {:?}",
                chars[idx]
            ));
            break;
        }
        let ident_start = idx;
        while idx < chars.len() && (chars[idx].is_ascii_alphanumeric() || chars[idx] == '_') {
            idx += 1;
        }
        let ident: String = chars[ident_start..idx].iter().collect();
        // expect (
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() || chars[idx] != '(' {
            out.unsupported_predicates
                .push(format!("predicate {ident} missing arguments"));
            break;
        }
        idx += 1; // skip (
        let args = read_args(&chars, &mut idx);
        // expect closing )
        if idx < chars.len() && chars[idx] == ')' {
            idx += 1;
        }
        handle_predicate(&mut out, &ident, &args);
    }

    out
}

fn handle_predicate(out: &mut ParsedRule, ident: &str, args: &[String]) {
    match ident {
        "Host" => out.hosts.extend(args.iter().cloned()),
        "HostRegexp" => {
            out.host_regexp = true;
        }
        "PathPrefix" => {
            if let Some(first) = args.first() {
                out.path = Some(PathMatch {
                    kind: PathMatchKind::Prefix,
                    value: normalize_path(first),
                });
            }
        }
        "Path" => {
            if let Some(first) = args.first() {
                out.path = Some(PathMatch {
                    kind: PathMatchKind::Exact,
                    value: normalize_path(first),
                });
            }
        }
        "PathRegexp" => {
            if let Some(first) = args.first() {
                out.path = Some(PathMatch {
                    kind: PathMatchKind::Regex,
                    value: first.clone(),
                });
            }
        }
        other => out.unsupported_predicates.push(other.to_string()),
    }
}

fn normalize_path(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

fn read_args(chars: &[char], idx: &mut usize) -> Vec<String> {
    let mut args = Vec::new();
    loop {
        while *idx < chars.len() && (chars[*idx].is_whitespace() || chars[*idx] == ',') {
            *idx += 1;
        }
        if *idx >= chars.len() || chars[*idx] == ')' {
            break;
        }
        if chars[*idx] == '`' {
            *idx += 1;
            let start = *idx;
            while *idx < chars.len() && chars[*idx] != '`' {
                *idx += 1;
            }
            let s: String = chars[start..*idx].iter().collect();
            args.push(s);
            if *idx < chars.len() {
                *idx += 1; // skip closing backtick
            }
        } else {
            // bare token until , or )
            let start = *idx;
            while *idx < chars.len() && chars[*idx] != ',' && chars[*idx] != ')' {
                *idx += 1;
            }
            args.push(chars[start..*idx].iter().collect::<String>().trim().to_string());
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_only() {
        let r = parse("Host(`example.com`)");
        assert_eq!(r.hosts, vec!["example.com"]);
        assert!(r.path.is_none());
        assert!(r.is_usable());
    }

    #[test]
    fn host_and_path_prefix() {
        let r = parse("Host(`example.com`) && PathPrefix(`/api`)");
        assert_eq!(r.hosts, vec!["example.com"]);
        assert_eq!(
            r.path,
            Some(PathMatch {
                kind: PathMatchKind::Prefix,
                value: "/api".into()
            })
        );
        assert!(r.is_usable());
    }

    #[test]
    fn host_with_multiple() {
        let r = parse("Host(`a.com`, `b.com`)");
        assert_eq!(r.hosts, vec!["a.com", "b.com"]);
    }

    #[test]
    fn host_and_exact_path() {
        let r = parse("Host(`example.com`) && Path(`/exact`)");
        assert_eq!(
            r.path,
            Some(PathMatch {
                kind: PathMatchKind::Exact,
                value: "/exact".into()
            })
        );
    }

    #[test]
    fn disjunction_is_unsupported() {
        let r = parse("Host(`a.com`) || Host(`b.com`)");
        assert!(r.has_disjunction);
        assert!(!r.is_usable());
    }

    #[test]
    fn host_regexp_marked() {
        let r = parse("HostRegexp(`{any:.+}.example.com`)");
        assert!(r.host_regexp);
        assert!(!r.is_usable());
    }

    #[test]
    fn unsupported_predicate_recorded() {
        let r = parse("Host(`a.com`) && Method(`GET`)");
        assert_eq!(r.unsupported_predicates, vec!["Method"]);
        assert!(!r.is_usable());
    }
}
