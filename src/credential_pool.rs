//! Provider credential pool — lean aligned with hermes `agent/credential_pool.py`.
//!
//! Multiple API keys per provider live in `<home>/credentials-pool.json`.
//! Resolution (`UlncLawConfig::resolve_api_key`) prefers an explicit config
//! key, then a pooled entry (round-robin across the provider's entries),
//! then environment variables. Hermes seeds its pool from env/OAuth/config
//! sources; the lean port keeps manual entries (dashboard/`POST
//! /api/credentials/pool`) and treats pool membership as the curation
//! signal — a provider with pooled entries resolves from the pool.
//!
//! Differences from hermes (documented in `docs/*/hermes-parity.md`):
//! no automatic seeding from env vars or OAuth singleton files, no
//! suppression/removal-step registry (manual entries have no external
//! state to clean up), and request-count rotation state persists on a
//! best-effort basis (no cross-process file lock).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Auth type for pool entries (hermes `AUTH_TYPE_API_KEY`).
pub const AUTH_TYPE_API_KEY: &str = "api_key";
/// Source marker for entries added through the dashboard/API (hermes
/// `SOURCE_MANUAL`).
pub const SOURCE_MANUAL: &str = "manual";

/// One pooled credential (hermes `PooledCredential` subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PooledCredential {
    /// Provider slug, normalized lowercase (`openai`, `anthropic`, ...).
    pub provider: String,
    /// Short unique id (hex fragment).
    pub id: String,
    /// Human label (`key #1`, custom).
    pub label: String,
    /// Always `api_key` in the lean port.
    pub auth_type: String,
    /// Higher entries win first (hermes `priority`).
    #[serde(default)]
    pub priority: i32,
    /// Where the entry came from (`manual`).
    #[serde(default)]
    pub source: String,
    /// The raw API key.
    pub access_token: String,
    /// Unix seconds when added.
    #[serde(default)]
    pub created_at: u64,
    /// Times this entry was picked (rotation state).
    #[serde(default)]
    pub request_count: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PoolFile {
    #[serde(default)]
    entries: Vec<PooledCredential>,
}

/// Credential pool backed by `<home>/credentials-pool.json`.
#[derive(Debug, Default)]
pub struct Pool {
    file: PoolFile,
}

/// Path of the pool store for a home directory.
pub fn pool_path(home: &Path) -> PathBuf {
    home.join("credentials-pool.json")
}

/// Normalize a provider id the way hermes does (strip + lowercase).
pub fn normalize_provider(provider: &str) -> String {
    provider.trim().to_lowercase()
}

impl Pool {
    /// Load the pool store; missing or corrupt files yield an empty pool
    /// (hermes treats a missing store as "no pooled credentials").
    pub fn load(home: &Path) -> Pool {
        let path = pool_path(home);
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<PoolFile>(&raw).ok())
            .unwrap_or_default();
        Pool { file }
    }

    /// Persist the pool atomically (tmp file + rename).
    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let path = pool_path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_vec_pretty(&self.file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &raw)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Distinct provider ids with pooled entries, sorted.
    pub fn providers(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .file
            .entries
            .iter()
            .map(|e| e.provider.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        out.sort();
        out
    }

    /// Entries for a provider, highest priority first (stable order
    /// otherwise — insertion order within a priority tier).
    pub fn entries(&self, provider: &str) -> Vec<&PooledCredential> {
        let provider = normalize_provider(provider);
        let mut out: Vec<&PooledCredential> = self
            .file
            .entries
            .iter()
            .filter(|e| e.provider == provider)
            .collect();
        out.sort_by(|a, b| b.priority.cmp(&a.priority));
        out
    }

    /// Append an entry (provider normalized).
    pub fn add(&mut self, mut entry: PooledCredential) {
        entry.provider = normalize_provider(&entry.provider);
        self.file.entries.push(entry);
    }

    /// Remove a provider entry by 1-based index (matches the list
    /// endpoint's numbering). Returns the removed entry.
    pub fn remove_index(&mut self, provider: &str, index: usize) -> Option<PooledCredential> {
        let provider = normalize_provider(provider);
        let matches: Vec<usize> = self
            .file
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.provider == provider)
            .map(|(i, _)| i)
            .collect();
        let slot = *matches.get(index.checked_sub(1)?)?;
        Some(self.file.entries.remove(slot))
    }

    /// Pick the next entry for a provider (hermes rotation semantics,
    /// lean): highest priority tier first, then the least-used entry.
    /// Increments `request_count`.
    pub fn pick(&mut self, provider: &str) -> Option<&PooledCredential> {
        let provider = normalize_provider(provider);
        let slot = {
            let mut best: Option<(usize, i32, u64)> = None;
            for (i, entry) in self.file.entries.iter().enumerate() {
                if entry.provider != provider {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((_, prio, count)) => {
                        entry.priority > prio
                            || (entry.priority == prio && entry.request_count < count)
                    }
                };
                if better {
                    best = Some((i, entry.priority, entry.request_count));
                }
            }
            best.map(|(i, _, _)| i)
        }?;
        self.file.entries[slot].request_count += 1;
        Some(&self.file.entries[slot])
    }

    /// Total entries across providers.
    pub fn len(&self) -> usize {
        self.file.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.file.entries.is_empty()
    }
}

