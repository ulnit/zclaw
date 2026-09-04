//! Secret redaction — port of hermes' `agent/redact.py` (core subset).
//!
//! Regex-based masking of API keys, tokens, and credentials before they
//! reach tool output returned to the model. Short tokens are fully
//! masked; longer tokens keep a head/tail slice for debuggability (log
//! mode) or become a non-reusable `«redacted:prefix…»` sentinel when the
//! text is file content the agent may write back (hermes #35519).
//!
//! Passes: vendor-prefix tokens, ENV assignments, config-file
//! assignments, JSON fields, auth headers, x-api-key headers, private
//! key blocks, DB connection strings, bare-token URL userinfo, JWTs.
//! Web-URL query-string redaction stays opt-in (`redact_url_credentials`)
//! because OAuth callback / magic-link / pre-signed URLs must survive
//! ordinary tool flows unchanged.

use regex::Regex;
use std::sync::OnceLock;

/// Mask a secret for display, preserving `head` and `tail` characters.
/// Values shorter than `floor` are fully masked (`***`).
pub fn mask_secret(value: &str, head: usize, tail: usize, floor: usize) -> String {
    if value.is_empty() {
        return String::new();
    }
    if value.len() < floor {
        return "***".to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    let head: String = chars.iter().take(head).collect();
    let tail: String = chars
        .iter()
        .skip(chars.len().saturating_sub(tail))
        .collect();
    format!("{head}...{tail}")
}

/// Conservative log-token mask: 18-char floor, 6 prefix / 4 suffix kept.
fn mask_token(token: &str) -> String {
    if token.is_empty() {
        return "***".to_string();
    }
    mask_secret(token, 6, 4, 18)
}

/// Vendor prefix patterns (hermes `_PREFIX_PATTERNS`).
const PREFIX_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_-]{10,}",
    r"ghp_[A-Za-z0-9]{10,}",
    r"github_pat_[A-Za-z0-9_]{10,}",
    r"gho_[A-Za-z0-9]{10,}",
    r"ghu_[A-Za-z0-9]{10,}",
    r"ghs_[A-Za-z0-9]{10,}",
    r"ghr_[A-Za-z0-9]{10,}",
    r"xapp-\d+-[A-Za-z0-9-]{10,}",
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    r"AIza[A-Za-z0-9_-]{30,}",
    r"pplx-[A-Za-z0-9]{10,}",
    r"fal_[A-Za-z0-9_-]{10,}",
    r"fc-[A-Za-z0-9]{10,}",
    r"bb_live_[A-Za-z0-9_-]{10,}",
    r"gAAAA[A-Za-z0-9_=-]{20,}",
    r"AKIA[A-Z0-9]{16}",
    r"sk_live_[A-Za-z0-9]{10,}",
    r"sk_test_[A-Za-z0-9]{10,}",
    r"rk_live_[A-Za-z0-9]{10,}",
    r"SG\.[A-Za-z0-9_-]{10,}",
    r"hf_[A-Za-z0-9]{10,}",
    r"r8_[A-Za-z0-9]{10,}",
    r"npm_[A-Za-z0-9]{10,}",
    r"pypi-[A-Za-z0-9_-]{10,}",
    r"dop_v1_[A-Za-z0-9]{10,}",
    r"doo_v1_[A-Za-z0-9]{10,}",
    r"am_[A-Za-z0-9_-]{10,}",
    r"sk_[A-Za-z0-9_]{10,}",
    r"tvly-[A-Za-z0-9]{10,}",
    r"exa_[A-Za-z0-9]{10,}",
    r"gsk_[A-Za-z0-9]{10,}",
    r"syt_[A-Za-z0-9]{10,}",
    r"retaindb_[A-Za-z0-9]{10,}",
    r"hsk-[A-Za-z0-9]{10,}",
    r"mem0_[A-Za-z0-9]{10,}",
    r"brv_[A-Za-z0-9]{10,}",
    r"xai-[A-Za-z0-9]{30,}",
    r"ntn_[A-Za-z0-9]{10,}",
    r"fw-[A-Za-z0-9]{30,}",
    r"fw_[A-Za-z0-9]{30,}",
    r"fpk_[A-Za-z0-9]{30,}",
    r"glpat-[A-Za-z0-9_\-]{10,}",
    r"gloas-[A-Za-z0-9_\-]{10,}",
    r"gldt-[A-Za-z0-9_\-]{10,}",
    r"glrt-[A-Za-z0-9_.\-]{10,}",
    r"glrtr-[A-Za-z0-9_.\-]{10,}",
    r"glcbt-[A-Za-z0-9_\-]{10,}",
    r"glptt-[A-Za-z0-9_\-]{10,}",
    r"glft-[A-Za-z0-9_\-]{10,}",
    r"glimt-[A-Za-z0-9_\-]{10,}",
    r"glagent-[A-Za-z0-9_\-]{10,}",
    r"glsoat-[A-Za-z0-9_\-]{10,}",
    r"glffct-[A-Za-z0-9_\-]{10,}",
    r"glwt-[A-Za-z0-9_\-]{10,}",
    r"GR1348941[A-Za-z0-9_\-]{10,}",
];

