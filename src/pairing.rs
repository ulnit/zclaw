//! Messaging pairing codes — DM authorization via pairing codes that the
//! bot owner approves from the CLI (hermes `gateway/pairing.py` port).
//!
//! An unknown sender who messages the bot receives a short pairing code;
//! the owner runs `ulnclaw pairing approve <platform> <code>` to grant
//! access. Grants live in `<home>/pairing/{platform}-approved.json` and
//! join the configured allowlist in a union at the auth gate.
//!
//! Hermes semantics preserved:
//! - 8-char codes from a 32-symbol alphabet (no I/O/0/1 lookalikes),
//!   generated with a CSPRNG;
//! - codes are never persisted — only a salted SHA-256 hash, keyed by a
//!   random entry id (reading the pending file reveals nothing);
//! - 1-hour code expiry, max 3 pending codes per platform;
//! - per-user rate limit: one pairing request per 10 minutes;
//! - 5 failed approvals lock the platform out for 1 hour;
//! - a successful approval resets the consecutive-failure streak;
//! - approve accepts either the server-side request id (from `pairing
//!   list`) or the code itself.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Code alphabet without I/O/0/1 lookalikes (hermes `ALPHABET`).
pub const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LENGTH: usize = 8;
/// Codes expire after one hour (hermes).
pub const CODE_TTL_SECONDS: u64 = 3600;
/// One pairing request per user per 10 minutes (hermes `RATE_LIMIT_SECONDS`).
pub const RATE_LIMIT_SECONDS: u64 = 600;
/// Max pending codes per platform (hermes `MAX_PENDING_PER_PLATFORM`).
pub const MAX_PENDING_PER_PLATFORM: usize = 3;
/// Failed approvals before a platform lockout (hermes `MAX_FAILED_ATTEMPTS`).
pub const MAX_FAILED_ATTEMPTS: u32 = 5;
/// Lockout duration after too many failed approvals (hermes `LOCKOUT_SECONDS`).
pub const LOCKOUT_SECONDS: u64 = 3600;

/// Approved pairing grant.
#[derive(Debug, Clone)]
pub struct PairingGrant {
    pub user_id: String,
    pub user_name: String,
}

/// Pending pairing request (codes are never exposed — hashed at rest).
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub request_id: String,
    pub user_id: String,
    pub user_name: String,
    pub age_minutes: u64,
}

