//! Outbound mutual TLS (client-certificate) policy for HTTPS egress.
//!
//! Deployments that route agent traffic through an mTLS-fronted gateway (a C2 /
//! metering control plane) need ulnclaw to *present a client certificate* on the
//! outbound handshake. Before this module ulnclaw had no such capability: every
//! `reqwest::Client` was built with the default trust store and every raw
//! `rustls` client used `with_no_client_auth()`.
//!
//! The policy is read from the environment (or `<home>/.env`) through
//! [`crate::config::get_env_value`]:
//!
//! | Variable | Meaning |
//! | --- | --- |
//! | `ULNCLAW_MTLS_CERT` | PEM path: leaf cert (+ chain), may contain the key |
//! | `ULNCLAW_MTLS_KEY` | PEM path of the private key (when not in the cert file) |
//! | `ULNCLAW_MTLS_CA` | PEM bundle of extra trust anchors (control-plane CA) |
//! | `ULNCLAW_MTLS_TRUST_BUILTIN` | `1` (default) also trust public roots, `0` = pin-only |
//! | `ULNCLAW_MTLS_HOSTS` | scope: `gw.example.com,*.internal` or `*` (required) |
//! | `ULNCLAW_MTLS_MIN_TLS` | `1.2` (default) or `1.3` |
//!
//! Design rules, in order of importance:
//!
//! 1. **Fail closed.** A half-configured or unreadable identity is an error on
//!    the egress path, never a silent downgrade to unauthenticated TLS.
//! 2. **Never leak the identity.** The certificate is presented only to hosts in
//!    `ULNCLAW_MTLS_HOSTS`; `*` has to be written down explicitly.
//! 3. **Verifiable.** [`probe`] performs a real handshake twice (with and
//!    without the identity) so operators can tell "the server demanded our
//!    certificate" from "the server never asked".
//!
//! See `docs/design/outbound-mtls.md` for the full design.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::get_env_value;

/// PEM path of the client certificate chain (leaf first).
pub const ENV_CERT: &str = "ULNCLAW_MTLS_CERT";
/// PEM path of the client private key, when it is not inside [`ENV_CERT`].
pub const ENV_KEY: &str = "ULNCLAW_MTLS_KEY";
/// PEM bundle of extra trust anchors for the peer.
pub const ENV_CA: &str = "ULNCLAW_MTLS_CA";
/// `1` (default) keeps the public roots, `0` trusts only [`ENV_CA`].
pub const ENV_TRUST_BUILTIN: &str = "ULNCLAW_MTLS_TRUST_BUILTIN";
/// Comma separated host scope; `*` means every host.
pub const ENV_HOSTS: &str = "ULNCLAW_MTLS_HOSTS";
/// `1.2` (default) or `1.3`.
pub const ENV_MIN_TLS: &str = "ULNCLAW_MTLS_MIN_TLS";

/// Hard-coded egress host for the ElevenLabs audio paths (`src/stt.rs`,
/// `src/tts.rs`), so their request chains can scope the identity without
/// re-parsing the URL literal.
pub const ELEVENLABS_HOST: &str = "api.elevenlabs.io";

/// Egress paths wired to [`apply_env`] today. `ulnclaw mtls status` reports this
/// list so operators know which traffic the policy actually covers; keep it in
/// sync with `docs/design/outbound-mtls.md`.
pub const WIRED_PATHS: &[&str] = &[
    "src/provider/openai.rs (OpenAI-compatible chat: OpenAI, DashScope, OpenRouter, Ollama, llama.cpp, custom)",
    "src/provider/anthropic.rs (Anthropic Messages API)",
    "src/stt.rs (speech-to-text: OpenAI-compatible + DeepInfra + ElevenLabs)",
    "src/tts.rs (text-to-speech: OpenAI-compatible + ElevenLabs)",
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Which hosts may see the client certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostScope {
    /// `*` — every host (explicit opt-in, documented as a leak risk).
    All,
    /// Exact host names and/or `*.suffix` wildcards.
    Hosts(Vec<String>),
}

impl HostScope {
    pub fn matches(&self, host: &str) -> bool {
        let host = normalize_host(host);
        if host.is_empty() {
            return false;
        }
        match self {
            HostScope::All => true,
            HostScope::Hosts(patterns) => patterns.iter().any(|p| scope_pattern_matches(p, &host)),
        }
    }

    pub fn label(&self) -> String {
        match self {
            HostScope::All => "*".to_string(),
            HostScope::Hosts(p) => p.join(","),
        }
    }

    pub fn json(&self) -> Value {
        match self {
            HostScope::All => json!({"kind": "all", "hosts": ["*"]}),
            HostScope::Hosts(p) => json!({"kind": "hosts", "hosts": p}),
        }
    }
}

/// Minimum acceptable TLS version for outbound connections honouring the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinTls {
    V1_2,
    V1_3,
}

impl MinTls {
    /// Parse `1.2` / `1.3` (also accepts `tls1.2`, `tlsv1.3`, `12`, `13`).
    pub fn parse(raw: &str) -> Result<Self, TlsPolicyError> {
        let cleaned: String = raw
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
            .collect();
        let digits: String = cleaned
            .trim_start_matches("tlsv")
            .trim_start_matches("tls")
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        match digits.as_str() {
            "1.2" | "12" => Ok(MinTls::V1_2),
            "1.3" | "13" => Ok(MinTls::V1_3),
            _ => Err(TlsPolicyError::BadMinTls {
                value: raw.trim().to_string(),
            }),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            MinTls::V1_2 => "1.2",
            MinTls::V1_3 => "1.3",
        }
    }

    fn rustls_versions(&self) -> &'static [&'static rustls::SupportedProtocolVersion] {
        // Static slices so callers can borrow them for the lifetime of the
        // TLS config they build.
        const TLS13_12: &[&rustls::SupportedProtocolVersion] =
            &[&rustls::version::TLS13, &rustls::version::TLS12];
        const TLS13: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
        match self {
            MinTls::V1_2 => TLS13_12,
            MinTls::V1_3 => TLS13,
        }
    }

    fn reqwest_version(&self) -> reqwest::tls::Version {
        match self {
            MinTls::V1_2 => reqwest::tls::Version::TLS_1_2,
            MinTls::V1_3 => reqwest::tls::Version::TLS_1_3,
        }
    }
}

/// Everything that can go wrong while resolving the outbound mTLS policy.
///
/// Every variant carries the fix in its `Display` text: the point of the type is
/// that a misconfigured deployment learns what to change instead of silently
/// sending anonymous traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsPolicyError {
    /// Only `ULNCLAW_MTLS_KEY`/`ULNCLAW_MTLS_CA` set — no certificate to present.
    CertMissing,
    /// Certificate configured but no private key found (neither inline nor via path).
    KeyMissing { cert: PathBuf },
    /// Configured file could not be read.
    Unreadable { path: PathBuf, reason: String },
    /// File was readable but carried no usable PEM block.
    BadPem { path: PathBuf, reason: String },
    /// Identity configured without a host scope.
    HostsUnscoped,
    /// `ULNCLAW_MTLS_MIN_TLS` is neither 1.2 nor 1.3.
    BadMinTls { value: String },
    /// The TLS stack refused to build a client from an otherwise valid policy.
    ClientBuild { reason: String },
}