/// Literal prefix substrings for the cheap pre-gate and the non-reusable
/// sentinel label (every pattern starts with one of these).
const PREFIX_SUBSTRINGS: &[&str] = &[
    "sk_live_",
    "sk_test_",
    "sk-",
    "sk_",
    "ghp_",
    "github_pat_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "xapp-",
    "xox",
    "AIza",
    "pplx-",
    "fal_",
    "fc-",
    "bb_live_",
    "gAAAA",
    "AKIA",
    "rk_live_",
    "SG.",
    "hf_",
    "r8_",
    "npm_",
    "pypi-",
    "dop_v1_",
    "doo_v1_",
    "am_",
    "tvly-",
    "exa_",
    "gsk_",
    "syt_",
    "retaindb_",
    "hsk-",
    "mem0_",
    "brv_",
    "xai-",
    "ntn_",
    "fw-",
    "fw_",
    "fpk_",
    "glpat-",
    "gloas-",
    "gldt-",
    "glrt-",
    "glrtr-",
    "glcbt-",
    "glptt-",
    "glft-",
    "glimt-",
    "glagent-",
    "glsoat-",
    "glffct-",
    "glwt-",
    "GR1348941",
];

fn prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let alternation = PREFIX_PATTERNS.join("|");
        Regex::new(&format!("({alternation})")).expect("static regex")
    })
}

fn has_known_prefix_substring(text: &str) -> bool {
    PREFIX_SUBSTRINGS.iter().any(|p| text.contains(p))
}

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// True when `text` contains what appears to be a vendor token prefix.
/// Emulates hermes `_PREFIX_RE` incl. its lookarounds: the characters
/// adjacent to the prefix must not be identifier chars (`[A-Za-z0-9_-]`),
/// so ordinary words that merely contain a prefix substring do not match.
/// Used to block URLs that embed secrets (hermes `web_tools` pre-check).
pub fn contains_token_prefix(text: &str) -> bool {
    if !has_known_prefix_substring(text) {
        return false;
    }
    for m in prefix_re().find_iter(text) {
        let before_ok = text[..m.start()]
            .chars()
            .next_back()
            .map_or(true, |c| !is_identifier_char(c));
        let after_ok = text[m.end()..]
            .chars()
            .next()
            .map_or(true, |c| !is_identifier_char(c));
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Non-reusable sentinel: keeps only the vendor prefix label, never any
/// secret bytes (hermes `_mask_token_nonreusable`).
fn mask_token_nonreusable(token: &str) -> String {
    if token.is_empty() {
        return "«redacted-secret»".to_string();
    }
    for sub in PREFIX_SUBSTRINGS {
        if token.starts_with(sub) {
            return format!("«redacted:{sub}…»");
        }
    }
    "«redacted-secret»".to_string()
}

fn env_assign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Quote (if any) captured separately; the value class excludes
        // quotes, so no backreference is needed to pair them.
        Regex::new(r#"([A-Z0-9_]{0,50}(?:API_?KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH)[A-Z0-9_]{0,50})\s*=\s*(['"]?)([^\s'"]+)"#)
            .expect("static regex")
    })
}

fn json_field_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)("(?:api_?[Kk]ey|token|secret|password|access_token|refresh_token|auth_token|bearer|secret_value|raw_secret|secret_input|key_material)")\s*:\s*"([^"]+)""#)
            .expect("static regex")
    })
}

