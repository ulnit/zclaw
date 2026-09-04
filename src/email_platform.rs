//! Email platform adapter — port of hermes `plugins/platforms/email`
//! @ v2026.8.3 (adapter.py).
//!
//! Users interact with the agent by email: IMAP polls the INBOX for new
//! messages (UID SEARCH UNSEEN + UID FETCH RFC822 over implicit TLS) and
//! SMTP delivers replies with proper threading headers (`Re:` subject,
//! `In-Reply-To`/`References`, generated `Message-ID`).
//!
//! Security parity with hermes (GHSA-rxqh-5572-8m77): the `From:` header
//! is attacker-controlled, so when an allowlist gates access the sender's
//! domain must be authenticated via the receiving server's
//! `Authentication-Results` header (DMARC pass, or aligned SPF/DKIM
//! pass). The check fails closed and can be opted out of with
//! `require_authenticated_sender = false` / `EMAIL_TRUST_FROM_HEADER=true`
//! for servers that do not stamp the header.
//!
//! Known differences: outbound `send_image`/`send_document` ride the
//! `MEDIA:<path>` reply-tag pipeline (attachments on the reply email);
//! the IPv4-only SMTP reconnect fallback collapses into tokio's happy
//!-eyeballs connect; the `GATEWAY_ALLOW_ALL_USERS` /
//! `GATEWAY_ALLOWED_USERS` global mirrors read the same env vars.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on the in-memory seen-UID set (hermes `_seen_uids_max`).
const SEEN_UIDS_MAX: usize = 2000;
/// Hermes `MAX_MESSAGE_LENGTH` for email bodies.
pub const MAX_MESSAGE_LENGTH: usize = 50_000;

/// Automated-sender address patterns (hermes `_NOREPLY_PATTERNS`).
const NOREPLY_PATTERNS: &[&str] = &[
    "noreply",
    "no-reply",
    "no_reply",
    "donotreply",
    "do-not-reply",
    "mailer-daemon",
    "postmaster",
    "bounce",
    "notifications@",
    "automated@",
    "auto-confirm",
    "auto-reply",
    "automailer",
];

/// `[messaging.email]` — IMAP/SMTP email adapter (hermes
/// `platforms.email` plugin config + `EMAIL_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    pub enabled: bool,
    /// Agent mailbox address (fallback `EMAIL_ADDRESS`).
    pub address: String,
    /// Mailbox password / app password (fallback `EMAIL_PASSWORD`).
    pub password: String,
    /// IMAP host (fallback `EMAIL_IMAP_HOST`).
    pub imap_host: String,
    /// IMAP port — implicit TLS (hermes default 993).
    pub imap_port: u16,
    /// SMTP host (fallback `EMAIL_SMTP_HOST`).
    pub smtp_host: String,
    /// SMTP port — 465 = implicit TLS, anything else STARTTLS (hermes
    /// default 587).
    pub smtp_port: u16,
    /// Seconds between mailbox checks (hermes default 15).
    pub poll_interval_secs: u64,
    /// Sender addresses allowed to talk to the agent (hermes
    /// `EMAIL_ALLOWED_USERS`). Empty = refuse all and log the senders.
    pub allowed_users: Vec<String>,
    /// Accept any sender (hermes `EMAIL_ALLOW_ALL_USERS` /
    /// `GATEWAY_ALLOW_ALL_USERS`).
    pub allow_all_users: bool,
    /// Ignore all attachment/inline parts (hermes `skip_attachments`).
    pub skip_attachments: bool,
    /// Require SPF/DKIM/DMARC-authenticated `From:` domain when the
    /// allowlist gates access (hermes default true, fail-closed;
    /// `EMAIL_TRUST_FROM_HEADER=true` disables).
    pub require_authenticated_sender: bool,
    /// Pin `Authentication-Results` to this authserv-id (defaults to the
    /// agent address' domain).
    pub authserv_id: String,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            address: String::new(),
            password: String::new(),
            imap_host: String::new(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 587,
            poll_interval_secs: 15,
            allowed_users: Vec::new(),
            allow_all_users: false,
            skip_attachments: false,
            require_authenticated_sender: true,
            authserv_id: String::new(),
        }
    }
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_bool(name: &str) -> Option<bool> {
    env_trim(name).map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
}

fn env_u16(name: &str) -> Option<u16> {
    env_trim(name).and_then(|v| v.parse().ok())
}

fn env_u64(name: &str) -> Option<u64> {
    env_trim(name).and_then(|v| v.parse().ok())
}

/// Resolved runtime credentials: env vars win over config (hermes
/// `_get_secret(...) or extra.get(...)`).
#[derive(Debug, Clone)]
pub struct ResolvedEmail {
    pub address: String,
    pub password: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub poll_interval_secs: u64,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub skip_attachments: bool,
    pub require_authenticated_sender: bool,
    pub authserv_id: String,
}

impl EmailConfig {
    pub fn resolve(&self) -> ResolvedEmail {
        let address = env_trim("EMAIL_ADDRESS").unwrap_or_else(|| self.address.trim().to_string());
        let password = env_trim("EMAIL_PASSWORD").unwrap_or_else(|| self.password.clone());
        let imap_host =
            env_trim("EMAIL_IMAP_HOST").unwrap_or_else(|| self.imap_host.trim().to_string());
        let smtp_host =
            env_trim("EMAIL_SMTP_HOST").unwrap_or_else(|| self.smtp_host.trim().to_string());
        let allowed_users = match env_trim("EMAIL_ALLOWED_USERS") {
            Some(raw) => raw
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            None => self
                .allowed_users
                .iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        };
        let allow_all_users = env_bool("EMAIL_ALLOW_ALL_USERS")
            .or_else(|| env_bool("GATEWAY_ALLOW_ALL_USERS"))
            .unwrap_or(self.allow_all_users);
        let require_authenticated_sender = if let Some(trust) = env_bool("EMAIL_TRUST_FROM_HEADER")
        {
            !trust
        } else {
            self.require_authenticated_sender
        };
        ResolvedEmail {
            address,
            password,
            imap_host,
            imap_port: env_u16("EMAIL_IMAP_PORT").unwrap_or(self.imap_port),
            smtp_host,
            smtp_port: env_u16("EMAIL_SMTP_PORT").unwrap_or(self.smtp_port),
            poll_interval_secs: env_u64("EMAIL_POLL_INTERVAL")
                .unwrap_or(self.poll_interval_secs.max(1)),
            allowed_users,
            allow_all_users,
            skip_attachments: self.skip_attachments,
            require_authenticated_sender,
            authserv_id: env_trim("EMAIL_AUTHSERV_ID")
                .unwrap_or_else(|| self.authserv_id.trim().to_lowercase()),
        }
    }
}