impl std::fmt::Display for TlsPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsPolicyError::CertMissing => write!(
                f,
                "outbound mTLS: {} is not set but key/CA configuration is present — \
                 set {} to the client certificate PEM (or unset the other variables)",
                ENV_CERT, ENV_CERT
            ),
            TlsPolicyError::KeyMissing { cert } => write!(
                f,
                "outbound mTLS: no private key found for {} — put the key in that PEM file or \
                 set {} to the key file (PKCS#8, PKCS#1 or SEC1)",
                cert.display(),
                ENV_KEY
            ),
            TlsPolicyError::Unreadable { path, reason } => write!(
                f,
                "outbound mTLS: cannot read {}: {}",
                path.display(),
                reason
            ),
            TlsPolicyError::BadPem { path, reason } => write!(
                f,
                "outbound mTLS: {} is not a usable PEM file: {}",
                path.display(),
                reason
            ),
            TlsPolicyError::HostsUnscoped => write!(
                f,
                "outbound mTLS: a client certificate is configured but {} is unset — presenting \
                 it to any host that asks is a credential leak, so the scope must be explicit. \
                 Set {} to the gateway host(s), e.g. {}=gw.example.com, or {}={} to allow every host",
                ENV_HOSTS, ENV_HOSTS, ENV_HOSTS, ENV_HOSTS, "*"
            ),
            TlsPolicyError::BadMinTls { value } => write!(
                f,
                "outbound mTLS: {}={:?} is not a supported minimum TLS version (use \"1.2\" or \"1.3\")",
                ENV_MIN_TLS, value
            ),
            TlsPolicyError::ClientBuild { reason } => write!(
                f,
                "outbound mTLS: the TLS stack refused to build the client: {}",
                reason
            ),
        }
    }
}

impl std::error::Error for TlsPolicyError {}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Resolved outbound mTLS policy. Cheap to clone (Arc-backed DER), cheap to
/// rebuild: callers resolve it when they build a client, so config changes take
/// effect without process restart.
#[derive(Clone)]
pub struct OutboundTls {
    cert_path: PathBuf,
    key_path: Option<PathBuf>,
    ca_path: Option<PathBuf>,
    chain: Arc<Vec<rustls::pki_types::CertificateDer<'static>>>,
    key: Arc<rustls::pki_types::PrivateKeyDer<'static>>,
    /// chain + key in one PEM buffer, for `reqwest::Identity::from_pem`.
    identity_pem: Arc<Vec<u8>>,
    ca_pem: Option<Arc<Vec<u8>>>,
    ca_certs: Arc<Vec<rustls::pki_types::CertificateDer<'static>>>,
    scope: HostScope,
    min_tls: MinTls,
    trust_builtin: bool,
}

impl std::fmt::Debug for OutboundTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately never prints key material.
        f.debug_struct("OutboundTls")
            .field("cert_path", &self.cert_path)
            .field("key_path", &self.key_path)
            .field("ca_path", &self.ca_path)
            .field("chain_len", &self.chain.len())
            .field("leaf_fingerprint", &self.leaf_fingerprint())
            .field("scope", &self.scope)
            .field("min_tls", &self.min_tls)
            .field("trust_builtin", &self.trust_builtin)
            .finish()
    }
}

impl OutboundTls {
    /// Resolve the policy from the process environment / `<home>/.env`.
    ///
    /// `Ok(None)` means "no outbound mTLS configured" (feature off). Any other
    /// outcome — including a broken half-configuration — is a hard error.
    pub fn from_env() -> Result<Option<Self>, TlsPolicyError> {
        Self::from_lookup(&|key| get_env_value(key))
    }

