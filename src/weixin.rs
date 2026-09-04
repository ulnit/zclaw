//! Weixin platform adapter — port of hermes `gateway/platforms/weixin.py`
//! @ v2026.8.3.
//!
//! Connects ulnclaw to WeChat personal accounts via Tencent's iLink Bot
//! API: long-poll `getupdates` drives inbound delivery, every outbound
//! reply echoes the latest `context_token` for the peer, media moves
//! through an AES-128-ECB encrypted CDN protocol, and QR login is exposed
//! as `ulnclaw weixin login` (hermes gateway-setup wizard equivalent).
//!
//! Known differences: the Python `qrcode` terminal rendering is replaced
//! by printing the scannable URL plus a local ASCII rendering, outbound
//! typing indicators are best-effort (ticket cache without proactive
//! refresh on every send), and the text-batch debounce key is the chat id
//! (hermes uses the full session key).

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use crate::pairing::PairingStore;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// hermes weixin.py constants.
const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const WEIXIN_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const ILINK_APP_ID: &str = "bot";
const CHANNEL_VERSION: &str = "2.2.0";
const ILINK_APP_CLIENT_VERSION: u32 = (2 << 16) | (2 << 8) | 0;

const EP_GET_UPDATES: &str = "ilink/bot/getupdates";
const EP_SEND_MESSAGE: &str = "ilink/bot/sendmessage";
const EP_SEND_TYPING: &str = "ilink/bot/sendtyping";
const EP_GET_CONFIG: &str = "ilink/bot/getconfig";
const EP_GET_UPLOAD_URL: &str = "ilink/bot/getuploadurl";
const EP_GET_BOT_QR: &str = "ilink/bot/get_bot_qrcode";
const EP_GET_QR_STATUS: &str = "ilink/bot/get_qrcode_status";

const LONG_POLL_TIMEOUT_MS: u64 = 35_000;
const API_TIMEOUT_MS: u64 = 15_000;
const CONFIG_TIMEOUT_MS: u64 = 10_000;
const QR_TIMEOUT_MS: u64 = 35_000;

const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const RETRY_DELAY_SECONDS: u64 = 2;
const BACKOFF_DELAY_SECONDS: u64 = 30;
const SESSION_EXPIRED_ERRCODE: i64 = -14;
/// iLink frequency limit — backoff and retry.
const RATE_LIMIT_ERRCODE: i64 = -2;
const MESSAGE_DEDUP_TTL_SECS: u64 = 300;

/// hermes MAX_MESSAGE_LENGTH for weixin.
const MAX_MESSAGE_LENGTH: usize = 2000;
/// hermes WEIXIN_COPY_LINE_WIDTH.
const COPY_LINE_WIDTH: usize = 120;
/// hermes `_SPLIT_THRESHOLD` — iLink chunks at ~2048 chars.
const SPLIT_THRESHOLD: usize = 1800;

const ITEM_TEXT: i64 = 1;
const ITEM_IMAGE: i64 = 2;
const ITEM_VOICE: i64 = 3;
const ITEM_FILE: i64 = 4;
const ITEM_VIDEO: i64 = 5;

const MSG_TYPE_BOT: i64 = 2;
const MSG_STATE_FINISH: i64 = 2;

const MEDIA_IMAGE: i64 = 1;
const MEDIA_VIDEO: i64 = 2;
const MEDIA_FILE: i64 = 3;
const MEDIA_VOICE: i64 = 4;

/// `[messaging.weixin]` — WeChat personal account via the iLink Bot API
/// (hermes `platforms.weixin` extra config + WEIXIN_* env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WeixinConfig {
    pub enabled: bool,
    /// iLink bot account id (fallback `WEIXIN_ACCOUNT_ID`).
    pub account_id: String,
    /// iLink bot token (fallback `WEIXIN_TOKEN`, then the persisted
    /// account file written by `ulnclaw weixin login`).
    pub token: String,
    /// iLink API base (fallback `WEIXIN_BASE_URL`).
    pub base_url: String,
    /// Media CDN base (fallback `WEIXIN_CDN_BASE_URL`).
    pub cdn_base_url: String,
    /// DM intake policy (hermes WEIXIN_DM_POLICY): `pairing` (default —
    /// unknown senders get interactive pairing codes), `allowlist`,
    /// `open` (requires WEIXIN_ALLOW_ALL_USERS opt-in), `disabled`.
    pub dm_policy: String,
    /// Group intake policy (hermes WEIXIN_GROUP_POLICY): `disabled`
    /// (default) or `allowlist` (group ids in `group_allow_from`).
    pub group_policy: String,
    /// DM allowlist (hermes WEIXIN_ALLOWED_USERS).
    pub allow_from: Vec<String>,
    /// Group allowlist (hermes WEIXIN_GROUP_ALLOWED_USERS).
    pub group_allow_from: Vec<String>,
    /// Delay between consecutive text chunks (hermes 1.5s).
    pub send_chunk_delay_seconds: f64,
    /// Per-chunk retry budget (hermes 4).
    pub send_chunk_retries: u32,
    /// Base retry delay for chunk sends (hermes 1.0s).
    pub send_chunk_retry_delay_seconds: f64,
    /// Text debounce quiet period (hermes 3.0s).
    pub text_batch_delay_seconds: f64,
    /// Debounce quiet period after a near-split-size chunk (hermes 5.0s).
    pub text_batch_split_delay_seconds: f64,
    /// Legacy one-unit-per-message splitting (hermes
    /// WEIXIN_SPLIT_MULTILINE_MESSAGES, default false).
    pub split_multiline_messages: bool,
}

impl Default for WeixinConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            account_id: String::new(),
            token: String::new(),
            base_url: String::new(),
            cdn_base_url: String::new(),
            dm_policy: "pairing".into(),
            group_policy: "disabled".into(),
            allow_from: Vec::new(),
            group_allow_from: Vec::new(),
            send_chunk_delay_seconds: 1.5,
            send_chunk_retries: 4,
            send_chunk_retry_delay_seconds: 1.0,
            text_batch_delay_seconds: 3.0,
            text_batch_split_delay_seconds: 5.0,
            split_multiline_messages: false,
        }
    }
}

fn env_or_none(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn resolve_account_id(cfg: &WeixinConfig) -> String {
    let trimmed = cfg.account_id.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    env_or_none("WEIXIN_ACCOUNT_ID").unwrap_or_default()
}

pub fn resolve_base_url(cfg: &WeixinConfig) -> String {
    let trimmed = cfg.base_url.trim();
    if !trimmed.is_empty() {
        return trimmed.trim_end_matches('/').to_string();
    }
    env_or_none("WEIXIN_BASE_URL")
        .map(|v| v.trim_end_matches('/').to_string())
        .unwrap_or_else(|| ILINK_BASE_URL.to_string())
}

pub fn resolve_cdn_base_url(cfg: &WeixinConfig) -> String {
    let trimmed = cfg.cdn_base_url.trim();
    if !trimmed.is_empty() {
        return trimmed.trim_end_matches('/').to_string();
    }
    env_or_none("WEIXIN_CDN_BASE_URL")
        .map(|v| v.trim_end_matches('/').to_string())
        .unwrap_or_else(|| WEIXIN_CDN_BASE_URL.to_string())
}

/// Token resolution: config → env → persisted account file (hermes
/// `load_weixin_account` fallback on adapter construction).
pub fn resolve_token(cfg: &WeixinConfig, home: &Path) -> String {
    let trimmed = cfg.token.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if let Some(token) = env_or_none("WEIXIN_TOKEN") {
        return token;
    }
    let account_id = resolve_account_id(cfg);
    if !account_id.is_empty() {
        if let Some(persisted) = load_weixin_account(home, &account_id) {
            let token = persisted
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !token.is_empty() {
                return token;
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Crypto — AES-128-ECB CDN media encryption (hermes `_aes128_ecb_*`)
// ---------------------------------------------------------------------------

pub fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad_len = 16 - (data.len() % 16);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad_len as u8).take(pad_len));
    out
}

pub fn pkcs7_unpad(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return data.to_vec();
    }
    let pad_len = *data.last().unwrap() as usize;
    if (1..=16).contains(&pad_len)
        && data.len() >= pad_len
        && data[data.len() - pad_len..]
            .iter()
            .all(|b| *b as usize == pad_len)
    {
        data[..data.len() - pad_len].to_vec()
    } else {
        data.to_vec()
    }
}

pub fn aes128_ecb_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = aes::Aes128::new_from_slice(key).expect("16-byte key");
    let mut out = pkcs7_pad(plaintext);
    for block in out.chunks_exact_mut(16) {
        cipher.encrypt_block(aes::cipher::generic_array::GenericArray::from_mut_slice(
            block,
        ));
    }
    out
}

pub fn aes128_ecb_decrypt(ciphertext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = aes::Aes128::new_from_slice(key).expect("16-byte key");
    let mut out = ciphertext.to_vec();
    for block in out.chunks_exact_mut(16) {
        cipher.decrypt_block(aes::cipher::generic_array::GenericArray::from_mut_slice(
            block,
        ));
    }
    pkcs7_unpad(&out)
}

/// hermes `_aes_padded_size`.
pub fn aes_padded_size(size: usize) -> usize {
    ((size + 1 + 15) / 16) * 16
}

/// hermes `_parse_aes_key`: base64 of either the raw 16-byte key or of a
/// 32-char ASCII hex string.
pub fn parse_aes_key(aes_key_b64: &str) -> std::result::Result<[u8; 16], String> {
    let decoded = base64_decode(aes_key_b64).map_err(|e| format!("aes_key base64: {e}"))?;
    if decoded.len() == 16 {
        let mut key = [0u8; 16];
        key.copy_from_slice(&decoded);
        return Ok(key);
    }
    if decoded.len() == 32 {
        let text: String = decoded.iter().map(|b| *b as char).collect();
        if !text.is_empty() && text.chars().all(|c| c.is_ascii_hexdigit()) {
            let mut key = [0u8; 16];
            for i in 0..16 {
                key[i] = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                    .map_err(|e| format!("aes_key hex: {e}"))?;
            }
            return Ok(key);
        }
    }
    Err(format!(
        "unexpected aes_key format ({} decoded bytes)",
        decoded.len()
    ))
}

fn base64_decode(input: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| e.to_string())
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    format!("{:x}", md5::Md5::digest(data))
}

// ---------------------------------------------------------------------------
// HTTP plumbing (hermes `_headers` / `_api_post` / `_api_get`)
// ---------------------------------------------------------------------------

/// hermes `_random_wechat_uin`: base64 of a random u32 decimal string.
pub fn random_wechat_uin() -> String {
    let mut bytes = [0u8; 4];
    getrandom_fill(&mut bytes);
    let value = u32::from_be_bytes(bytes);
    base64_encode(value.to_string().as_bytes())
}