/// Resolve the pooled key for a provider, advancing rotation state on a
/// best-effort basis. Returns `None` when the provider has no pooled
/// entries.
pub fn resolve_pooled_key(home: &Path, provider: &str) -> Option<String> {
    let mut pool = Pool::load(home);
    let key = pool.pick(provider)?.access_token.clone();
    let _ = pool.save(home);
    Some(key)
}

/// Mint a short entry id (hermes uses a 6-hex uuid fragment).
pub fn new_entry_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:06x}", (std::process::id() ^ nanos) & 0xFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(provider: &str, label: &str, token: &str, priority: i32) -> PooledCredential {
        PooledCredential {
            provider: provider.to_string(),
            id: new_entry_id(),
            label: label.to_string(),
            auth_type: AUTH_TYPE_API_KEY.to_string(),
            priority,
            source: SOURCE_MANUAL.to_string(),
            access_token: token.to_string(),
            created_at: 0,
            request_count: 0,
        }
    }

    #[test]
    fn test_pool_add_pick_rotate_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut pool = Pool::load(home);
        assert!(pool.is_empty());
        pool.add(entry("OpenAI", "key #1", "sk-one", 0));
        pool.add(entry("openai", "key #2", "sk-two", 0));
        pool.add(entry("anthropic", "a #1", "sk-ant", 0));
        pool.save(home).unwrap();

        // Reload: rotation picks the least-used entry within the top
        // priority tier, so both openai keys alternate.
        let mut pool = Pool::load(home);
        assert_eq!(pool.providers(), vec!["anthropic", "openai"]);
        assert_eq!(pool.entries("openai").len(), 2);
        let first = pool.pick("openai").unwrap().access_token.clone();
        let second = pool.pick("openai").unwrap().access_token.clone();
        assert_ne!(first, second, "rotation alternates keys");
        pool.save(home).unwrap();

        let pool = Pool::load(home);
        let counts: Vec<u64> = pool
            .entries("openai")
            .iter()
            .map(|e| e.request_count)
            .collect();
        assert_eq!(counts, vec![1, 1]);
    }

    #[test]
    fn test_pool_priority_wins_over_usage() {
        let dir = tempfile::tempdir().unwrap();
        let mut pool = Pool::load(dir.path());
        pool.add(entry("openai", "low", "sk-low", 0));
        pool.add(entry("openai", "high", "sk-high", 5));
        let picked = pool.pick("openai").unwrap();
        assert_eq!(picked.access_token, "sk-high");
    }

    #[test]
    fn test_pool_remove_index_one_based() {
        let dir = tempfile::tempdir().unwrap();
        let mut pool = Pool::load(dir.path());
        pool.add(entry("openai", "key #1", "sk-one", 0));
        pool.add(entry("openai", "key #2", "sk-two", 0));
        let removed = pool.remove_index("openai", 1).unwrap();
        assert_eq!(removed.access_token, "sk-one");
        assert!(pool.remove_index("openai", 5).is_none());
        assert!(pool.remove_index("openai", 0).is_none());
        assert_eq!(pool.entries("openai").len(), 1);
    }

    #[test]
    fn test_resolve_pooled_key_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_pooled_key(dir.path(), "openai").is_none());
        let mut pool = Pool::load(dir.path());
        pool.add(entry("openai", "k", "sk-pool", 0));
        pool.save(dir.path()).unwrap();
        assert_eq!(
            resolve_pooled_key(dir.path(), "OPENAI").as_deref(),
            Some("sk-pool")
        );
    }

    #[test]
    fn test_corrupt_store_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pool_path(dir.path()), "{ not json").unwrap();
        let pool = Pool::load(dir.path());
        assert!(pool.is_empty());
    }
}