    /// Resolve the policy from an arbitrary key/value lookup (testable seam).
    pub fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, TlsPolicyError> {
        let read = |key: &str| -> Option<String> {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let cert_raw = read(ENV_CERT);
        let key_raw = read(ENV_KEY);
        let ca_raw = read(ENV_CA);
        let hosts_raw = read(ENV_HOSTS);
        let min_tls_raw = read(ENV_MIN_TLS);
        let trust_raw = read(ENV_TRUST_BUILTIN);

        let configured = cert_raw.is_some()
            || key_raw.is_some()
            || ca_raw.is_some()
            || hosts_raw.is_some()
            || min_tls_raw.is_some()
            || trust_raw.is_some();
        if !configured {
            return Ok(None);
        }

        // A certificate is the whole point of the feature.
        let cert_path = cert_raw.ok_or(TlsPolicyError::CertMissing)?;
        let cert_path = PathBuf::from(cert_path);

        // Scope must be explicit: a client certificate is a credential.
        let hosts = match hosts_raw {
            None => return Err(TlsPolicyError::HostsUnscoped),
            Some(raw) => parse_hosts(&raw)?,
        };

        let min_tls = match min_tls_raw {
            None => MinTls::V1_2,
            Some(raw) => MinTls::parse(&raw)?,
        };

        let trust_builtin = match trust_raw.as_deref() {
            None => true,
            Some(raw) => parse_bool(raw, ENV_TRUST_BUILTIN)?,
        };

        // Load the certificate chain, and the key from the same file when it is
        // embedded there (the common `cert+key` PEM layout).
        let cert_bytes = read_file(&cert_path)?;
        let (chain, inline_key) = parse_pem_blocks(&cert_path, &cert_bytes)?;
        if chain.is_empty() {
            return Err(TlsPolicyError::BadPem {
                path: cert_path.clone(),
                reason: "no CERTIFICATE block found".to_string(),
            });
        }

        let (key, key_path, key_pem) = match (inline_key, &key_raw) {
            (Some(key), _) => (key, None, None),
            (None, Some(raw)) => {
                let path = PathBuf::from(raw);
                let bytes = read_file(&path)?;
                let (_, key) = parse_pem_blocks(&path, &bytes)?;
                let key = key.ok_or_else(|| TlsPolicyError::BadPem {
                    path: path.clone(),
                    reason: "no PRIVATE KEY block found".to_string(),
                })?;
                (key, Some(path), Some(bytes))
            }
            (None, None) => return Err(TlsPolicyError::KeyMissing { cert: cert_path }),
        };

        // reqwest wants cert + key in a single PEM buffer.
        let mut identity_pem = cert_bytes.clone();
        if let Some(bytes) = &key_pem {
            if !identity_pem.ends_with(b"\n") {
                identity_pem.push(b'\n');
            }
            identity_pem.extend_from_slice(bytes);
        }

        let (ca_pem, ca_certs) = match &ca_raw {
            None => (None, Vec::new()),
            Some(raw) => {
                let path = PathBuf::from(raw);
                let bytes = read_file(&path)?;
                let (certs, _) = parse_pem_blocks(&path, &bytes)?;
                if certs.is_empty() {
                    return Err(TlsPolicyError::BadPem {
                        path: path.clone(),
                        reason: "no CERTIFICATE block found".to_string(),
                    });
                }
                (Some(Arc::new(bytes)), certs)
            }
        };
        let ca_path = ca_raw.map(PathBuf::from);

        Ok(Some(OutboundTls {
            cert_path,
            key_path,
            ca_path,
            chain: Arc::new(chain),
            key: Arc::new(key),
            identity_pem: Arc::new(identity_pem),
            ca_pem,
            ca_certs: Arc::new(ca_certs),
            scope: hosts,
            min_tls,
            trust_builtin,
        }))
    }

    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    pub fn key_path(&self) -> Option<&Path> {
        self.key_path.as_deref()
    }

    pub fn ca_path(&self) -> Option<&Path> {
        self.ca_path.as_deref()
    }

    pub fn scope(&self) -> &HostScope {
        &self.scope
    }

    pub fn min_tls(&self) -> MinTls {
        self.min_tls
    }

    pub fn trust_builtin(&self) -> bool {
        self.trust_builtin
    }

    pub fn chain_len(&self) -> usize {
        self.chain.len()
    }

    /// `sha256:AB:CD:…` fingerprint of the leaf certificate (public data, safe to log).
    pub fn leaf_fingerprint(&self) -> String {
        match self.chain.first() {
            Some(leaf) => format!("sha256:{}", hex_fingerprint(leaf.as_ref())),
            None => "sha256:-".to_string(),
        }
    }

    /// Whether the certificate should be presented to `host`.
    pub fn applies_to(&self, host: &str) -> bool {
        self.scope.matches(host)
    }

    /// Apply the policy to a `reqwest` client builder for a connection to `host`.
    ///
    /// `host` is the request host; hosts outside the scope keep a plain client.
    pub fn apply(
        &self,
        builder: reqwest::ClientBuilder,
        host: &str,
    ) -> Result<reqwest::ClientBuilder, TlsPolicyError> {
        if !self.applies_to(host) {
            return Ok(builder);
        }

        let identity =
            reqwest::Identity::from_pem(&self.identity_pem).map_err(|e| TlsPolicyError::BadPem {
                path: self.cert_path.clone(),
                reason: format!("reqwest rejected the identity PEM: {e}"),
            })?;
        let mut builder = builder.identity(identity);

        if let Some(ca_pem) = &self.ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(ca_pem).map_err(|e| {
                TlsPolicyError::BadPem {
                    path: self
                        .ca_path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(ENV_CA)),
                    reason: format!("reqwest rejected the CA bundle: {e}"),
                }
            })?;
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }

        builder = builder.tls_built_in_root_certs(self.trust_builtin);
        builder = builder.min_tls_version(self.min_tls.reqwest_version());
        Ok(builder)
    }

    /// `rustls` client config for egress that does not go through `reqwest`
    /// (raw sockets, websockets): same identity, same trust, same minimum version.
    pub fn rustls_config(&self) -> Result<Arc<rustls::ClientConfig>, TlsPolicyError> {
        self.rustls_config_with_identity(true)
    }

    fn rustls_config_with_identity(
        &self,
        with_identity: bool,
    ) -> Result<Arc<rustls::ClientConfig>, TlsPolicyError> {
        let mut roots = rustls::RootCertStore::empty();
        if self.trust_builtin {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        if !self.ca_certs.is_empty() {
            let (added, ignored) = roots.add_parsable_certificates(self.ca_certs.iter().cloned());
            if added == 0 {
                return Err(TlsPolicyError::BadPem {
                    path: self
                        .ca_path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(ENV_CA)),
                    reason: format!("no usable trust anchor in the CA bundle ({ignored} ignored)"),
                });
            }
        }

        let builder = rustls::ClientConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(self.min_tls.rustls_versions())
            .map_err(|e| TlsPolicyError::ClientBuild {
                reason: format!("TLS version selection rejected: {e}"),
            })?
            .with_root_certificates(roots);
        let config = if with_identity {
            builder
                .with_client_auth_cert(self.chain.as_ref().clone(), self.key.as_ref().clone_key())
                .map_err(|e| TlsPolicyError::BadPem {
                    path: self.cert_path.clone(),
                    reason: format!("client certificate/key pair rejected: {e}"),
                })?
        } else {
            builder.with_no_client_auth()
        };
        Ok(Arc::new(config))
    }

    /// Redacted status document (`ulnclaw mtls status --json`).
    pub fn status_json(&self) -> Value {
        json!({
            "enabled": true,
            "cert": self.cert_path.display().to_string(),
            "key": self
                .key_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "inline in cert PEM".to_string()),
            "ca": self.ca_path.as_ref().map(|p| p.display().to_string()),
            "trust_builtin": self.trust_builtin,
            "hosts": self.scope.json(),
            "min_tls": self.min_tls.label(),
            "chain_len": self.chain.len(),
            "leaf_fingerprint": self.leaf_fingerprint(),
            "cert_not_after": cert_not_after(&self.cert_path),
            "wired_paths": WIRED_PATHS,
        })
    }

    /// Human-readable status (`ulnclaw mtls status`). Never prints key material;
    /// the identity is identified by the SHA-256 fingerprint of the leaf cert.
    pub fn status_text(&self) -> String {
        let mut out = String::new();
        out.push_str("出站 mTLS: 已启用\n");
        out.push_str(&format!("  证书        : {}\n", self.cert_path.display()));
        out.push_str(&format!(
            "  私钥        : {}\n",
            self.key_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "随证书 PEM 内联".to_string())
        ));
        out.push_str(&format!(
            "  信任锚      : {}{}\n",
            self.ca_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(未指定额外 CA)".to_string()),
            if self.trust_builtin {
                " + 系统内置根"
            } else {
                "（仅信任上述 CA）"
            }
        ));
        out.push_str(&format!("  生效主机    : {}\n", self.scope.label()));
        out.push_str(&format!("  最低 TLS    : {}\n", self.min_tls.label()));
        out.push_str(&format!("  链长        : {} 张证书\n", self.chain.len()));
        out.push_str(&format!("  叶证书指纹  : {}\n", self.leaf_fingerprint()));
        match cert_not_after(&self.cert_path) {
            Some(not_after) => out.push_str(&format!("  证书到期    : {not_after}\n")),
            None => out.push_str("  证书到期    : (需 openssl CLI 读取)\n"),
        }
        out.push_str("  已接线路径  :\n");
        for path in WIRED_PATHS {
            out.push_str(&format!("    - {path}\n"));
        }
        out.push_str("\n自检：ulnclaw mtls probe https://<gateway-host>/healthz\n");
        out
    }
}

