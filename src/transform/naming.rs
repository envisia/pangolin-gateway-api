//! K8s naming helpers. RFC 1123 DNS label: lowercase alphanumeric or `-`,
//! must start/end alphanumeric, max 63 chars.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub const MAX_DNS_LABEL: usize = 63;

/// Sanitize an arbitrary string to a DNS-1123 label, keeping a short hash suffix
/// to disambiguate when truncation collides.
pub fn dns_label(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' => c,
            _ => '-',
        })
        .collect();
    let collapsed = collapse_dashes(&cleaned);
    let trimmed = collapsed.trim_matches('-').to_string();
    let base = if trimmed.is_empty() {
        "pangolin".to_string()
    } else {
        trimmed
    };

    if base.len() <= MAX_DNS_LABEL {
        return base;
    }

    // Truncate and append a deterministic short hash so distinct long names
    // don't collide after truncation.
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    let suffix = format!("-{:x}", hasher.finish() & 0xffff_ffff);
    let head_len = MAX_DNS_LABEL.saturating_sub(suffix.len());
    let mut head: String = base.chars().take(head_len).collect();
    while head.ends_with('-') {
        head.pop();
    }
    format!("{head}{suffix}")
}

/// Stable composite name: `<prefix>-<sanitized>`, hashed when overlong.
pub fn prefixed_label(prefix: &str, name: &str) -> String {
    let combined = format!("{prefix}-{name}");
    dns_label(&combined)
}

fn collapse_dashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_dash {
                out.push('-');
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_label_lowercases_and_strips() {
        assert_eq!(dns_label("My_Service.foo"), "my-service-foo");
    }

    #[test]
    fn dns_label_trims_dashes() {
        assert_eq!(dns_label("---foo---"), "foo");
    }

    #[test]
    fn dns_label_truncates_with_hash() {
        let long = "a".repeat(120);
        let out = dns_label(&long);
        assert!(out.len() <= MAX_DNS_LABEL);
        assert!(!out.ends_with('-'));
    }

    #[test]
    fn empty_input_falls_back() {
        assert_eq!(dns_label(""), "pangolin");
        assert_eq!(dns_label("---"), "pangolin");
    }
}
