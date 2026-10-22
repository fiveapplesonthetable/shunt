//! Privacy scrubber and path relativizer.
//!
//! Enforces that no secrets, credentials, or arbitrary file contents enter
//! the trace. All scrubbing happens before truncation. Truncation is always
//! performed at valid UTF-8 character boundaries and appends `…`.

use regex::Regex;
use std::path::{Path, PathBuf};

/// Privacy scrubber that replaces credentials and secret shapes with `<redacted>`.
#[derive(Debug)]
pub struct Scrubber {
    auth_bearer: Regex,
    flags: Regex,
    kv_pairs: Regex,
    url_userinfo: Regex,
    aws_token: Regex,
    gcp_token: Regex,
    github_token: Regex,
    anthropic_token: Regex,
    openai_token: Regex,
    slack_token: Regex,
    pem_block: Regex,
    hex_run: Regex,
    base64_run: Regex,
}

impl Default for Scrubber {
    fn default() -> Self {
        Self::new()
    }
}

impl Scrubber {
    /// Creates a new scrubber compiled with standard secret patterns.
    pub fn new() -> Self {
        Self {
            auth_bearer: Regex::new(
                r"(?i)\b((?:Authorization\s*:\s*)?(?:Bearer\s+))[A-Za-z0-9._~+/-]+=*",
            )
            .unwrap(),
            flags: Regex::new(r#"(?i)(--(?:token|password|secret|api-key)(?:=|\s+))[^\s;&"']+"#)
                .unwrap(),
            kv_pairs: Regex::new(r#"(?i)\b(key|secret|token|password)=([^\s;&"']+)"#).unwrap(),
            url_userinfo: Regex::new(r"([a-zA-Z][a-zA-Z0-9+.-]*://)(?:[^:@\s]+(?::[^@\s]*)?@)")
                .unwrap(),
            aws_token: Regex::new(r"\bA[KS]IA[0-9A-Z]{16}\b").unwrap(),
            gcp_token: Regex::new(r"\bAIza[0-9A-Za-z\-_]{30,}\b").unwrap(),
            github_token: Regex::new(
                r"\b(?:(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9_]{36,255}|github_pat_[A-Za-z0-9_]{82})\b",
            )
            .unwrap(),
            anthropic_token: Regex::new(r"\bsk-ant-[a-zA-Z0-9_\-]{32,}\b").unwrap(),
            openai_token: Regex::new(r"\bsk-(?:proj-|none-)?[a-zA-Z0-9_\-]{32,}\b").unwrap(),
            slack_token: Regex::new(r"\bxox[baprs]-[0-9a-zA-Z\-]{10,}\b").unwrap(),
            pem_block: Regex::new(r"(?s)-----BEGIN [A-Z0-9 _-]+-----.*?-----END [A-Z0-9 _-]+-----")
                .unwrap(),
            hex_run: Regex::new(r"\b[0-9a-fA-F]{32,}\b").unwrap(),
            base64_run: Regex::new(r"\b[A-Za-z0-9+/]{32,}={0,2}").unwrap(),
        }
    }

    /// Scrubs secrets from text, replacing sensitive tokens with `<redacted>`.
    pub fn scrub(&self, input: &str) -> String {
        let mut s = input.to_string();

        // 1. PEM blocks
        s = self.pem_block.replace_all(&s, "<redacted>").into_owned();

        // 2. Specific vendor token shapes
        s = self.aws_token.replace_all(&s, "<redacted>").into_owned();
        s = self.gcp_token.replace_all(&s, "<redacted>").into_owned();
        s = self.github_token.replace_all(&s, "<redacted>").into_owned();
        s = self
            .anthropic_token
            .replace_all(&s, "<redacted>")
            .into_owned();
        s = self.openai_token.replace_all(&s, "<redacted>").into_owned();
        s = self.slack_token.replace_all(&s, "<redacted>").into_owned();

        // 3. Flags (--token=val, --password val)
        s = self.flags.replace_all(&s, "${1}<redacted>").into_owned();

        // 4. Key-value pairs (key=val, secret=val)
        s = self
            .kv_pairs
            .replace_all(&s, "${1}=<redacted>")
            .into_owned();

        // 5. Auth / Bearer headers
        s = self
            .auth_bearer
            .replace_all(&s, "${1}<redacted>")
            .into_owned();

        // 6. URL userinfo (https://user:pass@host -> https://<redacted>@host)
        s = self
            .url_userinfo
            .replace_all(&s, "${1}<redacted>@")
            .into_owned();

        // 7. Base64 and hex runs of 32+ chars
        s = self.hex_run.replace_all(&s, "<redacted>").into_owned();
        s = self.base64_run.replace_all(&s, "<redacted>").into_owned();

        s
    }
}

/// Truncates a string to at most `max_bytes` at a valid UTF-8 character boundary.
/// If truncated, appends `…` (3 UTF-8 bytes) while keeping total byte length <= `max_bytes`.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }
    const ELLIPSIS: &str = "…";
    const ELLIPSIS_LEN: usize = 3;

    if max_bytes < ELLIPSIS_LEN {
        let mut boundary = 0;
        for (idx, _) in s.char_indices() {
            if idx > max_bytes {
                break;
            }
            boundary = idx;
        }
        return s[..boundary].to_string();
    }

    let target = max_bytes - ELLIPSIS_LEN;
    let mut boundary = 0;
    for (idx, _) in s.char_indices() {
        if idx > target {
            break;
        }
        boundary = idx;
    }

    let mut res = String::with_capacity(boundary + ELLIPSIS_LEN);
    res.push_str(&s[..boundary]);
    res.push_str(ELLIPSIS);
    res
}

/// Relativizes a path with respect to `cwd`.
///
/// Paths within `cwd` are made relative to it.
/// Absolute paths outside `cwd` keep their first two directory components and become `<outside>/...`.
pub fn relativize_path(path: &str, cwd: &str) -> String {
    if path.is_empty() || path == "." {
        return ".".to_string();
    }

    let p = Path::new(path);
    let c = Path::new(cwd);

    // If path is inside cwd, return relative path
    if let Ok(rel) = p.strip_prefix(c) {
        let rel_str = rel.to_string_lossy();
        if rel_str.is_empty() {
            return ".".to_string();
        }
        return rel_str.into_owned();
    }

    // If relative path already, keep it
    if !p.is_absolute() {
        return path.to_string();
    }

    // Absolute path outside cwd: keep first two directory components
    let components: Vec<_> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    // components[0] is root "/"
    let parts: Vec<&str> = components
        .iter()
        .map(|s| s.as_str())
        .filter(|s| *s != "/")
        .collect();
    if parts.len() <= 2 {
        format!("<outside>/{}", parts.join("/"))
    } else {
        format!("<outside>/{}/{}/...", parts[0], parts[1])
    }
}

/// Relativizes working directory `cwd` by replacing `$HOME` prefix with `~`.
pub fn relativize_cwd(cwd: &str, home: Option<&str>) -> String {
    if let Some(h) = home {
        if !h.is_empty() {
            let h_path = PathBuf::from(h);
            let cwd_path = PathBuf::from(cwd);
            if let Ok(suffix) = cwd_path.strip_prefix(&h_path) {
                let suffix_str = suffix.to_string_lossy();
                if suffix_str.is_empty() {
                    return "~".to_string();
                }
                return format!("~/{}", suffix_str);
            }
        }
    }
    cwd.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrub_auth_bearer() {
        let scrubber = Scrubber::new();
        let input = "curl -H 'Authorization: Bearer my-secret-token-12345' https://example.com";
        let out = scrubber.scrub(input);
        assert!(
            out.contains("Authorization: Bearer <redacted>"),
            "got: {}",
            out
        );
        assert!(!out.contains("my-secret-token"));
    }

    #[test]
    fn test_scrub_flags() {
        let scrubber = Scrubber::new();
        let input = "tool --token=xyz123 --password secret456 --api-key=supersecret --secret shhh";
        let out = scrubber.scrub(input);
        assert!(out.contains("--token=<redacted>"));
        assert!(out.contains("--password <redacted>"));
        assert!(out.contains("--api-key=<redacted>"));
        assert!(out.contains("--secret <redacted>"));
    }

    #[test]
    fn test_scrub_kv_pairs() {
        let scrubber = Scrubber::new();
        let input = "app key=mykey secret=mysecret token=mytoken password=mypass";
        let out = scrubber.scrub(input);
        assert!(out.contains("key=<redacted>"));
        assert!(out.contains("secret=<redacted>"));
        assert!(out.contains("token=<redacted>"));
        assert!(out.contains("password=<redacted>"));
    }

    #[test]
    fn test_scrub_url_userinfo() {
        let scrubber = Scrubber::new();
        let input = "git clone https://zezeozue:secretpass@github.com/repo.git";
        let out = scrubber.scrub(input);
        assert!(out.contains("https://<redacted>@github.com/repo.git"));
        assert!(!out.contains("secretpass"));
    }

    #[test]
    fn test_scrub_vendor_tokens() {
        let scrubber = Scrubber::new();
        assert_eq!(
            scrubber.scrub("AWS: AKIAIOSFODNN7EXAMPLE"),
            "AWS: <redacted>"
        );
        assert_eq!(
            scrubber.scrub("GCP: AIzaSyD-1234567890abcdefghijklmnopqrstuv"),
            "GCP: <redacted>"
        );
        assert_eq!(
            scrubber.scrub("GitHub: ghp_1234567890abcdefghijklmnopqrstuvwxyzAB"),
            "GitHub: <redacted>"
        );
        assert_eq!(
            scrubber.scrub("Anthropic: sk-ant-api03-1234567890abcdefghijklmnopqrstuvwxyz123456"),
            "Anthropic: <redacted>"
        );
        assert_eq!(
            scrubber.scrub("OpenAI: sk-proj-1234567890abcdefghijklmnopqrstuvwxyz123456"),
            "OpenAI: <redacted>"
        );
        assert_eq!(
            scrubber.scrub("Slack: xoxb-1234567890-1234567890123-abc"),
            "Slack: <redacted>"
        );
    }

    #[test]
    fn test_scrub_pem_block() {
        let scrubber = Scrubber::new();
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0...\n-----END RSA PRIVATE KEY-----";
        let out = scrubber.scrub(pem);
        assert_eq!(out, "<redacted>");
    }

    #[test]
    fn test_scrub_hex_and_base64_runs() {
        let scrubber = Scrubber::new();
        let hex = "hash 0123456789abcdef0123456789abcdef";
        assert_eq!(scrubber.scrub(hex), "hash <redacted>");

        let b64 = "token YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoxMjM0NTY=";
        assert_eq!(scrubber.scrub(b64), "token <redacted>");
    }

    #[test]
    fn test_truncate_utf8() {
        assert_eq!(truncate_utf8("hello world", 5), "he…");
        assert_eq!(truncate_utf8("hello", 10), "hello");
        assert_eq!(truncate_utf8("hello", 0), "");
        // Multi-byte character: '€' is 3 bytes (0xE2, 0x82, 0xAC)
        let s = "€€€"; // 9 bytes
        let t = truncate_utf8(s, 6); // max 6 bytes: 3 bytes for '€', 3 for '…'
        assert_eq!(t, "€…");
        assert!(t.len() <= 6);
    }

    #[test]
    fn test_relativize_path() {
        let cwd = "/home/user/project";
        assert_eq!(
            relativize_path("/home/user/project/src/main.rs", cwd),
            "src/main.rs"
        );
        assert_eq!(relativize_path("/home/user/project", cwd), ".");
        assert_eq!(
            relativize_path("/var/log/syslog/extra", cwd),
            "<outside>/var/log/..."
        );
        assert_eq!(relativize_path("/etc/hosts", cwd), "<outside>/etc/hosts");
    }

    #[test]
    fn test_relativize_cwd() {
        assert_eq!(
            relativize_cwd("/home/user/dev/repo", Some("/home/user")),
            "~/dev/repo"
        );
        assert_eq!(relativize_cwd("/home/user", Some("/home/user")), "~");
        assert_eq!(relativize_cwd("/var/tmp", Some("/home/user")), "/var/tmp");
    }
}