/// Disabled-policy status (`ulnclaw mtls status --json` with nothing configured).
pub fn disabled_status_json() -> Value {
    json!({
        "enabled": false,
        "hosts": {"kind": "all", "hosts": []},
        "wired_paths": WIRED_PATHS,
        "configure": format!(
            "set {ENV_CERT} (+{ENV_KEY} if the key is not inline) and {ENV_HOSTS}=gw.example.com \
             (or '*'); optionally {ENV_CA} and {ENV_MIN_TLS}=1.3"
        ),
    })
}

/// Configuration surface for humans (`ulnclaw mtls status`) and docs.
pub fn config_env_vars() -> Vec<(&'static str, &'static str)> {
    vec![
        (ENV_CERT, "必填：客户端证书链 PEM 路径（可含私钥）"),
        (ENV_KEY, "可选：私钥 PEM 路径（证书 PEM 未内联私钥时必填）"),
        (ENV_HOSTS, "必填：生效主机白名单，逗号分隔；'*' 表示所有主机"),
        (ENV_CA, "可选：额外信任的 CA/链 PEM（如网关私有 CA）"),
        (ENV_TRUST_BUILTIN, "可选：是否同时信任系统内置根（默认 1；0=仅信任上面的 CA）"),
        (ENV_MIN_TLS, "可选：最低 TLS 版本 1.2(默认)/1.3"),
    ]
}

// ---------------------------------------------------------------------------
// Convenience entry points used by egress paths
// ---------------------------------------------------------------------------

/// Host part of a URL string (no port, lower-cased). Returns an empty string for
/// anything unparsable, which never matches a scope — an unknown host must not
/// receive the identity.
pub fn host_of(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => parsed.host_str().map(normalize_host).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// Apply the environment policy to a builder for `host`.
///
/// Returns the builder unchanged when the feature is off or `host` is out of
/// scope; returns a **hard error** on a broken configuration (fail closed).
pub fn apply_env(
    builder: reqwest::ClientBuilder,
    host: &str,
) -> Result<reqwest::ClientBuilder, TlsPolicyError> {
    match OutboundTls::from_env()? {
        None => Ok(builder),
        Some(policy) => policy.apply(builder, host),
    }
}

/// Convenience wrapper: default `reqwest` builder with the policy applied.
pub fn builder_for(host: &str) -> Result<reqwest::ClientBuilder, TlsPolicyError> {
    apply_env(reqwest::Client::builder(), host)
}

/// Ready-made client for one-shot egress call sites (`stt`/`tts`-style code that
/// builds a client inside a request chain). Streams/websockets should use
/// [`builder_for`] or [`OutboundTls::rustls_config`] instead.
pub fn client_for(host: &str) -> Result<reqwest::Client, TlsPolicyError> {
    builder_for(host)?
        .build()
        .map_err(|e| TlsPolicyError::ClientBuild {
            reason: e.to_string(),
        })
}

/// Resolve the environment policy once for callers that need the struct itself
/// (probe, status, raw-rustls paths).
pub fn policy_from_env() -> Result<Option<OutboundTls>, TlsPolicyError> {
    OutboundTls::from_env()
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// What the server did about client certificates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientAuthVerdict {
    /// Handshake succeeded *with* the identity and failed *without* it: the
    /// server demands mTLS and accepts our certificate.
    Enforced,
    /// Both handshakes succeeded: the server completed a TLS handshake with no
    /// client certificate, so it does not (yet) enforce mTLS.
    NotRequired,
    /// The handshake failed while presenting our identity (trust, chain or
    /// expiry problem at either end).
    Rejected,
    /// Reached the endpoint but no identity is configured locally, so the probe
    /// cannot tell whether the server would have asked for one.
    IdentityMissing,
}

impl ClientAuthVerdict {
    pub fn label(&self) -> &'static str {
        match self {
            ClientAuthVerdict::Enforced => "enforced",
            ClientAuthVerdict::NotRequired => "not-required",
            ClientAuthVerdict::Rejected => "rejected",
            ClientAuthVerdict::IdentityMissing => "identity-missing",
        }
    }

    pub fn explanation(&self) -> &'static str {
        match self {
            ClientAuthVerdict::Enforced => {
                "server requires a client certificate and accepted ours (mTLS is in force)"
            }
            ClientAuthVerdict::NotRequired => {
                "the endpoint served a request with no client certificate, so mTLS is not enforced here"
            }
            ClientAuthVerdict::Rejected => {
                "handshake with our client certificate failed — check that the server's CA signed it, that the chain is complete and that it has not expired"
            }
            ClientAuthVerdict::IdentityMissing => {
                "no client certificate configured locally, so the server was contacted anonymously"
            }
        }
    }

    /// What to do next when the verdict is not `Enforced`.
    pub fn remediation(&self) -> &'static str {
        match self {
            ClientAuthVerdict::Enforced => "无需操作；出站 mTLS 已就绪",
            ClientAuthVerdict::NotRequired => {
                "若该端点应强制 mTLS，请让网关开启 client-certificate 校验；ulnclaw 侧无需改动"
            }
            ClientAuthVerdict::Rejected => {
                "核对：网关 CA 是否签发了本机证书、ULNCLAW_MTLS_CA 是否为该 CA、链证书是否完整、证书是否已过期"
            }
            ClientAuthVerdict::IdentityMissing => {
                "先配置 ULNCLAW_MTLS_CERT（必要时 ULNCLAW_MTLS_KEY）与 ULNCLAW_MTLS_HOSTS，再重跑探测"
            }
        }
    }

    /// True when the endpoint provably enforced mTLS with our identity.
    pub fn is_passing(&self) -> bool {
        matches!(self, ClientAuthVerdict::Enforced)
    }
}

/// Outcome of one handshake arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeOutcome {
    pub ok: bool,
    /// Human readable detail: the TLS error, or a short success summary.
    pub detail: String,
    pub tls_version: Option<String>,
    pub peer_leaf_sha256: Option<String>,
    pub peer_chain_len: usize,
    /// What the endpoint did with a minimal request sent over the *anonymous*
    /// connection. A TLS 1.3 server is allowed to complete the handshake and
    /// only then discover that it wanted a client certificate, so a plain
    /// "handshake succeeded" is not proof that the endpoint is open.
    pub request: AfterHandshake,
}

/// Result of the one-line HTTP request the probe sends over the anonymous
/// connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterHandshake {
    /// Not attempted (the identity arm only needs the handshake itself).
    NotProbed,
    /// The server answered with an HTTP status code.
    Served(u16),
    /// The connection failed after the handshake (TLS alert, EOF, reset).
    Refused(String),
}

