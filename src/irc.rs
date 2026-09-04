//! IRC platform adapter — port of hermes `plugins/platforms/irc`
//! @ v2026.8.3 (adapter.py).
//!
//! Zero-dependency IRC client over tokio TCP (rustls for TLS): the
//! hermes registration sequence (`PASS` → `NICK` → `USER`, wait for
//! `001 RPL_WELCOME` with a 30 s timeout, optional NickServ
//! `IDENTIFY`, then `JOIN`), `433` nick-collision retries with
//! incrementing suffixes, PING/PONG keepalive, and CTCP handling
//! (`ACTION` becomes `* nick text`, everything else is dropped).
//!
//! Channel intake requires addressing (`nick:` / `nick,` / `nick `
//! prefix, stripped before dispatch); DMs always pass. The hermes IRC
//! allowlist is opt-in: an empty `allowed_users` allows everyone (IRC
//! nicks carry no real identity — hermes semantics, documented
//! divergence from the fail-closed default of chat platforms).
//!
//! Outbound replies are markdown-stripped (IRC variant: images → url,
//! links → `text (url)`) and split to fit the 510-byte IRC line limit
//! after `PRIVMSG <target> :` overhead, with a 0.3 s flood pause
//! between lines.

use crate::messaging::{Dispatcher, MessageEvent};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// hermes default `max_message_length` for IRC.
const DEFAULT_MAX_MESSAGE_LENGTH: usize = 450;
/// IRC line limit minus CRLF (hermes uses 510).
const IRC_MAX_LINE_BYTES: usize = 510;
/// hermes flood pause between PRIVMSG lines.
const SEND_PAUSE: Duration = Duration::from_millis(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(30);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// `[messaging.irc]` — IRC adapter (hermes `platforms.irc` plugin
/// config + `IRC_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IrcConfig {
    pub enabled: bool,
    /// IRC server host (fallback `IRC_SERVER`).
    pub server: String,
    /// IRC server port (fallback `IRC_PORT`, default 6697).
    pub port: u16,
    /// Bot nickname (fallback `IRC_NICKNAME`, default `ulnclaw-bot`).
    pub nickname: String,
    /// Channel to join, e.g. `#hermes` (fallback `IRC_CHANNEL`).
    pub channel: String,
    /// Use TLS (fallback `IRC_USE_TLS`, default true).
    pub use_tls: bool,
    /// Optional server password (fallback `IRC_SERVER_PASSWORD`).
    pub server_password: String,
    /// Optional NickServ password (fallback `IRC_NICKSERV_PASSWORD`).
    pub nickserv_password: String,
    /// Nicks allowed to talk to the bot (case-insensitive). Empty =
    /// allow all (hermes IRC semantics — nicks are not authenticated).
    pub allowed_users: Vec<String>,
    /// Per-line content cap before splitting (hermes
    /// `max_message_length`).
    pub max_message_length: usize,
}

