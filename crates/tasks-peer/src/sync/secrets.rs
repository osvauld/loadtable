//! The secret denylist: file names and content patterns that must never travel.
//!
//! Names are checked on every file that is not already tracked by the repo and on every
//! allowlisted extra; content is checked on the same files, only on bytes sync already reads
//! to hash. A hit is a named refusal ([`crate::sync::RefuseReason`]), never a silent skip.

/// A kind of secret, named in the refusal.
pub type SecretKind = &'static str;

/// Key material or credentials by file name. Names only: nothing like `.log` or `.txt` is
/// ever matched, and templates (`.env.example`) are fine.
pub fn secret_name(path: &str) -> Option<SecretKind> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
        let templ = ["example", "sample", "template", "dist", "defaults"];
        return (!templ.iter().any(|t| lower.ends_with(t))).then_some("environment file");
    }
    if matches!(
        lower.as_str(),
        ".netrc"
            | ".npmrc"
            | ".pypirc"
            | ".git-credentials"
            | "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
            | "credentials.json"
            | "service-account.json"
    ) {
        return Some("credentials file");
    }
    ["pem", "key", "p12", "pfx", "jks", "keystore", "kdbx"]
        .iter()
        .any(|ext| lower.rsplit_once('.').is_some_and(|(_, e)| e == *ext))
        .then_some("key or keystore file")
}

/// Most bytes of a file the content check looks at.
const SCAN_BYTES: usize = 4 * 1024 * 1024;

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/' | b'+' | b'=' | b'.')
}

/// True when `hay` has `prefix` followed by at least `min` token characters.
fn has_token(hay: &[u8], prefix: &[u8], min: usize) -> bool {
    let mut from = 0;
    while let Some(at) = find(hay.get(from..).unwrap_or(&[]), prefix) {
        let start = from + at + prefix.len();
        let run = hay
            .get(start..)
            .unwrap_or(&[])
            .iter()
            .take_while(|b| is_token_char(**b))
            .count();
        if run >= min {
            return true;
        }
        from = start;
    }
    false
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `name = <long opaque value>` where the name says secret. Short values and values with
/// spaces (prose, `password = changeme`) are not flagged.
fn secret_assignment(line: &[u8]) -> bool {
    let lower = line.to_ascii_lowercase();
    let Some(eq) = lower.iter().position(|b| *b == b'=' || *b == b':') else {
        return false;
    };
    let (key, value) = lower.split_at(eq);
    let key_hit = [
        "secret",
        "token",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "api-key",
        "private_key",
    ]
    .iter()
    .any(|k| find(key, k.as_bytes()).is_some());
    if !key_hit {
        return false;
    }
    let value: Vec<u8> = value
        .iter()
        .skip(1)
        .copied()
        .skip_while(|b| b.is_ascii_whitespace() || *b == b'"' || *b == b'\'')
        .take_while(|b| is_token_char(*b))
        .collect();
    value.len() >= 20
}

/// Looks for private key blocks, well-known token shapes and long secret-named assignments.
pub fn secret_content(bytes: &[u8]) -> Option<SecretKind> {
    let head = bytes.get(..SCAN_BYTES).unwrap_or(bytes);
    if find(head, b"-----BEGIN ").is_some()
        && (find(head, b"PRIVATE KEY-----").is_some()
            || find(head, b"PRIVATE KEY BLOCK-----").is_some())
    {
        return Some("private key block");
    }
    // 16 more upper-case alphanumerics after `AKIA` make an AWS access key id
    let mut from = 0;
    while let Some(at) = find(head.get(from..).unwrap_or(&[]), b"AKIA") {
        let s = from + at + 4;
        let run = head
            .get(s..)
            .unwrap_or(&[])
            .iter()
            .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            .count();
        if run >= 16 {
            return Some("AWS access key id");
        }
        from = s;
    }
    for (prefix, min, what) in [
        (&b"ghp_"[..], 30, "GitHub token"),
        (b"gho_", 30, "GitHub token"),
        (b"ghu_", 30, "GitHub token"),
        (b"ghs_", 30, "GitHub token"),
        (b"ghr_", 30, "GitHub token"),
        (b"github_pat_", 30, "GitHub token"),
        (b"xoxb-", 20, "Slack token"),
        (b"xoxp-", 20, "Slack token"),
        (b"xoxa-", 20, "Slack token"),
        (b"sk-ant-", 20, "Anthropic API key"),
        (b"sk_live_", 20, "Stripe live key"),
        (b"AIza", 35, "Google API key"),
    ] {
        if has_token(head, prefix, min) {
            return Some(what);
        }
    }
    head.split(|b| *b == b'\n')
        .any(secret_assignment)
        .then_some("secret-named assignment with a long value")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r155_secrets_by_name_are_named_and_templates_are_not() {
        assert_eq!(secret_name(".env"), Some("environment file"));
        assert_eq!(secret_name("app/.env.production"), Some("environment file"));
        assert_eq!(secret_name(".env.example"), None);
        assert_eq!(secret_name("home/.netrc"), Some("credentials file"));
        assert_eq!(
            secret_name("deploy/server.PEM"),
            Some("key or keystore file")
        );
        assert_eq!(
            secret_name("release.keystore"),
            Some("key or keystore file")
        );
        for ok in [
            "src/main.rs",
            "tests/fixtures/run.log",
            "notes.txt",
            "keyboard.rs",
            "monkey",
        ] {
            assert_eq!(secret_name(ok), None, "{ok}");
        }
    }

    #[test]
    fn r303_content_patterns_find_keys_and_tokens() {
        assert_eq!(
            secret_content(b"x\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n"),
            Some("private key block")
        );
        assert_eq!(
            secret_content(b"id = AKIAABCDEFGHIJKLMNOP\n"),
            Some("AWS access key id")
        );
        let gh = format!("t = ghp_{}\n", "a1".repeat(20));
        assert_eq!(secret_content(gh.as_bytes()), Some("GitHub token"));
        assert_eq!(
            secret_content(b"storePassword=abcdefghijklmnopqrstuvwx\n"),
            Some("secret-named assignment with a long value")
        );
    }

    #[test]
    fn r303_ordinary_text_is_not_flagged() {
        for ok in [
            &b"sdk.dir=/home/me/Android/Sdk\n"[..],
            b"password = changeme\n",
            b"the token ring has a password policy of many many characters here\n",
            b"AKIA is a prefix\n",
            b"ghp_short\n",
            b"-----BEGIN CERTIFICATE-----\nabc\n",
            b"",
        ] {
            assert_eq!(secret_content(ok), None, "{}", String::from_utf8_lossy(ok));
        }
    }
}