impl AfterHandshake {
    fn json(&self) -> Value {
        match self {
            AfterHandshake::NotProbed => json!({"kind": "not-probed"}),
            AfterHandshake::Served(code) => json!({"kind": "served", "status": code}),
            AfterHandshake::Refused(reason) => json!({"kind": "refused", "reason": reason}),
        }
    }

    fn detail(&self) -> String {
        match self {
            AfterHandshake::NotProbed => "request not sent".to_string(),
            AfterHandshake::Served(code) => format!("server answered anonymously with HTTP {code}"),
            AfterHandshake::Refused(reason) => {
                format!("connection refused after the handshake: {reason}")
            }
        }
    }

    /// Does this look like a server that wanted a client certificate?
    fn looks_like_enforcement(&self) -> bool {
        match self {
            // 495/496/497 are the nginx codes for "no/unknown/invalid client
            // certificate"; 400/403 are what other terminators answer with.
            AfterHandshake::Served(code) => matches!(code, 400 | 403 | 495 | 496 | 497),
            AfterHandshake::Refused(_) => true,
            AfterHandshake::NotProbed => false,
        }
    }
}

impl HandshakeOutcome {
    fn json(&self) -> Value {
        json!({
            "ok": self.ok,
            "detail": self.detail,
            "tls_version": self.tls_version,
            "peer_leaf_sha256": self.peer_leaf_sha256,
            "peer_chain_len": self.peer_chain_len,
            "request": self.request.json(),
        })
    }
}

/// Result of `ulnclaw mtls probe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub verdict: ClientAuthVerdict,
    pub with_identity: Option<HandshakeOutcome>,
    pub without_identity: HandshakeOutcome,
    pub identity_fingerprint: Option<String>,
    /// Why the verdict came out the way it did (evidence, not boilerplate).
    pub reason: String,
}

impl ProbeReport {
    /// True only when mTLS was provably enforced *and* our identity accepted.
    pub fn ok(&self) -> bool {
        self.verdict.is_passing()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "url": self.url,
            "host": self.host,
            "port": self.port,
            "verdict": self.verdict.label(),
            "explanation": self.verdict.explanation(),
            "reason": self.reason,
            "identity_fingerprint": self.identity_fingerprint,
            "with_identity": self.with_identity.as_ref().map(|o| o.json()),
            "without_identity": self.without_identity.json(),
        })
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("probe {}\n", self.url));
        out.push_str(&format!("  host            : {}:{}\n", self.host, self.port));
        out.push_str(&format!(
            "  identity        : {}\n",
            self.identity_fingerprint
                .clone()
                .unwrap_or_else(|| "(none configured)".to_string())
        ));
        if let Some(with) = &self.with_identity {
            out.push_str(&format!(
                "  with identity   : {} ({})\n",
                if with.ok { "handshake ok" } else { "FAILED" },
                with.detail
            ));
        }
        out.push_str(&format!(
            "  without identity: {} ({})\n",
            if self.without_identity.ok {
                "handshake ok"
            } else {
                "handshake refused"
            },
            self.without_identity.detail
        ));
        if self.without_identity.ok {
            out.push_str(&format!(
                "  anonymous request: {}\n",
                self.without_identity.request.detail()
            ));
        }
        out.push_str(&format!("  verdict         : {}\n", self.verdict.label()));
        out.push_str(&format!("  meaning         : {}\n", self.verdict.explanation()));
        out.push_str(&format!("  reason          : {}\n", self.reason));
        out
    }
}

/// Probe an HTTPS endpoint and report whether it enforces client certificates.
///
/// Two handshakes are attempted: one with the configured identity and one
/// anonymous (same trust anchors). The pair is what separates "mTLS accepted"
/// from "mTLS not required".
pub async fn probe(url: &str, timeout: Duration) -> Result<ProbeReport, String> {
    let policy = OutboundTls::from_env().map_err(|e| e.to_string())?;
    probe_with(url, timeout, policy.as_ref()).await
}

/// [`probe`] with an explicit policy (used by callers that already resolved it,
/// and by tests that must not touch process-global environment variables).
pub async fn probe_with(
    url: &str,
    timeout: Duration,
    policy: Option<&OutboundTls>,
) -> Result<ProbeReport, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL {url:?}: {e}"))?;
    let scheme = parsed.scheme().to_string();
    if scheme != "https" {
        return Err(format!(
            "probe requires an https:// URL (got {:?}) — mTLS has no meaning over plain TCP",
            scheme
        ));
    }
    let host = parsed
        .host_str()
        .map(normalize_host)
        .ok_or_else(|| format!("URL {url:?} has no host"))?;
    let port = parsed.port_or_known_default().unwrap_or(443);

    // Anonymous arm: identical trust anchors, no client certificate.
    let anon_config = match policy {
        Some(p) => p.rustls_config_with_identity(false).map_err(|e| e.to_string())?,
        None => anonymous_config().map_err(|e| e.to_string())?,
    };
    // The anonymous arm also sends one request: a TLS 1.3 server may accept
    // the handshake and only reject the request (post-handshake verification).
    let without_identity = handshake(&host, port, anon_config, timeout, true).await;

    let (with_identity, identity_fingerprint) = match policy {
        None => (None, None),
        Some(p) => {
            let cfg = p.rustls_config_with_identity(true).map_err(|e| e.to_string())?;
            let outcome = handshake(&host, port, cfg, timeout, false).await;
            (Some(outcome), Some(p.leaf_fingerprint()))
        }
    };

    let verdict = classify(with_identity.as_ref(), &without_identity);
    let reason = verdict_reason(&verdict, with_identity.as_ref(), &without_identity);
    Ok(ProbeReport {
        url: url.to_string(),
        host,
        port,
        verdict,
        with_identity,
        without_identity,
        identity_fingerprint,
        reason,
    })
}

/// Spell out the evidence behind a verdict so operators are not asked to trust
/// a one-word answer (notably: handshake ok + refused request == enforced).
fn verdict_reason(
    verdict: &ClientAuthVerdict,
    with: Option<&HandshakeOutcome>,
    without: &HandshakeOutcome,
) -> String {
    match verdict {
        ClientAuthVerdict::IdentityMissing => {
            "no client certificate is configured locally, so the endpoint was only contacted anonymously"
                .to_string()
        }
        ClientAuthVerdict::Rejected => format!(
            "the handshake failed while presenting our identity: {}",
            with.map(|o| o.detail.clone()).unwrap_or_default()
        ),
        ClientAuthVerdict::Enforced => {
            if !without.ok {
                format!(
                    "the anonymous handshake was refused by the server: {}",
                    without.detail
                )
            } else {
                format!(
                    "the anonymous handshake completed but the endpoint rejected the request ({}) — \
                     typical of a TLS 1.3 server that verifies the client certificate after the handshake",
                    without.request.detail()
                )
            }
        }
        ClientAuthVerdict::NotRequired => format!(
            "the endpoint served an anonymous request ({}), so a client certificate is not required here",
            without.request.detail()
        ),
    }
}