fn yaml_assign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Bare secret-word keys at line start (optionally after leading
        // whitespace); value class excludes quotes so no backreference.
        Regex::new(
            r#"(?i)((?:^|[\s])(?:api[ _.\-]?key|token|secret|passwd|password|credential)[ \t]*)([:=])([ \t]*)(['"]?)([^\s'"]+)"#,
        )
        .expect("static regex")
    })
}

fn auth_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)((?:Proxy-)?Authorization:\s*)([A-Za-z][\w.+-]*\s+)?([^\s"']+)"#)
            .expect("static regex")
    })
}

fn secret_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)((?:x-api-key|x-goog-api-key|api-key|apikey|x-api-token|x-auth-token|x-access-token)\s*:\s*)(\S+)")
            .expect("static regex")
    })
}

fn private_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"-----BEGIN[A-Z ]*PRIVATE KEY-----[\s\S]*?-----END[A-Z ]*PRIVATE KEY-----")
            .expect("static regex")
    })
}

fn db_connstr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)((?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp)://[^:\s]+:)([^@\s]+)(@)",
        )
        .expect("static regex")
    })
}

fn url_bare_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)((?:https?|wss?|git|ssh|ftp|ftps|sftp)://)([^\s:@/]{8,})(@[^\s]+)")
            .expect("static regex")
    })
}

fn jwt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"eyJ[A-Za-z0-9_-]{10,}(?:\.[A-Za-z0-9_=-]{4,}){0,2}").expect("static regex")
    })
}

/// Programmatic env lookups reference variable *names*, not secret values
/// (hermes #2852): `KEY=os.getenv('X')` must not be masked.
fn env_lookup_value_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:os\.(?:getenv|environ)|process\.env|\$ENV\{)").expect("static regex")
    })
}

/// Secret keyword at a word boundary inside a key name (hermes
/// `_key_has_secret_keyword`): rejects prose like `author=Smith`.
/// All-caps ENV-shape keys short-circuit to true.
fn key_has_secret_keyword(key: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)(?:api|auth|access|refresh|session|secret)[ _.\-]?(?:key|token)|token|secret|passwd|password|credential|auth")
            .expect("static regex")
    });
    if key.chars().all(|c| !c.is_ascii_lowercase()) {
        return true;
    }
    re.is_match(key)
}

/// Options for [`redact_sensitive_text`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RedactOpts {
    /// Text is source code: skip ENV-assignment/JSON/YAML passes to avoid
    /// false positives (prefix/header/key/JWT passes still run).
    pub code_file: bool,
    /// Text is file content returned to the agent: prefix-matched secrets
    /// become non-reusable sentinels; implies `code_file`.
    pub file_read: bool,
    /// Additionally redact credential-named query params and user:pass@
    /// userinfo in web URLs (off by default — actionable URLs survive).
    pub redact_url_credentials: bool,
}

const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "auth_token",
    "token",
    "secret",
    "password",
    "key",
    "signature",
    "sig",
];