fn getrandom_fill(bytes: &mut [u8]) {
    // /dev/urandom is the CSPRNG source on the musl targets ulnclaw
    // ships; fall back to a seeded LCG only if it is unreadable (same
    // convention as pairing.rs).
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if file.read_exact(bytes).is_ok() {
            return;
        }
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0x9E3779B97F4A7C15);
    for slot in bytes.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *slot = (seed >> 33) as u8;
    }
}

pub fn request_headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "AuthorizationType".to_string(),
            "ilink_bot_token".to_string(),
        ),
        ("X-WECHAT-UIN".to_string(), random_wechat_uin()),
        ("iLink-App-Id".to_string(), ILINK_APP_ID.to_string()),
        (
            "iLink-App-ClientVersion".to_string(),
            ILINK_APP_CLIENT_VERSION.to_string(),
        ),
    ];
    if let Some(token) = token {
        if !token.is_empty() {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
    }
    headers
}

fn base_info() -> Value {
    json!({ "channel_version": CHANNEL_VERSION })
}

async fn api_post(
    client: &reqwest::Client,
    base_url: &str,
    endpoint: &str,
    payload: Value,
    token: Option<&str>,
    timeout_ms: u64,
) -> std::result::Result<Value, String> {
    let mut merged = payload;
    if let Value::Object(obj) = &mut merged {
        obj.insert("base_info".to_string(), base_info());
    }
    let body = serde_json::to_string(&merged).map_err(|e| e.to_string())?;
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
    let mut request = client
        .post(&url)
        .timeout(Duration::from_millis(timeout_ms))
        .body(body.clone());
    for (key, value) in request_headers(token) {
        request = request.header(key, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("iLink POST {endpoint}: {e}"))?;
    let status = response.status();
    let raw = response
        .text()
        .await
        .map_err(|e| format!("iLink POST {endpoint} body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "iLink POST {endpoint} HTTP {}: {}",
            status.as_u16(),
            truncate_str(&raw, 200)
        ));
    }
    serde_json::from_str(&raw).map_err(|e| format!("iLink POST {endpoint} JSON: {e}"))
}

async fn api_get(
    client: &reqwest::Client,
    base_url: &str,
    endpoint_and_query: &str,
    token: Option<&str>,
    timeout_ms: u64,
) -> std::result::Result<Value, String> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint_and_query);
    let mut request = client.get(&url).timeout(Duration::from_millis(timeout_ms));
    for (key, value) in request_headers(token) {
        if key == "Content-Type" {
            continue;
        }
        request = request.header(key, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("iLink GET {endpoint_and_query}: {e}"))?;
    let status = response.status();
    let raw = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "iLink GET HTTP {}: {}",
            status.as_u16(),
            truncate_str(&raw, 200)
        ));
    }
    serde_json::from_str(&raw).map_err(|e| format!("iLink GET JSON: {e}"))
}

fn truncate_str(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        value.chars().take(max).collect()
    }
}

fn as_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|v| v.as_i64())
}

/// hermes `_is_stale_session_ret`: ret/errcode -2 with "unknown error"
/// is a stale-session signal, not a genuine rate limit.
pub fn is_stale_session_ret(ret: Option<i64>, errcode: Option<i64>, errmsg: Option<&str>) -> bool {
    if ret != Some(RATE_LIMIT_ERRCODE) && errcode != Some(RATE_LIMIT_ERRCODE) {
        return false;
    }
    errmsg
        .map(|m| m.to_lowercase() == "unknown error")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// CDN (hermes `_cdn_*`, `_download_and_decrypt_media`, `_upload_ciphertext`)
// ---------------------------------------------------------------------------

pub fn cdn_download_url(cdn_base_url: &str, encrypted_query_param: &str) -> String {
    format!(
        "{}/download?encrypted_query_param={}",
        cdn_base_url.trim_end_matches('/'),
        utf8_percent_encode(encrypted_query_param, NON_ALPHANUMERIC)
    )
}

pub fn cdn_upload_url(cdn_base_url: &str, upload_param: &str, filekey: &str) -> String {
    format!(
        "{}/upload?encrypted_query_param={}&filekey={}",
        cdn_base_url.trim_end_matches('/'),
        utf8_percent_encode(upload_param, NON_ALPHANUMERIC),
        utf8_percent_encode(filekey, NON_ALPHANUMERIC)
    )
}

/// hermes `_WEIXIN_CDN_ALLOWLIST` — SSRF guard for `full_url` fetches.
const WEIXIN_CDN_ALLOWLIST: &[&str] = &[
    "novac2c.cdn.weixin.qq.com",
    "ilinkai.weixin.qq.com",
    "wx.qlogo.cn",
    "thirdwx.qlogo.cn",
    "res.wx.qq.com",
    "mmbiz.qpic.cn",
    "mmbiz.qlogo.cn",
];

pub fn assert_weixin_cdn_url(url: &str) -> std::result::Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| format!("Unparseable media URL: {url:?}"))?;
    let scheme = parsed.scheme().to_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "Media URL has disallowed scheme {scheme:?}; only http/https are permitted."
        ));
    }
    let host = parsed.host_str().unwrap_or("").to_string();
    if !WEIXIN_CDN_ALLOWLIST.contains(&host.as_str()) {
        return Err(format!(
            "Media URL host {host:?} is not in the WeChat CDN allowlist. Refusing to fetch to prevent SSRF."
        ));
    }
    Ok(())
}

async fn download_bytes(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> std::result::Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("download {url}: {e}"))?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("download HTTP {} for {url}", status.as_u16()));
    }
    Ok(bytes.to_vec())
}

/// hermes `_download_and_decrypt_media`.
pub async fn download_and_decrypt_media(
    client: &reqwest::Client,
    cdn_base_url: &str,
    encrypted_query_param: Option<&str>,
    aes_key_b64: Option<&str>,
    full_url: Option<&str>,
    timeout: Duration,
) -> std::result::Result<Vec<u8>, String> {
    let mut raw = if let Some(param) = encrypted_query_param.filter(|p| !p.is_empty()) {
        download_bytes(client, &cdn_download_url(cdn_base_url, param), timeout).await?
    } else if let Some(url) = full_url.filter(|u| !u.is_empty()) {
        assert_weixin_cdn_url(url)?;
        download_bytes(client, url, timeout).await?
    } else {
        return Err("media item had neither encrypt_query_param nor full_url".into());
    };
    if let Some(key_b64) = aes_key_b64.filter(|k| !k.is_empty()) {
        let key = parse_aes_key(key_b64)?;
        raw = aes128_ecb_decrypt(&raw, &key);
    }
    Ok(raw)
}

/// hermes `_upload_ciphertext`: POST ciphertext, harvest the
/// `x-encrypted-param` response header.
pub async fn upload_ciphertext(
    client: &reqwest::Client,
    ciphertext: &[u8],
    upload_url: &str,
) -> std::result::Result<String, String> {
    let response = client
        .post(upload_url)
        .timeout(Duration::from_secs(120))
        .header("Content-Type", "application/octet-stream")
        .body(ciphertext.to_vec())
        .send()
        .await
        .map_err(|e| format!("CDN upload: {e}"))?;
    let status = response.status();
    if status.as_u16() == 200 {
        if let Some(param) = response.headers().get("x-encrypted-param") {
            let value = param.to_str().map_err(|e| e.to_string())?.to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
        let raw = response.text().await.unwrap_or_default();
        return Err(format!(
            "CDN upload missing x-encrypted-param header: {}",
            truncate_str(&raw, 200)
        ));
    }
    let raw = response.text().await.unwrap_or_default();
    Err(format!(
        "CDN upload HTTP {}: {}",
        status.as_u16(),
        truncate_str(&raw, 200)
    ))
}

// ---------------------------------------------------------------------------
// Account + context-token + sync-buf stores (hermes `save/load_weixin_account`,
// `ContextTokenStore`, `TypingTicketCache`, `_load/_save_sync_buf`)
// ---------------------------------------------------------------------------

fn accounts_dir(home: &Path) -> PathBuf {
    let path = home.join("weixin").join("accounts");
    std::fs::create_dir_all(&path).ok();
    path
}

fn atomic_json_write(path: &Path, value: &Value) -> std::result::Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

/// Persist iLink credentials for reuse (hermes `save_weixin_account`).
pub fn save_weixin_account(
    home: &Path,
    account_id: &str,
    token: &str,
    base_url: &str,
    user_id: &str,
) {
    let payload = json!({
        "token": token,
        "base_url": base_url,
        "user_id": user_id,
        "saved_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    });
    let path = accounts_dir(home).join(format!("{account_id}.json"));
    if let Err(e) = atomic_json_write(&path, &payload) {
        eprintln!("[weixin] failed to persist account {}: {e}", account_id);
    }
}

pub fn load_weixin_account(home: &Path, account_id: &str) -> Option<Value> {
    let path = accounts_dir(home).join(format!("{account_id}.json"));
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Disk-backed `context_token` cache keyed by account + peer (hermes
/// `ContextTokenStore`).
pub struct ContextTokenStore {
    path: PathBuf,
    cache: HashMap<String, String>,
}

impl ContextTokenStore {
    pub fn open(home: &Path, account_id: &str) -> Self {
        let mut store = Self {
            path: accounts_dir(home).join(format!("{account_id}.context-tokens.json")),
            cache: HashMap::new(),
        };
        if let Ok(raw) = std::fs::read_to_string(&store.path) {
            if let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&raw) {
                for (user_id, token) in data {
                    if let Some(token) = token.as_str() {
                        if !token.is_empty() {
                            store.cache.insert(user_id, token.to_string());
                        }
                    }
                }
            }
        }
        store
    }

    pub fn get(&self, user_id: &str) -> Option<String> {
        self.cache.get(user_id).cloned()
    }

    pub fn set(&mut self, user_id: &str, token: &str) {
        self.cache.insert(user_id.to_string(), token.to_string());
        self.persist();
    }

    pub fn remove(&mut self, user_id: &str) {
        self.cache.remove(user_id);
    }

    fn persist(&self) {
        let mut payload = serde_json::Map::new();
        for (user_id, token) in &self.cache {
            payload.insert(user_id.clone(), Value::String(token.clone()));
        }
        if let Err(e) = atomic_json_write(&self.path, &Value::Object(payload)) {
            eprintln!("[weixin] failed to persist context tokens: {e}");
        }
    }
}

/// Short-lived typing-ticket cache from `getconfig` (hermes
/// `TypingTicketCache`, 600s TTL).
pub struct TypingTicketCache {
    entries: HashMap<String, (String, Instant)>,
    ttl: Duration,
}

impl TypingTicketCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(600),
        }
    }

    pub fn get(&mut self, user_id: &str) -> Option<String> {
        let expired = self
            .entries
            .get(user_id)
            .map(|(_, at)| at.elapsed() >= self.ttl)
            .unwrap_or(false);
        if expired {
            self.entries.remove(user_id);
            return None;
        }
        self.entries.get(user_id).map(|(ticket, _)| ticket.clone())
    }

    pub fn set(&mut self, user_id: &str, ticket: &str) {
        self.entries
            .insert(user_id.to_string(), (ticket.to_string(), Instant::now()));
    }
}

fn sync_buf_path(home: &Path, account_id: &str) -> PathBuf {
    accounts_dir(home).join(format!("{account_id}.sync.json"))
}

pub fn load_sync_buf(home: &Path, account_id: &str) -> String {
    std::fs::read_to_string(sync_buf_path(home, account_id))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| {
            v.get("get_updates_buf")
                .and_then(|b| b.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

pub fn save_sync_buf(home: &Path, account_id: &str, sync_buf: &str) {
    let path = sync_buf_path(home, account_id);
    atomic_json_write(&path, &json!({ "get_updates_buf": sync_buf })).ok();
}

/// hermes `MessageDeduplicator` (TTL map).
pub struct MessageDeduplicator {
    seen: Mutex<HashMap<String, Instant>>,
    ttl: Duration,
}

impl MessageDeduplicator {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    pub fn is_duplicate(&self, key: &str) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if seen.len() > 1024 {
            seen.retain(|_, at| at.elapsed() < self.ttl);
        }
        if let Some(at) = seen.get(key) {
            if at.elapsed() < self.ttl {
                return true;
            }
        }
        seen.insert(key.to_string(), Instant::now());
        false
    }
}

// ---------------------------------------------------------------------------
// Markdown formatting + delivery chunking (hermes `_normalize_markdown_blocks`,
// `_wrap_copy_friendly_lines_for_weixin`, `_split_*_for_weixin*`)
// ---------------------------------------------------------------------------

fn fence_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^```([^\n`]*)\s*$").unwrap())
}