fn classify(with: Option<&HandshakeOutcome>, without: &HandshakeOutcome) -> ClientAuthVerdict {
    match with {
        None => ClientAuthVerdict::IdentityMissing,
        Some(with) if !with.ok => ClientAuthVerdict::Rejected,
        Some(_) if !without.ok => ClientAuthVerdict::Enforced,
        Some(_) if without.request.looks_like_enforcement() => ClientAuthVerdict::Enforced,
        Some(_) => ClientAuthVerdict::NotRequired,
    }
}

/// Resolve the process-wide rustls crypto provider, installing the crate's
/// built-in default when the embedding application has not chosen one yet.
///
/// `rustls` cannot always infer a provider from crate features (feature
/// unification with `reqwest`/`tokio-rustls` is enough to break the
/// auto-detection), and a panic there would take the process down at the first
/// egress call — so we pin one explicitly, exactly like `src/tts.rs` does.
fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    if let Some(existing) = rustls::crypto::CryptoProvider::get_default() {
        return existing.clone();
    }
    let shared = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    // `install_default` consumes the provider, so hand it a fresh copy; if
    // another thread won the race its provider is the one that counts.
    if rustls::crypto::CryptoProvider::install_default(
        rustls::crypto::aws_lc_rs::default_provider(),
    )
    .is_err()
    {
        if let Some(existing) = rustls::crypto::CryptoProvider::get_default() {
            return existing.clone();
        }
    }
    shared
}

fn anonymous_config() -> Result<Arc<rustls::ClientConfig>, TlsPolicyError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let builder = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| TlsPolicyError::ClientBuild {
            reason: format!("TLS version selection rejected: {e}"),
        })?;
    Ok(Arc::new(
        builder.with_root_certificates(roots).with_no_client_auth(),
    ))
}

async fn handshake(
    host: &str,
    port: u16,
    config: Arc<rustls::ClientConfig>,
    timeout: Duration,
    http_probe: bool,
) -> HandshakeOutcome {
    let connect = async {
        let stream = tokio::net::TcpStream::connect((host, port)).await?;
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid TLS name"))?;
        tokio_rustls::TlsConnector::from(config)
            .connect(name, stream)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e.to_string()))
    };

    match tokio::time::timeout(timeout, connect).await {
        Err(_) => HandshakeOutcome {
            ok: false,
            detail: format!("timed out after {:?}", timeout),
            tls_version: None,
            peer_leaf_sha256: None,
            peer_chain_len: 0,
            request: AfterHandshake::NotProbed,
        },
        Ok(Err(e)) => HandshakeOutcome {
            ok: false,
            detail: e.to_string(),
            tls_version: None,
            peer_leaf_sha256: None,
            peer_chain_len: 0,
            request: AfterHandshake::NotProbed,
        },
        Ok(Ok(stream)) => {
            let session = stream.get_ref().1;
            let peers = session.peer_certificates().unwrap_or_default();
            let tls_version = session
                .protocol_version()
                .map(|v| format!("{v:?}").replace("TLSv", "1."));
            let peer_leaf_sha256 = peers
                .first()
                .map(|c| format!("sha256:{}", hex_fingerprint(c.as_ref())));
            let peer_chain_len = peers.len();
            let request = if http_probe {
                http_request(host, stream, timeout).await
            } else {
                AfterHandshake::NotProbed
            };
            HandshakeOutcome {
                ok: true,
                detail: "TLS handshake completed".to_string(),
                tls_version,
                peer_leaf_sha256,
                peer_chain_len,
                request,
            }
        }
    }
}