// ---------------------------------------------------------------------------
// TLS helpers
// ---------------------------------------------------------------------------

fn tls_client_config() -> Arc<rustls::ClientConfig> {
    static CFG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

async fn wrap_tls<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    host: &str,
    stream: S,
) -> Result<tokio_rustls::client::TlsStream<S>, String> {
    let name = rustls_pki_name(host)?;
    let connector = tokio_rustls::TlsConnector::from(tls_client_config());
    connector
        .connect(name, stream)
        .await
        .map_err(|e| format!("TLS handshake with {host}: {e}"))
}

fn rustls_pki_name(host: &str) -> Result<rustls::pki_types::ServerName<'static>, String> {
    rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| format!("invalid TLS hostname: {host}"))
}

async fn tcp_connect(host: &str, port: u16) -> Result<tokio::net::TcpStream, String> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS lookup {host}:{port}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("no addresses for {host}:{port}"));
    }
    // Hermes' IPv4-only retry: try everything first, then retry with
    // IPv4-only when the happy-eyeballs attempt fails (unreachable-IPv6
    // networks).
    let attempt = |list: Vec<std::net::SocketAddr>| async move {
        let mut last = String::new();
        for addr in list {
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await
            {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) => last = e.to_string(),
                Err(_) => last = "connect timeout".into(),
            }
        }
        Err(last)
    };
    match attempt(addrs.clone()).await {
        Ok(stream) => Ok(stream),
        Err(first_err) => {
            let v4: Vec<_> = addrs.into_iter().filter(|a| a.is_ipv4()).collect();
            if v4.is_empty() {
                Err(first_err)
            } else {
                attempt(v4)
                    .await
                    .map_err(|e| format!("connect {host}:{port}: {first_err}; ipv4 retry: {e}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal IMAP client (implicit TLS, UID commands, literal-aware reader)
// ---------------------------------------------------------------------------

struct ImapConn {
    reader: BufReader<tokio::io::ReadHalf<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>,
    writer: tokio::io::WriteHalf<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    tag_counter: u32,
}

/// One parsed IMAP response: untagged lines plus any `{N}` literals that
/// followed them (RFC 3501 §4.3).
struct ImapResponse {
    lines: Vec<String>,
    literals: Vec<Vec<u8>>,
}

impl ImapConn {
    async fn connect(host: &str, port: u16) -> Result<Self, String> {
        let tcp = tcp_connect(host, port).await?;
        let tls = wrap_tls(host, tcp).await?;
        let (read_half, write_half) = tokio::io::split(tls);
        let mut conn = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            tag_counter: 0,
        };
        let greeting = conn.read_physical_line().await?;
        if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
            return Err(format!("unexpected IMAP greeting: {greeting}"));
        }
        Ok(conn)
    }

    async fn read_physical_line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("IMAP read: {e}"))?;
        if n == 0 {
            return Err("IMAP connection closed".into());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    /// Read one logical line, consuming a trailing `{N}` literal when
    /// present (returns the literal bytes separately).
    async fn read_logical_line(&mut self) -> Result<(String, Option<Vec<u8>>), String> {
        let line = self.read_physical_line().await?;
        let literal_len = literal_len(&line);
        if let Some(len) = literal_len {
            let mut buf = vec![0u8; len];
            self.reader
                .read_exact(&mut buf)
                .await
                .map_err(|e| format!("IMAP literal read: {e}"))?;
            Ok((line, Some(buf)))
        } else {
            Ok((line, None))
        }
    }

    /// Send a tagged command and collect the response until the tagged
    /// status line arrives.
    async fn command(&mut self, cmd: &str) -> Result<ImapResponse, String> {
        self.tag_counter += 1;
        let tag = format!("A{}", self.tag_counter);
        self.writer
            .write_all(format!("{tag} {cmd}\r\n").as_bytes())
            .await
            .map_err(|e| format!("IMAP write: {e}"))?;
        self.writer.flush().await.map_err(|e| e.to_string())?;

        let mut resp = ImapResponse {
            lines: Vec::new(),
            literals: Vec::new(),
        };
        loop {
            let (line, literal) = self.read_logical_line().await?;
            if let Some(bytes) = literal {
                resp.literals.push(bytes);
                // After a literal the server closes the fetch item with a
                // trailing `)` line — keep reading.
                continue;
            }
            if line.starts_with(&format!("{tag} ")) {
                if line.contains("OK") {
                    return Ok(resp);
                }
                return Err(format!("IMAP command failed: {line}"));
            }
            resp.lines.push(line);
        }
    }

    async fn login(&mut self, user: &str, pass: &str) -> Result<(), String> {
        self.command(&format!("LOGIN {} {}", imap_quote(user), imap_quote(pass)))
            .await?;
        Ok(())
    }

    /// RFC 2971 ID — required by 163/NetEase after LOGIN; best-effort
    /// (hermes `_send_imap_id`).
    async fn send_id(&mut self) {
        let version = env!("CARGO_PKG_VERSION");
        let cmd =
            format!("ID (\"name\" \"ulnclaw\" \"version\" \"{version}\" \"vendor\" \"ulnclaw\")");
        let _ = self.command(&cmd).await;
    }

    async fn uid_search(&mut self, criteria: &str) -> Result<Vec<u64>, String> {
        let resp = self.command(&format!("UID SEARCH {criteria}")).await?;
        let mut uids = Vec::new();
        for line in resp.lines {
            if let Some(rest) = line.strip_prefix("* SEARCH") {
                for tok in rest.split_whitespace() {
                    if let Ok(uid) = tok.parse::<u64>() {
                        uids.push(uid);
                    }
                }
            }
        }
        Ok(uids)
    }

    async fn uid_fetch_rfc822(&mut self, uid: u64) -> Result<Vec<u8>, String> {
        let resp = self.command(&format!("UID FETCH {uid} (RFC822)")).await?;
        resp.literals
            .into_iter()
            .next()
            .ok_or_else(|| format!("UID FETCH {uid}: no message literal"))
    }

    async fn logout(mut self) {
        let _ = self.command("LOGOUT").await;
    }
}

/// `... {123}` or `... {123+}` trailer → literal byte count.
fn literal_len(line: &str) -> Option<usize> {
    let trimmed = line.trim_end();
    let open = trimmed.rfind('{')?;
    let close = trimmed.rfind('}')?;
    if close <= open || close != trimmed.len() - 1 {
        return None;
    }
    let inner = trimmed[open + 1..close].trim_end_matches('+');
    inner.parse().ok()
}

/// IMAP quoted-string escaping (backslash + double quote).
fn imap_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Minimal SMTP client (implicit TLS on 465, STARTTLS elsewhere)
// ---------------------------------------------------------------------------

enum SmtpStream {
    Plain(tokio::net::TcpStream),
    Tls(tokio_rustls::client::TlsStream<tokio::net::TcpStream>),
}

impl SmtpStream {
    async fn read_line(&mut self) -> Result<String, String> {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = match self {
                SmtpStream::Plain(s) => s.read(&mut byte).await,
                SmtpStream::Tls(s) => s.read(&mut byte).await,
            }
            .map_err(|e| format!("SMTP read: {e}"))?;
            if n == 0 {
                return Err("SMTP connection closed".into());
            }
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&buf).trim_end().to_string())
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        match self {
            SmtpStream::Plain(s) => s.write_all(data).await,
            SmtpStream::Tls(s) => s.write_all(data).await,
        }
        .map_err(|e| format!("SMTP write: {e}"))
    }
}

struct SmtpConn {
    stream: SmtpStream,
    caps: Vec<String>,
}

impl SmtpConn {
    /// Connect + EHLO (+ STARTTLS upgrade when not implicit-TLS).
    async fn connect(host: &str, port: u16) -> Result<Self, String> {
        let tcp = tcp_connect(host, port).await?;
        let mut conn = if port == 465 {
            let tls = wrap_tls(host, tcp).await?;
            Self {
                stream: SmtpStream::Tls(tls),
                caps: Vec::new(),
            }
        } else {
            Self {
                stream: SmtpStream::Plain(tcp),
                caps: Vec::new(),
            }
        };
        let greeting = conn.read_reply().await?;
        if !greeting.starts_with('2') {
            return Err(format!("SMTP greeting: {greeting}"));
        }
        conn.caps = conn.ehlo().await?;
        if port != 465 && conn.caps.iter().any(|c| c.eq_ignore_ascii_case("STARTTLS")) {
            conn.stream.write_all(b"STARTTLS\r\n").await?;
            let reply = conn.read_reply().await?;
            if !reply.starts_with('2') {
                return Err(format!("STARTTLS rejected: {reply}"));
            }
            let plain = match conn.stream {
                SmtpStream::Plain(s) => s,
                _ => unreachable!(),
            };
            let tls = wrap_tls(host, plain).await?;
            conn.stream = SmtpStream::Tls(tls);
            conn.caps = conn.ehlo().await?;
        }
        Ok(conn)
    }

    async fn ehlo(&mut self) -> Result<Vec<String>, String> {
        self.stream.write_all(b"EHLO ulnclaw\r\n").await?;
        let mut caps = Vec::new();
        loop {
            let line = self.stream.read_line().await?;
            if line.len() >= 4 && line.as_bytes()[3] == b'-' {
                caps.push(line[4..].to_string());
            } else {
                if !line.starts_with('2') {
                    return Err(format!("EHLO rejected: {line}"));
                }
                caps.push(line.get(4..).unwrap_or("").to_string());
                return Ok(caps);
            }
        }
    }

    async fn read_reply(&mut self) -> Result<String, String> {
        loop {
            let line = self.stream.read_line().await?;
            if line.len() >= 4 && line.as_bytes()[3] == b'-' {
                continue;
            }
            return Ok(line);
        }
    }

    async fn login(&mut self, user: &str, pass: &str) -> Result<(), String> {
        let joined = self.caps.join(" ").to_uppercase();
        if joined.contains("PLAIN") {
            let mut raw = Vec::new();
            raw.push(0);
            raw.extend_from_slice(user.as_bytes());
            raw.push(0);
            raw.extend_from_slice(pass.as_bytes());
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
            self.stream
                .write_all(format!("AUTH PLAIN {b64}\r\n").as_bytes())
                .await?;
            let reply = self.read_reply().await?;
            if reply.starts_with('2') {
                return Ok(());
            }
            return Err(format!("AUTH PLAIN rejected: {reply}"));
        }
        if joined.contains("LOGIN") {
            use base64::Engine;
            let engine = base64::engine::general_purpose::STANDARD;
            self.stream.write_all(b"AUTH LOGIN\r\n").await?;
            let reply = self.read_reply().await?;
            if !reply.starts_with('3') {
                return Err(format!("AUTH LOGIN rejected: {reply}"));
            }
            self.stream
                .write_all(format!("{}\r\n", engine.encode(user.as_bytes())).as_bytes())
                .await?;
            let reply = self.read_reply().await?;
            if !reply.starts_with('3') {
                return Err(format!("AUTH LOGIN user rejected: {reply}"));
            }
            self.stream
                .write_all(format!("{}\r\n", engine.encode(pass.as_bytes())).as_bytes())
                .await?;
            let reply = self.read_reply().await?;
            if reply.starts_with('2') {
                return Ok(());
            }
            return Err(format!("AUTH LOGIN password rejected: {reply}"));
        }
        Err("SMTP server offers no supported AUTH mechanism".into())
    }

    async fn send_message(&mut self, from: &str, to: &str, data: &str) -> Result<(), String> {
        self.stream
            .write_all(format!("MAIL FROM:<{from}>\r\n").as_bytes())
            .await?;
        let reply = self.read_reply().await?;
        if !reply.starts_with('2') {
            return Err(format!("MAIL FROM rejected: {reply}"));
        }
        self.stream
            .write_all(format!("RCPT TO:<{to}>\r\n").as_bytes())
            .await?;
        let reply = self.read_reply().await?;
        if !reply.starts_with('2') {
            return Err(format!("RCPT TO rejected: {reply}"));
        }
        self.stream.write_all(b"DATA\r\n").await?;
        let reply = self.read_reply().await?;
        if !reply.starts_with('3') {
            return Err(format!("DATA rejected: {reply}"));
        }
        self.stream.write_all(dot_stuff(data).as_bytes()).await?;
        self.stream.write_all(b"\r\n.\r\n").await?;
        let reply = self.read_reply().await?;
        if !reply.starts_with('2') {
            return Err(format!("message rejected: {reply}"));
        }
        Ok(())
    }

    async fn quit(mut self) {
        let _ = self.stream.write_all(b"QUIT\r\n").await;
    }
}

/// RFC 5321 dot-stuffing + trailing-newline guarantee.
fn dot_stuff(data: &str) -> String {
    let mut out = String::with_capacity(data.len() + 16);
    for line in data.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix('.') {
            out.push('.');
            out.push('.');
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
        out.push_str("\r\n");
    }
    out.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// MIME parsing (mailparse) — hermes _extract_text_body/_extract_attachments
// ---------------------------------------------------------------------------

/// RFC 2047 encoded-word decoder (`=?charset?B|Q?payload?=`).
pub fn decode_rfc2047(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let parsed = (|| -> Option<(String, usize)> {
            let q1 = after.find('?')?;
            let charset = &after[..q1];
            let after2 = &after[q1 + 1..];
            let enc = after2.chars().next()?.to_ascii_uppercase();
            if !after2[1..].starts_with('?') {
                return None;
            }
            let payload_and_rest = &after2[2..];
            let end = payload_and_rest.find("?=")?;
            let payload = &payload_and_rest[..end];
            let bytes = match enc {
                'B' => {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(payload)
                        .ok()?
                }
                'Q' => {
                    let mut bytes = Vec::new();
                    let mut chars = payload.bytes();
                    while let Some(b) = chars.next() {
                        match b {
                            b'=' => {
                                let h = chars.next()?;
                                let l = chars.next()?;
                                let hex = format!("{}{}", h as char, l as char);
                                bytes.push(u8::from_str_radix(&hex, 16).ok()?);
                            }
                            b'_' => bytes.push(b' '),
                            other => bytes.push(other),
                        }
                    }
                    bytes
                }
                _ => return None,
            };
            let text = decode_charset(charset, &bytes);
            let consumed = start + 2 + q1 + 1 + 2 + end + 2;
            Some((text, consumed))
        })();
        match parsed {
            Some((text, consumed)) => {
                out.push_str(&text);
                rest = &rest[consumed..];
            }
            None => {
                out.push_str("=?");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_charset(charset_name: &str, bytes: &[u8]) -> String {
    let lower = charset_name.trim().to_lowercase();
    if lower.is_empty() || lower == "utf-8" || lower == "utf8" {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    match charset::Charset::for_label(lower.as_bytes()) {
        Some(cs) => cs.decode(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Plain-text body extraction with HTML fallback (hermes
/// `_extract_text_body`).
pub fn extract_text_body(mail: &mailparse::ParsedMail<'_>) -> String {
    if mail.subparts.is_empty() {
        let body = mail.get_body().unwrap_or_default();
        if mail.ctype.mimetype.to_lowercase() == "text/html" {
            return strip_html(&body);
        }
        return body;
    }
    // Prefer text/plain parts that are not attachments.
    for part in &mail.subparts {
        if part_is_attachment(part) {
            continue;
        }
        if part.ctype.mimetype.to_lowercase() == "text/plain" {
            let body = part.get_body().unwrap_or_default();
            if !body.trim().is_empty() {
                return body;
            }
        }
    }
    // Fallback: text/html with tags stripped.
    for part in &mail.subparts {
        if part_is_attachment(part) {
            continue;
        }
        if part.ctype.mimetype.to_lowercase() == "text/html" {
            let body = part.get_body().unwrap_or_default();
            if !body.trim().is_empty() {
                return strip_html(&body);
            }
        }
    }
    String::new()
}

/// True when the part carries an explicit Content-Disposition header
/// (hermes keys attachment handling off the disposition string).
fn part_is_attachment(part: &mailparse::ParsedMail<'_>) -> bool {
    part.headers
        .iter()
        .any(|h| h.get_key().eq_ignore_ascii_case("content-disposition"))
}

/// Naive HTML tag stripper (hermes `_strip_html`).
pub fn strip_html(html: &str) -> String {
    let mut text = html.to_string();
    for (pattern, replacement) in [
        (
            regex::Regex::new(r"(?i)<br\s*/?>").unwrap(),
            "\n".to_string(),
        ),
        (
            regex::Regex::new(r"(?i)<p[^>]*>").unwrap(),
            "\n".to_string(),
        ),
        (regex::Regex::new(r"(?i)</p>").unwrap(), "\n".to_string()),
    ] {
        text = pattern
            .replace_all(&text, replacement.as_str())
            .into_owned();
    }
    text = regex::Regex::new(r"<[^>]+>")
        .unwrap()
        .replace_all(&text, "")
        .into_owned();
    for (entity, ch) in [
        ("&nbsp;", " "),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
    ] {
        text = text.replace(entity, ch);
    }
    let collapse = regex::Regex::new(r"\n{3,}").unwrap();
    collapse.replace_all(&text, "\n\n").trim().to_string()
}

/// Bare address out of `Name <addr>` (hermes `_extract_email_address`).
pub fn extract_email_address(raw: &str) -> String {
    if let Some(start) = raw.find('<') {
        if let Some(end) = raw.find('>') {
            if end > start {
                return raw[start + 1..end].trim().to_lowercase();
            }
        }
    }
    raw.trim().to_lowercase()
}

fn domain_of(address: &str) -> String {
    address
        .rsplit_once('@')
        .map(|(_, d)| d.trim().trim_end_matches('.').to_lowercase())
        .unwrap_or_default()
}

/// Relaxed DMARC alignment: equal domains or dot-suffix relationship
/// (hermes `_domains_aligned`).
fn domains_aligned(a: &str, b: &str) -> bool {
    let a = a.trim().trim_end_matches('.').to_lowercase();
    let b = b.trim().trim_end_matches('.').to_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

/// Automated/bulk-mail detection (hermes `_is_automated_sender`).
pub fn is_automated_sender(address: &str, headers: &[(String, String)]) -> bool {
    let addr = address.to_lowercase();
    if NOREPLY_PATTERNS.iter().any(|p| addr.contains(p)) {
        return true;
    }
    let header = |name: &str| -> String {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let auto_submitted = header("Auto-Submitted");
    if !auto_submitted.is_empty() && !auto_submitted.eq_ignore_ascii_case("no") {
        return true;
    }
    let precedence = header("Precedence").to_lowercase();
    if matches!(precedence.as_str(), "bulk" | "list" | "junk") {
        return true;
    }
    if !header("X-Auto-Response-Suppress").is_empty() {
        return true;
    }
    if !header("List-Unsubscribe").is_empty() {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Authentication-Results verification (GHSA-rxqh-5572-8m77 parity)
// ---------------------------------------------------------------------------

/// Verify the `From:` domain is authenticated per the trusted
/// `Authentication-Results` header (hermes `_verify_sender_authentication`).
/// Returns `(authenticated, reason)`; fails closed when the header is
/// absent.
pub fn verify_sender_authentication(
    auth_results: &[String],
    from_addr: &str,
    authserv_id: &str,
) -> (bool, String) {
    let from_domain = domain_of(from_addr);
    if from_domain.is_empty() {
        return (false, "missing From domain".into());
    }
    if auth_results.is_empty() {
        return (false, "no Authentication-Results header".into());
    }
    // The receiving server prepends its result, so the first header is the
    // trusted one — pinned to the configured authserv-id when provided.
    let mut trusted: Option<String> = None;
    for raw in auth_results {
        let value: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if !authserv_id.is_empty() {
            let serv = value.split(';').next().unwrap_or("").trim().to_lowercase();
            if !domains_aligned(&serv, authserv_id) && serv != authserv_id.to_lowercase() {
                continue;
            }
        }
        trusted = Some(value);
        break;
    }
    let Some(trusted) = trusted else {
        return (
            false,
            "no Authentication-Results from trusted authserv-id".into(),
        );
    };

    let method_re = regex::Regex::new(r"\b(dmarc|dkim|spf)\s*=\s*([a-z]+)").unwrap();
    let prop_re = regex::Regex::new(
        r"\b(header\.from|header\.d|smtp\.mailfrom|smtp\.from|envelope-from)\s*=\s*([^\s;]+)",
    )
    .unwrap();
    let lower = trusted.to_lowercase();
    let methods: HashMap<String, String> = method_re
        .captures_iter(&lower)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect();
    let props: HashMap<String, String> = prop_re
        .captures_iter(&lower)
        .map(|c| (c[1].to_string(), c[2].trim_matches('"').to_string()))
        .collect();

    // 1) DMARC pass already enforces From alignment.
    if methods.get("dmarc").map(|s| s.as_str()) == Some("pass") {
        return (true, "dmarc=pass".into());
    }
    // 2) SPF pass aligned with the From domain.
    if methods.get("spf").map(|s| s.as_str()) == Some("pass") {
        let mut spf_domain = props.get("smtp.mailfrom").cloned().unwrap_or_default();
        if spf_domain.is_empty() {
            spf_domain = props.get("smtp.from").cloned().unwrap_or_default();
        }
        if spf_domain.is_empty() {
            spf_domain = props.get("envelope-from").cloned().unwrap_or_default();
        }
        let spf_domain = if spf_domain.contains('@') {
            domain_of(&spf_domain)
        } else {
            spf_domain
        };
        if domains_aligned(&spf_domain, &from_domain) {
            return (true, "spf=pass aligned".into());
        }
    }
    // 3) DKIM pass aligned via header.d.
    if methods.get("dkim").map(|s| s.as_str()) == Some("pass") {
        let mut dkim_domain = props.get("header.d").cloned().unwrap_or_default();
        if dkim_domain.is_empty() {
            dkim_domain = domain_of(props.get("header.from").map(|s| s.as_str()).unwrap_or(""));
        }
        if domains_aligned(&dkim_domain, &from_domain) {
            return (true, "dkim=pass aligned".into());
        }
    }
    let preview: String = trusted.chars().take(120).collect();
    (false, format!("authentication failed ({preview})"))
}

// ---------------------------------------------------------------------------
// MIME message → inbound record
// ---------------------------------------------------------------------------

struct InboundEmail {
    sender_addr: String,
    sender_name: String,
    subject: String,
    message_id: String,
    body: String,
    attachments: Vec<ParsedAttachment>,
    headers: Vec<(String, String)>,
    auth_results: Vec<String>,
}

struct ParsedAttachment {
    bytes: Vec<u8>,
    filename: String,
    mime: String,
}

fn parse_email(raw: &[u8]) -> Result<InboundEmail, String> {
    let mail = mailparse::parse_mail(raw).map_err(|e| format!("MIME parse: {e}"))?;
    let headers: Vec<(String, String)> = mail
        .headers
        .iter()
        .map(|h| (h.get_key().to_lowercase(), h.get_value()))
        .collect();
    let get = |name: &str| -> String {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let sender_raw = get("from");
    let sender_addr = extract_email_address(&sender_raw);
    let mut sender_name = decode_rfc2047(&sender_raw);
    if let Some(idx) = sender_name.find('<') {
        sender_name = sender_name[..idx].trim().trim_matches('"').to_string();
    }
    let subject_raw = get("subject");
    let subject = if subject_raw.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        decode_rfc2047(&subject_raw)
    };
    let auth_results: Vec<String> = headers
        .iter()
        .filter(|(k, _)| k == "authentication-results")
        .map(|(_, v)| v.clone())
        .collect();

    let body = extract_text_body(&mail);
    let mut attachments = Vec::new();
    collect_attachments(&mail, &mut attachments);

    Ok(InboundEmail {
        sender_addr,
        sender_name,
        subject,
        message_id: get("message-id"),
        body,
        attachments,
        headers,
        auth_results,
    })
}

fn collect_attachments(mail: &mailparse::ParsedMail<'_>, out: &mut Vec<ParsedAttachment>) {
    if mail.subparts.is_empty() {
        return;
    }
    for part in &mail.subparts {
        collect_attachments(part, out);
        if !part_is_attachment(part) {
            continue;
        }
        let disp = part.get_content_disposition();
        let mime = part.ctype.mimetype.to_lowercase();
        // Hermes skips text body parts unless explicitly attached.
        let explicit_attachment = matches!(
            disp.disposition,
            mailparse::DispositionType::Attachment | mailparse::DispositionType::FormData
        );
        if (mime == "text/plain" || mime == "text/html") && !explicit_attachment {
            continue;
        }
        let bytes = match part.get_body_raw() {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.is_empty() {
            continue;
        }
        let filename = disp
            .params
            .get("filename")
            .or_else(|| part.ctype.params.get("name"))
            .map(|f| decode_rfc2047(f))
            .unwrap_or_else(|| format!("attachment.{}", mime.split('/').nth(1).unwrap_or("bin")));
        out.push(ParsedAttachment {
            bytes,
            filename,
            mime,
        });
    }
}

// ---------------------------------------------------------------------------
// Runtime: poll loop + dispatch + reply delivery
// ---------------------------------------------------------------------------

struct EmailRuntime {
    cfg: ResolvedEmail,
    /// chat_id (sender address) → (subject, message-id) for reply
    /// threading (hermes `_thread_context`).
    thread_context: Mutex<HashMap<String, (String, String)>>,
    seen_uids: Mutex<HashSet<u64>>,
}

impl EmailRuntime {
    fn authserv_id(&self) -> String {
        if !self.cfg.authserv_id.is_empty() {
            return self.cfg.authserv_id.clone();
        }
        domain_of(&self.cfg.address)
    }

    fn allowlist_in_effect(&self) -> bool {
        !self.cfg.allowed_users.is_empty() || env_trim("GATEWAY_ALLOWED_USERS").is_some()
    }

    fn sender_allowed(&self, addr: &str) -> bool {
        if self.cfg.allow_all_users {
            return true;
        }
        let addr = addr.to_lowercase();
        if self.cfg.allowed_users.iter().any(|a| *a == addr) {
            return true;
        }
        if let Some(global) = env_trim("GATEWAY_ALLOWED_USERS") {
            return global
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .any(|a| a == addr);
        }
        false
    }
}

/// Entry point spawned by `run_messaging` when `[messaging.email]` is
/// enabled.
pub async fn run(
    cfg: EmailConfig,
    dispatcher: Arc<Dispatcher>,
    _pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    let missing: Vec<&str> = [
        ("EMAIL_ADDRESS", resolved.address.as_str()),
        ("EMAIL_PASSWORD", resolved.password.as_str()),
        ("EMAIL_IMAP_HOST", resolved.imap_host.as_str()),
        ("EMAIL_SMTP_HOST", resolved.smtp_host.as_str()),
    ]
    .iter()
    .filter(|(_, v)| v.trim().is_empty())
    .map(|(n, _)| *n)
    .collect();
    if !missing.is_empty() {
        // hermes `_set_fatal_error(retryable=False)`: do not spin forever
        // against an empty host (#40715).
        eprintln!(
            "[email] not configured — missing {}. Set [messaging.email] or the env vars.",
            missing.join(", ")
        );
        return;
    }
    let runtime = Arc::new(EmailRuntime {
        cfg: resolved.clone(),
        thread_context: Mutex::new(HashMap::new()),
        seen_uids: Mutex::new(HashSet::new()),
    });
    crate::messaging::register_platform_sender(
        "email",
        Arc::new(EmailSender {
            runtime: runtime.clone(),
        }),
    );

    // Startup test + seed the seen-UID set with every existing message
    // (hermes `connect()`).
    match startup_seed(&runtime).await {
        Ok(skipped) => {
            println!(
                "[email] connected as {} — {skipped} existing messages skipped",
                resolved.address
            );
        }
        Err(e) => {
            eprintln!("[email] IMAP connection failed: {e}");
            eprintln!(
                "[email] will keep polling every {}s",
                resolved.poll_interval_secs
            );
        }
    }

    loop {
        if let Err(e) = poll_once(&runtime, &dispatcher).await {
            eprintln!("[email] poll error: {e}");
        }
        tokio::time::sleep(Duration::from_secs(resolved.poll_interval_secs)).await;
    }
}

async fn startup_seed(runtime: &Arc<EmailRuntime>) -> Result<usize, String> {
    let cfg = &runtime.cfg;
    let mut conn = ImapConn::connect(&cfg.imap_host, cfg.imap_port).await?;
    conn.login(&cfg.address, &cfg.password).await?;
    conn.send_id().await;
    conn.command("SELECT INBOX").await?;
    let uids = conn.uid_search("ALL").await?;
    {
        let mut seen = runtime.seen_uids.lock().await;
        for uid in &uids {
            seen.insert(*uid);
        }
        trim_seen_uids(&mut seen);
    }
    conn.logout().await;
    Ok(uids.len())
}

/// hermes `_trim_seen_uids`: keep the highest half once over the cap.
fn trim_seen_uids(seen: &mut HashSet<u64>) {
    if seen.len() <= SEEN_UIDS_MAX {
        return;
    }
    let mut sorted: Vec<u64> = seen.iter().copied().collect();
    sorted.sort_unstable();
    let keep = SEEN_UIDS_MAX / 2;
    let keepers: HashSet<u64> = sorted[sorted.len() - keep..].iter().copied().collect();
    *seen = keepers;
}

async fn poll_once(
    runtime: &Arc<EmailRuntime>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), String> {
    let cfg = &runtime.cfg;
    let mut conn = ImapConn::connect(&cfg.imap_host, cfg.imap_port).await?;
    conn.login(&cfg.address, &cfg.password)
        .await
        .map_err(|e| format!("IMAP login: {e}"))?;
    conn.send_id().await;
    conn.command("SELECT INBOX").await?;
    let unseen = conn.uid_search("UNSEEN").await?;
    let mut fresh: Vec<u64> = Vec::new();
    {
        let mut seen = runtime.seen_uids.lock().await;
        for uid in unseen {
            if !seen.contains(&uid) {
                seen.insert(uid);
                fresh.push(uid);
            }
        }
        trim_seen_uids(&mut seen);
    }
    for uid in fresh {
        let raw = match conn.uid_fetch_rfc822(uid).await {
            Ok(raw) => raw,
            Err(e) => {
                eprintln!("[email] UID FETCH {uid} failed: {e}");
                continue;
            }
        };
        let inbound = match parse_email(&raw) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[email] UID {uid}: {e}");
                continue;
            }
        };
        dispatch_email(runtime, dispatcher, inbound).await;
    }
    conn.logout().await;
    Ok(())
}

async fn dispatch_email(
    runtime: &Arc<EmailRuntime>,
    dispatcher: &Arc<Dispatcher>,
    inbound: InboundEmail,
) {
    if is_automated_sender(&inbound.sender_addr, &inbound.headers) {
        return;
    }
    // From: authentication gate (only meaningful when the allowlist gates
    // access and allow-all is off — hermes `_dispatch_message`).
    if runtime.cfg.require_authenticated_sender
        && runtime.allowlist_in_effect()
        && !runtime.cfg.allow_all_users
    {
        let (ok, reason) = verify_sender_authentication(
            &inbound.auth_results,
            &inbound.sender_addr,
            &runtime.authserv_id(),
        );
        if !ok {
            eprintln!(
                "[email] dropping sender with unauthenticated From: {} ({reason})",
                inbound.sender_addr
            );
            return;
        }
    }
    if !runtime.sender_allowed(&inbound.sender_addr) {
        eprintln!(
            "[email] unauthorized sender {} — add to [messaging.email] allowed_users",
            inbound.sender_addr
        );
        return;
    }

    let subject = inbound.subject.clone();
    let mut text = inbound.body.trim().to_string();
    if text.is_empty() {
        text = "(empty email)".to_string();
    }
    if !subject.is_empty() && !subject.starts_with("Re:") {
        text = format!("[Subject: {subject}]\n\n{text}");
    }

    // Cache attachments into the media cache.
    let home = crate::config::ulnclaw_home();
    let mut attachments: Vec<MediaAttachment> = Vec::new();
    if !runtime.cfg.skip_attachments {
        for att in &inbound.attachments {
            match crate::media_cache::cache_media_bytes(&home, &att.bytes, &att.mime, &att.filename)
            {
                Ok(path) => attachments.push(MediaAttachment {
                    bytes: att.bytes.len() as u64,
                    mime: att.mime.clone(),
                    path,
                    original_name: att.filename.clone(),
                }),
                Err(e) => eprintln!("[email] attachment cache failed: {e}"),
            }
        }
    }

    runtime.thread_context.lock().await.insert(
        inbound.sender_addr.clone(),
        (subject.clone(), inbound.message_id.clone()),
    );

    let event = MessageEvent {
        platform: "email".into(),
        chat_id: inbound.sender_addr.clone(),
        sender_id: inbound.sender_addr.clone(),
        sender_name: if inbound.sender_name.is_empty() {
            inbound.sender_addr.clone()
        } else {
            inbound.sender_name.clone()
        },
        text,
        message_id: inbound.message_id.clone(),
        attachments,
    };
    println!(
        "[email] new message from {}: {subject}",
        inbound.sender_addr
    );
    let outcome = match dispatcher.handle_event(event).await {
        Ok(outcome) => outcome,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    let mut full_reply = String::new();
    for echo in &outcome.transcript_echoes {
        full_reply.push_str(echo);
        full_reply.push('\n');
    }
    full_reply.push_str(&outcome.reply);
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&full_reply);
    if !reply_text.trim().is_empty() || !media_paths.is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("email", &inbound.sender_addr, &reply_text, || async {
                match send_email(runtime, &inbound.sender_addr, &reply_text, &media_paths).await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        eprintln!("[email] reply to {} failed: {e}", inbound.sender_addr);
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
}

/// Build + send one email (reply threading from `_thread_context`).
async fn send_email(
    runtime: &Arc<EmailRuntime>,
    to_addr: &str,
    body: &str,
    attachment_paths: &[std::path::PathBuf],
) -> Result<String, String> {
    let cfg = &runtime.cfg;
    let ctx = runtime
        .thread_context
        .lock()
        .await
        .get(to_addr)
        .cloned()
        .unwrap_or_default();
    let mut subject = if ctx.0.is_empty() {
        "Ulnclaw Agent".to_string()
    } else {
        ctx.0.clone()
    };
    if !subject.starts_with("Re:") {
        subject = format!("Re: {subject}");
    }
    let (data, msg_id) = render_mime(
        &cfg.address,
        to_addr,
        &subject,
        ctx.1.trim(),
        body,
        attachment_paths,
    );
    let mut conn = SmtpConn::connect(&cfg.smtp_host, cfg.smtp_port).await?;
    conn.login(&cfg.address, &cfg.password).await?;
    conn.send_message(&cfg.address, to_addr, &data).await?;
    conn.quit().await;
    Ok(msg_id)
}

fn message_id_domain(address: &str) -> String {
    match address.rsplit_once('@') {
        Some((_, d)) if !d.is_empty() => d.to_string(),
        _ => "localhost".to_string(),
    }
}

/// Assemble the MIME document (headers + base64 body + attachments).
/// Returns the wire data plus the generated Message-ID.
fn render_mime(
    from: &str,
    to: &str,
    subject: &str,
    in_reply_to: &str,
    body: &str,
    attachment_paths: &[std::path::PathBuf],
) -> (String, String) {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let boundary = format!("ulnclaw-{}", uuid::Uuid::new_v4().simple());
    let msg_id = format!(
        "<ulnclaw-{}@{}>",
        &uuid::Uuid::new_v4().simple().to_string()[..12],
        message_id_domain(from)
    );
    let date = chrono::Local::now()
        .format("%a, %d %b %Y %H:%M:%S %z")
        .to_string();
    let encoded_subject = encode_rfc2047(subject);

    let mut out = String::new();
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("To: {to}\r\n"));
    out.push_str(&format!("Subject: {encoded_subject}\r\n"));
    if !in_reply_to.is_empty() {
        out.push_str(&format!("In-Reply-To: {in_reply_to}\r\n"));
        out.push_str(&format!("References: {in_reply_to}\r\n"));
    }
    out.push_str(&format!("Date: {date}\r\n"));
    out.push_str(&format!("Message-ID: {msg_id}\r\n"));
    out.push_str("MIME-Version: 1.0\r\n");

    if attachment_paths.is_empty() {
        out.push_str("Content-Type: text/plain; charset=\"utf-8\"\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        out.push_str(&wrap_b64(&engine.encode(body.as_bytes())));
    } else {
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"
        ));
        out.push_str(&format!("--{boundary}\r\n"));
        out.push_str("Content-Type: text/plain; charset=\"utf-8\"\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        out.push_str(&wrap_b64(&engine.encode(body.as_bytes())));
        for path in attachment_paths {
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[email] failed to attach {}: {e}", path.display());
                    continue;
                }
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".into());
            let mime = crate::media_cache::mime_for_ext(path);
            out.push_str(&format!("--{boundary}\r\n"));
            out.push_str(&format!("Content-Type: {mime}\r\n"));
            out.push_str("Content-Transfer-Encoding: base64\r\n");
            out.push_str(&format!(
                "Content-Disposition: attachment; filename=\"{name}\"\r\n\r\n"
            ));
            out.push_str(&wrap_b64(&engine.encode(&bytes)));
        }
        out.push_str(&format!("--{boundary}--\r\n"));
    }
    (out, msg_id)
}

/// RFC 2047 B-encoding for non-ASCII subjects.
fn encode_rfc2047(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii() && b != b'=' && b != b'\n' && b != b'\r')
    {
        return s.to_string();
    }
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    format!("=?utf-8?B?{b64}?=")
}

fn wrap_b64(b64: &str) -> String {
    let mut out = String::new();
    for chunk in b64.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\r\n");
    }
    out
}

struct EmailSender {
    runtime: Arc<EmailRuntime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for EmailSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = send_email(&self.runtime, chat_id, text, &[]).await {
            eprintln!("[email] send_text to {chat_id} failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automated_sender_patterns() {
        let headers: Vec<(String, String)> = Vec::new();
        assert!(is_automated_sender("noreply@github.com", &headers));
        assert!(is_automated_sender("Mailer-Daemon@google.com", &headers));
        assert!(!is_automated_sender("alice@example.com", &headers));
    }

    #[test]
    fn automated_headers_detected() {
        let headers = vec![("auto-submitted".to_string(), "auto-replied".to_string())];
        assert!(is_automated_sender("bob@example.com", &headers));
        let headers = vec![("auto-submitted".to_string(), "no".to_string())];
        assert!(!is_automated_sender("bob@example.com", &headers));
        let headers = vec![("precedence".to_string(), "bulk".to_string())];
        assert!(is_automated_sender("list@example.com", &headers));
        let headers = vec![("list-unsubscribe".to_string(), "<mailto:x>".to_string())];
        assert!(is_automated_sender("news@example.com", &headers));
    }

    #[test]
    fn extract_address_from_display_form() {
        assert_eq!(
            extract_email_address("Alice <Alice@Example.com>"),
            "alice@example.com"
        );
        assert_eq!(extract_email_address("bob@example.com "), "bob@example.com");
    }

    #[test]
    fn domains_alignment_relaxed() {
        assert!(domains_aligned("example.com", "example.com"));
        assert!(domains_aligned("mail.example.com", "example.com"));
        assert!(domains_aligned("example.com", "mail.example.com"));
        assert!(!domains_aligned("example.com", "evil.com"));
        assert!(!domains_aligned("", "example.com"));
    }

    #[test]
    fn auth_results_dmarc_pass() {
        let ar = vec!["mx.example.com; dmarc=pass header.from=sender.com".to_string()];
        let (ok, reason) = verify_sender_authentication(&ar, "user@sender.com", "");
        assert!(ok, "{reason}");
        assert_eq!(reason, "dmarc=pass");
    }

    #[test]
    fn auth_results_spf_aligned_pass() {
        let ar = vec!["mx.example.com; spf=pass smtp.mailfrom=user@mail.sender.com".to_string()];
        let (ok, _) = verify_sender_authentication(&ar, "user@sender.com", "");
        assert!(ok);
    }

    #[test]
    fn auth_results_dkim_aligned_pass() {
        let ar = vec!["mx.example.com; dkim=pass header.d=sender.com".to_string()];
        let (ok, _) = verify_sender_authentication(&ar, "user@sender.com", "");
        assert!(ok);
    }

    #[test]
    fn auth_results_missing_fails_closed() {
        let (ok, reason) = verify_sender_authentication(&[], "user@sender.com", "");
        assert!(!ok);
        assert_eq!(reason, "no Authentication-Results header");
    }

    #[test]
    fn auth_results_fail_misaligned_spf() {
        let ar = vec!["mx.example.com; spf=pass smtp.mailfrom=user@evil.com".to_string()];
        let (ok, _) = verify_sender_authentication(&ar, "user@sender.com", "");
        assert!(!ok);
    }

    #[test]
    fn auth_results_first_header_wins() {
        // Attacker-injected header sorts after the receiving server's.
        let ar = vec![
            "mx.example.com; spf=fail smtp.mailfrom=user@evil.com".to_string(),
            "attacker.com; dmarc=pass header.from=sender.com".to_string(),
        ];
        let (ok, _) = verify_sender_authentication(&ar, "user@sender.com", "");
        assert!(!ok);
    }

    #[test]
    fn authserv_id_pinning() {
        let ar = vec!["other-mx.com; dmarc=pass header.from=sender.com".to_string()];
        let (ok, reason) = verify_sender_authentication(&ar, "user@sender.com", "mx.example.com");
        assert!(!ok);
        assert!(reason.contains("authserv-id"));
        let ar = vec!["mx.example.com; dmarc=pass header.from=sender.com".to_string()];
        let (ok, _) = verify_sender_authentication(&ar, "user@sender.com", "mx.example.com");
        assert!(ok);
    }

    #[test]
    fn rfc2047_b_and_q_decode() {
        assert_eq!(decode_rfc2047("=?utf-8?B?5L2g5aW9?= test"), "你好 test");
        assert_eq!(decode_rfc2047("=?utf-8?Q?Hello_World?="), "Hello World");
        assert_eq!(decode_rfc2047("plain subject"), "plain subject");
    }

    #[test]
    fn strip_html_basic() {
        let html = "<p>Hi&nbsp;there</p><br/><b>bold</b>&amp;more";
        let text = strip_html(html);
        assert!(text.contains("Hi there"));
        assert!(text.contains("bold"));
        assert!(text.contains("&more"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn dot_stuffing() {
        assert_eq!(dot_stuff(".lead"), "..lead");
        assert_eq!(dot_stuff("a\nb"), "a\r\nb");
    }

    #[test]
    fn literal_len_parsing() {
        assert_eq!(literal_len("* 1 FETCH (RFC822 {123}"), Some(123));
        assert_eq!(literal_len("* 1 FETCH (RFC822 {123+}"), Some(123));
        assert_eq!(literal_len("* OK done"), None);
        assert_eq!(literal_len("* OK {not a number}"), None);
    }

    #[test]
    fn imap_quote_escapes() {
        assert_eq!(imap_quote("plain"), "\"plain\"");
        assert_eq!(imap_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn trim_seen_uids_keeps_highest_half() {
        let mut seen: HashSet<u64> = (1..=2500).collect();
        trim_seen_uids(&mut seen);
        assert_eq!(seen.len(), SEEN_UIDS_MAX / 2);
        assert!(seen.contains(&2500));
        assert!(!seen.contains(&1));
    }

    #[test]
    fn parse_simple_email() {
        let raw = b"From: Alice <alice@example.com>\r\nSubject: =?utf-8?B?5L2g5aW9?=\r\nMessage-ID: <m1@example.com>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello body\r\n";
        let mail = parse_email(raw).unwrap();
        assert_eq!(mail.sender_addr, "alice@example.com");
        assert_eq!(mail.sender_name, "Alice");
        assert_eq!(mail.subject, "你好");
        assert_eq!(mail.message_id, "<m1@example.com>");
        assert!(mail.body.contains("Hello body"));
    }

    #[test]
    fn parse_multipart_prefers_plain_text() {
        let raw = concat!(
            "From: bob@example.com\r\n",
            "Subject: hi\r\n",
            "Content-Type: multipart/alternative; boundary=\"B1\"\r\n\r\n",
            "--B1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nplain version\r\n",
            "--B1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>html version</p>\r\n",
            "--B1--\r\n",
        )
        .as_bytes();
        let mail = parse_email(raw).unwrap();
        assert!(mail.body.contains("plain version"));
        assert!(!mail.body.contains("html"));
    }

    #[test]
    fn subject_thread_prefix() {
        let subject = "Question about X".to_string();
        let mut reply_subject = subject.clone();
        if !reply_subject.starts_with("Re:") {
            reply_subject = format!("Re: {reply_subject}");
        }
        assert_eq!(reply_subject, "Re: Question about X");
        let already = "Re: Question".to_string();
        assert!(already.starts_with("Re:"));
    }

    #[test]
    fn mime_message_has_threading_headers() {
        let (data, msg_id) = render_mime(
            "agent@example.com",
            "user@example.com",
            "Re: Hello",
            "<orig@example.com>",
            "body text",
            &[],
        );
        assert!(msg_id.starts_with("<ulnclaw-"));
        assert!(msg_id.ends_with("@example.com>"));
        assert!(data.contains("In-Reply-To: <orig@example.com>"));
        assert!(data.contains("References: <orig@example.com>"));
    }

    #[test]
    fn encode_rfc2047_passthrough_ascii() {
        assert_eq!(encode_rfc2047("plain"), "plain");
        let encoded = encode_rfc2047("你好");
        assert!(encoded.starts_with("=?utf-8?B?"));
        assert_eq!(decode_rfc2047(&encoded), "你好");
    }
}
