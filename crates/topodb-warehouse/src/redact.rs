//! Deterministic secret scrubbing (spec §6.3). One Rust implementation,
//! applied at drain, never in JS. Versioned so a stricter rule set can be
//! re-applied later.
use crate::event::Redaction;
use regex::Regex;
use std::sync::OnceLock;

pub const REDACT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct RedactOutcome {
    pub text: String,
    pub redactions: Vec<Redaction>,
}

struct Rule {
    class: &'static str,
    re: Regex,
    group: usize,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let r = |class, pat, group| Rule { class, re: Regex::new(pat).expect("valid regex"), group };
        vec![
            r("pem", r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", 0),
            r("aws", r"\bAKIA[0-9A-Z]{16}\b", 0),
            r("github", r"\bgh[pousr]_[A-Za-z0-9]{20,}\b", 0),
            r("anthropic", r"\bsk-ant-[A-Za-z0-9_\-]{20,}", 0),
            r("openai", r"\bsk-(?:proj-)?[A-Za-z0-9]{32,}\b", 0),
            r("slack", r"\bxox[baprs]-[A-Za-z0-9\-]{20,}", 0),
            r("google", r"\bAIza[0-9A-Za-z_\-]{35}", 0),
            r("stripe", r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}\b", 0),
            r("bearer", r"(?i)\b(?:bearer|authorization:)\s+(?:bearer\s+)?([A-Za-z0-9._\-+/=]{16,})", 1),
            r("kv", r#"(?i)\b(?:[A-Z0-9_]*(?:password|passwd|secret|token|api_key|apikey|access_key)[A-Z0-9_]*)\s*[=:]\s*["']?([^\s"'`,;]{8,})"#, 1),
        ]
    })
}

fn is_hashlike(tok: &str) -> bool {
    let hex = tok.chars().all(|c| c.is_ascii_hexdigit());
    if hex && (tok.len() == 40 || tok.len() == 64 || tok.len() == 7 || tok.len() == 8) {
        return true;
    } // git sha / sha256 / short sha
    if tok.len() == 26
        && tok
            .chars()
            .all(|c| c.is_ascii_digit() || (c.is_ascii_uppercase() && !"ILOU".contains(c)))
    {
        return true;
    } // ULID
    if tok.starts_with("b3:") || tok.starts_with("sha256:") {
        return true;
    }
    false
}

fn shannon_bits_per_char(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    let n = s.chars().count() as f64;
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn entropy_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9+/=_\-]{32,}").expect("valid regex"))
}

fn bump(out: &mut Vec<Redaction>, class: &str) {
    if let Some(r) = out.iter_mut().find(|r| r.class == class) {
        r.count += 1;
    } else {
        out.push(Redaction {
            class: class.to_string(),
            count: 1,
        });
    }
}

pub fn redact(text: &str) -> RedactOutcome {
    let mut redactions = Vec::new();
    let mut cur = text.to_string();
    for rule in rules() {
        let mut next = String::with_capacity(cur.len());
        let mut last = 0usize;
        for caps in rule.re.captures_iter(&cur) {
            let m = caps.get(rule.group).expect("group exists");
            next.push_str(&cur[last..m.start()]);
            next.push_str(&format!(
                "«redacted:{}:{}»",
                rule.class,
                m.as_str().chars().count()
            ));
            bump(&mut redactions, rule.class);
            last = m.end();
        }
        next.push_str(&cur[last..]);
        cur = next;
    }
    // entropy fallback: long charset-limited tokens, not hash-like, high entropy
    let mut next = String::with_capacity(cur.len());
    let mut last = 0usize;
    for m in entropy_re().find_iter(&cur) {
        let tok = m.as_str();
        // skip tokens that sit inside a path or an already-redacted marker
        let prev = cur[..m.start()].chars().next_back();
        if is_hashlike(tok)
            || prev == Some('/')
            || prev == Some(':')
            || tok.contains('/')
            || shannon_bits_per_char(tok) < 4.0
        {
            continue;
        }
        next.push_str(&cur[last..m.start()]);
        next.push_str(&format!("«redacted:entropy:{}»", tok.chars().count()));
        bump(&mut redactions, "entropy");
        last = m.end();
    }
    next.push_str(&cur[last..]);
    RedactOutcome {
        text: next,
        redactions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn classes(o: &RedactOutcome) -> Vec<(String, u32)> {
        o.redactions
            .iter()
            .map(|r| (r.class.clone(), r.count))
            .collect()
    }

    #[test]
    fn known_key_shapes_are_replaced() {
        let cases = [
            ("aws", concat!("key AKIA", "IOSFODNN7EXAMPLE here")),
            ("github", concat!("tok ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij")),
            ("anthropic", concat!("sk-ant-", "api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz0123456789")),
            ("openai", concat!("sk-", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijkl")),
            ("slack", concat!("xoxb-", "1234567890-1234567890123-ABCDEFGHIJKLMNOPQRSTUVWX")),
            ("google", concat!("AIza", "SyA-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456")),
            ("stripe", concat!("sk_live_", "ABCDEFGHIJKLMNOPQRSTUVWX")),
            ("bearer", "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcDEFghiJKLmnoPQRstuVWXyz0123456789"),
            ("pem", "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----"),
            ("kv", "password=hunter2hunter2"),
            ("kv", "OPENAI_API_KEY=abcdefghijklmnopqrstuvwxyz0123456789ABCD"),
        ];
        for (class, input) in cases {
            let o = redact(input);
            assert!(
                o.text.contains(&format!("«redacted:{class}:")),
                "{class}: {}",
                o.text
            );
            assert!(
                classes(&o).iter().any(|(c, n)| c == class && *n >= 1),
                "{class}"
            );
            assert!(!o.text.contains("EXAMPLE") || class != "aws");
        }
    }

    #[test]
    fn high_entropy_token_is_redacted() {
        let o = redact("blob 9f8s7d6f5g4h3j2k1l0zXcVbNmQwErTyUiOpAsDf");
        assert!(o.text.contains("«redacted:entropy:"));
    }

    #[test]
    fn false_positive_guards() {
        let keep = [
            "commit 3279248abcdef0123456789abcdef0123456789", // git sha (40 hex)
            "id 01ARZ3NDEKTSV4RRFFQ69G5FAV",                  // ULID
            "b3:0000000000000000000000000000000000000000000000000000000000000000", // our hash
            "This is a perfectly ordinary sentence about the warehouse design.",
            "path /Users/drew/TopoDB/crates/topodb-warehouse/src/segment.rs",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", // low entropy
        ];
        for k in keep {
            let o = redact(k);
            assert_eq!(o.text, k, "should not redact: {k}");
            assert!(o.redactions.is_empty());
        }
    }

    #[test]
    fn replacement_records_length_and_counts_per_class() {
        let o = redact(concat!("a=AKIA", "IOSFODNN7EXAMPLE b=AKIA", "IOSFODNN7EXAMPLE"));
        assert_eq!(classes(&o), vec![("aws".to_string(), 2)]);
        assert!(o.text.contains("«redacted:aws:20»"));
    }
}
