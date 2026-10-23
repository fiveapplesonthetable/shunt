//! Privacy scrubber and path relativizer.
//!
//! Enforces that no secrets, credentials, or arbitrary file contents enter
//! the trace. All scrubbing happens before truncation. Truncation is always
//! performed at valid UTF-8 character boundaries and appends `…`.

use regex::{Captures, Regex};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

struct ScrubberPatterns {
    line_continuation: Regex,
    pem_block: Regex,
    aws_token: Regex,
    gcp_ya29: Regex,
    gcp_aiza: Regex,
    github_token: Regex,
    anthropic_token: Regex,
    openai_token: Regex,
    slack_token: Regex,
    jwt_token: Regex,
    stripe_token: Regex,
    npm_token: Regex,
    pypi_token: Regex,
    gitlab_token: Regex,
    netrc_password: Regex,
    url_userinfo: Regex,
    header_dquoted: Regex,
    header_squoted: Regex,
    header_unquoted: Regex,
    auth_scheme: Regex,
    auth_bearer: Regex,
    auth_general: Regex,
    flag_quoted: Regex,
    flag_unquoted: Regex,
    flag_space_quoted: Regex,
    flag_space_unquoted: Regex,
    short_p_dquoted: Regex,
    short_p_squoted: Regex,
    short_p_attached: Regex,
    short_p_space: Regex,
    short_u_dquoted: Regex,
    short_u_squoted: Regex,
    short_u_unquoted: Regex,
    short_u_attached: Regex,
    short_u_colon_only: Regex,
    assign_quoted: Regex,
    assign_unquoted: Regex,
    json_dquoted_dquoted: Regex,
    json_squoted_squoted: Regex,
    json_dquoted_squoted: Regex,
    json_squoted_dquoted: Regex,
    json_dquoted_unquoted: Regex,
    json_squoted_unquoted: Regex,
    json_unquoted_dquoted: Regex,
    json_unquoted_squoted: Regex,
    json_unquoted_unquoted: Regex,
    /// Generic secret key=value / key: value pattern.
    ///
    /// Anchors keys with start-of-input, start-of-line, JSON/config delimiter (`{`, `[`, `,`, `;`),
    /// or non-alphanumeric boundary. Keys may include prefixes like `FOO_TOKEN` or `--api-key`.
    /// When using `:` as the separator, unprefixed keys must be standalone (at line/doc start or after
    /// delimiters) to prevent redacting conversational prose such as `the secret: implementation` or
    /// `see the secret: implementation_detail`. Values have a minimum length of 4 and fully consume
    /// single or double quoted strings including whitespace.
    generic_secret_kv: Regex,
    hex_run: Regex,
    base64_run: Regex,
}

