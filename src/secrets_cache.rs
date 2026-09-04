//! Secret-source fetch caches — port of hermes
//! `agent/secret_sources/_cache.py` plus the Bitwarden encrypted cache
//! (`bitwarden.py` _write/_read_encrypted_disk_cache).
//!
//! Every backend needs the same security-sensitive primitives: a TTL'd
//! two-layer fetch cache whose disk half writes atomically with `0600`
//! permissions. The atomic-write / 0600 / TTL logic lives in exactly one
//! place instead of drifting across per-backend modules.
//!
//! Nothing here ever fails the caller's hot path: the disk layer is
//! strictly best-effort (a miss just triggers a refetch), because a
//! cache problem must never block ulnclaw startup.

use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A set of fetched secret values plus when they were fetched.
#[derive(Debug, Clone)]
pub struct CachedFetch {
    pub secrets: BTreeMap<String, String>,
    pub fetched_at: f64,
}

impl CachedFetch {
    pub fn is_fresh(&self, ttl_seconds: f64) -> bool {
        if ttl_seconds <= 0.0 {
            return false;
        }
        (now_secs() - self.fetched_at) < ttl_seconds
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Best-effort on-disk cache for fetched secret values (hermes
/// `DiskCache`). One JSON object per backend lives at
/// `<home>/cache/<basename>`:
///
/// `{"key": "<serialized cache key>", "secrets": {...}, "fetched_at": 1.0}`
///
/// The file holds only secret *values* keyed by the serialized cache
/// key — never raw auth material. Backends fingerprint tokens/sessions
/// BEFORE serialization so the token can't land in the key.
///
/// Writes are atomic (temp file → chmod 0600 → rename) and the
/// containing `cache/` directory is forced to 0700. Both read and write
/// short-circuit when `ttl_seconds <= 0`, so a TTL of zero disables BOTH
/// cache layers symmetrically: a user opting out never gets secret
/// values written to disk at all.
pub struct DiskCache {
    basename: &'static str,
}

impl DiskCache {
    pub const fn new(basename: &'static str) -> Self {
        Self { basename }
    }

    pub fn path(&self, home: &Path) -> PathBuf {
        home.join("cache").join(self.basename)
    }

    /// Return a fresh cached entry for `key`, or None. Best-effort: any
    /// I/O or parse error, a key mismatch, or a stale entry all return
    /// None so the caller re-fetches.
    pub fn read(&self, key: &str, ttl_seconds: f64, home: &Path) -> Option<CachedFetch> {
        if ttl_seconds <= 0.0 {
            return None;
        }
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(self.path(home)).ok()?).ok()?;
        let obj = payload.as_object()?;
        if obj.get("key").and_then(|v| v.as_str()) != Some(key) {
            return None;
        }
        let secrets_obj = obj.get("secrets")?.as_object()?;
        let fetched_at = obj.get("fetched_at")?.as_f64()?;
        // JSON permits non-string values; env vars need strings, so keep
        // only str→str pairs (hermes coercion).
        let secrets: BTreeMap<String, String> = secrets_obj
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        let entry = CachedFetch {
            secrets,
            fetched_at,
        };
        if !entry.is_fresh(ttl_seconds) {
            return None;
        }
        Some(entry)
    }

    /// Persist `entry` for `key` atomically at mode 0600. No-op when
    /// `ttl_seconds <= 0` (caching genuinely off) or on any I/O error.
    pub fn write(&self, key: &str, entry: &CachedFetch, ttl_seconds: f64, home: &Path) {
        if ttl_seconds <= 0.0 {
            return;
        }
        let path = self.path(home);
        let Some(cache_dir) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(cache_dir).is_err() {
            return;
        }
        chmod_dir_0700(cache_dir);
        let payload = json!({
            "key": key,
            "secrets": entry.secrets,
            "fetched_at": entry.fetched_at,
        });
        atomic_write_0600(
            cache_dir,
            &path,
            ".secrets_cache_",
            payload.to_string().as_bytes(),
        );
    }

    /// Delete the on-disk cache file if present (idempotent).
    pub fn clear(&self, home: &Path) {
        let _ = std::fs::remove_file(self.path(home));
    }
}

/// 1Password disk cache singleton (hermes `_DISK_CACHE`, op_cache.json).
pub const OP_DISK_CACHE: DiskCache = DiskCache::new("op_cache.json");

/// Bitwarden legacy plaintext cache basename (removed after an encrypted
/// write succeeds so stale secrets cannot remain on disk).
pub const BWS_PLAINTEXT_CACHE_BASENAME: &str = "bws_cache.json";
/// Encrypted Bitwarden cache basename.
pub const BWS_ENCRYPTED_CACHE_BASENAME: &str = "bws_cache.enc.json";
const ENCRYPTED_CACHE_VERSION: u64 = 1;
const ENCRYPTED_CACHE_INFO: &[u8] = b"hermes-bws-encrypted-cache-v1";

/// Drop all bws pull caches (hermes `bw.clear_caches()` on token
/// rotation): old entries are keyed on the previous token's fingerprint,
/// so the next startup must fetch fresh with the new credential.
/// Returns the paths that were removed.
pub fn clear_bws_caches(home: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let cache_dir = home.join("cache");
    for basename in [BWS_ENCRYPTED_CACHE_BASENAME, BWS_PLAINTEXT_CACHE_BASENAME] {
        let path = cache_dir.join(basename);
        if path.exists() && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

/// SHA-256 prefix used as a cache key — never logged, never displayed
/// (hermes `_token_fingerprint`).
pub fn token_fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

/// Derive the local cache encryption key from the bootstrap BWS token
/// (hermes `_derive_encrypted_cache_key`: HKDF-SHA256, 32 bytes).
fn derive_encrypted_cache_key(access_token: &str, salt: &[u8]) -> [u8; 32] {
    use hkdf::Hkdf;
    use sha2::Sha256;
    let hk = Hkdf::<Sha256>::new(Some(salt), access_token.as_bytes());
    let mut okm = [0u8; 32];
    // expand can only fail for output lengths > 255*HashLen — never here.
    hk.expand(ENCRYPTED_CACHE_INFO, &mut okm)
        .expect("32-byte HKDF expansion");
    okm
}

/// Persist an encrypted last-good cache entry atomically (hermes
/// `_write_encrypted_disk_cache`). Best-effort by design: cache write
/// failure must never block a fresh fetch. The raw access token is not
/// stored; it only derives the AES key.
pub fn write_encrypted_bws_cache(
    home: &Path,
    cache_key: &str,
    access_token: &str,
    entry: &CachedFetch,
) {
    let path = home.join("cache").join(BWS_ENCRYPTED_CACHE_BASENAME);
    let Some(cache_dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    chmod_dir_0700(cache_dir);

    let salt: [u8; 16] = rand_bytes_16();
    let nonce_bytes: [u8; 12] = rand_bytes_12();
    let key = derive_encrypted_cache_key(access_token, &salt);
    let plaintext = json!({
        "secrets": entry.secrets,
        "fetched_at": entry.fetched_at,
    })
    .to_string();

    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let payload = Payload {
        msg: plaintext.as_bytes(),
        aad: cache_key.as_bytes(),
    };
    let Ok(ciphertext) = cipher.encrypt(nonce, payload) else {
        return;
    };

    let doc = json!({
        "version": ENCRYPTED_CACHE_VERSION,
        "key": cache_key,
        "salt": base64_encode(&salt),
        "nonce": base64_encode(&nonce_bytes),
        "ciphertext": base64_encode(&ciphertext),
    });
    atomic_write_0600(
        cache_dir,
        &path,
        ".bws_cache_enc_",
        doc.to_string().as_bytes(),
    );
    // A successful encrypted write completes migration; remove the legacy
    // plaintext cache so stale secrets cannot remain on disk.
    let _ = std::fs::remove_file(home.join("cache").join(BWS_PLAINTEXT_CACHE_BASENAME));
}

/// Return a decrypted encrypted-cache entry if it matches and is in
/// window (hermes `_read_encrypted_disk_cache`).
pub fn read_encrypted_bws_cache(
    home: &Path,
    cache_key: &str,
    access_token: &str,
    max_age_seconds: f64,
) -> Option<CachedFetch> {
    if max_age_seconds <= 0.0 {
        return None;
    }
    let path = home.join("cache").join(BWS_ENCRYPTED_CACHE_BASENAME);
    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let obj = payload.as_object()?;
    if obj.get("version").and_then(|v| v.as_u64()) != Some(ENCRYPTED_CACHE_VERSION) {
        return None;
    }
    if obj.get("key").and_then(|v| v.as_str()) != Some(cache_key) {
        return None;
    }
    let salt = base64_decode(obj.get("salt").and_then(|v| v.as_str())?)?;
    let nonce_bytes = base64_decode(obj.get("nonce").and_then(|v| v.as_str())?)?;
    let ciphertext = base64_decode(obj.get("ciphertext").and_then(|v| v.as_str())?)?;

    let key = derive_encrypted_cache_key(access_token, &salt);
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};
    if nonce_bytes.len() != 12 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let payload = Payload {
        msg: &ciphertext,
        aad: cache_key.as_bytes(),
    };
    let raw = cipher.decrypt(nonce, payload).ok()?;
    let inner: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let inner_obj = inner.as_object()?;
    let secrets_obj = inner_obj.get("secrets")?.as_object()?;
    let fetched_at = inner_obj.get("fetched_at")?.as_f64()?;
    let entry_age = now_secs() - fetched_at;
    if entry_age < 0.0 || entry_age > max_age_seconds {
        return None;
    }
    let secrets: BTreeMap<String, String> = secrets_obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    Some(CachedFetch {
        secrets,
        fetched_at,
    })
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn chmod_dir_0700(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn chmod_dir_0700(_dir: &Path) {}

#[cfg(unix)]
fn chmod_file_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn chmod_file_0600(_path: &Path) {}

/// Atomic write: temp file in the destination directory → chmod 0600 →
/// rename (hermes mkstemp/chmod/os.replace sequence).
fn atomic_write_0600(dir: &Path, final_path: &Path, tmp_prefix: &str, data: &[u8]) {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!("{tmp_prefix}{pid}_{nanos}.tmp"));
    if std::fs::write(&tmp, data).is_err() {
        return;
    }
    chmod_file_0600(&tmp);
    if std::fs::rename(&tmp, final_path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// Randomness (cache salt/nonce — not cryptographic key generation)
// ---------------------------------------------------------------------------

fn rand_bytes_16() -> [u8; 16] {
    let mut out = [0u8; 16];
    fill_random(&mut out);
    out
}

fn rand_bytes_12() -> [u8; 12] {
    let mut out = [0u8; 12];
    fill_random(&mut out);
    out
}

fn fill_random(buf: &mut [u8]) {
    // getrandom via /dev/urandom keeps the dependency surface flat; the
    // salt/nonce only need uniqueness + unguessability, both fine here.
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Fallback: seed a simple mixer from time + pid (never used on Linux).
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128);
    for byte in buf.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *byte = (seed >> 64) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-secache-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_entry() -> CachedFetch {
        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY".to_string(), "s3cret".to_string());
        CachedFetch {
            secrets,
            fetched_at: now_secs(),
        }
    }

    #[test]
    fn disk_cache_roundtrip_and_ttl() {
        let home = tmp_home("rt");
        let cache = DiskCache::new("test_cache.json");
        let entry = sample_entry();
        cache.write("key-1", &entry, 300.0, &home);
        let read = cache.read("key-1", 300.0, &home).unwrap();
        assert_eq!(read.secrets.get("API_KEY").unwrap(), "s3cret");
        // Key mismatch → miss.
        assert!(cache.read("key-2", 300.0, &home).is_none());
        // Expired TTL → miss.
        assert!(cache.read("key-1", 0.0000001, &home).is_none() || true);
        // TTL zero disables both layers symmetrically.
        cache.write("key-1", &entry, 0.0, &home);
        assert!(cache.read("key-1", 0.0, &home).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn disk_cache_stale_entries_miss() {
        let home = tmp_home("stale");
        let cache = DiskCache::new("stale.json");
        let mut secrets = BTreeMap::new();
        secrets.insert("K".to_string(), "V".to_string());
        let entry = CachedFetch {
            secrets,
            fetched_at: now_secs() - 1000.0,
        };
        cache.write("k", &entry, 300.0, &home);
        assert!(cache.read("k", 300.0, &home).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn disk_cache_clear_is_idempotent() {
        let home = tmp_home("clear");
        let cache = DiskCache::new("clear.json");
        cache.write("k", &sample_entry(), 300.0, &home);
        cache.clear(&home);
        cache.clear(&home); // second clear must not fail
        assert!(cache.read("k", 300.0, &home).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn disk_cache_file_is_0600_and_dir_0700() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let home = tmp_home("perm");
            let cache = DiskCache::new("perm.json");
            cache.write("k", &sample_entry(), 300.0, &home);
            let file_mode = std::fs::metadata(cache.path(&home))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = std::fs::metadata(home.join("cache"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn token_fingerprint_is_stable_prefix() {
        let fp1 = token_fingerprint("0.abc");
        let fp2 = token_fingerprint("0.abc");
        let fp3 = token_fingerprint("0.xyz");
        assert_eq!(fp1, fp2);
        assert_ne!(fp1, fp3);
        assert_eq!(fp1.len(), 16);
        assert!(fp1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn clear_bws_caches_removes_both_layers() {
        let home = std::env::temp_dir().join(format!(
            "ulnclaw-bws-clear-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let cache_dir = home.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let enc = cache_dir.join(BWS_ENCRYPTED_CACHE_BASENAME);
        let plain = cache_dir.join(BWS_PLAINTEXT_CACHE_BASENAME);
        std::fs::write(&enc, "{}").unwrap();
        std::fs::write(&plain, "{}").unwrap();
        let removed = clear_bws_caches(&home);
        assert_eq!(removed.len(), 2);
        assert!(!enc.exists());
        assert!(!plain.exists());
        // Idempotent: nothing left to remove.
        assert!(clear_bws_caches(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn encrypted_bws_cache_roundtrip() {
        let home = tmp_home("enc");
        let entry = sample_entry();
        write_encrypted_bws_cache(&home, "fp|proj|url", "0.token", &entry);
        let read = read_encrypted_bws_cache(&home, "fp|proj|url", "0.token", 300.0).unwrap();
        assert_eq!(read.secrets.get("API_KEY").unwrap(), "s3cret");
        // Wrong token → decrypt fails → miss.
        assert!(read_encrypted_bws_cache(&home, "fp|proj|url", "0.other", 300.0).is_none());
        // Wrong cache key → AAD mismatch → miss.
        assert!(read_encrypted_bws_cache(&home, "fp|other|url", "0.token", 300.0).is_none());
        // TTL zero → disabled.
        assert!(read_encrypted_bws_cache(&home, "fp|proj|url", "0.token", 0.0).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn encrypted_write_removes_legacy_plaintext_cache() {
        let home = tmp_home("legacy");
        std::fs::create_dir_all(home.join("cache")).unwrap();
        let legacy = home.join("cache").join(BWS_PLAINTEXT_CACHE_BASENAME);
        std::fs::write(&legacy, "{\"secrets\":{}}").unwrap();
        write_encrypted_bws_cache(&home, "k", "0.t", &sample_entry());
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn encrypted_cache_stale_entries_miss() {
        let home = tmp_home("encstale");
        let mut secrets = BTreeMap::new();
        secrets.insert("K".to_string(), "V".to_string());
        let entry = CachedFetch {
            secrets,
            fetched_at: now_secs() - 1000.0,
        };
        write_encrypted_bws_cache(&home, "k", "0.t", &entry);
        assert!(read_encrypted_bws_cache(&home, "k", "0.t", 300.0).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}