impl Default for IrcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: String::new(),
            port: 6697,
            nickname: "ulnclaw-bot".into(),
            channel: String::new(),
            use_tls: true,
            server_password: String::new(),
            nickserv_password: String::new(),
            allowed_users: Vec::new(),
            max_message_length: DEFAULT_MAX_MESSAGE_LENGTH,
        }
    }
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_list(name: &str) -> Option<Vec<String>> {
    env_trim(name).map(|raw| {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedIrc {
    pub server: String,
    pub port: u16,
    pub nickname: String,
    pub channel: String,
    pub use_tls: bool,
    pub server_password: String,
    pub nickserv_password: String,
    pub allowed_users: Vec<String>,
    pub max_message_length: usize,
}

impl IrcConfig {
    pub fn resolve(&self) -> ResolvedIrc {
        ResolvedIrc {
            server: env_trim("IRC_SERVER").unwrap_or_else(|| self.server.clone()),
            port: env_trim("IRC_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.port),
            nickname: env_trim("IRC_NICKNAME").unwrap_or_else(|| self.nickname.clone()),
            channel: env_trim("IRC_CHANNEL").unwrap_or_else(|| self.channel.clone()),
            use_tls: env_trim("IRC_USE_TLS")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(self.use_tls),
            server_password: env_trim("IRC_SERVER_PASSWORD")
                .unwrap_or_else(|| self.server_password.clone()),
            nickserv_password: env_trim("IRC_NICKSERV_PASSWORD")
                .unwrap_or_else(|| self.nickserv_password.clone()),
            allowed_users: env_list("IRC_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            max_message_length: self.max_message_length.max(1),
        }
    }
}

/// hermes `_parse_irc_message` — split a raw line into prefix/command/
/// params (with `:trailing` support).
pub fn parse_irc_message(raw: &str) -> (String, String, Vec<String>) {
    let mut rest = raw;
    let mut prefix = String::new();
    if let Some(stripped) = rest.strip_prefix(':') {
        match stripped.split_once(' ') {
            Some((p, r)) => {
                prefix = p.to_string();
                rest = r;
            }
            None => {
                prefix = stripped.to_string();
                rest = "";
            }
        }
    }
    let mut trailing = String::new();
    if let Some((head, tail)) = rest.split_once(" :") {
        rest = head;
        trailing = tail.to_string();
    }
    let mut parts = rest.split_whitespace().map(|s| s.to_string());
    let command = parts.next().unwrap_or_default();
    let mut params: Vec<String> = parts.collect();
    if !trailing.is_empty() || raw.contains(" :") {
        params.push(trailing);
    }
    (prefix, command, params)
}

/// hermes `_extract_nick` — `nick!user@host` → `nick`.
pub fn extract_nick(prefix: &str) -> &str {
    prefix.split('!').next().unwrap_or(prefix)
}

/// hermes IRC `_strip_markdown` (images → url, links → `text (url)`).
pub fn strip_markdown_irc(text: &str) -> String {
    let mut out = text.to_string();
    out = crate::sms::strip_paired_pub(&out, "**");
    out = crate::sms::strip_paired_pub(&out, "__");
    out = crate::sms::strip_paired_pub(&out, "*");
    out = crate::sms::strip_paired_pub(&out, "`");
    // Code fences.
    out = out.replace("```", "");
    // Images: ![alt](url) → url (before links).
    out = strip_images(&out);
    // Links: [text](url) → text (url).
    out = strip_links_with_url(&out);
    out
}

fn strip_images(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("![") {
        let Some(close_label) = rest[start..].find(']') else {
            break;
        };
        let label_end = start + close_label;
        if rest[label_end..].starts_with("](") {
            let Some(close_target) = rest[label_end..].find(')') else {
                break;
            };
            out.push_str(&rest[..start]);
            out.push_str(&rest[label_end + 2..label_end + close_target]);
            rest = &rest[label_end + close_target + 1..];
        } else {
            out.push_str(&rest[..label_end + 1]);
            rest = &rest[label_end + 1..];
        }
    }
    out.push_str(rest);
    out
}

fn strip_links_with_url(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('[') {
        let Some(close_label) = rest[start..].find(']') else {
            break;
        };
        let label_end = start + close_label;
        if rest[label_end..].starts_with("](") {
            let Some(close_target) = rest[label_end..].find(')') else {
                break;
            };
            let label = &rest[start + 1..label_end];
            let target = &rest[label_end + 2..label_end + close_target];
            out.push_str(&rest[..start]);
            out.push_str(&format!("{label} ({target})"));
            rest = &rest[label_end + close_target + 1..];
        } else {
            out.push_str(&rest[..label_end + 1]);
            rest = &rest[label_end + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// hermes `_split_message` — byte-safe split at the 510-line limit
/// minus protocol overhead, preferring space boundaries.
pub fn split_message(content: &str, target: &str, user_limit: usize) -> Vec<String> {
    let overhead = format!("PRIVMSG {target} :").len() + 2; // +2 for CRLF
    let max_bytes = IRC_MAX_LINE_BYTES.saturating_sub(overhead);
    let limit = user_limit.min(max_bytes);
    let mut lines = Vec::new();
    for paragraph in content.split('\n') {
        if paragraph.trim().is_empty() {
            continue;
        }
        let mut para = paragraph.to_string();
        loop {
            if para.as_bytes().len() <= limit {
                if !para.trim().is_empty() {
                    lines.push(para);
                }
                break;
            }
            // Largest char-boundary prefix that fits.
            let mut split_at = 0;
            let mut byte_count = 0;
            for (idx, ch) in para.char_indices() {
                let ch_len = ch.len_utf8();
                if byte_count + ch_len > limit {
                    break;
                }
                byte_count += ch_len;
                split_at = idx + ch_len;
            }
            // Prefer a space boundary past the first third.
            if let Some(space) = para[..split_at].rfind(' ') {
                if space > split_at / 3 {
                    split_at = space;
                }
            }
            lines.push(para[..split_at].trim_end().to_string());
            para = para[split_at..].trim_start().to_string();
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

enum IrcStream {
    Plain(tokio::net::TcpStream),
    Tls(tokio_rustls::client::TlsStream<tokio::net::TcpStream>),
}

impl IrcStream {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            IrcStream::Plain(s) => s.read(buf).await,
            IrcStream::Tls(s) => s.read(buf).await,
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            IrcStream::Plain(s) => s.write_all(buf).await,
            IrcStream::Tls(s) => s.write_all(buf).await,
        }
    }
}

struct Runtime {
    cfg: ResolvedIrc,
    current_nick: Mutex<String>,
    /// Outbound request channel into the live session loop (target, text).
    outbound: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<(String, String)>>>,
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: IrcConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    if resolved.server.is_empty() || resolved.channel.is_empty() {
        eprintln!(
            "[irc] disabled: server/channel not configured (set [messaging.irc] or IRC_SERVER/IRC_CHANNEL)"
        );
        return;
    }
    let runtime = Arc::new(Runtime {
        current_nick: Mutex::new(resolved.nickname.clone()),
        outbound: std::sync::Mutex::new(None),
        cfg: resolved,
    });
    crate::messaging::register_platform_sender(
        "irc",
        Arc::new(IrcSender {
            runtime: runtime.clone(),
        }),
    );
    loop {
        match run_session(&runtime, &dispatcher, &pairing).await {
            Ok(()) => {}
            Err(e) => eprintln!("[irc] session error: {e}"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn connect_stream(cfg: &ResolvedIrc) -> Result<IrcStream, String> {
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((cfg.server.as_str(), cfg.port)),
    )
    .await
    .map_err(|_| {
        format!(
            "connect timeout {}:{} after {}s",
            cfg.server,
            cfg.port,
            CONNECT_TIMEOUT.as_secs()
        )
    })?
    .map_err(|e| format!("connect {}:{}: {e}", cfg.server, cfg.port))?;
    if cfg.use_tls {
        let name = rustls::pki_types::ServerName::try_from(cfg.server.clone())
            .map_err(|_| format!("invalid TLS hostname: {}", cfg.server))?;
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tls = connector
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS handshake with {}: {e}", cfg.server))?;
        Ok(IrcStream::Tls(tls))
    } else {
        Ok(IrcStream::Plain(tcp))
    }
}

async fn send_raw(stream: &mut IrcStream, line: &str) -> Result<(), String> {
    stream
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))
}

async fn run_session(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    _pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), String> {
    let mut stream = connect_stream(&runtime.cfg).await?;
    let (send_tx, mut send_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    *runtime.outbound.lock().unwrap() = Some(send_tx);
    // Registration sequence (hermes connect()).
    if !runtime.cfg.server_password.is_empty() {
        send_raw(
            &mut stream,
            &format!("PASS {}", runtime.cfg.server_password),
        )
        .await?;
    }
    send_raw(&mut stream, &format!("NICK {}", runtime.cfg.nickname)).await?;
    send_raw(
        &mut stream,
        &format!("USER {} 0 * :UlncLaw Agent", runtime.cfg.nickname),
    )
    .await?;

    let mut buffer = Vec::new();
    let deadline = tokio::time::Instant::now() + REGISTRATION_TIMEOUT;
    // Wait for 001 RPL_WELCOME, handling 433 nick retries on the way.
    loop {
        let line = tokio::time::timeout(
            deadline.duration_since(tokio::time::Instant::now()),
            read_line(&mut stream, &mut buffer),
        )
        .await
        .map_err(|_| "registration timed out (no RPL_WELCOME)".to_string())??;
        let (prefix, command, params) = parse_irc_message(&line);
        match command.as_str() {
            "PING" => {
                let payload = params.first().cloned().unwrap_or_default();
                send_raw(&mut stream, &format!("PONG :{payload}")).await?;
            }
            "001" => {
                if let Some(nick) = params.first() {
                    *runtime.current_nick.lock().await = nick.clone();
                }
                break;
            }
            "433" => {
                let next = next_collision_nick(
                    &runtime.cfg.nickname,
                    &runtime.current_nick.lock().await.clone(),
                );
                *runtime.current_nick.lock().await = next.clone();
                send_raw(&mut stream, &format!("NICK {next}")).await?;
            }
            _ => {
                let _ = prefix;
            }
        }
    }
    if !runtime.cfg.nickserv_password.is_empty() {
        send_raw(
            &mut stream,
            &format!(
                "PRIVMSG NickServ :IDENTIFY {}",
                runtime.cfg.nickserv_password
            ),
        )
        .await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    send_raw(&mut stream, &format!("JOIN {}", runtime.cfg.channel)).await?;
    eprintln!(
        "[irc] connected to {}:{} as {}, joined {}",
        runtime.cfg.server,
        runtime.cfg.port,
        runtime.current_nick.lock().await,
        runtime.cfg.channel
    );

    // Main receive loop (selects outbound requests onto the same socket).
    loop {
        tokio::select! {
            line = read_line(&mut stream, &mut buffer) => {
                let line = line?;
                let (_prefix, command, params) = parse_irc_message(&line);
                match command.as_str() {
                    "PING" => {
                        let payload = params.first().cloned().unwrap_or_default();
                        send_raw(&mut stream, &format!("PONG :{payload}")).await?;
                    }
                    "PRIVMSG" if params.len() >= 2 => {
                        let sender = extract_nick(&_prefix).to_string();
                        handle_privmsg(
                            runtime, dispatcher, &mut stream, &sender, &params[0], &params[1],
                        )
                        .await;
                    }
                    "NICK" => {
                        let who = extract_nick(&_prefix);
                        let current = runtime.current_nick.lock().await.clone();
                        if who.eq_ignore_ascii_case(&current) {
                            if let Some(new_nick) = params.first() {
                                *runtime.current_nick.lock().await = new_nick.clone();
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some((target, text)) = send_rx.recv() => {
                let formatted = strip_markdown_irc(&text);
                let limit = runtime.cfg.max_message_length;
                for line in split_message(&formatted, &target, limit) {
                    let _ = send_raw(&mut stream, &format!("PRIVMSG {target} :{line}")).await;
                    tokio::time::sleep(SEND_PAUSE).await;
                }
            }
        }
    }
}

async fn read_line(stream: &mut IrcStream, buffer: &mut Vec<u8>) -> Result<String, String> {
    loop {
        if let Some(pos) = buffer.windows(2).position(|w| w == b"\r\n") {
            let line: Vec<u8> = buffer.drain(..pos + 2).collect();
            let trimmed = &line[..line.len().saturating_sub(2)];
            return Ok(String::from_utf8_lossy(trimmed).to_string());
        }
        let mut chunk = [0u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed".into());
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

/// hermes 433 retry: `base`, `base_`, `base_1`, `base_2`, ...
pub fn next_collision_nick(configured: &str, current: &str) -> String {
    let base: String = configured
        .trim_end_matches(|c: char| c == '_' || c.is_ascii_digit())
        .to_string();
    if let Some(underscore) = current.rfind('_') {
        let suffix = &current[underscore + 1..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            let next: u64 = suffix.parse().unwrap_or(0) + 1;
            return format!("{base}_{next}");
        }
    }
    if current == configured {
        format!("{configured}_")
    } else {
        format!("{base}_1")
    }
}

/// hermes PRIVMSG handling: echo/CTCP filters, channel addressing,
/// allowlist, dispatch + reply.
async fn handle_privmsg(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    stream: &mut IrcStream,
    sender: &str,
    target: &str,
    raw_text: &str,
) {
    let current_nick = runtime.current_nick.lock().await.clone();
    if sender.eq_ignore_ascii_case(&current_nick) {
        return;
    }
    let mut text = raw_text.to_string();
    // CTCP ACTION (/me) → "* nick text"; other CTCP dropped.
    if text.starts_with("\x01ACTION ") && text.ends_with('\x01') {
        text = format!("* {} {}", sender, &text[8..text.len() - 1]);
    } else if text.starts_with('\x01') {
        return;
    }
    let is_channel = target.starts_with('#') || target.starts_with('&');
    let chat_id = if is_channel {
        target.to_string()
    } else {
        sender.to_string()
    };
    if is_channel {
        let mut addressed = false;
        for prefix in [
            format!("{current_nick}:"),
            format!("{current_nick},"),
            format!("{current_nick} "),
        ] {
            if text.to_lowercase().starts_with(&prefix.to_lowercase()) {
                text = text[prefix.len()..].trim().to_string();
                addressed = true;
                break;
            }
        }
        if !addressed {
            return;
        }
    }
    // Hermes IRC semantics: empty allowlist = allow all.
    if !runtime.cfg.allowed_users.is_empty()
        && !runtime
            .cfg
            .allowed_users
            .iter()
            .any(|u| u.eq_ignore_ascii_case(sender))
    {
        return;
    }
    let event = MessageEvent {
        platform: "irc".into(),
        chat_id: chat_id.clone(),
        sender_id: sender.to_string(),
        sender_name: sender.to_string(),
        text,
        message_id: format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ),
        attachments: Vec::new(),
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut { event.clone() }).await {
        return;
    }
    let outcome = match dispatcher.handle_event(event).await {
        Ok(o) => o,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    let mut full = String::new();
    for echo in &outcome.transcript_echoes {
        full.push_str(echo);
        full.push('\n');
    }
    full.push_str(&outcome.reply);
    let (reply_text, _media) = crate::messaging::extract_media_tags(&full);
    let reply_text = reply_text.trim().to_string();
    if reply_text.is_empty() {
        return;
    }
    let formatted = strip_markdown_irc(&reply_text);
    // P705: ledger-protected reply delivery (the whole line batch).
    dispatcher
        .send_with_ledger("irc", &chat_id, &reply_text, || async {
            for line in split_message(&formatted, &chat_id, runtime.cfg.max_message_length) {
                let _ = send_raw(stream, &format!("PRIVMSG {chat_id} :{line}")).await;
                tokio::time::sleep(SEND_PAUSE).await;
            }
        })
        .await;
}

/// Sender for clarify/cron delivery — routes through the live session
/// socket via the outbound channel (hermes sends always ride the live
/// connection).
struct IrcSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for IrcSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        let tx = self.runtime.outbound.lock().unwrap().clone();
        match tx {
            Some(tx) => {
                if tx.send((chat_id.to_string(), text.to_string())).is_err() {
                    eprintln!("[irc] send_text to {chat_id} failed: session closed");
                }
            }
            None => eprintln!("[irc] send_text to {chat_id} dropped: no live session"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_privmsg() {
        let (prefix, command, params) =
            parse_irc_message(":nick!user@host PRIVMSG #chan :hello world");
        assert_eq!(prefix, "nick!user@host");
        assert_eq!(command, "PRIVMSG");
        assert_eq!(params, vec!["#chan".to_string(), "hello world".to_string()]);
    }

    #[test]
    fn parse_ping_and_numeric() {
        let (prefix, command, params) = parse_irc_message("PING :irc.server.net");
        assert_eq!(prefix, "");
        assert_eq!(command, "PING");
        assert_eq!(params, vec!["irc.server.net".to_string()]);

        let (_, command, params) =
            parse_irc_message(":server 001 mynick :Welcome to the IRC network");
        assert_eq!(command, "001");
        assert_eq!(
            params,
            vec![
                "mynick".to_string(),
                "Welcome to the IRC network".to_string()
            ]
        );
    }

    #[test]
    fn parse_no_trailing() {
        let (_, command, params) = parse_irc_message("NICK newnick");
        assert_eq!(command, "NICK");
        assert_eq!(params, vec!["newnick".to_string()]);
    }

    #[test]
    fn nick_extraction() {
        assert_eq!(extract_nick("nick!user@host"), "nick");
        assert_eq!(extract_nick("server.example.com"), "server.example.com");
    }

    #[test]
    fn collision_nick_progression() {
        assert_eq!(next_collision_nick("hermes", "hermes"), "hermes_");
        assert_eq!(next_collision_nick("hermes", "hermes_"), "hermes_1");
        assert_eq!(next_collision_nick("hermes", "hermes_1"), "hermes_2");
        assert_eq!(next_collision_nick("hermes", "hermes_9"), "hermes_10");
        // A configured nick that already ends in _N climbs the number
        // (hermes strips trailing _0-9 from the base).
        assert_eq!(next_collision_nick("bot_3", "bot_3"), "bot_4");
    }

    #[test]
    fn split_message_respects_byte_limit() {
        let long = "word ".repeat(200); // 1000 bytes
        let lines = split_message(long.trim(), "#chan", 450);
        assert!(lines.len() > 2);
        let overhead = "PRIVMSG #chan :".len() + 2;
        for line in &lines {
            assert!(line.as_bytes().len() + overhead <= IRC_MAX_LINE_BYTES);
        }
    }

    #[test]
    fn split_message_multibyte_safe() {
        let long: String = "汉".repeat(300); // 900 bytes UTF-8
        let lines = split_message(&long, "#x", 450);
        let rejoined: String = lines.join("");
        assert_eq!(rejoined, long);
    }

    #[test]
    fn split_message_drops_blank_paragraphs() {
        let lines = split_message("hi\n\n\nthere", "#x", 450);
        assert_eq!(lines, vec!["hi".to_string(), "there".to_string()]);
    }

    #[test]
    fn irc_markdown_stripping() {
        assert_eq!(strip_markdown_irc("**bold** and `code`"), "bold and code");
        assert_eq!(
            strip_markdown_irc("![pic](https://x/y.png)"),
            "https://x/y.png"
        );
        assert_eq!(
            strip_markdown_irc("[docs](https://example.com)"),
            "docs (https://example.com)"
        );
    }

    #[test]
    fn resolve_env_overrides() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("IRC_SERVER", "irc.example.net");
        std::env::set_var("IRC_USE_TLS", "false");
        let cfg = IrcConfig::default();
        let resolved = cfg.resolve();
        assert_eq!(resolved.server, "irc.example.net");
        assert!(!resolved.use_tls);
        assert_eq!(resolved.port, 6697);
        std::env::remove_var("IRC_SERVER");
        std::env::remove_var("IRC_USE_TLS");
    }
}