static PATTERNS: LazyLock<ScrubberPatterns> = LazyLock::new(|| {
    ScrubberPatterns {
    line_continuation: Regex::new(r"[ \t]*\\(?:\r?\n)[ \t]*").unwrap(),
    pem_block: Regex::new(
        r"(?s)(?i)-----BEGIN [A-Z0-9 _-]+-----.*?-----END [A-Z0-9 _-]+-----",
    )
    .unwrap(),
    aws_token: Regex::new(r"(?i)\bA[KS]IA[0-9A-Z]{16}\b").unwrap(),
    gcp_ya29: Regex::new(r"(?i)\bya29\.[0-9A-Za-z\-_]+\b").unwrap(),
    gcp_aiza: Regex::new(r"(?i)\bAIza[0-9A-Za-z\-_]{30,}\b").unwrap(),
    github_token: Regex::new(
        r"(?i)\b(?:(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9_]{36,255}|github_pat_[A-Za-z0-9_]{20,255})\b",
    )
    .unwrap(),
    anthropic_token: Regex::new(r"(?i)\bsk-ant-[a-zA-Z0-9_\-]{32,}\b").unwrap(),
    openai_token: Regex::new(r"(?i)\bsk-(?:proj-|none-)?[a-zA-Z0-9_\-]{32,}\b").unwrap(),
    slack_token: Regex::new(r"(?i)\bxox[abprs]-[0-9a-zA-Z_\-]{10,}\b").unwrap(),
    jwt_token: Regex::new(
        r"(?i)\bey[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
    )
    .unwrap(),
    stripe_token: Regex::new(
        r"(?i)\b(?:(?:sk|rk)_(?:live|test)_[0-9a-zA-Z_]{24,}|whsec_[0-9a-zA-Z_]{24,})\b",
    )
    .unwrap(),
    npm_token: Regex::new(r"\bnpm_[A-Za-z0-9]{20,255}\b").unwrap(),
    pypi_token: Regex::new(r"\bpypi-[A-Za-z0-9_\-]{20,255}\b").unwrap(),
    gitlab_token: Regex::new(r"\bglpat-[A-Za-z0-9\-]{20,}\b").unwrap(),
    netrc_password: Regex::new(
        r#"(?i)(^|[\s;&|])((?:(?:machine|default)\s+\S+\s+)?(?:login\s+\S+\s+)?password\s+)(".*?"|'.*?'|[^\s;&"']+)"#,
    )
    .unwrap(),
    url_userinfo: Regex::new(r"([a-zA-Z][a-zA-Z0-9+.-]*://)(?:[^:@\s/]+(?::[^@\s/]*)?@)")
        .unwrap(),
    header_dquoted: Regex::new(
        r#"(?i)(-H\s+|--header\s+|--header=)"(Authorization|Proxy-Authorization|Cookie|Set-Cookie|X-[a-zA-Z0-9_-]*(?:Key|Token)|[a-zA-Z0-9_-]*(?:Token|Secret|Key|Auth))\s*:\s*([^"]+)""#,
    )
    .unwrap(),
    header_squoted: Regex::new(
        r#"(?i)(-H\s+|--header\s+|--header=)'(Authorization|Proxy-Authorization|Cookie|Set-Cookie|X-[a-zA-Z0-9_-]*(?:Key|Token)|[a-zA-Z0-9_-]*(?:Token|Secret|Key|Auth))\s*:\s*([^']+)'"#,
    )
    .unwrap(),
    header_unquoted: Regex::new(
        r#"(?i)(^|[\r\n]|[\s;&|])(Cookie|Set-Cookie)\s*:\s*([^\r\n;&|]+)"#,
    )
    .unwrap(),
    auth_scheme: Regex::new(
        r#"(?i)((?:Authorization|Proxy-Authorization)\s*:\s*[a-zA-Z0-9_-]+)(?:\s|\\\r?\n)+(".*?"|'.*?'|[^\s;&"'\\][^\s;&"']*)"#,
    )
    .unwrap(),
    auth_bearer: Regex::new(
        r#"(?i)(^|[^-\w])(Bearer|Basic)(?:\s|\\\r?\n)+(".*?"|'.*?'|[^\s;&"'\\][^\s;&"']*)"#,
    )
    .unwrap(),
    auth_general: Regex::new(
        r#"(?i)((?:Authorization|Proxy-Authorization)\s*:\s*)(?:\s|\\\r?\n)*(".*?"|'.*?'|[^\s;&"'\\][^\s;&"']*)"#,
    )
    .unwrap(),
    flag_quoted: Regex::new(r#"(?i)(--[a-zA-Z0-9_-]+)\s*=\s*(?:\\\r?\n\s*)?(".*?"|'.*?')"#).unwrap(),
    flag_unquoted: Regex::new(r#"(?i)(--[a-zA-Z0-9_-]+)\s*=\s*(?:\\\r?\n\s*)?([^\s;&"'\\][^\s;&"']*)"#).unwrap(),
    flag_space_quoted: Regex::new(r#"(?i)(--[a-zA-Z0-9_-]+)(?:\s|\\\r?\n)+(".*?"|'.*?')"#).unwrap(),
    flag_space_unquoted: Regex::new(r#"(?i)(--[a-zA-Z0-9_-]+)(?:\s|\\\r?\n)+([^\s;&"'\\][^\s;&"']*)"#).unwrap(),
    short_p_dquoted: Regex::new(r#"(?i)(^|[\s;&|])(-p)(\s*(?:\\\r?\n\s*)?)"([^"]*)""#).unwrap(),
    short_p_squoted: Regex::new(r#"(?i)(^|[\s;&|])(-p)(\s*(?:\\\r?\n\s*)?)'([^']*)'"#).unwrap(),
    short_p_attached: Regex::new(r#"(?i)(^|[\s;&|])(-p)([^\s;&"'\\][^\s;&"']*)"#).unwrap(),
    short_p_space: Regex::new(r#"(?i)(^|[\s;&|])(-p)(?:\s|\\\r?\n)+([^\s;&"'\\][^\s;&"']*)"#).unwrap(),
    short_u_dquoted: Regex::new(
        r#"(?i)(^|[\s;&|])(-u|--user)(=|\s+|\\\r?\n+)"([^:\s;&"]*):\s*([^@\s;&"]*)""#,
    )
    .unwrap(),
    short_u_squoted: Regex::new(
        r#"(?i)(^|[\s;&|])(-u|--user)(=|\s+|\\\r?\n)+'([^:\s;&']*):\s*([^@\s;&']*)'"#,
    )
    .unwrap(),
    short_u_unquoted: Regex::new(
        r#"(?i)(^|[\s;&|])(-u|--user)(=|\s+|\\\r?\n)+([^:\s;&"'\\][^:\s;&"']*):\s*([^@\s;&"']+)"#,
    )
    .unwrap(),
    short_u_attached: Regex::new(
        r#"(?i)(^|[\s;&|])-u([^:\s;&"'\\][^:\s;&"']*):\s*([^@\s;&"']+)"#,
    )
    .unwrap(),
    short_u_colon_only: Regex::new(
        r#"(?i)(^|[\s;&|])(-u|--user)(=|\s+|\\\r?\n)+:\s*([^@\s;&"']+)"#,
    )
    .unwrap(),
    assign_quoted: Regex::new(r#"(?i)\b([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*(?:\\\r?\n\s*)?(".*?"|'.*?')"#).unwrap(),
    assign_unquoted: Regex::new(r#"(?i)\b([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*(?:\\\r?\n\s*)?([^\s;&"'\\][^\s;&"']*)"#).unwrap(),
    json_dquoted_dquoted: Regex::new(
        r#"(?i)"([a-zA-Z0-9_-]+)"(?:\s*:\s*(?:\\\r?\n\s*)?)"([^"]*)""#,
    )
    .unwrap(),
    json_squoted_squoted: Regex::new(
        r#"(?i)'([a-zA-Z0-9_-]+)'(?:\s*:\s*(?:\\\r?\n\s*)?)'([^']*)'"#,
    )
    .unwrap(),
    json_dquoted_squoted: Regex::new(
        r#"(?i)"([a-zA-Z0-9_-]+)"(?:\s*:\s*(?:\\\r?\n\s*)?)'([^']*)'"#,
    )
    .unwrap(),
    json_squoted_dquoted: Regex::new(
        r#"(?i)'([a-zA-Z0-9_-]+)'(?:\s*:\s*(?:\\\r?\n\s*)?)"([^"]*)""#,
    )
    .unwrap(),
    json_dquoted_unquoted: Regex::new(
        r#"(?i)"([a-zA-Z0-9_-]+)"(?:\s*:\s*(?:\\\r?\n\s*)?)([^\s,;}{"'\\][^\s,;}{"']*)"#,
    )
    .unwrap(),
    json_squoted_unquoted: Regex::new(
        r#"(?i)'([a-zA-Z0-9_-]+)'(?:\s*:\s*(?:\\\r?\n\s*)?)([^\s,;}{"'\\][^\s,;}{"']*)"#,
    )
    .unwrap(),
    json_unquoted_dquoted: Regex::new(
        r#"(?i)\b([a-zA-Z0-9_-]+)(?:\s*:\s*(?:\\\r?\n\s*)?)"([^"]*)""#,
    )
    .unwrap(),
    json_unquoted_squoted: Regex::new(
        r#"(?i)\b([a-zA-Z0-9_-]+)(?:\s*:\s*(?:\\\r?\n\s*)?)'([^']*)'"#,
    )
    .unwrap(),
    json_unquoted_unquoted: Regex::new(
        r#"(?i)(^|[\r\n{\[,;])\s*([a-zA-Z0-9_-]+)(?:\s*:\s*(?:\\\r?\n\s*)?)([^\s,;}{"'\\][^\s,;}{"']*)"#,
    )
    .unwrap(),
    generic_secret_kv: Regex::new(concat!(
        r#"(?i)"#,
        // Branch 1: Standalone key followed by ':' at line/doc start or after delimiter
        r#"(?:(^|[\r\n{\[,;])\s*([A-Za-z0-9_]*[_-]?(?:secret|password|passwd|pwd|token|api[_-]?key|apikey|access[_-]?key|auth[_-]?token|credential|private[_-]?key|client[_-]?secret))\s*(:)\s*"#,
        // Branch 2: Prefixed key (e.g. FOO_TOKEN, --api-key) followed by ':'
        r#"|(?:\b((?:[A-Za-z0-9_]+[_-]|--)(?:secret|password|passwd|pwd|token|api[_-]?key|apikey|access[_-]?key|auth[_-]?token|credential|private[_-]?key|client[_-]?secret)))\s*(:)\s*"#,
        // Branch 3: Key or prefixed key followed by '=' anywhere with non-alnum boundary
        r#"|(^|[^A-Za-z0-9_])((?:--|[A-Za-z0-9_]+[_-])?(?:secret|password|passwd|pwd|token|api[_-]?key|apikey|access[_-]?key|auth[_-]?token|credential|private[_-]?key|client[_-]?secret))\s*(=)\s*)"#,
        // Value: double-quoted, single-quoted, or unquoted (min length 4)
        r#"("([^"\r\n]{4,})"|'([^'\r\n]{4,})'|([^\s'";&|]{4,}))"#
    ))
    .unwrap(),
    hex_run: Regex::new(r"\b[0-9a-fA-F]{32,}\b").unwrap(),
    base64_run: Regex::new(r"\b[A-Za-z0-9+/]{32,}={0,2}").unwrap(),
}
});

/// Privacy scrubber that replaces credentials and secret shapes with `<redacted>` when enabled.
///
/// Enabled by default (`enabled = true` unless `TACHO_SCRUB=0`/`false`/`no` or `scrub = false` in `config.toml`).
#[derive(Debug, Clone, Copy)]
pub struct Scrubber {
    enabled: bool,
}

impl Default for Scrubber {
    fn default() -> Self {
        Self::new()
    }
}

impl Scrubber {
    /// Creates a new scrubber (enabled by default unless `TACHO_SCRUB=0`/`false`/`no` is set).
    pub fn new() -> Self {
        let enabled = std::env::var("TACHO_SCRUB")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);
        Self::with_enabled(enabled)
    }

    /// Creates a scrubber with secret redaction explicitly enabled.
    pub fn enabled() -> Self {
        Self::with_enabled(true)
    }

    /// Creates a scrubber with the specified `enabled` state.
    pub fn with_enabled(enabled: bool) -> Self {
        if enabled {
            let _ = &*PATTERNS;
        }
        Self { enabled }
    }

    /// Returns whether secret redaction is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Scrubs secrets from text, replacing sensitive tokens with `<redacted>` when enabled.
    pub fn scrub(&self, input: &str) -> String {
        if !self.enabled {
            return input.to_string();
        }
        let p = &*PATTERNS;
        // Normalise shell line continuations (\ + newline + leading spaces -> one space) before tokenising
        let mut s = p.line_continuation.replace_all(input, " ").into_owned();

        // 1. PEM blocks across lines
        s = p.pem_block.replace_all(&s, "<redacted>").into_owned();

        // 2. Sensitive quoted headers (-H 'Authorization: ...', -H 'Cookie: ...', etc.)
        s = p
            .header_dquoted
            .replace_all(&s, |caps: &Captures| {
                let prefix = &caps[1];
                let header_name = &caps[2];
                let val = caps[3].trim();

                if header_name.eq_ignore_ascii_case("Authorization")
                    || header_name.eq_ignore_ascii_case("Proxy-Authorization")
                {
                    if let Some((scheme, _rest)) = val.split_once(' ') {
                        if !scheme.is_empty() {
                            return format!("{}\"{}: {} <redacted>\"", prefix, header_name, scheme);
                        }
                    }
                }
                format!("{}\"{}: <redacted>\"", prefix, header_name)
            })
            .into_owned();

        s = p
            .header_squoted
            .replace_all(&s, |caps: &Captures| {
                let prefix = &caps[1];
                let header_name = &caps[2];
                let val = caps[3].trim();

                if header_name.eq_ignore_ascii_case("Authorization")
                    || header_name.eq_ignore_ascii_case("Proxy-Authorization")
                {
                    if let Some((scheme, _rest)) = val.split_once(' ') {
                        if !scheme.is_empty() {
                            return format!("{}'{}: {} <redacted>'", prefix, header_name, scheme);
                        }
                    }
                }
                format!("{}'{}: <redacted>'", prefix, header_name)
            })
            .into_owned();

        // 3. Sensitive unquoted headers (Cookie, Set-Cookie)
        s = p
            .header_unquoted
            .replace_all(&s, "${1}${2}: <redacted>")
            .into_owned();

        // 4. Specific vendor token shapes (all case-insensitive)
        s = p.aws_token.replace_all(&s, "<redacted>").into_owned();
        s = p.gcp_ya29.replace_all(&s, "<redacted>").into_owned();
        s = p.gcp_aiza.replace_all(&s, "<redacted>").into_owned();
        s = p.github_token.replace_all(&s, "<redacted>").into_owned();
        s = p.anthropic_token.replace_all(&s, "<redacted>").into_owned();
        s = p.openai_token.replace_all(&s, "<redacted>").into_owned();
        s = p.slack_token.replace_all(&s, "<redacted>").into_owned();
        s = p.jwt_token.replace_all(&s, "<redacted>").into_owned();
        s = p.stripe_token.replace_all(&s, "<redacted>").into_owned();
        s = p.npm_token.replace_all(&s, "<redacted>").into_owned();
        s = p.pypi_token.replace_all(&s, "<redacted>").into_owned();
        s = p.gitlab_token.replace_all(&s, "<redacted>").into_owned();
        s = p
            .netrc_password
            .replace_all(&s, |caps: &Captures| {
                let delim = &caps[1];
                let prefix = &caps[2];
                let val = &caps[3];
                let quote = if val.starts_with('"') {
                    "\""
                } else if val.starts_with('\'') {
                    "'"
                } else {
                    ""
                };
                format!("{}{}{}<redacted>{}", delim, prefix, quote, quote)
            })
            .into_owned();

        // 3. URL userinfo (https://user:pass@host -> https://<redacted>@host)
        s = p
            .url_userinfo
            .replace_all(&s, "${1}<redacted>@")
            .into_owned();

        // 4. Auth & Bearer headers
        s = p
            .auth_scheme
            .replace_all(&s, "${1} <redacted>")
            .into_owned();
        s = p
            .auth_bearer
            .replace_all(&s, "${1}${2} <redacted>")
            .into_owned();
        s = p
            .auth_general
            .replace_all(&s, |caps: &Captures| {
                let val = &caps[2];
                if val == "<redacted>"
                    || val.eq_ignore_ascii_case("bearer")
                    || val.eq_ignore_ascii_case("basic")
                    || val.eq_ignore_ascii_case("apikey")
                    || val.eq_ignore_ascii_case("token")
                    || val.eq_ignore_ascii_case("key")
                    || val.eq_ignore_ascii_case("secret")
                {
                    caps[0].to_string()
                } else {
                    format!("{}<redacted>", &caps[1])
                }
            })
            .into_owned();

        // 5. Short flags -u (user:pass style) and -p (password style)
        s = p
            .short_u_dquoted
            .replace_all(&s, "${1}${2}${3}\"${4}:<redacted>\"")
            .into_owned();
        s = p
            .short_u_squoted
            .replace_all(&s, "${1}${2}${3}'${4}:<redacted>'")
            .into_owned();
        s = p
            .short_u_unquoted
            .replace_all(&s, "${1}${2}${3}${4}:<redacted>")
            .into_owned();
        s = p
            .short_u_attached
            .replace_all(&s, "${1}-u${2}:<redacted>")
            .into_owned();
        s = p
            .short_u_colon_only
            .replace_all(&s, "${1}${2}${3}:<redacted>")
            .into_owned();

        s = p
            .short_p_dquoted
            .replace_all(&s, "${1}${2}${3}\"<redacted>\"")
            .into_owned();
        s = p
            .short_p_squoted
            .replace_all(&s, "${1}${2}${3}'<redacted>'")
            .into_owned();
        s = p
            .short_p_attached
            .replace_all(&s, |caps: &Captures| {
                let full = format!("p{}", &caps[3].to_ascii_lowercase());
                if full.starts_with("pthread")
                    || full == "pipe"
                    || full.starts_with("print")
                    || full.starts_with("prune")
                    || full == "pedantic"
                    || full == "pie"
                    || full == "pic"
                    || full == "pg"
                {
                    caps[0].to_string()
                } else {
                    format!("{}-p<redacted>", &caps[1])
                }
            })
            .into_owned();

        // Space-separated -p <val>: redact only if not preceded by known non-secret commands like cargo, mkdir, git, cp
        s = p
            .short_p_space
            .replace_all(&s, |caps: &Captures| {
                let match_start = caps.get(0).unwrap().start();
                let prefix = &s[..match_start];
                if is_non_secret_p_context(prefix) {
                    caps[0].to_string()
                } else {
                    format!("{}-p <redacted>", &caps[1])
                }
            })
            .into_owned();

        // 6. Long flags (--token="val", --token val, --api_key=val, etc.)
        s = p
            .flag_quoted
            .replace_all(&s, |caps: &Captures| {
                let flag = &caps[1];
                if is_sensitive_name(&flag[2..]) {
                    let val = &caps[2];
                    let quote = &val[..1];
                    format!("{}={}<redacted>{}", flag, quote, quote)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .flag_unquoted
            .replace_all(&s, |caps: &Captures| {
                let flag = &caps[1];
                if is_sensitive_name(&flag[2..]) {
                    format!("{}=<redacted>", flag)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .flag_space_quoted
            .replace_all(&s, |caps: &Captures| {
                let flag = &caps[1];
                if is_sensitive_name(&flag[2..]) {
                    let val = &caps[2];
                    let quote = &val[..1];
                    format!("{} {}<redacted>{}", flag, quote, quote)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .flag_space_unquoted
            .replace_all(&s, |caps: &Captures| {
                let flag = &caps[1];
                if is_sensitive_name(&flag[2..]) {
                    format!("{} <redacted>", flag)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        // 7. Assignments / Environment variables / Key-value pairs
        s = p
            .assign_quoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    let val = &caps[2];
                    let quote = &val[..1];
                    format!("{}={}<redacted>{}", key, quote, quote)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .assign_unquoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("{}=<redacted>", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        // 8. JSON and YAML structures ("key": "val", 'key': 'val', key: val)
        s = p
            .json_dquoted_dquoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("\"{}\": \"<redacted>\"", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_squoted_squoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("'{}': '<redacted>'", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_dquoted_squoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("\"{}\": '<redacted>'", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_squoted_dquoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("'{}': \"<redacted>\"", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_dquoted_unquoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("\"{}\": <redacted>", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_squoted_unquoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("'{}': <redacted>", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_unquoted_dquoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("{}: \"<redacted>\"", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_unquoted_squoted
            .replace_all(&s, |caps: &Captures| {
                let key = &caps[1];
                if is_sensitive_name(key) {
                    format!("{}: '<redacted>'", key)
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        s = p
            .json_unquoted_unquoted
            .replace_all(&s, |caps: &Captures| {
                let prefix = caps.get(1).map_or("", |m| m.as_str());
                let key = &caps[2];
                let val = &caps[3];
                if key.eq_ignore_ascii_case("authorization")
                    || key.eq_ignore_ascii_case("proxy-authorization")
                {
                    caps[0].to_string()
                } else if is_sensitive_name(key) {
                    if val.eq_ignore_ascii_case("bearer")
                        || val.eq_ignore_ascii_case("basic")
                        || val == "<redacted>"
                    {
                        caps[0].to_string()
                    } else {
                        format!("{}{}: <redacted>", prefix, key)
                    }
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        // 8b. Generic secret key=value rule:
        // Anchored by start/delimiter for ':' or non-alnum for '='.
        // Key set: secret, password, passwd, pwd, token, api_key, apikey, access_key, auth_token, credential, private_key, client_secret
        // (with optional prefix like FOO_TOKEN or --api-key). Values are min 4 chars and consume full quoted strings.
        s = p
            .generic_secret_kv
            .replace_all(&s, |caps: &Captures| {
                let (prefix, key, sep) = if let Some(k) = caps.get(2) {
                    (
                        caps.get(1).map_or("", |m| m.as_str()),
                        k.as_str(),
                        caps.get(3).unwrap().as_str(),
                    )
                } else if let Some(k) = caps.get(4) {
                    ("", k.as_str(), caps.get(5).unwrap().as_str())
                } else if let Some(k) = caps.get(7) {
                    (
                        caps.get(6).map_or("", |m| m.as_str()),
                        k.as_str(),
                        caps.get(8).unwrap().as_str(),
                    )
                } else {
                    return caps[0].to_string();
                };

                let full_val = caps.get(9).unwrap().as_str();
                if full_val.contains("<redacted>") {
                    return caps[0].to_string();
                }

                if caps.get(10).is_some() {
                    // Double-quoted
                    if sep == "=" {
                        format!("{}{}=\"<redacted>\"", prefix, key)
                    } else {
                        format!("{}{}: \"<redacted>\"", prefix, key)
                    }
                } else if caps.get(11).is_some() {
                    // Single-quoted
                    if sep == "=" {
                        format!("{}{}'<redacted>'", prefix, key)
                    } else {
                        format!("{}{}: '<redacted>'", prefix, key)
                    }
                } else {
                    // Unquoted
                    if sep == "=" {
                        format!("{}{}=<redacted>", prefix, key)
                    } else {
                        format!("{}{}: <redacted>", prefix, key)
                    }
                }
            })
            .into_owned();

        // 9. Hex runs >= 32 characters (excluding context-verified 40-hex Git SHAs and 64-hex digests)
        s = p
            .hex_run
            .replace_all(&s, |caps: &Captures| {
                let m = &caps[0];
                let match_start = caps.get(0).unwrap().start();
                if is_exempt_value_shape(&s, match_start, m, false) {
                    m.to_string()
                } else {
                    "<redacted>".to_string()
                }
            })
            .into_owned();

        // 10. Base64 runs >= 32 characters (excluding file paths, Git SHAs, digests, identifiers, and non-secrets)
        s = p
            .base64_run
            .replace_all(&s, |caps: &Captures| {
                let m = &caps[0];
                let match_start = caps.get(0).unwrap().start();
                if is_exempt_value_shape(&s, match_start, m, true) {
                    m.to_string()
                } else {
                    "<redacted>".to_string()
                }
            })
            .into_owned();

        s
    }
}

/// Helper to determine if a hex or base64 value shape should be exempted from redaction.
fn is_exempt_value_shape(s: &str, pos: usize, m: &str, is_base64: bool) -> bool {
    if is_git_sha_40_in_context(s, pos, m) || is_digest_context(s, pos, m) {
        return true;
    }
    if is_base64 && (is_likely_file_path(m) || is_likely_identifier_or_test_name(m)) {
        return true;
    }
    false
}

/// Checks if a 40-hex string is immediately preceded by a git context token.
fn is_git_sha_40_in_context(s: &str, pos: usize, m: &str) -> bool {
    if m.len() != 40 || !m.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let bytes = s.as_bytes();
    let mut end = pos;
    // Trim trailing spaces, tabs, '=', and ':'
    while end > 0 && matches!(bytes[end - 1], b' ' | b'\t' | b'=' | b':') {
        end -= 1;
    }
    let mut trimmed = &bytes[..end];

    // Range forms: e.g. HEAD..<sha>, v1.0..<sha>, <sha1>..<sha2>, <sha1>...<sha2>
    if trimmed.ends_with(b"..") || trimmed.ends_with(b"...") {
        return true;
    }

    // Skip flags immediately preceding the SHA (e.g. `git reset --hard <sha>`)
    while trimmed.ends_with(b"-")
        || trimmed
            .rsplit(|&b| b == b' ' || b == b'\t')
            .next()
            .is_some_and(|word| word.starts_with(b"-"))
    {
        let last_space = trimmed
            .iter()
            .rposition(|&b| b == b' ' || b == b'\t')
            .unwrap_or(0);
        let mut prev_end = last_space;
        while prev_end > 0 && matches!(bytes[prev_end - 1], b' ' | b'\t' | b'=' | b':') {
            prev_end -= 1;
        }
        if prev_end == trimmed.len() {
            break;
        }
        trimmed = &bytes[..prev_end];
    }

    const GIT_CMD_TOKENS: &[&[u8]] = &[
        b"commit",
        b"checkout",
        b"cherry-pick",
        b"revert",
        b"rebase",
        b"reset",
        b"merge-base",
        b"rev-parse",
        b"sha",
        b"sha1",
        b"show",
        b"diff",
        b"log",
        b"tag",
        b"branch",
        b"merge",
        b"blame",
        b"bisect",
        b"tree",
        b"parent",
        b"rev",
        b"ref",
        b"refs/heads/",
        b"refs/tags/",
        b"refs/remotes/",
        b"refs/",
        b"head",
    ];

    for tok in GIT_CMD_TOKENS {
        if trimmed.len() >= tok.len() {
            let start = trimmed.len() - tok.len();
            let suffix = &trimmed[start..];
            if suffix
                .iter()
                .zip(tok.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
                && (start == 0 || !trimmed[start - 1].is_ascii_alphanumeric())
            {
                return true;
            }
        }
    }

    false
}

/// Checks if a 64-hex string is immediately preceded by a digest/checksum token or in `sha256sum -c` format.
fn is_digest_context(s: &str, pos: usize, hex: &str) -> bool {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let bytes = s.as_bytes();
    let window_start = pos.saturating_sub(32);
    let prefix = &bytes[window_start..pos];

    const DIGEST_PREFIXES: &[&[u8]] = &[
        b"sha256:",
        b"sha256=",
        b"sha256 ",
        b"sha512:",
        b"sha1:",
        b"blake3:",
        b"blake3=",
        b"md5:",
        b"digest:",
        b"digest=",
        b"checksum:",
        b"checksum=",
        b"checksum ",
        b"sha256sum ",
        b"shasum ",
        b"b2sum ",
        b"md5sum ",
    ];

    for tok in DIGEST_PREFIXES {
        if prefix.len() >= tok.len() {
            let start = prefix.len() - tok.len();
            let suffix = &prefix[start..];
            if suffix
                .iter()
                .zip(tok.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
            {
                return true;
            }
        }
    }

    let end = pos + hex.len();
    if end < bytes.len() {
        let suffix = &bytes[end..];
        if suffix.starts_with(b"  ") || suffix.starts_with(b" *") {
            return true;
        }
    }
    false
}

/// Checks if a run of 32+ characters consists entirely of ASCII alphabetic characters,
/// indicating an identifier or test name rather than a high-entropy secret.
fn is_likely_identifier_or_test_name(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphabetic())
}

/// Checks if a word before `-p` belongs to a known tool where `-p` is not a password.
fn is_non_secret_p_context(prefix: &str) -> bool {
    let trimmed = prefix.trim_end();
    let last_word = trimmed.split_whitespace().last().unwrap_or("");
    let last_program = last_word.rsplit('/').next().unwrap_or(last_word);
    matches!(
        last_program,
        "cargo" | "mkdir" | "git" | "cp" | "rm" | "test" | "build" | "check" | "clippy" | "run"
    )
}

/// Determines whether a flag or variable name is sensitive.
pub fn is_sensitive_name(raw: &str) -> bool {
    let clean = raw.trim_start_matches('-');
    let lower = clean.to_ascii_lowercase();
    let norm = lower.replace('-', "_");

    if norm.ends_with("_stdin")
        || norm.ends_with("-stdin")
        || norm.ends_with("_prompt")
        || norm.ends_with("-prompt")
    {
        return false;
    }

    match norm.as_str() {
        "token"
        | "secret"
        | "password"
        | "passwd"
        | "pass"
        | "pwd"
        | "key"
        | "api_key"
        | "apikey"
        | "auth"
        | "authorization"
        | "proxy_authorization"
        | "credential"
        | "credentials"
        | "private_key"
        | "session"
        | "session_id"
        | "cookie"
        | "set_cookie"
        | "bearer"
        | "pgpass"
        | "pgpassword"
        | "whsec" => return true,
        _ => {}
    }

    if norm.ends_with("_key")
        || norm.ends_with("_token")
        || norm.ends_with("_secret")
        || norm.ends_with("_password")
        || norm.ends_with("_passwd")
        || norm.ends_with("_pass")
        || norm.ends_with("_pwd")
        || norm.ends_with("_auth")
        || norm.ends_with("_credential")
        || norm.ends_with("_credentials")
        || norm.ends_with("password")
        || norm.ends_with("passwd")
        || norm.ends_with("token")
        || norm.ends_with("secret")
        || norm.ends_with("apikey")
        || norm.ends_with("api_key")
        || norm.ends_with("privatekey")
        || norm.ends_with("private_key")
        || norm.ends_with("credential")
        || norm.ends_with("credentials")
        || norm.ends_with("authorization")
        || norm.ends_with("cookie")
        || norm.starts_with("key_")
        || norm.starts_with("token_")
        || norm.starts_with("secret_")
        || norm.starts_with("password_")
        || norm.starts_with("passwd_")
        || norm.starts_with("auth_")
        || norm.starts_with("api_key")
        || norm.starts_with("apikey")
        || norm.starts_with("private_key")
        || norm.starts_with("whsec")
    {
        return true;
    }

    if norm.contains("api_key")
        || norm.contains("apikey")
        || norm.contains("private_key")
        || norm.contains("auth_token")
        || norm.contains("access_token")
        || norm.contains("secret_key")
        || norm.contains("refresh_token")
    {
        return true;
    }

    false
}

/// Checks if a string containing slashes is a filesystem path rather than base64 data.
fn is_likely_file_path(s: &str) -> bool {
    if !s.contains('/') {
        return false;
    }
    if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with("~/") {
        return true;
    }
    let extensions = [
        ".rs", ".ts", ".js", ".jsx", ".tsx", ".py", ".go", ".c", ".h", ".cpp", ".cc", ".java",
        ".json", ".toml", ".yaml", ".yml", ".md", ".txt", ".sh", ".proto", ".lock", ".xml",
        ".html", ".css", ".sql", ".log",
    ];
    for ext in &extensions {
        if s.ends_with(ext) {
            return true;
        }
    }
    if !s.contains('=') && !s.contains('+') {
        let all_short = s.split('/').all(|part| part.len() <= 30);
        if all_short {
            return true;
        }
    }
    false
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
    use proptest::prelude::*;

    #[test]
    fn test_truncate_utf8() {
        assert_eq!(truncate_utf8("hello world", 5), "he…");
        assert_eq!(truncate_utf8("hello", 10), "hello");
        assert_eq!(truncate_utf8("hello", 0), "");
        let s = "€€€";
        let t = truncate_utf8(s, 6);
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

    static SCRUB_TABLE: &[(&str, &str)] = &[
            // 1. Quoted flags
            (
                "tool --token=\"secret123\"",
                "tool --token=\"<redacted>\"",
            ),
            (
                "tool --token='secret123'",
                "tool --token='<redacted>'",
            ),
            (
                "tool --password \"shhh\"",
                "tool --password \"<redacted>\"",
            ),
            (
                "tool --password 'shhh'",
                "tool --password '<redacted>'",
            ),
            (
                "tool --secret=\"topsecret\"",
                "tool --secret=\"<redacted>\"",
            ),
            (
                "tool --secret='topsecret'",
                "tool --secret='<redacted>'",
            ),
            (
                "tool --api-key=\"abcde12345\"",
                "tool --api-key=\"<redacted>\"",
            ),
            (
                "tool --api-key='abcde12345'",
                "tool --api-key='<redacted>'",
            ),
            (
                "tool --api_key=\"abcde12345\"",
                "tool --api_key=\"<redacted>\"",
            ),
            (
                "tool --api_key='abcde12345'",
                "tool --api_key='<redacted>'",
            ),

            // 2. Underscore and hyphen flags (unquoted)
            (
                "tool --api_key=mysecret",
                "tool --api_key=<redacted>",
            ),
            (
                "tool --api_key mysecret",
                "tool --api_key <redacted>",
            ),
            (
                "tool --api-key=mysecret",
                "tool --api-key=<redacted>",
            ),
            (
                "tool --api-key mysecret",
                "tool --api-key <redacted>",
            ),
            (
                "tool --auth_token=mysecret",
                "tool --auth_token=<redacted>",
            ),
            (
                "tool --auth-token mysecret",
                "tool --auth-token <redacted>",
            ),
            (
                "tool --private_key=mysecret",
                "tool --private_key=<redacted>",
            ),
            (
                "tool --private-key mysecret",
                "tool --private-key <redacted>",
            ),
            (
                "tool --session_id=mysecret",
                "tool --session_id=<redacted>",
            ),
            (
                "tool --cookie mysecret",
                "tool --cookie <redacted>",
            ),
            (
                "tool --bearer mysecret",
                "tool --bearer <redacted>",
            ),
            (
                "tool --password shhh",
                "tool --password <redacted>",
            ),
            (
                "tool --token xyz123",
                "tool --token <redacted>",
            ),

            // 3. Short flags (-p and -u)
            (
                "mysql -p'mypass'",
                "mysql -p'<redacted>'",
            ),
            (
                "mysql -p\"mypass\"",
                "mysql -p\"<redacted>\"",
            ),
            (
                "mysql -ppassword123",
                "mysql -p<redacted>",
            ),
            (
                "mysql -p password123",
                "mysql -p <redacted>",
            ),
            (
                "psql -p secret",
                "psql -p <redacted>",
            ),
            (
                "tool -p 'pass'",
                "tool -p '<redacted>'",
            ),
            (
                "tool -p \"pass\"",
                "tool -p \"<redacted>\"",
            ),
            (
                "curl -u user:pass",
                "curl -u user:<redacted>",
            ),
            (
                "curl -u 'user:pass'",
                "curl -u 'user:<redacted>'",
            ),
            (
                "curl -u \"user:pass\"",
                "curl -u \"user:<redacted>\"",
            ),
            (
                "curl -uuser:pass",
                "curl -uuser:<redacted>",
            ),
            (
                "curl --user user:pass",
                "curl --user user:<redacted>",
            ),
            (
                "curl --user=user:pass",
                "curl --user=user:<redacted>",
            ),
            (
                "curl -u :secretpass",
                "curl -u :<redacted>",
            ),

            // 4. Prefixed env vars / assignments
            (
                "MY_API_KEY=val",
                "MY_API_KEY=<redacted>",
            ),
            (
                "FOO_KEY=val",
                "FOO_KEY=<redacted>",
            ),
            (
                "DATABASE_PASSWORD=val",
                "DATABASE_PASSWORD=<redacted>",
            ),
            (
                "DATABASE_PASSWORD=\"val\"",
                "DATABASE_PASSWORD=\"<redacted>\"",
            ),
            (
                "DATABASE_PASSWORD='val'",
                "DATABASE_PASSWORD='<redacted>'",
            ),
            (
                "GITHUB_TOKEN=secret",
                "GITHUB_TOKEN=<redacted>",
            ),
            (
                "SLACK_TOKEN=secret",
                "SLACK_TOKEN=<redacted>",
            ),
            (
                "APP_SECRET=secret",
                "APP_SECRET=<redacted>",
            ),
            (
                "SECRET_KEY=val",
                "SECRET_KEY=<redacted>",
            ),
            (
                "AUTH_TOKEN=val",
                "AUTH_TOKEN=<redacted>",
            ),
            (
                "ACCESS_TOKEN=val",
                "ACCESS_TOKEN=<redacted>",
            ),
            (
                "REFRESH_TOKEN=val",
                "REFRESH_TOKEN=<redacted>",
            ),
            (
                "export MY_API_KEY=val",
                "export MY_API_KEY=<redacted>",
            ),
            (
                "key=mykey",
                "key=<redacted>",
            ),
            (
                "secret=mysecret",
                "secret=<redacted>",
            ),
            (
                "token=mytoken",
                "token=<redacted>",
            ),
            (
                "password=mypass",
                "password=<redacted>",
            ),
            (
                "passwd=mypasswd",
                "passwd=<redacted>",
            ),

            // 5. JSON in argv
            (
                "{\"token\": \"secret\"}",
                "{\"token\": \"<redacted>\"}",
            ),
            (
                "{\"api_key\": \"secret\"}",
                "{\"api_key\": \"<redacted>\"}",
            ),
            (
                "{\"password\": \"secret\"}",
                "{\"password\": \"<redacted>\"}",
            ),
            (
                "{\"private_key\": \"secret\"}",
                "{\"private_key\": \"<redacted>\"}",
            ),
            (
                "{\"secret\": \"val\"}",
                "{\"secret\": \"<redacted>\"}",
            ),
            (
                "{'token': 'secret'}",
                "{'token': '<redacted>'}",
            ),
            (
                "{'password': 'secret'}",
                "{'password': '<redacted>'}",
            ),

            // 6. YAML and key-value pairs
            (
                "token: secret",
                "token: <redacted>",
            ),
            (
                "token: \"secret\"",
                "token: \"<redacted>\"",
            ),
            (
                "token: 'secret'",
                "token: '<redacted>'",
            ),
            (
                "password: secret",
                "password: <redacted>",
            ),
            (
                "api_key: secret",
                "api_key: <redacted>",
            ),

            // 7. URL userinfo
            (
                "git clone https://zezeozue:secretpass@github.com/repo.git",
                "git clone https://<redacted>@github.com/repo.git",
            ),
            (
                "curl http://admin:pass123@example.com/api",
                "curl http://<redacted>@example.com/api",
            ),
            (
                "git clone git+ssh://git:key123@example.com/repo.git",
                "git clone git+ssh://<redacted>@example.com/repo.git",
            ),

            // 8. Auth & Bearer headers
            (
                "curl -H 'Authorization: Bearer my-secret-token-12345' https://example.com",
                "curl -H 'Authorization: Bearer <redacted>' https://example.com",
            ),
            (
                "curl -H \"Authorization: Bearer secret\"",
                "curl -H \"Authorization: Bearer <redacted>\"",
            ),
            (
                "curl -H 'Authorization: Basic dXNlcjpwYXNz'",
                "curl -H 'Authorization: Basic <redacted>'",
            ),
            (
                "Bearer my-secret-token-xyz",
                "Bearer <redacted>",
            ),

            // 9. Vendor tokens
            (
                concat!("AWS: AKIA", "IOSFODNN7EXAMPLE"),
                "AWS: <redacted>",
            ),
            (
                concat!("aws: akia", "iosfodnn7example"),
                "aws: <redacted>",
            ),
            (
                concat!("ASIA", "IOSFODNN7EXAMPLE"),
                "<redacted>",
            ),
            (
                concat!("gcp token: ya29.", "a0AfH6SMCx1234567890abcdef"),
                "gcp token: <redacted>",
            ),
            (
                concat!("GCP: AIza", "SyD-1234567890abcdefghijklmnopqrstuv"),
                "GCP: <redacted>",
            ),
            (
                concat!("GitHub: ghp_", "1234567890abcdefghijklmnopqrstuvwxyzAB"),
                "GitHub: <redacted>",
            ),
            (
                concat!("GitHub: gho_", "1234567890abcdefghijklmnopqrstuvwxyzAB"),
                "GitHub: <redacted>",
            ),
            (
                concat!("GitHub: ghs_", "1234567890abcdefghijklmnopqrstuvwxyzAB"),
                "GitHub: <redacted>",
            ),
            (
                concat!(
                    "GitHub PAT: github_pat_",
                    "11ABCD1234567890abcdefghijklmnopqrstuvwxyz1234567890abcdefghijklmnopqrstuvwxyz12"
                ),
                "GitHub PAT: <redacted>",
            ),
            (
                concat!(
                    "Anthropic: sk-ant-",
                    "api03-1234567890abcdefghijklmnopqrstuvwxyz123456"
                ),
                "Anthropic: <redacted>",
            ),
            (
                concat!("OpenAI: sk-", "1234567890abcdefghijklmnopqrstuvwxyz123456"),
                "OpenAI: <redacted>",
            ),
            (
                concat!(
                    "OpenAI: sk-proj-",
                    "1234567890abcdefghijklmnopqrstuvwxyz123456"
                ),
                "OpenAI: <redacted>",
            ),
            (
                concat!("Slack: xoxb-", "1234567890-1234567890123-abc"),
                "Slack: <redacted>",
            ),
            (
                concat!("Slack: xoxp-", "1234567890-1234567890123-abc"),
                "Slack: <redacted>",
            ),
            (
                concat!("Slack: xoxa-", "1234567890-1234567890123-abc"),
                "Slack: <redacted>",
            ),
            (
                concat!("Slack: xoxr-", "1234567890-1234567890123-abc"),
                "Slack: <redacted>",
            ),
            (
                "jwt: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
                "jwt: <redacted>",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0...\n-----END RSA PRIVATE KEY-----",
                "<redacted>",
            ),
            (
                "-----begin private key-----\nmieo...\n-----end private key-----",
                "<redacted>",
            ),

            // 10. Hex runs & Base64 runs (>= 32 chars)
            (
                "hash 0123456789abcdef0123456789abcdef",
                "hash <redacted>",
            ),
            (
                "secret: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "secret: <redacted>",
            ),
            (
                "token YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoxMjM0NTY=",
                "token <redacted>",
            ),
            (
                "YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoxMjM0==",
                "<redacted>",
            ),
            (
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                "<redacted>",
            ),
            (
                "curl https://example.com/checksum?x=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "curl https://example.com/checksum?x=<redacted>",
            ),
            (
                "/tmp/dir_secret=my_canary_value_99/sub",
                "/tmp/dir_secret=<redacted>",
            ),

            // 11. Benign lines (at least 20 lines that MUST REMAIN UNTOUCHED)
            (
                "git checkout 0123456789abcdef0123456789abcdef01234567",
                "git checkout 0123456789abcdef0123456789abcdef01234567",
            ),
            (
                "commit 4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                "commit 4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            ),
            (
                "git show 4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                "git show 4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            ),
            (
                "git reset --hard 4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                "git reset --hard 4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            ),
            (
                "ls -la /usr/local/google/home/zezeozue/dev/tachograph/crates/tacho-core/src/scrub.rs",
                "ls -la /usr/local/google/home/zezeozue/dev/tachograph/crates/tacho-core/src/scrub.rs",
            ),
            (
                "cat /var/log/syslog/extra_long_directory_name/service.log",
                "cat /var/log/syslog/extra_long_directory_name/service.log",
            ),
            (
                "cargo test test_order_550e8400-e29b-41d4-a716-446655440000",
                "cargo test test_order_550e8400-e29b-41d4-a716-446655440000",
            ),
            (
                "pytest tests/test_user_123e4567-e89b-12d3-a456-426614174000.py",
                "pytest tests/test_user_123e4567-e89b-12d3-a456-426614174000.py",
            ),
            (
                "mkdir -p /tmp/test_dir/nested",
                "mkdir -p /tmp/test_dir/nested",
            ),
            (
                "cargo test -p core --lib",
                "cargo test -p core --lib",
            ),
            (
                "cargo build -p tacho-core",
                "cargo build -p tacho-core",
            ),
            (
                "git checkout -p",
                "git checkout -p",
            ),
            (
                "git push origin main",
                "git push origin main",
            ),
            (
                "echo 'hello world'",
                "echo 'hello world'",
            ),
            (
                "pwd",
                "pwd",
            ),
            (
                "RUST_LOG=debug cargo run",
                "RUST_LOG=debug cargo run",
            ),
            (
                "python3 -m pytest tests/unit",
                "python3 -m pytest tests/unit",
            ),
            (
                "curl -H 'Content-Type: application/json' https://api.example.com/items",
                "curl -H 'Content-Type: application/json' https://api.example.com/items",
            ),
            (
                "docker build -t my-image:v1.0.0 .",
                "docker build -t my-image:v1.0.0 .",
            ),
            (
                "PGPASSWORD=mysecret123 psql -h localhost",
                "PGPASSWORD=<redacted> psql -h localhost",
            ),
            (
                "PGPASS=mysecret123 psql",
                "PGPASS=<redacted> psql",
            ),
            (
                "curl -H 'X-Api-Key: secret123' https://api.example.com",
                "curl -H 'X-Api-Key: <redacted>' https://api.example.com",
            ),
            (
                "curl --header=\"Authorization: Bearer my-secret-token\"",
                "curl --header=\"Authorization: Bearer <redacted>\"",
            ),
            (
                "mysql -uroot -psecret123",
                "mysql -uroot -p<redacted>",
            ),
            (
                "docker login -u myuser -p mysecretpass",
                "docker login -u myuser -p <redacted>",
            ),
            (
                "machine github.com login user password mynetrcpassword",
                "machine github.com login user password <redacted>",
            ),
            (
                concat!("stripe key: sk_", "live_51Abcdefghijklmnopqrstuvw"),
                "stripe key: <redacted>",
            ),
            (
                concat!("npm: npm_", "0123456789abcdefghijklmnopqrstuv"),
                "npm: <redacted>",
            ),
            (
                concat!("gitlab: glpat-", "abcdefghijklmnopqrst"),
                "gitlab: <redacted>",
            ),
            (
                "curl --token \\\n  mysecret123",
                "curl --token <redacted>",
            ),
            (
                "export MY_SECRET_TOKEN=\\\n  secretvalue",
                "export MY_SECRET_TOKEN=<redacted>",
            ),
            (
                "tar -czf archive.tar.gz src/ tests/",
                "tar -czf archive.tar.gz src/ tests/",
            ),
            (
                "gcc -pthread -O2 test.c",
                "gcc -pthread -O2 test.c",
            ),
            (
                "clang -pipe test.c",
                "clang -pipe test.c",
            ),
            (
                "find . -print",
                "find . -print",
            ),
            (
                "echo \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  empty.txt\" | sha256sum -c",
                "echo \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  empty.txt\" | sha256sum -c",
            ),
            (
                "docker pull ubuntu@sha256:7247f6e5c5214238d1759b61373d1546573f76e3473ff00c036ac143cc84981a",
                "docker pull ubuntu@sha256:7247f6e5c5214238d1759b61373d1546573f76e3473ff00c036ac143cc84981a",
            ),
            (
                "cargo test testUserRegistrationWithValidEmailAndPasswordConfirmation",
                "cargo test testUserRegistrationWithValidEmailAndPasswordConfirmation",
            ),
            (
                "docker login --password-stdin",
                "docker login --password-stdin",
            ),
            (
                "curl --token \\\nsecrettoken1234567890 https://api.com",
                "curl --token <redacted> https://api.com",
            ),
            (
                "mysql -u root -p\\\nsecretpassword123",
                "mysql -u root -p <redacted>",
            ),
            (
                "curl -u \\\nmyuser:secretpassword123 https://api.com",
                "curl -u myuser:<redacted> https://api.com",
            ),
            (
                "echo '{\"token\": \\\n\"secrettoken123\"}'",
                "echo '{\"token\": \"<redacted>\"}'",
            ),
            (
                "curl -H 'Authorization: ApiKey mytoken123' https://api.com",
                "curl -H 'Authorization: ApiKey <redacted>' https://api.com",
            ),
            (
                "curl -H 'Authorization: Bearer \\\nmysecret123' https://api.com",
                "curl -H 'Authorization: Bearer <redacted>' https://api.com",
            ),
            (
                concat!(
                    "stripe listen --whsec whsec_",
                    "1234567890abcdef1234567890abcdef"
                ),
                "stripe listen --whsec <redacted>",
            ),
            (
                "cat <<EOF > ~/.netrc\nmachine api.com\nlogin u\npassword p\nEOF",
                "cat <<EOF > ~/.netrc\nmachine api.com\nlogin u\npassword <redacted>\nEOF",
            ),
            (
                "echo 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  target.tar.gz' | sha256sum -c",
                "echo 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  target.tar.gz' | sha256sum -c",
            ),
            (
                "curl -H 'Proxy-Authorization: Basic dXNlcjpwYXNz' https://example.com",
                "curl -H 'Proxy-Authorization: Basic <redacted>' https://example.com",
            ),
            (
                "curl -H 'Cookie: session=12345; user=alice' https://example.com",
                "curl -H 'Cookie: <redacted>' https://example.com",
            ),
            (
                "curl -H 'Set-Cookie: session=12345; Secure; HttpOnly' https://example.com",
                "curl -H 'Set-Cookie: <redacted>' https://example.com",
            ),
            (
                "curl -H 'X-Auth-Token: secrettoken12345' https://example.com",
                "curl -H 'X-Auth-Token: <redacted>' https://example.com",
            ),
        ];

    #[test]
    fn test_scrubber_opt_in_default_disabled() {
        let scrubber = Scrubber::with_enabled(false);
        assert!(!scrubber.is_enabled());
        let raw = "curl -H 'Authorization: Bearer sk-ant-api03-1234567890abcdef1234567890abcdef' --password=secret";
        assert_eq!(scrubber.scrub(raw), raw);
    }

    #[test]
    fn test_scrubber_table() {
        let scrubber = Scrubber::enabled();
        assert!(
            SCRUB_TABLE.len() >= 80,
            "Must test at least 80 cases, got {}",
            SCRUB_TABLE.len()
        );

        for &(input, expected) in SCRUB_TABLE {
            let actual = scrubber.scrub(input);
            assert_eq!(
                actual, expected,
                "Scrubbing {:?} produced {:?}, expected {:?}",
                input, actual, expected
            );
        }
    }

    proptest! {
        #[test]
        fn test_scrub_idempotent(s in ".*") {
            let scrubber = Scrubber::enabled();
            let first = scrubber.scrub(&s);
            let second = scrubber.scrub(&first);
            prop_assert_eq!(first, second);
        }

        #[test]
        fn test_scrub_no_panic_arbitrary_utf8(s in "\\PC*") {
            let scrubber = Scrubber::enabled();
            let _ = scrubber.scrub(&s);
        }

        #[test]
        fn test_scrub_leak_cases_with_line_continuations(
            case_idx in 0..150usize,
            boundary_idx in 0..50usize,
            crlf in proptest::bool::ANY,
            spaces in 0..3usize,
        ) {
            let scrubber = Scrubber::enabled();
            let leak_cases: Vec<(&str, &str)> = SCRUB_TABLE
                .iter()
                .copied()
                .filter(|(_, exp)| exp.contains("<redacted>"))
                .collect();
            prop_assume!(!leak_cases.is_empty());
            let (input, _expected) = leak_cases[case_idx % leak_cases.len()];

            let boundaries: Vec<usize> = input
                .char_indices()
                .filter(|&(i, c)| {
                    if c.is_whitespace() && c != '\n' && c != '\r' {
                        let prev = if i > 0 { input[..i].chars().next_back() } else { None };
                        let next = input[i..].chars().nth(1);
                        prev != Some('\\') && next != Some('\\')
                    } else {
                        c == '=' && i + 1 < input.len() && !input[..i].contains("://")
                    }
                })
                .map(|(i, c)| if c == '=' { i + 1 } else { i })
                .collect();

            if !boundaries.is_empty() {
                let idx = boundaries[boundary_idx % boundaries.len()];
                let nl = if crlf { "\\\r\n" } else { "\\\n" };
                let pad = " ".repeat(spaces);
                let continuation = format!(" {}{}", nl, pad);
                let wrapped = format!("{}{}{}", &input[..idx], continuation, &input[idx..]);

                let scrubbed = scrubber.scrub(&wrapped);
                prop_assert!(
                    scrubbed.contains("<redacted>"),
                    "Wrapped input {:?} scrubbed to {:?}, expected to contain <redacted>",
                    wrapped,
                    scrubbed
                );
            }
        }

        #[test]
        fn test_scrub_no_panic_multibyte_prefix_hex(
            prefix in "\\PC*",
            hex in "[0-9a-fA-F]{40,64}",
            suffix in "\\PC*",
        ) {
            let scrubber = Scrubber::enabled();
            let input = format!("{}{}{}", prefix, hex, suffix);
            let _ = scrubber.scrub(&input);
        }
    }

    #[test]
    fn test_git_sha_exemption_context() {
        let scrubber = Scrubber::enabled();
        let sha40 = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

        // Redacted cases (exemption should not apply)
        assert_eq!(
            scrubber.scrub(&format!("catalog {}", sha40)),
            "catalog <redacted>"
        );
        assert_eq!(
            scrubber.scrub(&format!("git clone x && echo {}", sha40)),
            "git clone x && echo <redacted>"
        );

        // Preserved cases (immediately preceding git token)
        assert_eq!(
            scrubber.scrub(&format!("git checkout {}", sha40)),
            format!("git checkout {}", sha40)
        );
        assert_eq!(
            scrubber.scrub(&format!("git show {}", sha40)),
            format!("git show {}", sha40)
        );
    }

    #[test]
    fn test_generic_secret_kv_rule() {
        let scrubber = Scrubber::enabled();

        assert_eq!(scrubber.scrub("password: hunter2"), "password: <redacted>");
        assert_eq!(
            scrubber.scrub("token: \"abc def ghi\""),
            "token: \"<redacted>\""
        );
        assert_eq!(
            scrubber.scrub("FOO_TOKEN=abcd1234 cmd"),
            "FOO_TOKEN=<redacted> cmd"
        );
        assert_eq!(scrubber.scrub("--api-key abcdefgh"), "--api-key <redacted>");
        // Prose forms must NOT be redacted
        assert_eq!(
            scrubber.scrub("the secret: implementation"),
            "the secret: implementation"
        );
        assert_eq!(
            scrubber.scrub("see the secret: implementation_detail"),
            "see the secret: implementation_detail"
        );
    }

    #[test]
    fn test_scrub_multibyte_boundary_no_panic() {
        let scrubber = Scrubber::enabled();
        let sha40 = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let sha64 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for pad_len in 0..70 {
            let input40 = format!("🦀{} {}", "a".repeat(pad_len), sha40);
            let _ = scrubber.scrub(&input40);
            let input64 = format!("🦀{} {}", "a".repeat(pad_len), sha64);
            let _ = scrubber.scrub(&input64);
        }
    }
}