fn redact_query_string(query: &str) -> String {
    query
        .split('&')
        .map(|pair| {
            if let Some((k, v)) = pair.split_once('=') {
                let lower = k.to_ascii_lowercase();
                if SENSITIVE_QUERY_PARAMS.contains(&lower.as_str()) && !v.is_empty() {
                    return format!("{k}={}", mask_token(v));
                }
            }
            pair.to_string()
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Apply all redaction patterns to a block of text. Non-matching text
/// passes through unchanged (port of hermes `redact_sensitive_text`).
pub fn redact_sensitive_text(text: &str, opts: RedactOpts) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = text.to_string();
    let mut code_file = opts.code_file;
    if opts.file_read {
        code_file = true;
    }

    // 1. Known vendor prefixes.
    if has_known_prefix_substring(&out) {
        let re = prefix_re();
        if opts.file_read {
            out = re
                .replace_all(&out, |caps: &regex::Captures| {
                    mask_token_nonreusable(&caps[1])
                })
                .into_owned();
        } else {
            out = re
                .replace_all(&out, |caps: &regex::Captures| mask_token(&caps[1]))
                .into_owned();
        }
    }

    // 2. ENV assignments (skip for code files — false positives).
    if !code_file && out.contains('=') {
        let re = env_assign_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                let name = &caps[1];
                let quote = &caps[2];
                let value = &caps[3];
                if env_lookup_value_re().is_match(value) || !key_has_secret_keyword(name) {
                    return caps[0].to_string();
                }
                format!("{name}={quote}{}{quote}", mask_token(value))
            })
            .into_owned();
    }

    // 3. JSON fields (skip for code files).
    if !code_file && out.contains(':') && out.contains('"') {
        let re = json_field_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                let key = &caps[1];
                let value = &caps[2];
                if env_lookup_value_re().is_match(value) {
                    return caps[0].to_string();
                }
                format!("{key}: \"{}\"", mask_token(value))
            })
            .into_owned();
    }

    // 4. Unquoted YAML/colon config (skip URLs — web params pass through).
    if !code_file && out.contains(':') && !out.contains("://") {
        let re = yaml_assign_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                let value = &caps[5];
                if env_lookup_value_re().is_match(value) {
                    return caps[0].to_string();
                }
                format!(
                    "{}{}{}{}{}",
                    &caps[1],
                    &caps[2],
                    &caps[3],
                    &caps[4],
                    mask_token(value)
                )
            })
            .into_owned();
    }

    // 5. Authorization headers (any scheme).
    if out.to_ascii_lowercase().contains("authorization") {
        let re = auth_header_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                let head = &caps[1];
                let scheme = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                format!("{head}{scheme}{}", mask_token(&caps[3]))
            })
            .into_owned();
    }

    // 6. x-api-key style headers.
    if out.to_ascii_lowercase().contains("api-key")
        || out.to_ascii_lowercase().contains("apikey")
        || out.to_ascii_lowercase().contains("x-auth-token")
    {
        let re = secret_header_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                format!("{}{}", &caps[1], mask_token(&caps[2]))
            })
            .into_owned();
    }

    // 7. Private key blocks.
    if out.contains("PRIVATE KEY-----") {
        let re = private_key_re();
        out = re
            .replace_all(&out, "*** redacted private key ***")
            .into_owned();
    }

    // 8. DB connection strings.
    if out.contains("://") {
        let re = db_connstr_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                format!("{}{}{}", &caps[1], mask_token(&caps[2]), &caps[3])
            })
            .into_owned();
    }

    // 9. Bare-token URL userinfo.
    if out.contains("://") && out.contains('@') {
        let re = url_bare_token_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                format!("{}{}{}", &caps[1], mask_token(&caps[2]), &caps[3])
            })
            .into_owned();
    }

    // 10. JWTs.
    if out.contains("eyJ") {
        let re = jwt_re();
        out = re
            .replace_all(&out, |caps: &regex::Captures| mask_token(&caps[0]))
            .into_owned();
    }

    // 11. Opt-in web-URL credential redaction (query params).
    if opts.redact_url_credentials && out.contains('?') {
        let re =
            Regex::new(r"(?i)([a-z][a-z0-9+.-]*://[^\s?#]*)\?([^\s#]*)").expect("static regex");
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                format!("{}?{}", &caps[1], redact_query_string(&caps[2]))
            })
            .into_owned();
    }

    out
}

/// True if `command` dumps environment variables to stdout (hermes
/// `is_env_dump_command`): env/printenv/set/export/declare as the first
/// token of any pipeline/sequence segment.
pub fn is_env_dump_command(command: &str) -> bool {
    if command.trim().is_empty() {
        return false;
    }
    for segment in command.split(['|', ';', '&']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let first = segment.split_whitespace().next().unwrap_or("");
        if matches!(first, "env" | "printenv" | "set" | "export" | "declare") {
            return true;
        }
    }
    false
}