fn header_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^(#{1,6})\s+(.+?)\s*$").unwrap())
}

fn table_rule_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^\s*\|?(?:\s*:?-{3,}:?\s*\|)+\s*:?-{3,}:?\s*\|?\s*$").unwrap()
    })
}

fn bold_only_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\*\*[^*]+\*\*$").unwrap())
}

fn numbered_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\d+\.\s").unwrap())
}

/// hermes `_normalize_markdown_blocks`: rstrip lines, collapse blank runs,
/// keep fenced code intact.
pub fn normalize_markdown_blocks(content: &str) -> String {
    let mut result: Vec<&str> = Vec::new();
    let mut in_code_block = false;
    let mut blank_run = 0usize;
    let mut owned: Vec<String> = Vec::new();

    for raw_line in content.lines() {
        let line_end = raw_line.trim_end_matches([' ', '\t', '\r']);
        owned.push(line_end.to_string());
    }
    for line in owned.iter() {
        if fence_re().is_match(line.trim()) {
            in_code_block = !in_code_block;
            result.push(line.as_str());
            blank_run = 0;
            continue;
        }
        if in_code_block {
            result.push(line.as_str());
            continue;
        }
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                result.push("");
            }
            continue;
        }
        blank_run = 0;
        result.push(line.as_str());
    }
    let joined: Vec<String> = result.iter().map(|s| s.to_string()).collect();
    joined.join("\n").trim().to_string()
}

/// Word-wrap one long line without breaking words (hermes textwrap with
/// break_long_words=False, break_on_hyphens=False).
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for word in line.split_whitespace() {
        let word_len = word.chars().count();
        if current.is_empty() {
            current.push_str(word);
            current_len = word_len;
        } else if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_len = word_len;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(line.to_string());
    }
    lines
}

/// hermes `_wrap_copy_friendly_lines_for_weixin`: wrap display lines over
/// 120 chars that are hard to copy in WeChat clients (code fences, table
/// rows and blank lines untouched).
pub fn wrap_copy_friendly_lines(content: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }
    let mut wrapped: Vec<String> = Vec::new();
    let mut in_code_block = false;
    for raw_line in content.lines() {
        let line = raw_line.trim_end_matches([' ', '\t', '\r']).to_string();
        let stripped = line.trim();
        if fence_re().is_match(stripped) {
            in_code_block = !in_code_block;
            wrapped.push(line);
            continue;
        }
        if in_code_block
            || line.chars().count() <= COPY_LINE_WIDTH
            || stripped.is_empty()
            || stripped.starts_with('|')
            || table_rule_re().is_match(stripped)
        {
            wrapped.push(line);
            continue;
        }
        wrapped.extend(wrap_line(&line, COPY_LINE_WIDTH));
    }
    wrapped.join("\n").trim().to_string()
}

/// hermes `format_message`.
pub fn format_message(content: &str) -> String {
    wrap_copy_friendly_lines(&normalize_markdown_blocks(content))
}

/// hermes `_split_markdown_blocks`: blank-line separated blocks with
/// fenced code blocks kept intact.
pub fn split_markdown_blocks(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut in_code_block = false;

    for raw_line in content.lines() {
        let line = raw_line.trim_end_matches([' ', '\t', '\r']).to_string();
        if fence_re().is_match(line.trim()) {
            if !in_code_block && !current.is_empty() {
                blocks.push(current.join("\n").trim().to_string());
                current.clear();
            }
            current.push(line);
            in_code_block = !in_code_block;
            if !in_code_block {
                blocks.push(current.join("\n").trim().to_string());
                current.clear();
            }
            continue;
        }
        if in_code_block {
            current.push(line);
            continue;
        }
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(current.join("\n").trim().to_string());
                current.clear();
            }
            continue;
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(current.join("\n").trim().to_string());
    }
    blocks.into_iter().filter(|b| !b.is_empty()).collect()
}

/// hermes `helpers.greedy_pack_blocks`.
pub fn greedy_pack_blocks(
    blocks: &[String],
    max_length: usize,
    sep: &str,
    overflow: Option<&dyn Fn(&str) -> Vec<String>>,
) -> Vec<String> {
    let mut packed: Vec<String> = Vec::new();
    let mut current = String::new();
    for block in blocks {
        let candidate = if current.is_empty() {
            block.clone()
        } else {
            format!("{current}{sep}{block}")
        };
        if candidate.chars().count() <= max_length {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            packed.push(std::mem::take(&mut current));
        }
        if block.chars().count() <= max_length {
            current = block.clone();
            continue;
        }
        if let Some(overflow) = overflow {
            packed.extend(overflow(block));
        } else {
            packed.push(block.clone());
        }
    }
    if !current.is_empty() {
        packed.push(current);
    }
    packed
}

/// Hard-split an oversized block: pack lines greedily, char-chunk any
/// single line that still exceeds the limit.
fn split_oversized_block(block: &str, max_length: usize) -> Vec<String> {
    let mut packed: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in block.lines() {
        if line.chars().count() > max_length {
            if !current.is_empty() {
                packed.push(std::mem::take(&mut current));
            }
            let mut piece = String::new();
            let mut piece_len = 0usize;
            for ch in line.chars() {
                if piece_len + 1 > max_length {
                    packed.push(std::mem::take(&mut piece));
                    piece_len = 0;
                }
                piece.push(ch);
                piece_len += 1;
            }
            if !piece.is_empty() {
                packed.push(piece);
            }
            continue;
        }
        let candidate = if current.is_empty() {
            line.to_string()
        } else {
            format!("{current}\n{line}")
        };
        if candidate.chars().count() <= max_length {
            current = candidate;
        } else {
            if !current.is_empty() {
                packed.push(std::mem::take(&mut current));
            }
            current = line.to_string();
        }
    }
    if !current.is_empty() {
        packed.push(current);
    }
    packed
}

fn pack_markdown_blocks_for_weixin(content: &str, max_length: usize) -> Vec<String> {
    if content.chars().count() <= max_length {
        return vec![content.to_string()];
    }
    let blocks = split_markdown_blocks(content);
    greedy_pack_blocks(
        &blocks,
        max_length,
        "\n\n",
        Some(&|block: &str| split_oversized_block(block, max_length)),
    )
}

/// hermes `_split_delivery_units_for_weixin`.
pub fn split_delivery_units(content: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    for block in split_markdown_blocks(content) {
        let first_line = block.lines().next().unwrap_or("").trim().to_string();
        if fence_re().is_match(&first_line) {
            units.push(block);
            continue;
        }
        let mut current: Vec<String> = Vec::new();
        for raw_line in block.lines() {
            let line = raw_line.trim_end_matches([' ', '\t', '\r']).to_string();
            if line.trim().is_empty() {
                if !current.is_empty() {
                    units.push(current.join("\n").trim().to_string());
                    current.clear();
                }
                continue;
            }
            let is_continuation =
                !current.is_empty() && (raw_line.starts_with(' ') || raw_line.starts_with('\t'));
            if is_continuation {
                current.push(line);
                continue;
            }
            if !current.is_empty() {
                units.push(current.join("\n").trim().to_string());
                current.clear();
            }
            current.push(line);
        }
        if !current.is_empty() {
            units.push(current.join("\n").trim().to_string());
        }
    }
    units.into_iter().filter(|u| !u.is_empty()).collect()
}

fn looks_like_chatty_line(line: &str) -> bool {
    let stripped = line.trim();
    if stripped.is_empty() {
        return false;
    }
    if stripped.chars().count() > 48 {
        return false;
    }
    if line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    if stripped.starts_with(['>', '-', '*', '#', '|']) || stripped.starts_with('【') {
        return false;
    }
    if table_rule_re().is_match(stripped) {
        return false;
    }
    if bold_only_re().is_match(stripped) {
        return false;
    }
    if numbered_re().is_match(stripped) {
        return false;
    }
    true
}

fn looks_like_heading_line(line: &str) -> bool {
    let stripped = line.trim();
    if stripped.is_empty() {
        return false;
    }
    if header_re().is_match(stripped) {
        return true;
    }
    stripped.chars().count() <= 24 && (stripped.ends_with(':') || stripped.ends_with('：'))
}

fn should_split_short_chat_block(block: &str) -> bool {
    let lines: Vec<&str> = block.lines().filter(|l| !l.trim().is_empty()).collect();
    if !(2..=6).contains(&lines.len()) {
        return false;
    }
    if looks_like_heading_line(lines[0]) {
        return false;
    }
    lines.iter().all(|line| looks_like_chatty_line(line))
}

/// hermes `_split_text_for_weixin_delivery`.
pub fn split_text_for_weixin_delivery(
    content: &str,
    max_length: usize,
    split_per_line: bool,
) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    if split_per_line {
        if content.chars().count() <= max_length && !content.contains('\n') {
            return vec![content.to_string()];
        }
        let mut chunks: Vec<String> = Vec::new();
        for unit in split_delivery_units(content) {
            if unit.chars().count() <= max_length {
                chunks.push(unit);
                continue;
            }
            chunks.extend(pack_markdown_blocks_for_weixin(&unit, max_length));
        }
        let chunks: Vec<String> = chunks.into_iter().filter(|c| !c.is_empty()).collect();
        return if chunks.is_empty() {
            vec![content.to_string()]
        } else {
            chunks
        };
    }
    if content.chars().count() <= max_length {
        return if should_split_short_chat_block(content) {
            let units: Vec<String> = split_delivery_units(content)
                .into_iter()
                .filter(|u| !u.is_empty())
                .collect();
            if units.is_empty() {
                vec![content.to_string()]
            } else {
                units
            }
        } else {
            vec![content.to_string()]
        };
    }
    let packed = pack_markdown_blocks_for_weixin(content, max_length);
    if packed.is_empty() {
        vec![content.to_string()]
    } else {
        packed
    }
}