/// File-backed pairing store rooted at `<home>/pairing/`.
pub struct PairingStore {
    dir: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hash_code(code: &str, salt: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(code.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Constant-time string comparison (hermes `secrets.compare_digest`).
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

impl PairingStore {
    pub fn open(home: &Path) -> Self {
        let dir = home.join("pairing");
        std::fs::create_dir_all(&dir).ok();
        Self { dir }
    }

    fn pending_path(&self, platform: &str) -> PathBuf {
        self.dir.join(format!("{platform}-pending.json"))
    }

    fn approved_path(&self, platform: &str) -> PathBuf {
        self.dir.join(format!("{platform}-approved.json"))
    }

    fn rate_limit_path(&self) -> PathBuf {
        self.dir.join("_rate_limits.json")
    }

    fn load_json(&self, path: &Path) -> Value {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null)
    }

    fn save_json(&self, path: &Path, value: &Value) {
        if let Ok(text) = serde_json::to_string_pretty(value) {
            std::fs::write(path, text).ok();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
            }
        }
    }

    // ----- rate limiting / lockout -----

    fn rate_limits(&self) -> Value {
        let limits = self.load_json(&self.rate_limit_path());
        if limits.is_null() {
            json!({})
        } else {
            limits
        }
    }

    fn save_rate_limits(&self, limits: &Value) {
        self.save_json(&self.rate_limit_path(), limits);
    }

    /// One pairing request per user per `RATE_LIMIT_SECONDS` (hermes).
    pub fn is_rate_limited(&self, platform: &str, user_id: &str) -> bool {
        let key = format!("{platform}:{user_id}");
        let Some(last) = self.rate_limits().get(&key).and_then(|v| v.as_u64()) else {
            return false;
        };
        now_secs().saturating_sub(last) < RATE_LIMIT_SECONDS
    }

    /// Record a pairing request for rate-limit purposes (hermes calls
    /// this on the too-many-requests path so follow-ups stay silent).
    pub fn record_rate_limit(&self, platform: &str, user_id: &str) {
        let mut limits = self.rate_limits();
        limits[format!("{platform}:{user_id}")] = json!(now_secs());
        self.save_rate_limits(&limits);
    }

    /// Platform locked out after `MAX_FAILED_ATTEMPTS` failed approvals.
    pub fn is_locked_out(&self, platform: &str) -> bool {
        let key = format!("_lockout:{platform}");
        let Some(until) = self.rate_limits().get(&key).and_then(|v| v.as_u64()) else {
            return false;
        };
        now_secs() < until
    }

    fn record_failed_attempt(&self, platform: &str) {
        let mut limits = self.rate_limits();
        let fail_key = format!("_failures:{platform}");
        let fails = limits.get(&fail_key).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        if fails >= MAX_FAILED_ATTEMPTS as u64 {
            limits[format!("_lockout:{platform}")] = json!(now_secs() + LOCKOUT_SECONDS);
            limits[&fail_key] = json!(0);
            eprintln!(
                "[pairing] platform {platform} locked out for {LOCKOUT_SECONDS}s after \
                 {MAX_FAILED_ATTEMPTS} failed attempts"
            );
        } else {
            limits[&fail_key] = json!(fails);
        }
        self.save_rate_limits(&limits);
    }

    fn reset_failed_attempts(&self, platform: &str) {
        let mut limits = self.rate_limits();
        let fail_key = format!("_failures:{platform}");
        if limits.get(&fail_key).and_then(|v| v.as_u64()).unwrap_or(0) > 0 {
            limits[&fail_key] = json!(0);
            self.save_rate_limits(&limits);
        }
    }

    // ----- pending codes -----

    fn cleanup_expired(&self, pending: &mut Value) {
        let cutoff = now_secs().saturating_sub(CODE_TTL_SECONDS);
        if let Some(map) = pending.as_object_mut() {
            map.retain(|_, entry| {
                entry
                    .get("created_at")
                    .and_then(|v| v.as_u64())
                    .map(|created| created >= cutoff)
                    .unwrap_or(false)
            });
        }
    }

    /// Generate a pairing code for a new user. Returns `None` when the
    /// user is rate-limited, the platform is locked out, or the pending
    /// queue is full (hermes `generate_code`). The code is returned once
    /// and never persisted in plaintext.
    pub fn generate_code(&self, platform: &str, user_id: &str, user_name: &str) -> Option<String> {
        if self.is_locked_out(platform) || self.is_rate_limited(platform, user_id) {
            return None;
        }
        let mut pending = self.load_json(&self.pending_path(platform));
        if pending.is_null() {
            pending = json!({});
        }
        self.cleanup_expired(&mut pending);
        if pending.as_object().map(|m| m.len()).unwrap_or(0) >= MAX_PENDING_PER_PLATFORM {
            return None;
        }
        let code: String = (0..CODE_LENGTH)
            .map(|_| {
                let idx = rand_u32() as usize % ALPHABET.len();
                ALPHABET[idx] as char
            })
            .collect();
        let salt: Vec<u8> = (0..16).map(|_| rand_u32() as u8).collect();
        let entry_id = format!("{:016x}{:016x}", rand_u64(), rand_u64());
        pending[&entry_id] = json!({
            "hash": hash_code(&code, &salt),
            "salt": hex_encode(&salt),
            "user_id": user_id,
            "user_name": user_name,
            "created_at": now_secs(),
        });
        self.save_json(&self.pending_path(platform), &pending);
        self.record_rate_limit(platform, user_id);
        Some(code)
    }

    /// Approve a pairing request by code or request id (hermes
    /// `approve_code`). Returns the granted user, or `None` when the
    /// code/id is unknown, expired, or the platform is locked out.
    pub fn approve_code(&self, platform: &str, code_or_id: &str) -> Option<PairingGrant> {
        if self.is_locked_out(platform) {
            return None;
        }
        let mut pending = self.load_json(&self.pending_path(platform));
        if pending.is_null() {
            pending = json!({});
        }
        self.cleanup_expired(&mut pending);

        // Request-id match first (hermes: id lookup is case-insensitive).
        let id_hit = pending.as_object().and_then(|map| {
            map.keys()
                .find(|key| constant_time_eq(&key.to_lowercase(), &code_or_id.to_lowercase()))
                .cloned()
        });
        // Otherwise scan salted hashes.
        let mut code_hit: Option<String> = None;
        if id_hit.is_none() {
            if let Some(map) = pending.as_object() {
                for (entry_id, entry) in map {
                    let Some(salt_hex) = entry.get("salt").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let Some(hash) = entry.get("hash").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let salt = hex_decode(salt_hex);
                    if constant_time_eq(&hash_code(code_or_id, &salt), hash) {
                        code_hit = Some(entry_id.clone());
                        break;
                    }
                }
            }
        }
        let entry_id = match id_hit.or(code_hit) {
            Some(entry_id) => entry_id,
            None => {
                self.record_failed_attempt(platform);
                return None;
            }
        };

        let entry = pending.get(&entry_id).cloned().unwrap_or(Value::Null);
        if let Some(map) = pending.as_object_mut() {
            map.remove(&entry_id);
        }
        self.save_json(&self.pending_path(platform), &pending);
        // A successful approval proves the requester is legitimate — the
        // consecutive-failure streak must not carry over (hermes).
        self.reset_failed_attempts(platform);

        let user_id = entry
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let user_name = entry
            .get("user_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.approve_user(platform, &user_id, &user_name);
        Some(PairingGrant { user_id, user_name })
    }

    fn approve_user(&self, platform: &str, user_id: &str, user_name: &str) {
        let mut approved = self.load_json(&self.approved_path(platform));
        if approved.is_null() {
            approved = json!({});
        }
        approved[user_id] = json!({
            "user_name": user_name,
            "approved_at": now_secs(),
        });
        self.save_json(&self.approved_path(platform), &approved);
    }

    // ----- queries -----

    /// Union member: user paired via an approved code (hermes `is_approved`).
    pub fn is_approved(&self, platform: &str, user_id: &str) -> bool {
        let approved = self.load_json(&self.approved_path(platform));
        approved
            .as_object()
            .map(|map| map.contains_key(user_id))
            .unwrap_or(false)
    }

    pub fn list_pending(&self, platform: &str) -> Vec<PendingRequest> {
        let mut pending = self.load_json(&self.pending_path(platform));
        if pending.is_null() {
            pending = json!({});
        }
        self.cleanup_expired(&mut pending);
        let mut requests = Vec::new();
        if let Some(map) = pending.as_object() {
            for (entry_id, entry) in map {
                let created_at = entry
                    .get("created_at")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                requests.push(PendingRequest {
                    request_id: entry_id.clone(),
                    user_id: entry
                        .get("user_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    user_name: entry
                        .get("user_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    age_minutes: now_secs().saturating_sub(created_at) / 60,
                });
            }
        }
        requests.sort_by(|a, b| a.request_id.cmp(&b.request_id));
        requests
    }

    pub fn list_approved(&self, platform: &str) -> Vec<PairingGrant> {
        let approved = self.load_json(&self.approved_path(platform));
        let mut grants = Vec::new();
        if let Some(map) = approved.as_object() {
            for (user_id, info) in map {
                grants.push(PairingGrant {
                    user_id: user_id.clone(),
                    user_name: info
                        .get("user_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
        grants.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        grants
    }

    /// Platforms that have pending or approved state on disk.
    pub fn known_platforms(&self) -> Vec<String> {
        let mut platforms = std::collections::BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(platform) = name.strip_suffix("-pending.json") {
                    platforms.insert(platform.to_string());
                } else if let Some(platform) = name.strip_suffix("-approved.json") {
                    platforms.insert(platform.to_string());
                }
            }
        }
        platforms.into_iter().collect()
    }

    /// Revoke a paired user (hermes `revoke`). Returns true when removed.
    pub fn revoke(&self, platform: &str, user_id: &str) -> bool {
        let mut approved = self.load_json(&self.approved_path(platform));
        let removed = approved
            .as_object_mut()
            .map(|map| map.remove(user_id).is_some())
            .unwrap_or(false);
        if removed {
            self.save_json(&self.approved_path(platform), &approved);
        }
        removed
    }

    /// Drop every pending code for a platform (hermes `clear-pending`).
    pub fn clear_pending(&self, platform: &str) -> usize {
        let mut pending = self.load_json(&self.pending_path(platform));
        let count = pending.as_object().map(|m| m.len()).unwrap_or(0);
        if count > 0 {
            pending = json!({});
            self.save_json(&self.pending_path(platform), &pending);
        }
        count
    }
}

// ----- small CSPRNG-backed helpers (no rand crate dependency) -----

fn rand_u64() -> u64 {
    let mut buf = [0u8; 8];
    getrandom_fill(&mut buf);
    u64::from_le_bytes(buf)
}

fn rand_u32() -> u32 {
    let mut buf = [0u8; 4];
    getrandom_fill(&mut buf);
    u32::from_le_bytes(buf)
}

fn getrandom_fill(buf: &mut [u8]) {
    // /dev/urandom is the CSPRNG source on the musl targets ulnclaw ships;
    // fall back to a seeded LCG only if it is unreadable (shouldn't happen).
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if file.read_exact(buf).is_ok() {
            return;
        }
    }
    let mut seed = now_secs();
    for slot in buf.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *slot = (seed >> 33) as u8;
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let (Some(a), Some(b)) = (chars.next(), chars.next()) {
        let byte = u8::from_str_radix(&format!("{a}{b}"), 16).unwrap_or(0);
        out.push(byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (PairingStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-pairing-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = PairingStore::open(&dir);
        (store, dir)
    }

    #[test]
    fn code_generation_and_approval_roundtrip() {
        let (store, dir) = temp_store("roundtrip");
        let code = store.generate_code("telegram", "user-1", "Alice").unwrap();
        assert_eq!(code.len(), CODE_LENGTH);
        assert!(code.chars().all(|c| ALPHABET.contains(&(c as u8))));
        // Pending request visible, code itself never stored.
        let pending = store.list_pending("telegram");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].user_id, "user-1");
        let raw = std::fs::read_to_string(store.pending_path("telegram")).unwrap();
        assert!(!raw.contains(&code));
        assert!(raw.contains("Alice"));
        // Approve by code → grant union membership.
        assert!(!store.is_approved("telegram", "user-1"));
        let grant = store.approve_code("telegram", &code).unwrap();
        assert_eq!(grant.user_id, "user-1");
        assert_eq!(grant.user_name, "Alice");
        assert!(store.is_approved("telegram", "user-1"));
        assert!(store.list_pending("telegram").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn approve_accepts_request_id_and_is_case_insensitive() {
        let (store, dir) = temp_store("reqid");
        let _code = store.generate_code("discord", "user-2", "Bob").unwrap();
        let request_id = store.list_pending("discord")[0].request_id.clone();
        let grant = store
            .approve_code("discord", &request_id.to_uppercase())
            .unwrap();
        assert_eq!(grant.user_id, "user-2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_code_fails_and_locks_out_after_five_attempts() {
        let (store, dir) = temp_store("lockout");
        let code = store.generate_code("slack", "user-3", "").unwrap();
        for _ in 0..MAX_FAILED_ATTEMPTS {
            assert!(store.approve_code("slack", "ZZZZZZZZ").is_none());
        }
        assert!(store.is_locked_out("slack"));
        // Even the right code is refused during lockout.
        assert!(store.approve_code("slack", &code).is_none());
        assert!(store.generate_code("slack", "user-4", "").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn success_resets_failure_streak() {
        let (store, dir) = temp_store("streak");
        let code_a = store.generate_code("telegram", "user-a", "").unwrap();
        for _ in 0..MAX_FAILED_ATTEMPTS - 1 {
            assert!(store.approve_code("telegram", "ZZZZZZZZ").is_none());
        }
        // A success resets the streak, so one more failure must not lock.
        assert!(store.approve_code("telegram", &code_a).is_some());
        assert!(store.approve_code("telegram", "ZZZZZZZZ").is_none());
        assert!(!store.is_locked_out("telegram"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn per_user_rate_limit() {
        let (store, dir) = temp_store("ratelimit");
        assert!(store.generate_code("telegram", "user-5", "").is_some());
        // Same user within RATE_LIMIT_SECONDS → refused.
        assert!(store.generate_code("telegram", "user-5", "").is_none());
        // A different user is unaffected.
        assert!(store.generate_code("telegram", "user-6", "").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_pending_per_platform() {
        let (store, dir) = temp_store("maxpending");
        for idx in 0..MAX_PENDING_PER_PLATFORM {
            assert!(store
                .generate_code("telegram", &format!("u{idx}"), "")
                .is_some());
        }
        assert!(store
            .generate_code("telegram", "one-too-many", "")
            .is_none());
        assert_eq!(
            store.list_pending("telegram").len(),
            MAX_PENDING_PER_PLATFORM
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expired_codes_are_cleaned() {
        let (store, dir) = temp_store("expiry");
        let code = store.generate_code("telegram", "user-7", "").unwrap();
        // Backdate the entry past the TTL.
        let path = store.pending_path("telegram");
        let mut pending: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (_, entry) in pending.as_object_mut().unwrap().iter_mut() {
            entry["created_at"] = json!(now_secs().saturating_sub(CODE_TTL_SECONDS + 60));
        }
        std::fs::write(&path, pending.to_string()).unwrap();
        assert!(store.approve_code("telegram", &code).is_none());
        assert!(store.list_pending("telegram").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn revoke_and_clear_pending() {
        let (store, dir) = temp_store("revoke");
        let code = store.generate_code("telegram", "user-8", "Cara").unwrap();
        store.approve_code("telegram", &code).unwrap();
        assert!(store.is_approved("telegram", "user-8"));
        assert!(store.revoke("telegram", "user-8"));
        assert!(!store.is_approved("telegram", "user-8"));
        assert!(!store.revoke("telegram", "user-8"));

        store.generate_code("telegram", "user-9", "").unwrap();
        assert_eq!(store.clear_pending("telegram"), 1);
        assert!(store.list_pending("telegram").is_empty());
        assert_eq!(store.clear_pending("telegram"), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn known_platforms_discovered_from_files() {
        let (store, dir) = temp_store("known");
        store.generate_code("telegram", "u1", "").unwrap();
        let code = store.generate_code("discord", "u2", "").unwrap();
        store.approve_code("discord", &code).unwrap();
        assert_eq!(
            store.known_platforms(),
            vec!["discord".to_string(), "telegram".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