/// Redact secrets from terminal/process stdout (hermes
/// `redact_terminal_output`). Env-dump commands enable the ENV-assignment
/// pass; anything else runs in code-file mode to avoid false positives.
pub fn redact_terminal_output(output: &str, command: &str) -> String {
    if output.is_empty() {
        return String::new();
    }
    let opts = RedactOpts {
        code_file: !is_env_dump_command(command),
        ..Default::default()
    };
    redact_sensitive_text(output, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_lengths() {
        assert_eq!(mask_secret("", 4, 4, 12), "");
        assert_eq!(mask_secret("short", 4, 4, 12), "***");
        assert_eq!(
            mask_secret("sk-proj-abcdef1234567890", 4, 4, 12),
            "sk-p...7890"
        );
    }

    #[test]
    fn redacts_vendor_prefixes() {
        let opts = RedactOpts::default();
        let out = redact_sensitive_text("key is ghp_ABCDEF1234567890XYZ ok", opts);
        assert!(!out.contains("ghp_ABCDEF1234567890XYZ"), "got: {out}");
        assert!(out.contains("ghp_AB"), "head kept: {out}");
        // file_read mode: non-reusable sentinel, no secret bytes.
        let out = redact_sensitive_text(
            "key is ghp_ABCDEF1234567890XYZ ok",
            RedactOpts {
                file_read: true,
                ..Default::default()
            },
        );
        assert!(out.contains("«redacted:ghp_…»"), "got: {out}");
        assert!(!out.contains("ABCDEF"), "got: {out}");
    }

    #[test]
    fn redacts_env_assignments_only_for_dumps() {
        let text = "MY_API_TOKEN=supersecretvalue123";
        // env-dump mode masks.
        let out = redact_terminal_output(text, "env");
        assert!(!out.contains("supersecretvalue123"), "got: {out}");
        assert!(out.contains("MY_API_TOKEN="), "got: {out}");
        // code-file mode (ordinary commands) leaves it alone.
        let out = redact_terminal_output(text, "cat foo.txt");
        assert_eq!(out, text);
        // Prose keys are not masked even in dump mode.
        let out = redact_terminal_output("author=Smith", "env");
        assert_eq!(out, "author=Smith");
        // Programmatic lookups pass through.
        let out = redact_terminal_output("API_KEY=os.getenv('X')", "env");
        assert!(out.contains("os.getenv"), "got: {out}");
    }

    #[test]
    fn redacts_json_fields() {
        let out = redact_sensitive_text(
            r#"{"password": "hunter2hunter2hunter2"}"#,
            RedactOpts::default(),
        );
        assert!(!out.contains("hunter2hunter2hunter2"), "got: {out}");
        assert!(out.contains("\"password\""), "got: {out}");
    }

    #[test]
    fn redacts_auth_headers() {
        let out = redact_sensitive_text(
            "Authorization: Bearer sk-proj-abcdef1234567890",
            RedactOpts {
                code_file: true,
                ..Default::default()
            },
        );
        assert!(
            !out.contains("abcdef1234567890") || out.contains("***"),
            "got: {out}"
        );
        assert!(out.contains("Authorization: Bearer"), "got: {out}");
    }

    #[test]
    fn redacts_private_keys_and_db_connstrings() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
        let out = redact_sensitive_text(pem, RedactOpts::default());
        assert!(!out.contains("MIIEow"), "got: {out}");
        let out = redact_sensitive_text(
            "postgres://admin:s3cretpass@db.internal:5432/app",
            RedactOpts {
                code_file: true,
                ..Default::default()
            },
        );
        assert!(!out.contains("s3cretpass"), "got: {out}");
        assert!(out.contains("admin:"), "got: {out}");
        assert!(out.contains("@db.internal"), "got: {out}");
    }

    #[test]
    fn redacts_jwt_and_bare_url_token() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0In0.abc123sig";
        let out = redact_sensitive_text(jwt, RedactOpts::default());
        assert!(!out.contains("eyJzdWIi"), "got: {out}");
        let out = redact_sensitive_text(
            "git push https://glpat-abcdef123456@github.com/x/y.git",
            RedactOpts::default(),
        );
        assert!(!out.contains("glpat-abcdef123456"), "got: {out}");
    }

    #[test]
    fn url_query_params_only_when_opted_in() {
        let url = "https://api.example.com/v1?api_key=secret1234567890ab&x=1";
        let passthrough = redact_sensitive_text(url, RedactOpts::default());
        assert_eq!(passthrough, url);
        let redacted = redact_sensitive_text(
            url,
            RedactOpts {
                redact_url_credentials: true,
                ..Default::default()
            },
        );
        assert!(!redacted.contains("secret1234567890ab"), "got: {redacted}");
        assert!(redacted.contains("x=1"), "got: {redacted}");
    }

    #[test]
    fn env_dump_detection() {
        assert!(is_env_dump_command("env"));
        assert!(is_env_dump_command("printenv | grep KEY"));
        assert!(is_env_dump_command("cd /tmp && export"));
        assert!(!is_env_dump_command("envsubst < tpl"));
        assert!(!is_env_dump_command("cat .env"));
        assert!(!is_env_dump_command(""));
    }

    #[test]
    fn clean_text_unchanged() {
        let text = "regular output\nwith lines and no secrets";
        assert_eq!(redact_sensitive_text(text, RedactOpts::default()), text);
    }
}