// ---------------------------------------------------------------------------
// Inbound parsing (hermes `_extract_text`, `_guess_chat_type`)
// ---------------------------------------------------------------------------

/// hermes `_extract_text`: first text item, quote prefixes for referenced
/// messages, voice-without-media STT fallback note.
pub fn extract_text(item_list: &[Value]) -> String {
    for item in item_list {
        if item.get("type").and_then(|v| v.as_i64()) == Some(ITEM_TEXT) {
            let text = item
                .get("text_item")
                .and_then(|v| v.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let ref_item = item
                .get("ref_msg")
                .and_then(|v| v.get("message_item"))
                .filter(|v| v.is_object());
            if let Some(ref_item) = ref_item {
                let ref_type = ref_item.get("type").and_then(|v| v.as_i64());
                let title = item
                    .get("ref_msg")
                    .and_then(|v| v.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if matches!(
                    ref_type,
                    Some(ITEM_IMAGE) | Some(ITEM_VIDEO) | Some(ITEM_FILE) | Some(ITEM_VOICE)
                ) {
                    let prefix = if title.is_empty() {
                        "[引用媒体]\n".to_string()
                    } else {
                        format!("[引用媒体: {title}]\n")
                    };
                    return format!("{prefix}{text}").trim().to_string();
                }
                let mut parts: Vec<String> = Vec::new();
                if !title.is_empty() {
                    parts.push(title);
                }
                let ref_text = extract_text(std::slice::from_ref(ref_item));
                if !ref_text.is_empty() {
                    parts.push(ref_text);
                }
                if !parts.is_empty() {
                    return format!("[引用: {}]\n{}", parts.join(" | "), text)
                        .trim()
                        .to_string();
                }
            }
            return text;
        }
    }
    for item in item_list {
        if item.get("type").and_then(|v| v.as_i64()) == Some(ITEM_VOICE) {
            // hermes #27300: Tencent's STT is unreliable for non-Chinese
            // audio — prefer downloading raw audio for the central STT
            // pipeline; only use the supplied text when no media exists.
            let voice_item = item.get("voice_item").filter(|v| v.is_object());
            let has_media = voice_item
                .and_then(|v| v.get("media"))
                .map(|m| m.is_object() && m.as_object().map(|o| !o.is_empty()).unwrap_or(false))
                .unwrap_or(false);
            if !has_media {
                let voice_text = voice_item
                    .and_then(|v| v.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !voice_text.is_empty() {
                    return format!("[Voice transcription provided by Weixin]\n{voice_text}");
                }
            }
        }
    }
    String::new()
}

/// hermes `_guess_chat_type`: (chat_type, effective_chat_id).
pub fn guess_chat_type(message: &Value, account_id: &str) -> (String, String) {
    let room_id = message
        .get("room_id")
        .or_else(|| message.get("chat_room_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let to_user_id = message
        .get("to_user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let msg_type = message.get("msg_type").and_then(|v| v.as_i64());
    let from_user_id = message
        .get("from_user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_group = !room_id.is_empty()
        || (!to_user_id.is_empty()
            && !account_id.is_empty()
            && to_user_id != account_id
            && msg_type == Some(1));
    if is_group {
        let chat_id = if !room_id.is_empty() {
            room_id
        } else if !to_user_id.is_empty() {
            to_user_id
        } else {
            from_user_id
        };
        ("group".to_string(), chat_id)
    } else {
        ("dm".to_string(), from_user_id)
    }
}

// ---------------------------------------------------------------------------
// QR login (hermes `qr_login` — gateway setup wizard equivalent)
// ---------------------------------------------------------------------------

fn render_qr_ascii(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let mut out = String::new();
    for row in colors.chunks(width) {
        for color in row {
            out.push_str(match color {
                qrcode::Color::Dark => "██",
                qrcode::Color::Light => "  ",
            });
        }
        out.push('\n');
    }
    Some(out)
}

/// Run the interactive iLink QR login flow. Returns the credential dict
/// (account_id/token/base_url/user_id) on success.
pub async fn qr_login(
    home: &Path,
    bot_type: &str,
    timeout_seconds: u64,
) -> std::result::Result<Option<HashMap<String, String>>, String> {
    let client = reqwest::Client::new();
    let mut qr_resp = api_get(
        &client,
        ILINK_BASE_URL,
        &format!("{EP_GET_BOT_QR}?bot_type={bot_type}"),
        None,
        QR_TIMEOUT_MS,
    )
    .await?;

    let mut qrcode_value = qr_resp
        .get("qrcode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut qrcode_url = qr_resp
        .get("qrcode_img_content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if qrcode_value.is_empty() {
        return Err("weixin: QR response missing qrcode".into());
    }

    // qrcode_url is the full scannable liteapp URL; qrcode_value is just
    // the hex token — WeChat must scan the full URL.
    let qr_scan_data = if qrcode_url.is_empty() {
        qrcode_value.clone()
    } else {
        qrcode_url.clone()
    };
    println!("\n请使用微信扫描以下二维码：");
    if !qrcode_url.is_empty() {
        println!("{qrcode_url}");
    }
    match render_qr_ascii(&qr_scan_data) {
        Some(ascii) => println!("{ascii}"),
        None => println!("（终端二维码渲染失败，请直接打开上面的二维码链接）"),
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let mut current_base_url = ILINK_BASE_URL.to_string();
    let mut refresh_count = 0u32;

    while Instant::now() < deadline {
        let status_resp = match api_get(
            &client,
            &current_base_url,
            &format!("{EP_GET_QR_STATUS}?qrcode={qrcode_value}"),
            None,
            QR_TIMEOUT_MS,
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => {
                if e.contains("timeout") || e.contains("operation timed out") {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                eprintln!("[weixin] QR poll error: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let status = status_resp
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("wait")
            .to_string();
        match status.as_str() {
            "wait" => print!("."),
            "scaned" => println!("\n已扫码，请在微信里确认..."),
            "scaned_but_redirected" => {
                let redirect_host = status_resp
                    .get("redirect_host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !redirect_host.is_empty() {
                    current_base_url = format!("https://{redirect_host}");
                }
            }
            "expired" => {
                refresh_count += 1;
                if refresh_count > 3 {
                    println!("\n二维码多次过期，请重新执行登录。");
                    return Ok(None);
                }
                println!("\n二维码已过期，正在刷新... ({refresh_count}/3)");
                qr_resp = api_get(
                    &client,
                    ILINK_BASE_URL,
                    &format!("{EP_GET_BOT_QR}?bot_type={bot_type}"),
                    None,
                    QR_TIMEOUT_MS,
                )
                .await?;
                qrcode_value = qr_resp
                    .get("qrcode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                qrcode_url = qr_resp
                    .get("qrcode_img_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let scan_data = if qrcode_url.is_empty() {
                    qrcode_value.clone()
                } else {
                    qrcode_url.clone()
                };
                if !qrcode_url.is_empty() {
                    println!("{qrcode_url}");
                }
                if let Some(ascii) = render_qr_ascii(&scan_data) {
                    println!("{ascii}");
                }
            }
            "confirmed" => {
                let account_id = status_resp
                    .get("ilink_bot_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let token = status_resp
                    .get("bot_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let base_url = status_resp
                    .get("baseurl")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(ILINK_BASE_URL)
                    .to_string();
                let user_id = status_resp
                    .get("ilink_user_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if account_id.is_empty() || token.is_empty() {
                    return Err("weixin: QR confirmed but credential payload was incomplete".into());
                }
                save_weixin_account(home, &account_id, &token, &base_url, &user_id);
                println!("\n微信连接成功，account_id={account_id}");
                let mut creds = HashMap::new();
                creds.insert("account_id".into(), account_id);
                creds.insert("token".into(), token);
                creds.insert("base_url".into(), base_url);
                creds.insert("user_id".into(), user_id);
                return Ok(Some(creds));
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!("\n微信登录超时。");
    Ok(None)
}

// ---------------------------------------------------------------------------
// Shared adapter handle (outbound path)
// ---------------------------------------------------------------------------

/// hermes rate-limit circuit breaker fields.
struct RateLimitCircuit {
    threshold: usize,
    window: Duration,
    open_for: Duration,
    events: VecDeque<Instant>,
    until: Option<Instant>,
}

impl RateLimitCircuit {
    fn new() -> Self {
        Self {
            threshold: 1,
            window: Duration::from_secs(30),
            open_for: Duration::from_secs(30),
            events: VecDeque::new(),
            until: None,
        }
    }

    fn cooldown_remaining(&self) -> Duration {
        self.until
            .map(|until| until.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    }

    /// Record a rate-limit event; true when the circuit tripped open.
    fn record(&mut self) -> bool {
        let now = Instant::now();
        self.events.push_back(now);
        while let Some(front) = self.events.front() {
            if now.duration_since(*front) > self.window {
                self.events.pop_front();
            } else {
                break;
            }
        }
        if self.events.len() >= self.threshold {
            self.until = Some(now + self.open_for);
            self.events.clear();
            true
        } else {
            false
        }
    }

    fn reset(&mut self) {
        self.until = None;
        self.events.clear();
    }
}

/// Shared state for one connected weixin adapter (poll + send paths).
pub struct WeixinHandle {
    client: reqwest::Client,
    account_id: String,
    token: String,
    base_url: String,
    cdn_base_url: String,
    send_chunk_delay_seconds: f64,
    send_chunk_retries: u32,
    send_chunk_retry_delay_seconds: f64,
    tokens: Mutex<ContextTokenStore>,
    circuit: Mutex<RateLimitCircuit>,
}

impl WeixinHandle {
    async fn ilink_post(
        &self,
        endpoint: &str,
        payload: Value,
        timeout_ms: u64,
    ) -> std::result::Result<Value, String> {
        api_post(
            &self.client,
            &self.base_url,
            endpoint,
            payload,
            Some(&self.token),
            timeout_ms,
        )
        .await
    }

    /// hermes `_send_message`.
    async fn send_message_raw(
        &self,
        to: &str,
        item_list: Value,
        context_token: Option<&str>,
        client_id: &str,
    ) -> std::result::Result<Value, String> {
        let mut message = json!({
            "from_user_id": "",
            "to_user_id": to,
            "client_id": client_id,
            "message_type": MSG_TYPE_BOT,
            "message_state": MSG_STATE_FINISH,
            "item_list": item_list,
        });
        if let Some(context_token) = context_token.filter(|t| !t.is_empty()) {
            message["context_token"] = json!(context_token);
        }
        self.ilink_post(EP_SEND_MESSAGE, json!({ "msg": message }), API_TIMEOUT_MS)
            .await
    }

    /// hermes `_send_text_chunk` + `_send_text_chunk_locked`: per-chunk
    /// retry, session-expired tokenless fallback, rate-limit backoff +
    /// circuit breaker.
    pub async fn send_text_chunk(
        &self,
        chat_id: &str,
        chunk: &str,
    ) -> std::result::Result<(), String> {
        if chunk.trim().is_empty() {
            return Err("_send_message: text must not be empty".into());
        }
        let mut context_token = self.tokens.lock().unwrap().get(chat_id);
        let mut retried_without_token = false;
        let mut last_error: Option<String> = None;

        for attempt in 0..=self.send_chunk_retries {
            let cooldown = self.circuit.lock().unwrap().cooldown_remaining();
            if cooldown > Duration::ZERO {
                return Err(format!(
                    "weixin rate-limit circuit open for another {:.0}s",
                    cooldown.as_secs_f64()
                ));
            }
            let client_id = format!("ulnclaw-weixin-{}", new_client_id_hex());
            match self
                .send_message_raw(
                    chat_id,
                    json!([{"type": ITEM_TEXT, "text_item": {"text": chunk}}]),
                    context_token.as_deref(),
                    &client_id,
                )
                .await
            {
                Ok(resp) => {
                    let ret = as_i64(&resp, "ret");
                    let errcode = as_i64(&resp, "errcode");
                    let bad_ret = ret.map(|r| r != 0).unwrap_or(false);
                    let bad_err = errcode.map(|e| e != 0).unwrap_or(false);
                    if bad_ret || bad_err {
                        let errmsg = resp
                            .get("errmsg")
                            .or_else(|| resp.get("msg"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string();
                        let session_expired = ret == Some(SESSION_EXPIRED_ERRCODE)
                            || errcode == Some(SESSION_EXPIRED_ERRCODE)
                            || is_stale_session_ret(ret, errcode, Some(&errmsg));
                        if session_expired && !retried_without_token && context_token.is_some() {
                            retried_without_token = true;
                            context_token = None;
                            self.tokens.lock().unwrap().remove(chat_id);
                            eprintln!("[weixin] session expired for {chat_id}; retrying without context_token");
                            continue;
                        }
                        let rate_limited =
                            ret == Some(RATE_LIMIT_ERRCODE) || errcode == Some(RATE_LIMIT_ERRCODE);
                        if rate_limited {
                            let tripped = self.circuit.lock().unwrap().record();
                            last_error = Some(format!("iLink sendmessage rate limited: ret={ret:?} errcode={errcode:?} errmsg={errmsg}"));
                            if tripped {
                                return Err("weixin rate-limit circuit tripped open".into());
                            }
                            if attempt >= self.send_chunk_retries {
                                break;
                            }
                            let wait = self.send_chunk_retry_delay_seconds * 3.0;
                            eprintln!("[weixin] rate limited for {chat_id}; backing off {wait:.1}s before retry");
                            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                            continue;
                        }
                        return Err(format!("iLink sendmessage error: ret={ret:?} errcode={errcode:?} errmsg={errmsg}"));
                    }
                    self.circuit.lock().unwrap().reset();
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt >= self.send_chunk_retries {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs_f64(
                        self.send_chunk_retry_delay_seconds,
                    ))
                    .await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| "send failed".into()))
    }

    /// Format + chunk + deliver text with inter-chunk delays (hermes
    /// `send()` text half).
    pub async fn send_text(&self, chat_id: &str, text: &str, split_multiline: bool) {
        let formatted = format_message(text);
        let chunks: Vec<String> =
            split_text_for_weixin_delivery(&formatted, MAX_MESSAGE_LENGTH, split_multiline)
                .into_iter()
                .filter(|c| !c.trim().is_empty())
                .collect();
        for (idx, chunk) in chunks.iter().enumerate() {
            if let Err(e) = self.send_text_chunk(chat_id, chunk).await {
                eprintln!("[weixin] send chunk failed to={chat_id}: {e}");
                return;
            }
            if idx < chunks.len() - 1 && self.send_chunk_delay_seconds > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(self.send_chunk_delay_seconds)).await;
            }
        }
    }

    /// hermes `_get_upload_url`.
    async fn get_upload_url(
        &self,
        to_user_id: &str,
        media_type: i64,
        filekey: &str,
        rawsize: usize,
        rawfilemd5: &str,
        filesize: usize,
        aeskey_hex: &str,
    ) -> std::result::Result<Value, String> {
        self.ilink_post(
            EP_GET_UPLOAD_URL,
            json!({
                "filekey": filekey,
                "media_type": media_type,
                "to_user_id": to_user_id,
                "rawsize": rawsize,
                "rawfilemd5": rawfilemd5,
                "filesize": filesize,
                "no_need_thumb": true,
                "aeskey": aeskey_hex,
            }),
            API_TIMEOUT_MS,
        )
        .await
    }

    /// hermes `_send_file`: AES-ECB CDN upload + media message item.
    pub async fn send_file(
        &self,
        chat_id: &str,
        path: &Path,
        caption: &str,
    ) -> std::result::Result<String, String> {
        let plaintext = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let (media_type, item_kind) = outbound_media_builder(path);
        let filekey = new_client_id_hex();
        let mut aes_key = [0u8; 16];
        getrandom_fill(&mut aes_key);
        let rawsize = plaintext.len();
        let rawfilemd5 = md5_hex(&plaintext);
        let upload_response = self
            .get_upload_url(
                chat_id,
                media_type,
                &filekey,
                rawsize,
                &rawfilemd5,
                aes_padded_size(rawsize),
                &hex_encode(&aes_key),
            )
            .await?;
        let upload_param = upload_response
            .get("upload_param")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let upload_full_url = upload_response
            .get("upload_full_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ciphertext = aes128_ecb_encrypt(&plaintext, &aes_key);

        // Prefer upload_full_url (direct CDN), fall back to the CDN URL
        // built from upload_param. Both use POST (hermes: the old PUT for
        // upload_full_url caused 404s).
        let upload_url = if !upload_full_url.is_empty() {
            upload_full_url
        } else if !upload_param.is_empty() {
            cdn_upload_url(&self.cdn_base_url, &upload_param, &filekey)
        } else {
            return Err(format!(
                "getUploadUrl returned neither upload_param nor upload_full_url: {upload_response}"
            ));
        };

        let encrypted_query_param =
            upload_ciphertext(&self.client, &ciphertext, &upload_url).await?;
        let context_token = self.tokens.lock().unwrap().get(chat_id);
        // The iLink API expects aes_key as base64(hex_string), not
        // base64(raw_bytes) — otherwise images render as grey boxes.
        let aes_key_for_api = base64_encode(hex_encode(&aes_key).as_bytes());
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_silk_voice =
            media_type == MEDIA_VOICE && path.extension().map(|e| e == "silk").unwrap_or(false);

        let media = json!({
            "encrypt_query_param": encrypted_query_param,
            "aes_key": aes_key_for_api,
            "encrypt_type": 1,
        });
        let media_item = match item_kind {
            OutboundKind::Image => {
                json!({"type": ITEM_IMAGE, "image_item": {"media": media, "mid_size": ciphertext.len()}})
            }
            OutboundKind::Video => json!({
                "type": ITEM_VIDEO,
                "video_item": {"media": media, "video_size": ciphertext.len(), "play_length": 0, "video_md5": rawfilemd5},
            }),
            OutboundKind::Voice => json!({
                "type": ITEM_VOICE,
                "voice_item": {
                    "media": media,
                    "encode_type": if is_silk_voice { json!(6) } else { Value::Null },
                    "bits_per_sample": if is_silk_voice { json!(16) } else { Value::Null },
                    "sample_rate": if is_silk_voice { json!(24000) } else { Value::Null },
                    "playtime": 0,
                },
            }),
            OutboundKind::File => json!({
                "type": ITEM_FILE,
                "file_item": {"media": media, "file_name": file_name, "len": rawsize.to_string()},
            }),
        };

        if !caption.trim().is_empty() {
            let caption_client_id = format!("ulnclaw-weixin-{}", new_client_id_hex());
            self.send_message_raw(
                chat_id,
                json!([{"type": ITEM_TEXT, "text_item": {"text": format_message(caption)}}]),
                context_token.as_deref(),
                &caption_client_id,
            )
            .await?;
        }

        let last_message_id = format!("ulnclaw-weixin-{}", new_client_id_hex());
        self.send_message_raw(
            chat_id,
            json!([media_item]),
            context_token.as_deref(),
            &last_message_id,
        )
        .await?;
        Ok(last_message_id)
    }

    /// Extension-based media routing (hermes `_deliver_media`).
    pub async fn deliver_media_path(&self, chat_id: &str, path: &Path) {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        const AUDIO_EXTS: &[&str] = &["ogg", "opus", "mp3", "wav", "m4a", "flac"];
        const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "mkv", "webm", "3gp"];
        const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];
        let result = if AUDIO_EXTS.contains(&ext.as_str()) || ext == "silk" {
            self.send_file(chat_id, path, "").await
        } else if VIDEO_EXTS.contains(&ext.as_str()) {
            self.send_file(chat_id, path, "").await
        } else if IMAGE_EXTS.contains(&ext.as_str()) {
            self.send_file(chat_id, path, "").await
        } else {
            self.send_file(chat_id, path, "").await
        };
        if let Err(e) = result {
            eprintln!("[weixin] media delivery failed for {}: {e}", path.display());
        }
    }

    /// Best-effort typing indicator (hermes `send_typing` with cached
    /// ticket from `getconfig`).
    pub async fn send_typing(&self, chat_id: &str, ticket_cache: &Mutex<TypingTicketCache>) {
        let ticket = {
            let mut cache = ticket_cache.lock().unwrap();
            match cache.get(chat_id) {
                Some(ticket) => Some(ticket),
                None => None,
            }
        };
        let ticket = match ticket {
            Some(ticket) => ticket,
            None => {
                let context_token = self.tokens.lock().unwrap().get(chat_id);
                let mut payload = json!({ "ilink_user_id": chat_id });
                if let Some(context_token) = context_token {
                    payload["context_token"] = json!(context_token);
                }
                match self
                    .ilink_post(EP_GET_CONFIG, payload, CONFIG_TIMEOUT_MS)
                    .await
                {
                    Ok(resp) => {
                        let ticket = resp
                            .get("typing_ticket")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if ticket.is_empty() {
                            return;
                        }
                        ticket_cache.lock().unwrap().set(chat_id, &ticket);
                        ticket
                    }
                    Err(_) => return,
                }
            }
        };
        let payload = json!({ "ilink_user_id": chat_id, "typing_ticket": ticket, "status": 1 });
        self.ilink_post(EP_SEND_TYPING, payload, CONFIG_TIMEOUT_MS)
            .await
            .ok();
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OutboundKind {
    Image,
    Video,
    Voice,
    File,
}

/// hermes `_outbound_media_builder`: (media_type, item kind).
fn outbound_media_builder(path: &Path) -> (i64, OutboundKind) {
    let mime = crate::media_cache::mime_for_ext(path);
    let is_silk = path.extension().map(|e| e == "silk").unwrap_or(false);
    if mime.starts_with("image/") {
        return (MEDIA_IMAGE, OutboundKind::Image);
    }
    if mime.starts_with("video/") {
        return (MEDIA_VIDEO, OutboundKind::Video);
    }
    if is_silk {
        return (MEDIA_VOICE, OutboundKind::Voice);
    }
    (MEDIA_FILE, OutboundKind::File)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_client_id_hex() -> String {
    let mut bytes = [0u8; 16];
    getrandom_fill(&mut bytes);
    hex_encode(&bytes)
}

// ---------------------------------------------------------------------------
// Inbound pipeline: poll loop, message processing, text debounce batching
// ---------------------------------------------------------------------------

struct PendingBatch {
    text: String,
    last_chunk_len: usize,
    generation: u64,
    attachments: Vec<MediaAttachment>,
    sender_id: String,
    sender_name: String,
    chat_id: String,
}

struct Runner {
    handle: Arc<WeixinHandle>,
    cfg: WeixinConfig,
    home: PathBuf,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<PairingStore>>,
    dedup: MessageDeduplicator,
    typing_cache: Arc<Mutex<TypingTicketCache>>,
    batches: Mutex<HashMap<String, PendingBatch>>,
}

/// Start the weixin adapter (hermes `WeixinAdapter.connect` + poll loop).
pub async fn run(
    cfg: WeixinConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<PairingStore>>,
) {
    let home = crate::config::ulnclaw_home();
    let account_id = resolve_account_id(&cfg);
    let token = resolve_token(&cfg, &home);
    if token.is_empty() {
        eprintln!(
            "[weixin] disabled: no token configured (run `ulnclaw weixin login` or set messaging.weixin.token / WEIXIN_TOKEN)"
        );
        return;
    }
    let base_url = resolve_base_url(&cfg);
    let cdn_base_url = resolve_cdn_base_url(&cfg);

    let handle = Arc::new(WeixinHandle {
        client: reqwest::Client::new(),
        account_id: account_id.clone(),
        token,
        base_url: base_url.clone(),
        cdn_base_url,
        send_chunk_delay_seconds: cfg.send_chunk_delay_seconds,
        send_chunk_retries: cfg.send_chunk_retries,
        send_chunk_retry_delay_seconds: cfg.send_chunk_retry_delay_seconds,
        tokens: Mutex::new(ContextTokenStore::open(&home, &account_id)),
        circuit: Mutex::new(RateLimitCircuit::new()),
    });

    register_sender(handle.clone(), cfg.split_multiline_messages);

    let runner = Arc::new(Runner {
        handle,
        cfg: cfg.clone(),
        home,
        dispatcher,
        pairing,
        dedup: MessageDeduplicator::new(MESSAGE_DEDUP_TTL_SECS),
        typing_cache: Arc::new(Mutex::new(TypingTicketCache::new())),
        batches: Mutex::new(HashMap::new()),
    });

    eprintln!(
        "[weixin] connected account={} base={}",
        safe_id(&account_id),
        base_url
    );
    if cfg.group_policy != "disabled" {
        eprintln!(
            "[weixin] group_policy={} is set, but QR-login connects an iLink bot identity that \
             typically cannot be invited into ordinary WeChat groups; group messages may never \
             arrive (hermes caveat)",
            cfg.group_policy
        );
    }

    poll_loop(runner).await;
}

fn safe_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "?".to_string()
    } else if trimmed.chars().count() <= 8 {
        trimmed.to_string()
    } else {
        trimmed.chars().take(8).collect()
    }
}

struct WeixinSender {
    handle: Arc<WeixinHandle>,
    split_multiline: bool,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for WeixinSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        self.handle
            .send_text(chat_id, text, self.split_multiline)
            .await;
    }
}

fn register_sender(handle: Arc<WeixinHandle>, split_multiline: bool) {
    crate::messaging::register_platform_sender(
        "weixin",
        Arc::new(WeixinSender {
            handle,
            split_multiline,
        }),
    );
}

/// hermes `_poll_loop`.
async fn poll_loop(runner: Arc<Runner>) {
    let sync_buf_initial = load_sync_buf(&runner.home, &runner.handle.account_id);
    let mut sync_buf = sync_buf_initial;
    let mut timeout_ms: u64 = LONG_POLL_TIMEOUT_MS;
    let mut consecutive_failures: u32 = 0;

    loop {
        let response = get_updates(&runner.handle, &sync_buf, timeout_ms).await;
        match response {
            Ok(response) => {
                if let Some(suggested) = as_i64(&response, "longpolling_timeout_ms") {
                    if suggested > 0 {
                        timeout_ms = suggested as u64;
                    }
                }
                let ret = as_i64(&response, "ret");
                let errcode = as_i64(&response, "errcode");
                let bad_ret = ret.map(|r| r != 0).unwrap_or(false);
                let bad_err = errcode.map(|e| e != 0).unwrap_or(false);
                if bad_ret || bad_err {
                    let errmsg = response
                        .get("errmsg")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let session_expired = ret == Some(SESSION_EXPIRED_ERRCODE)
                        || errcode == Some(SESSION_EXPIRED_ERRCODE)
                        || is_stale_session_ret(ret, errcode, Some(&errmsg));
                    if session_expired {
                        eprintln!("[weixin] session expired; pausing for 10 minutes");
                        tokio::time::sleep(Duration::from_secs(600)).await;
                        consecutive_failures = 0;
                        continue;
                    }
                    consecutive_failures += 1;
                    eprintln!(
                        "[weixin] getUpdates failed ret={ret:?} errcode={errcode:?} errmsg={errmsg} ({consecutive_failures}/{MAX_CONSECUTIVE_FAILURES})"
                    );
                    let delay = if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        BACKOFF_DELAY_SECONDS
                    } else {
                        RETRY_DELAY_SECONDS
                    };
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        consecutive_failures = 0;
                    }
                    continue;
                }

                consecutive_failures = 0;
                let new_sync_buf = response
                    .get("get_updates_buf")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !new_sync_buf.is_empty() {
                    sync_buf = new_sync_buf.clone();
                    save_sync_buf(&runner.home, &runner.handle.account_id, &new_sync_buf);
                }

                let msgs = response
                    .get("msgs")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                for message in msgs {
                    let runner = runner.clone();
                    tokio::spawn(async move {
                        if let Err(e) = process_message(&runner, &message).await {
                            let from = message
                                .get("from_user_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            eprintln!(
                                "[weixin] unhandled inbound error from={}: {e}",
                                safe_id(from)
                            );
                        }
                    });
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                eprintln!(
                    "[weixin] poll error ({consecutive_failures}/{MAX_CONSECUTIVE_FAILURES}): {e}"
                );
                let delay = if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    BACKOFF_DELAY_SECONDS
                } else {
                    RETRY_DELAY_SECONDS
                };
                tokio::time::sleep(Duration::from_secs(delay)).await;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    consecutive_failures = 0;
                }
            }
        }
    }
}

/// hermes `_get_updates` — timeout degrades to an empty batch.
async fn get_updates(
    handle: &WeixinHandle,
    sync_buf: &str,
    timeout_ms: u64,
) -> std::result::Result<Value, String> {
    // iLink holds the connection server-side for ~timeout_ms; give the
    // client extra slack so transport timeouts don't mask real messages.
    let result = handle
        .ilink_post(
            EP_GET_UPDATES,
            json!({ "get_updates_buf": sync_buf }),
            timeout_ms + 10_000,
        )
        .await;
    match result {
        Ok(value) => Ok(value),
        Err(e) if e.contains("timeout") || e.contains("timed out") => Ok(json!({
            "ret": 0,
            "msgs": [],
            "get_updates_buf": sync_buf,
        })),
        Err(e) => Err(e),
    }
}

/// hermes `_process_message` (safe wrapper inlined).
async fn process_message(runner: &Arc<Runner>, message: &Value) -> std::result::Result<(), String> {
    let sender_id = message
        .get("from_user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if sender_id.is_empty() || sender_id == runner.handle.account_id {
        return Ok(());
    }

    let message_id = message
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !message_id.is_empty() && runner.dedup.is_duplicate(&message_id) {
        return Ok(());
    }

    let item_list: Vec<Value> = message
        .get("item_list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let text = extract_text(&item_list);
    if !text.is_empty() {
        // Secondary content-fingerprint dedup for text messages (hermes).
        let content_key = format!("content:{sender_id}:{}", md5_hex(text.as_bytes()));
        if runner.dedup.is_duplicate(&content_key) {
            return Ok(());
        }
    }

    let (chat_type, effective_chat_id) = guess_chat_type(message, &runner.handle.account_id);
    if chat_type == "group" {
        match runner.cfg.group_policy.as_str() {
            "disabled" => return Ok(()),
            "allowlist" => {
                if !runner
                    .cfg
                    .group_allow_from
                    .iter()
                    .any(|id| *id == effective_chat_id)
                {
                    return Ok(());
                }
            }
            // hermes: group pairing is not supported — drop.
            _ => return Ok(()),
        }
    } else if !dm_intake_allowed(runner, &sender_id) {
        offer_pairing(runner, &sender_id, &effective_chat_id).await;
        return Ok(());
    }

    let context_token = message
        .get("context_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !context_token.is_empty() {
        runner
            .handle
            .tokens
            .lock()
            .unwrap()
            .set(&sender_id, &context_token);
    }

    // Collect media from items and referenced messages (hermes
    // `_collect_media` incl. ref_msg items).
    let mut attachments: Vec<MediaAttachment> = Vec::new();
    let mut items: Vec<Value> = item_list.clone();
    for item in &item_list {
        if let Some(ref_item) = item
            .get("ref_msg")
            .and_then(|v| v.get("message_item"))
            .filter(|v| v.is_object())
        {
            items.push(ref_item.clone());
        }
    }
    for item in &items {
        if let Some(attachment) = collect_media(runner, item).await {
            attachments.push(attachment);
        }
    }

    if text.is_empty() && attachments.is_empty() {
        return Ok(());
    }

    eprintln!(
        "[weixin] inbound from={} type={} media={}",
        safe_id(&sender_id),
        chat_type,
        attachments.len()
    );

    if chat_type == "dm" && !is_dm_fully_authorized(runner, &sender_id) {
        offer_pairing(runner, &sender_id, &effective_chat_id).await;
        return Ok(());
    }

    // Best-effort typing indicator while the turn runs.
    {
        let handle = runner.handle.clone();
        let chat = effective_chat_id.clone();
        let cache = runner.typing_cache.clone();
        tokio::spawn(async move {
            handle.send_typing(&chat, &cache).await;
        });
    }

    if !text.is_empty() && attachments.is_empty() {
        enqueue_text_event(runner, &effective_chat_id, &sender_id, &sender_id, &text);
    } else {
        dispatch_event(
            runner,
            &effective_chat_id,
            &sender_id,
            &sender_id,
            &text,
            attachments,
        )
        .await;
    }
    Ok(())
}

/// DM intake: hermes `_is_dm_intake_allowed` mapped onto the ulnclaw
/// allowlist∪pairing model.
fn dm_intake_allowed(runner: &Runner, sender_id: &str) -> bool {
    match runner.cfg.dm_policy.as_str() {
        "disabled" => false,
        "allowlist" => {
            runner.cfg.allow_from.iter().any(|id| id == sender_id)
                || runner
                    .pairing
                    .as_ref()
                    .map(|store| store.is_approved("weixin", sender_id))
                    .unwrap_or(false)
        }
        "pairing" => true,
        "open" => {
            matches!(
                std::env::var("GATEWAY_ALLOW_ALL_USERS")
                    .unwrap_or_default()
                    .to_lowercase()
                    .as_str(),
                "true" | "1" | "yes"
            ) || matches!(
                std::env::var("WEIXIN_ALLOW_ALL_USERS")
                    .unwrap_or_default()
                    .to_lowercase()
                    .as_str(),
                "true" | "1" | "yes"
            )
        }
        _ => false,
    }
}

/// Fully authorized = allowlist∪approved pairing (ulnclaw gate); with
/// dm_policy=pairing unknown senders stop here and get a pairing offer.
fn is_dm_fully_authorized(runner: &Runner, sender_id: &str) -> bool {
    if runner.cfg.dm_policy == "open" {
        return true;
    }
    runner.cfg.allow_from.iter().any(|id| id == sender_id)
        || runner
            .pairing
            .as_ref()
            .map(|store| store.is_approved("weixin", sender_id))
            .unwrap_or(false)
}

async fn offer_pairing(runner: &Runner, sender_id: &str, chat_id: &str) {
    eprintln!(
        "[weixin] refusing message from {sender_id} — add it to messaging.weixin.allow_from or approve a pairing code"
    );
    if let Some(store) = &runner.pairing {
        if let Some(reply) =
            crate::messaging::pairing_offer_public(store, "weixin", sender_id, sender_id)
        {
            if let Err(e) = runner.handle.send_text_chunk(chat_id, &reply).await {
                eprintln!("[weixin] pairing reply failed: {e}");
            }
        }
    }
}

/// hermes `_collect_media` for one item.
async fn collect_media(runner: &Arc<Runner>, item: &Value) -> Option<MediaAttachment> {
    let item_type = item.get("type").and_then(|v| v.as_i64())?;
    let home = crate::config::ulnclaw_home();
    match item_type {
        ITEM_IMAGE => {
            let media = item
                .get("image_item")
                .and_then(|v| v.get("media"))
                .filter(|v| v.is_object());
            let image_item = item.get("image_item").filter(|v| v.is_object());
            // hermes: image aeskey arrives as hex inside image_item, the
            // CDN reference carries the base64 form.
            let aes_key_b64 = match image_item
                .and_then(|v| v.get("aeskey"))
                .and_then(|v| v.as_str())
            {
                Some(hex_key) if !hex_key.is_empty() => Some(base64_encode(hex_key.as_bytes())),
                _ => media
                    .and_then(|m| m.get("aes_key"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            };
            let data = download_and_decrypt_media(
                &runner.handle.client,
                &runner.handle.cdn_base_url,
                media
                    .and_then(|m| m.get("encrypt_query_param"))
                    .and_then(|v| v.as_str()),
                aes_key_b64.as_deref(),
                media
                    .and_then(|m| m.get("full_url"))
                    .and_then(|v| v.as_str()),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| {
                eprintln!("[weixin] image download failed: {e}");
                e
            })
            .ok()?;
            cache_attachment(&home, &data, "image/jpeg", "image.jpg")
        }
        ITEM_VIDEO => {
            let media = item
                .get("video_item")
                .and_then(|v| v.get("media"))
                .filter(|v| v.is_object());
            let data = download_and_decrypt_media(
                &runner.handle.client,
                &runner.handle.cdn_base_url,
                media
                    .and_then(|m| m.get("encrypt_query_param"))
                    .and_then(|v| v.as_str()),
                media
                    .and_then(|m| m.get("aes_key"))
                    .and_then(|v| v.as_str()),
                media
                    .and_then(|m| m.get("full_url"))
                    .and_then(|v| v.as_str()),
                Duration::from_secs(120),
            )
            .await
            .map_err(|e| {
                eprintln!("[weixin] video download failed: {e}");
                e
            })
            .ok()?;
            cache_attachment(&home, &data, "video/mp4", "video.mp4")
        }
        ITEM_FILE => {
            let file_item = item.get("file_item").filter(|v| v.is_object());
            let media = file_item
                .and_then(|v| v.get("media"))
                .filter(|v| v.is_object());
            let filename = file_item
                .and_then(|v| v.get("file_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("document.bin")
                .to_string();
            let mime = crate::media_cache::mime_for_ext(Path::new(&filename));
            let data = download_and_decrypt_media(
                &runner.handle.client,
                &runner.handle.cdn_base_url,
                media
                    .and_then(|m| m.get("encrypt_query_param"))
                    .and_then(|v| v.as_str()),
                media
                    .and_then(|m| m.get("aes_key"))
                    .and_then(|v| v.as_str()),
                media
                    .and_then(|m| m.get("full_url"))
                    .and_then(|v| v.as_str()),
                Duration::from_secs(60),
            )
            .await
            .map_err(|e| {
                eprintln!("[weixin] file download failed: {e}");
                e
            })
            .ok()?;
            cache_attachment(&home, &data, &mime, &filename)
        }
        ITEM_VOICE => {
            let voice_item = item.get("voice_item").filter(|v| v.is_object());
            let media = voice_item
                .and_then(|v| v.get("media"))
                .filter(|v| v.is_object());
            // hermes #27300: always download raw audio for the central STT
            // pipeline; Tencent's voice_item.text is unreliable for
            // non-Chinese speech.
            let data = download_and_decrypt_media(
                &runner.handle.client,
                &runner.handle.cdn_base_url,
                media
                    .and_then(|m| m.get("encrypt_query_param"))
                    .and_then(|v| v.as_str()),
                media
                    .and_then(|m| m.get("aes_key"))
                    .and_then(|v| v.as_str()),
                media
                    .and_then(|m| m.get("full_url"))
                    .and_then(|v| v.as_str()),
                Duration::from_secs(60),
            )
            .await
            .map_err(|e| {
                eprintln!("[weixin] voice download failed: {e}");
                e
            })
            .ok()?;
            cache_attachment(&home, &data, "audio/silk", "voice.silk")
        }
        _ => None,
    }
}

fn cache_attachment(
    home: &Path,
    data: &[u8],
    mime: &str,
    filename_hint: &str,
) -> Option<MediaAttachment> {
    let path = crate::media_cache::cache_media_bytes(home, data, mime, filename_hint).ok()?;
    Some(MediaAttachment {
        path,
        mime: mime.to_string(),
        bytes: data.len() as u64,
        original_name: filename_hint.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Text debounce batching (hermes `_enqueue_text_event` / `_flush_text_batch`)
// ---------------------------------------------------------------------------

fn enqueue_text_event(
    runner: &Arc<Runner>,
    chat_id: &str,
    sender_id: &str,
    sender_name: &str,
    text: &str,
) {
    let chunk_len = text.chars().count();
    let generation;
    {
        let mut batches = runner.batches.lock().unwrap();
        let entry = batches
            .entry(chat_id.to_string())
            .or_insert_with(|| PendingBatch {
                text: String::new(),
                last_chunk_len: 0,
                generation: 0,
                attachments: Vec::new(),
                sender_id: sender_id.to_string(),
                sender_name: sender_name.to_string(),
                chat_id: chat_id.to_string(),
            });
        if !text.is_empty() {
            if entry.text.is_empty() {
                entry.text = text.to_string();
            } else {
                entry.text.push('\n');
                entry.text.push_str(text);
            }
        }
        entry.last_chunk_len = chunk_len;
        entry.generation += 1;
        generation = entry.generation;
    }
    let delay = if chunk_len >= SPLIT_THRESHOLD {
        runner.cfg.text_batch_split_delay_seconds
    } else {
        runner.cfg.text_batch_delay_seconds
    };
    let runner = runner.clone();
    let key = chat_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        let pending = {
            let mut batches = runner.batches.lock().unwrap();
            match batches.get(&key) {
                Some(batch) if batch.generation == generation => batches.remove(&key),
                _ => None,
            }
        };
        if let Some(batch) = pending {
            dispatch_event(
                &runner,
                &batch.chat_id,
                &batch.sender_id,
                &batch.sender_name,
                &batch.text,
                batch.attachments,
            )
            .await;
        }
    });
}

/// Auth-gate-free dispatch (gating already happened at intake): run the
/// agent turn and deliver echoes/media/text back through iLink.
async fn dispatch_event(
    runner: &Arc<Runner>,
    chat_id: &str,
    sender_id: &str,
    sender_name: &str,
    text: &str,
    attachments: Vec<MediaAttachment>,
) {
    let mut event = MessageEvent {
        platform: "weixin".into(),
        chat_id: chat_id.to_string(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        text: text.to_string(),
        message_id: String::new(),
        attachments,
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
    }
    let outcome = match runner.dispatcher.handle_event(event).await {
        Ok(outcome) => outcome,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    for echo in &outcome.transcript_echoes {
        if let Err(e) = runner.handle.send_text_chunk(chat_id, echo).await {
            eprintln!("[weixin] transcript echo failed: {e}");
        }
    }
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&outcome.reply);
    // hermes delivers media attachments before the text body.
    for path in &media_paths {
        runner.handle.deliver_media_path(chat_id, path).await;
    }
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        runner
            .dispatcher
            .send_with_ledger("weixin", chat_id, &reply_text, || {
                runner
                    .handle
                    .send_text(chat_id, &reply_text, runner.cfg.split_multiline_messages)
            })
            .await;
    }
}

/// One-shot send helper for webhook/cron delivery (hermes
/// `send_weixin_direct`).
pub async fn send_weixin_direct(
    cfg: &WeixinConfig,
    chat_id: &str,
    message: &str,
) -> std::result::Result<(), String> {
    let home = crate::config::ulnclaw_home();
    let token = resolve_token(cfg, &home);
    if token.is_empty() {
        return Err("weixin: no token configured".into());
    }
    let account_id = resolve_account_id(cfg);
    let handle = WeixinHandle {
        client: reqwest::Client::new(),
        account_id,
        token,
        base_url: resolve_base_url(cfg),
        cdn_base_url: resolve_cdn_base_url(cfg),
        send_chunk_delay_seconds: cfg.send_chunk_delay_seconds,
        send_chunk_retries: cfg.send_chunk_retries,
        send_chunk_retry_delay_seconds: cfg.send_chunk_retry_delay_seconds,
        tokens: Mutex::new(ContextTokenStore::open(&home, chat_id)),
        circuit: Mutex::new(RateLimitCircuit::new()),
    };
    handle
        .send_text(chat_id, message, cfg.split_multiline_messages)
        .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkcs7_pad_unpad_roundtrip() {
        for len in [0usize, 1, 15, 16, 17, 31, 32, 100] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let padded = pkcs7_pad(&data);
            assert_eq!(padded.len() % 16, 0);
            assert!(padded.len() > data.len());
            assert_eq!(pkcs7_unpad(&padded), data);
        }
    }

    #[test]
    fn aes128_ecb_fips197_vector() {
        // FIPS-197 appendix C.1 single-block vector.
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plaintext: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let encrypted = aes128_ecb_encrypt(&plaintext, &key);
        // PKCS7 adds a full block for exact-size input.
        assert_eq!(encrypted.len(), 32);
        let expected = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        assert_eq!(&encrypted[..16], &expected);
        assert_eq!(aes128_ecb_decrypt(&encrypted, &key), plaintext);
    }

    #[test]
    fn aes_ecb_roundtrip_various_sizes() {
        let key = [0x42u8; 16];
        for len in [1usize, 15, 16, 17, 1000, 65537] {
            let data: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
            let encrypted = aes128_ecb_encrypt(&data, &key);
            assert_eq!(encrypted.len(), aes_padded_size(len));
            assert_eq!(aes128_ecb_decrypt(&encrypted, &key), data);
        }
    }

    #[test]
    fn parse_aes_key_raw_and_hex_forms() {
        let raw = [0xABu8; 16];
        let b64_raw = base64_encode(&raw);
        assert_eq!(parse_aes_key(&b64_raw).unwrap(), raw);

        // base64 of the 32-char ASCII hex string (hermes iLink form).
        let hex_str: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let b64_hex = base64_encode(hex_str.as_bytes());
        assert_eq!(parse_aes_key(&b64_hex).unwrap(), raw);

        assert!(parse_aes_key(&base64_encode(&[0u8; 7])).is_err());
    }

    #[test]
    fn normalize_markdown_collapses_blank_runs() {
        let input = "line one\n\n\n\nline two\n\n\nline three";
        assert_eq!(
            normalize_markdown_blocks(input),
            "line one\n\nline two\n\nline three"
        );
    }

    #[test]
    fn normalize_markdown_keeps_code_fence_blanks() {
        let input = "before\n```\ncode\n\ncode\n```\nafter";
        assert_eq!(
            normalize_markdown_blocks(input),
            "before\n```\ncode\n\ncode\n```\nafter"
        );
    }

    #[test]
    fn wrap_copy_friendly_wraps_long_lines() {
        let long = vec!["word"; 40].join(" "); // ~199 chars
        let wrapped = wrap_copy_friendly_lines(&long);
        for line in wrapped.lines() {
            assert!(
                line.chars().count() <= COPY_LINE_WIDTH,
                "line too long: {line}"
            );
        }
        assert!(wrapped.lines().count() > 1);
        // Words survive the wrap.
        assert_eq!(wrapped.split_whitespace().count(), 40);
    }

    #[test]
    fn wrap_copy_friendly_leaves_code_and_tables() {
        let long_in_code = vec!["x"; 200].join("");
        let input = format!(
            "```\n{long_in_code}\n```\n| {} | b |\n",
            vec!["y"; 200].join("")
        );
        let wrapped = wrap_copy_friendly_lines(&input);
        assert_eq!(wrapped, input.trim());
    }

    #[test]
    fn split_markdown_blocks_keeps_fences_intact() {
        let input = "para one\n\n```\ncode\ngated\n```\n\npara two";
        let blocks = split_markdown_blocks(input);
        assert_eq!(
            blocks,
            vec!["para one", "```\ncode\ngated\n```", "para two"]
        );
    }

    #[test]
    fn split_delivery_units_keeps_indented_continuations() {
        let input = "- item one\n  nested detail\n- item two";
        let units = split_delivery_units(input);
        assert_eq!(units, vec!["- item one\n  nested detail", "- item two"]);
    }

    #[test]
    fn split_short_text_stays_single() {
        let chunks = split_text_for_weixin_delivery("hello there", MAX_MESSAGE_LENGTH, false);
        assert_eq!(chunks, vec!["hello there"]);
    }

    #[test]
    fn split_chatty_block_bubbles() {
        let input = "Sounds good!\nOn my way.";
        let chunks = split_text_for_weixin_delivery(input, MAX_MESSAGE_LENGTH, false);
        assert_eq!(chunks, vec!["Sounds good!", "On my way."]);
    }

    #[test]
    fn split_long_content_respects_limit() {
        let mut content = String::new();
        for i in 0..60 {
            content.push_str(&format!(
                "Paragraph {i} with a bit of body text to push the size up.\n\n"
            ));
        }
        let chunks = split_text_for_weixin_delivery(&content, MAX_MESSAGE_LENGTH, false);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_MESSAGE_LENGTH);
        }
        // Nothing lost: all paragraphs present across chunks.
        let rejoined = chunks.join("\n\n");
        for i in 0..60 {
            assert!(rejoined.contains(&format!("Paragraph {i}")));
        }
    }

    #[test]
    fn split_oversized_code_block_hard_wraps() {
        let fence = format!("```\n{}\n```", vec!["z"; MAX_MESSAGE_LENGTH + 500].join(""));
        let chunks = split_text_for_weixin_delivery(&fence, MAX_MESSAGE_LENGTH, false);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_MESSAGE_LENGTH);
        }
    }

    #[test]
    fn extract_text_plain_and_quoted() {
        let items = vec![json!({"type": 1, "text_item": {"text": "hello"}})];
        assert_eq!(extract_text(&items), "hello");

        let quoted = vec![json!({
            "type": 1,
            "text_item": {"text": "my reply"},
            "ref_msg": {"title": "", "message_item": {"type": 2, "image_item": {}}},
        })];
        assert_eq!(extract_text(&quoted), "[引用媒体]\nmy reply");

        let quoted_text = vec![json!({
            "type": 1,
            "text_item": {"text": "answering"},
            "ref_msg": {"title": "Alice", "message_item": {"type": 1, "text_item": {"text": "question?"}}},
        })];
        assert_eq!(
            extract_text(&quoted_text),
            "[引用: Alice | question?]\nanswering"
        );
    }

    #[test]
    fn extract_text_voice_stt_fallback() {
        // Voice with media → empty (download raw audio for STT instead).
        let with_media = vec![
            json!({"type": 3, "voice_item": {"text": "hi", "media": {"encrypt_query_param": "x"}}}),
        ];
        assert_eq!(extract_text(&with_media), "");

        // Voice without media → use Tencent's transcription with a marker.
        let without_media = vec![json!({"type": 3, "voice_item": {"text": "你好"}})];
        assert_eq!(
            extract_text(&without_media),
            "[Voice transcription provided by Weixin]\n你好"
        );
    }

    #[test]
    fn guess_chat_type_group_and_dm() {
        let group = json!({"from_user_id": "u1", "room_id": "123@chatroom", "msg_type": 1});
        assert_eq!(
            guess_chat_type(&group, "bot"),
            ("group".into(), "123@chatroom".into())
        );

        let to_other = json!({"from_user_id": "u1", "to_user_id": "someone-else", "msg_type": 1});
        assert_eq!(
            guess_chat_type(&to_other, "bot"),
            ("group".into(), "someone-else".into())
        );

        let dm = json!({"from_user_id": "u1", "to_user_id": "bot", "msg_type": 1});
        assert_eq!(guess_chat_type(&dm, "bot"), ("dm".into(), "u1".into()));
    }

    #[test]
    fn dedup_flags_repeats_within_ttl() {
        let dedup = MessageDeduplicator::new(MESSAGE_DEDUP_TTL_SECS);
        assert!(!dedup.is_duplicate("m1"));
        assert!(dedup.is_duplicate("m1"));
        assert!(!dedup.is_duplicate("m2"));
    }

    #[test]
    fn cdn_urls_percent_encode() {
        let download = cdn_download_url("https://cdn.example/c2c/", "abc+def/ghi=");
        assert_eq!(
            download,
            "https://cdn.example/c2c/download?encrypted_query_param=abc%2Bdef%2Fghi%3D"
        );
        let upload = cdn_upload_url("https://cdn.example/c2c", "p a r a m", "key&1");
        assert!(upload.contains("encrypted_query_param=p%20a%20r%20a%20m"));
        assert!(upload.contains("filekey=key%261"));
    }

    #[test]
    fn cdn_allowlist_blocks_ssrf() {
        assert!(assert_weixin_cdn_url("https://novac2c.cdn.weixin.qq.com/c2c/x").is_ok());
        assert!(assert_weixin_cdn_url("https://evil.example.com/x").is_err());
        assert!(assert_weixin_cdn_url("ftp://novac2c.cdn.weixin.qq.com/x").is_err());
    }

    #[test]
    fn wechat_uin_is_base64_of_digits() {
        for _ in 0..8 {
            let uin = random_wechat_uin();
            let decoded = base64_decode(&uin).unwrap();
            let text = String::from_utf8(decoded).unwrap();
            assert!(!text.is_empty() && text.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn stale_session_detection() {
        assert!(is_stale_session_ret(Some(-2), None, Some("Unknown error")));
        assert!(is_stale_session_ret(None, Some(-2), Some("unknown error")));
        assert!(!is_stale_session_ret(
            Some(-2),
            None,
            Some("frequency limit")
        ));
        assert!(!is_stale_session_ret(
            Some(0),
            Some(0),
            Some("unknown error")
        ));
    }

    #[test]
    fn greedy_pack_respects_limit_and_overflow() {
        let blocks: Vec<String> = vec!["aaaa".into(), "bbbb".into(), "cc".into()];
        let packed = greedy_pack_blocks(&blocks, 10, "\n\n", None);
        assert_eq!(packed, vec!["aaaa\n\nbbbb", "cc"]);

        let oversized: Vec<String> = vec!["x".repeat(25)];
        let packed = greedy_pack_blocks(
            &oversized,
            10,
            "\n",
            Some(&|block: &str| split_oversized_block(block, 10)),
        );
        assert!(packed.iter().all(|c| c.chars().count() <= 10));
        assert_eq!(packed.join("").chars().count(), 25);
    }

    #[test]
    fn request_headers_carry_ilink_identity() {
        let headers: HashMap<String, String> = request_headers(Some("tok")).into_iter().collect();
        assert_eq!(headers.get("AuthorizationType").unwrap(), "ilink_bot_token");
        assert_eq!(headers.get("iLink-App-Id").unwrap(), "bot");
        assert_eq!(headers.get("iLink-App-ClientVersion").unwrap(), "131584");
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer tok");
        assert!(headers.contains_key("X-WECHAT-UIN"));

        let anon: HashMap<String, String> = request_headers(None).into_iter().collect();
        assert!(!anon.contains_key("Authorization"));
    }

    #[test]
    fn format_message_pipeline() {
        let input = "Title\n\n\n\nBody line one.";
        assert_eq!(format_message(input), "Title\n\nBody line one.");
    }
}