/// Send one minimal HTTP/1.1 request over an established TLS session and see
/// whether the endpoint answers — the only way to tell a genuinely open
/// endpoint apart from a TLS 1.3 server that enforces the client certificate
/// after the handshake.
async fn http_request(
    host: &str,
    mut stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    timeout: Duration,
) -> AfterHandshake {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let request = format!(
        "GET / HTTP/1.1\r\nHost: {host}\r\nUser-Agent: ulnclaw-mtls-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    let mut buf = vec![0u8; 512];
    let exchange = async {
        stream.write_all(request.as_bytes()).await?;
        let read = stream.read(&mut buf).await?;
        Ok::<usize, std::io::Error>(read)
    };

    match tokio::time::timeout(timeout, exchange).await {
        Err(_) => AfterHandshake::Refused(format!("no response within {timeout:?}")),
        Ok(Err(e)) => AfterHandshake::Refused(e.to_string()),
        Ok(Ok(0)) => AfterHandshake::Refused("connection closed without a response".to_string()),
        Ok(Ok(n)) => {
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            match http_status(&text) {
                Some(code) => AfterHandshake::Served(code),
                None => AfterHandshake::Refused(format!(
                    "non-HTTP response: {}",
                    text.trim().chars().take(80).collect::<String>()
                )),
            }
        }
    }
}

/// Parse the status code out of a response head (`HTTP/1.1 403 Forbidden`).
fn http_status(head: &str) -> Option<u16> {
    let first = head.lines().next()?;
    let mut parts = first.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize_host(host: &str) -> String {
    let mut host = host.trim().to_ascii_lowercase();
    if let Some((h, _port)) = host.rsplit_once(':') {
        if !h.is_empty() && host.matches(':').count() == 1 && !host.contains('[') {
            host = h.to_string();
        }
    }
    host.trim_end_matches('.').to_string()
}

fn scope_pattern_matches(pattern: &str, host: &str) -> bool {
    let pattern = normalize_host(pattern);
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // `*.example.com` matches `a.example.com` and `a.b.example.com`, but not
        // the apex `example.com` — and, because the boundary is a literal dot,
        // not lookalikes such as `evil-internal.example.com` either.
        let boundary = format!(".{suffix}");
        return host.len() > boundary.len() && host.ends_with(&boundary);
    }
    pattern == host
}

fn parse_hosts(raw: &str) -> Result<HostScope, TlsPolicyError> {
    let entries: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if entries.is_empty() {
        return Err(TlsPolicyError::HostsUnscoped);
    }
    if entries.iter().any(|e| e == "*") {
        return Ok(HostScope::All);
    }
    Ok(HostScope::Hosts(
        entries.iter().map(|e| normalize_host(e)).collect(),
    ))
}

fn parse_bool(raw: &str, name: &str) -> Result<bool, TlsPolicyError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(TlsPolicyError::BadPem {
            path: PathBuf::from(format!("{name}={other}")),
            reason: "expected a boolean (1/0, true/false)".to_string(),
        }),
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, TlsPolicyError> {
    std::fs::read(path).map_err(|e| TlsPolicyError::Unreadable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Split a PEM file into certificates and (optionally) the first private key.
fn parse_pem_blocks(
    path: &Path,
    bytes: &[u8],
) -> Result<
    (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        Option<rustls::pki_types::PrivateKeyDer<'static>>,
    ),
    TlsPolicyError,
> {
    use rustls_pemfile::Item;

    let mut certs = Vec::new();
    let mut key = None;
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));

    for item in rustls_pemfile::read_all(&mut reader) {
        match item {
            Ok(Item::X509Certificate(cert)) => certs.push(cert),
            Ok(Item::Pkcs8Key(k)) => {
                key.get_or_insert(rustls::pki_types::PrivateKeyDer::Pkcs8(k));
            }
            Ok(Item::Pkcs1Key(k)) => {
                key.get_or_insert(rustls::pki_types::PrivateKeyDer::Pkcs1(k));
            }
            Ok(Item::Sec1Key(k)) => {
                key.get_or_insert(rustls::pki_types::PrivateKeyDer::Sec1(k));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(TlsPolicyError::BadPem {
                    path: path.to_path_buf(),
                    reason: e.to_string(),
                })
            }
        }
    }

    Ok((certs, key))
}

fn hex_fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Best-effort `notAfter` for operator expiry checks (`openssl x509 -enddate`).
/// Missing openssl is not an error: the field is informational only.
fn cert_not_after(path: &Path) -> Option<String> {
    let output = std::process::Command::new("openssl")
        .arg("x509")
        .arg("-enddate")
        .arg("-noout")
        .arg("-in")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    text.strip_prefix("notAfter=").map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only self-signed material from `tests/fixtures/mtls/` (generated
    /// with openssl for this test suite; never used outside the tests).
    const TEST_CERT: &str = include_str!("../tests/fixtures/mtls/client.crt");
    const TEST_KEY: &str = include_str!("../tests/fixtures/mtls/client.key");
    const TEST_CA: &str = include_str!("../tests/fixtures/mtls/ca.crt");

    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn write_identity(dir: &Path, with_inline_key: bool) -> PathBuf {
        let path = dir.join(if with_inline_key {
            "identity-inline.pem"
        } else {
            "cert.pem"
        });
        let mut body = TEST_CERT.to_string();
        if with_inline_key {
            body.push('\n');
            body.push_str(TEST_KEY);
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ulnclaw-mtls-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_configuration_means_feature_off() {
        let lookup = lookup_from(&[]);
        assert!(OutboundTls::from_lookup(&lookup).unwrap().is_none());
    }

    #[test]
    fn key_without_cert_is_an_error() {
        let lookup = lookup_from(&[(ENV_KEY, "/tmp/nope.pem")]);
        let err = OutboundTls::from_lookup(&lookup).unwrap_err();
        assert_eq!(err, TlsPolicyError::CertMissing);
        assert!(err.to_string().contains(ENV_CERT));
    }

    #[test]
    fn identity_without_scope_is_an_error() {
        let dir = scratch_dir("unscoped");
        let cert = write_identity(&dir, true);
        let lookup = lookup_from(&[(ENV_CERT, cert.to_str().unwrap())]);
        assert_eq!(
            OutboundTls::from_lookup(&lookup).unwrap_err(),
            TlsPolicyError::HostsUnscoped
        );
    }

    #[test]
    fn missing_key_file_is_reported_against_the_certificate() {
        let dir = scratch_dir("nokey");
        let cert = write_identity(&dir, false);
        let lookup = lookup_from(&[
            (ENV_CERT, cert.to_str().unwrap()),
            (ENV_HOSTS, "gw.example.com"),
        ]);
        let err = OutboundTls::from_lookup(&lookup).unwrap_err();
        assert_eq!(
            err,
            TlsPolicyError::KeyMissing {
                cert: cert.clone()
            }
        );
        assert!(err.to_string().contains(ENV_KEY));
    }

    #[test]
    fn inline_key_and_separate_key_both_work() {
        let dir = scratch_dir("keys");
        let inline = write_identity(&dir, true);
        let separate_cert = write_identity(&dir, false);
        let key_path = dir.join("key.pem");
        std::fs::write(&key_path, TEST_KEY).unwrap();

        let inline_policy = OutboundTls::from_lookup(&lookup_from(&[
            (ENV_CERT, inline.to_str().unwrap()),
            (ENV_HOSTS, "gw.example.com"),
        ]))
        .unwrap()
        .unwrap();
        assert!(inline_policy.key_path().is_none());
        assert_eq!(inline_policy.chain_len(), 1);

        let split_policy = OutboundTls::from_lookup(&lookup_from(&[
            (ENV_CERT, separate_cert.to_str().unwrap()),
            (ENV_KEY, key_path.to_str().unwrap()),
            (ENV_HOSTS, "gw.example.com"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(split_policy.key_path(), Some(key_path.as_path()));
        assert_eq!(
            split_policy.leaf_fingerprint(),
            inline_policy.leaf_fingerprint()
        );
        assert!(split_policy.leaf_fingerprint().starts_with("sha256:"));
    }

    #[test]
    fn unreadable_certificate_is_an_error() {
        let lookup = lookup_from(&[
            (ENV_CERT, "/nonexistent/definitely-missing.pem"),
            (ENV_HOSTS, "gw.example.com"),
        ]);
        match OutboundTls::from_lookup(&lookup).unwrap_err() {
            TlsPolicyError::Unreadable { .. } => {}
            other => panic!("expected Unreadable, got {other:?}"),
        }
    }

    #[test]
    fn pem_without_certificate_block_is_an_error() {
        let dir = scratch_dir("notacert");
        let path = dir.join("junk.pem");
        std::fs::write(&path, "not a pem file at all\n").unwrap();
        let lookup = lookup_from(&[
            (ENV_CERT, path.to_str().unwrap()),
            (ENV_HOSTS, "gw.example.com"),
        ]);
        match OutboundTls::from_lookup(&lookup).unwrap_err() {
            TlsPolicyError::BadPem { reason, .. } => assert!(reason.contains("CERTIFICATE")),
            other => panic!("expected BadPem, got {other:?}"),
        }
    }

    #[test]
    fn host_scope_matching() {
        assert!(HostScope::All.matches("anything.example.com"));
        let scope = HostScope::Hosts(vec![
            "gw.example.com".to_string(),
            "*.internal.example.com".to_string(),
        ]);
        assert!(scope.matches("gw.example.com"));
        assert!(scope.matches("GW.Example.COM"));
        assert!(scope.matches("gw.example.com:8443"));
        assert!(scope.matches("gw.example.com."));
        assert!(scope.matches("a.internal.example.com"));
        assert!(scope.matches("a.b.internal.example.com"));
        assert!(!scope.matches("internal.example.com"));
        assert!(!scope.matches("evil-internal.example.com"));
        assert!(!scope.matches(""));
    }

    #[test]
    fn scope_is_required_for_any_host_configuration() {
        assert_eq!(
            parse_hosts(" , ").unwrap_err(),
            TlsPolicyError::HostsUnscoped
        );
        assert_eq!(parse_hosts("*").unwrap(), HostScope::All);
        assert_eq!(
            parse_hosts("gw.example.com, *.internal").unwrap(),
            HostScope::Hosts(vec!["gw.example.com".to_string(), "*.internal".to_string()])
        );
    }

    #[test]
    fn min_tls_parsing_and_validation() {
        assert_eq!(MinTls::parse("1.2").unwrap(), MinTls::V1_2);
        assert_eq!(MinTls::parse("TLSv1.3").unwrap(), MinTls::V1_3);
        assert_eq!(MinTls::parse(" 12 ").unwrap(), MinTls::V1_2);
        assert!(matches!(
            MinTls::parse("1.1").unwrap_err(),
            TlsPolicyError::BadMinTls { .. }
        ));
    }

    #[test]
    fn trust_builtin_flag_parsing() {
        let dir = scratch_dir("trust");
        let cert = write_identity(&dir, true);
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, TEST_CA).unwrap();
        let pin_only = OutboundTls::from_lookup(&lookup_from(&[
            (ENV_CERT, cert.to_str().unwrap()),
            (ENV_HOSTS, "*"),
            (ENV_TRUST_BUILTIN, "0"),
            (ENV_CA, ca.to_str().unwrap()),
            (ENV_MIN_TLS, "1.3"),
        ]))
        .unwrap()
        .unwrap();
        assert!(!pin_only.trust_builtin());
        assert_eq!(pin_only.min_tls(), MinTls::V1_3);
        assert_eq!(pin_only.ca_path(), Some(ca.as_path()));
        // Pin-only trust with the test CA as the only anchor still builds a
        // usable rustls config.
        assert!(pin_only.rustls_config().is_ok());
        assert!(pin_only
            .apply(reqwest::Client::builder(), "gw.example.com")
            .is_ok());

        let bad = OutboundTls::from_lookup(&lookup_from(&[
            (ENV_CERT, cert.to_str().unwrap()),
            (ENV_HOSTS, "*"),
            (ENV_TRUST_BUILTIN, "maybe"),
        ]))
        .unwrap_err();
        assert!(bad.to_string().contains(ENV_TRUST_BUILTIN));
    }

    #[test]
    fn out_of_scope_host_keeps_a_plain_client() {
        let dir = scratch_dir("scope");
        let cert = write_identity(&dir, true);
        let policy = OutboundTls::from_lookup(&lookup_from(&[
            (ENV_CERT, cert.to_str().unwrap()),
            (ENV_HOSTS, "gw.example.com"),
        ]))
        .unwrap()
        .unwrap();
        assert!(policy.applies_to("gw.example.com"));
        assert!(!policy.applies_to("api.openai.com"));
        // Out-of-scope hosts must still build (plain) clients.
        assert!(policy.apply(reqwest::Client::builder(), "api.openai.com").is_ok());
    }

    #[test]
    fn host_of_extracts_and_normalizes() {
        assert_eq!(host_of("https://GW.Example.com:8443/v1/chat"), "gw.example.com");
        assert_eq!(host_of("https://gw.example.com./v1"), "gw.example.com");
        assert_eq!(host_of("not a url"), "");
        assert_eq!(host_of("http://127.0.0.1:8080"), "127.0.0.1");
    }

    #[test]
    fn status_json_is_redacted_and_complete() {
        let dir = scratch_dir("status");
        let cert = write_identity(&dir, true);
        let policy = OutboundTls::from_lookup(&lookup_from(&[
            (ENV_CERT, cert.to_str().unwrap()),
            (ENV_HOSTS, "gw.example.com,*.internal"),
        ]))
        .unwrap()
        .unwrap();
        let status = policy.status_json();
        assert_eq!(status["enabled"], json!(true));
        assert_eq!(status["hosts"]["kind"], json!("hosts"));
        assert_eq!(status["min_tls"], json!("1.2"));
        assert_eq!(status["trust_builtin"], json!(true));
        assert!(status["leaf_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        assert!(status["wired_paths"].as_array().unwrap().len() >= 2);
        let rendered = status.to_string();
        assert!(rendered.contains("gw.example.com"));
        // Key material must never appear in diagnostics.
        assert!(!rendered.contains("PRIVATE KEY"));
        let key_body = TEST_KEY.lines().nth(1).unwrap_or("");
        assert!(!key_body.is_empty());
        assert!(!rendered.contains(key_body));

        let debug = format!("{policy:?}");
        assert!(!debug.contains("PRIVATE KEY"));
    }

    #[test]
    fn disabled_status_documents_the_configuration() {
        let status = disabled_status_json();
        assert_eq!(status["enabled"], json!(false));
        assert!(status["configure"]
            .as_str()
            .unwrap()
            .contains(ENV_HOSTS));
    }

    #[test]
    fn verdict_classification() {
        let anonymous_with_identity = |request: AfterHandshake| HandshakeOutcome {
            ok: true,
            detail: "ok".into(),
            tls_version: Some("1.3".into()),
            peer_leaf_sha256: None,
            peer_chain_len: 1,
            request,
        };
        let served = anonymous_with_identity(AfterHandshake::Served(200));
        let refused = anonymous_with_identity(AfterHandshake::Refused("alert".into()));
        let rejected = anonymous_with_identity(AfterHandshake::Served(403));
        let handshake_refused = HandshakeOutcome {
            ok: false,
            detail: "certificate required".into(),
            tls_version: None,
            peer_leaf_sha256: None,
            peer_chain_len: 0,
            request: AfterHandshake::NotProbed,
        };
        assert_eq!(
            classify(Some(&served), &handshake_refused),
            ClientAuthVerdict::Enforced,
            "anonymous handshake refused => server enforces mTLS"
        );
        assert_eq!(
            classify(Some(&served), &refused),
            ClientAuthVerdict::Enforced,
            "TLS 1.3: anonymous handshake ok but the request was refused => enforced"
        );
        assert_eq!(
            classify(Some(&served), &rejected),
            ClientAuthVerdict::Enforced,
            "a terminator answering 403 to an anonymous request counts as enforced"
        );
        assert_eq!(classify(Some(&served), &served), ClientAuthVerdict::NotRequired);
        assert_eq!(
            classify(Some(&handshake_refused), &served),
            ClientAuthVerdict::Rejected,
            "our own identity being refused must never read as success"
        );
        assert_eq!(classify(None, &served), ClientAuthVerdict::IdentityMissing);
    }

    #[tokio::test]
    async fn probe_rejects_non_https_and_parses_hosts() {
        let err = probe("http://example.com/health", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.contains("https"));
    }
}