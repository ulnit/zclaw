//! Skill Sync — port of hermes `tools/skills_sync_client.py` +
//! `hermes sync` (personal skills across devices).
//!
//! Hermes syncs through the Nous Portal; ulnclaw keeps the exact UX but
//! makes the transport service-agnostic: `[sync] base_url` may be an
//! HTTP(S) endpoint (REST + bearer token) or a local/shared directory
//! (offline/NAS sync). Sync is INERT unless a base_url is configured —
//! commands report the state rather than failing opaquely (hermes
//! `SyncInertError` semantics).

use crate::error::{AgentError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `[sync]` config block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Sync endpoint: https://... or a local directory path. Empty = inert.
    #[serde(default)]
    pub base_url: String,
    /// Static API key (bearer) for HTTP endpoints; OAuth tokens are used
    /// automatically when present.
    #[serde(default)]
    pub api_key: String,
    /// Device label attached to pushed manifests (hermes device name).
    #[serde(default)]
    pub device_name: String,
}

/// Persisted sync state (`<home>/skills-sync-state.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncState {
    /// Stable per-install id (hermes `stable_device_id`).
    #[serde(default)]
    pub device_id: String,
    /// Device label (set via `sync device --name`; overrides `[sync]
    /// device_name`).
    #[serde(default)]
    pub device_name: String,
    /// Skills opted into sync (hermes opt-in model).
    #[serde(default)]
    pub enabled: Vec<String>,
}

fn state_path(home: &Path) -> PathBuf {
    home.join("skills-sync-state.json")
}

pub fn load_state(home: &Path) -> SyncState {
    let mut state: SyncState = std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if state.device_id.is_empty() {
        state.device_id = uuid::Uuid::new_v4().to_string();
        save_state(home, &state).ok();
    }
    state
}

pub fn save_state(home: &Path, state: &SyncState) -> Result<()> {
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| AgentError::config(format!("serialize sync state: {e}")))?;
    std::fs::write(state_path(home), text)
        .map_err(|e| AgentError::config(format!("write sync state: {e}")))
}

/// hermes `stable_device_id`: created on first use, stable afterwards.
pub fn stable_device_id(home: &Path) -> String {
    load_state(home).device_id
}

/// Set/replace the device label (hermes `set_device_name`).
pub fn set_device_name(home: &Path, name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(AgentError::config(
            "device name must be 1-64 non-empty characters",
        ));
    }
    let mut state = load_state(home);
    state.device_name = trimmed.to_string();
    save_state(home, &state)?;
    Ok(trimmed.to_string())
}

/// Sync inertness (hermes SyncInertError): no base_url = report, don't fail.
pub fn inert_reason(cfg: &SyncConfig) -> Option<String> {
    if cfg.base_url.trim().is_empty() {
        Some(
            "sync is inert — set [sync] base_url (https endpoint or a shared \
             directory path) to enable it"
                .to_string(),
        )
    } else {
        None
    }
}

/// One skill's payload in a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncedSkill {
    pub name: String,
    pub device: String,
    pub pushed_at: u64,
    /// Relative path → file content (skills are small text bundles).
    pub files: BTreeMap<String, String>,
}

/// The sync manifest: name → skill payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncManifest {
    #[serde(default)]
    pub skills: BTreeMap<String, SyncedSkill>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Collect an opted-in skill directory into a payload.
fn collect_skill(skills_dir: &Path, name: &str, device: &str) -> Result<SyncedSkill> {
    let root = skills_dir.join(name);
    if !root.join("SKILL.md").is_file() {
        return Err(AgentError::config(format!(
            "skill {name:?} not found in {}",
            skills_dir.display()
        )));
    }
    let mut files = BTreeMap::new();
    collect_files(&root, &root, &mut files)?;
    Ok(SyncedSkill {
        name: name.to_string(),
        device: device.to_string(),
        pushed_at: now_secs(),
        files,
    })
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| AgentError::config(format!("read {}: {e}", dir.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            // Text-only payloads; skip binaries quietly.
            if let Ok(content) = std::fs::read_to_string(&path) {
                out.insert(rel, content);
            }
        }
    }
    Ok(())
}

fn manifest_path_for(base: &str) -> Result<ManifestTarget> {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Ok(ManifestTarget::Http(trimmed.to_string()))
    } else {
        let path = trimmed.strip_prefix("file://").unwrap_or(trimmed);
        Ok(ManifestTarget::Dir(PathBuf::from(path)))
    }
}

enum ManifestTarget {
    Http(String),
    Dir(PathBuf),
}

/// Read the remote manifest.
pub async fn read_manifest(
    cfg: &SyncConfig,
    oauth: &crate::oauth::OAuthConfig,
    home: &Path,
) -> Result<SyncManifest> {
    match manifest_path_for(&cfg.base_url)? {
        ManifestTarget::Dir(dir) => {
            let path = dir.join("manifest.json");
            if !path.is_file() {
                return Ok(SyncManifest::default());
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| AgentError::config(format!("read manifest: {e}")))?;
            serde_json::from_str(&text)
                .map_err(|e| AgentError::config(format!("parse manifest: {e}")))
        }
        ManifestTarget::Http(base) => {
            let client = reqwest::Client::new();
            let mut request = client
                .get(format!("{base}/v1/skills-sync/manifest"))
                .timeout(std::time::Duration::from_secs(30));
            if let Some(token) = bearer(cfg, oauth, home).await {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = request
                .send()
                .await
                .map_err(|e| AgentError::Tool(format!("sync manifest GET: {e}")))?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(SyncManifest::default());
            }
            let value: Value = response
                .json()
                .await
                .map_err(|e| AgentError::Tool(format!("sync manifest parse: {e}")))?;
            serde_json::from_value(value)
                .map_err(|e| AgentError::Tool(format!("sync manifest shape: {e}")))
        }
    }
}

/// Write the remote manifest.
pub async fn write_manifest(
    cfg: &SyncConfig,
    oauth: &crate::oauth::OAuthConfig,
    home: &Path,
    manifest: &SyncManifest,
) -> Result<()> {
    match manifest_path_for(&cfg.base_url)? {
        ManifestTarget::Dir(dir) => {
            std::fs::create_dir_all(&dir)
                .map_err(|e| AgentError::config(format!("create sync dir: {e}")))?;
            let text = serde_json::to_string_pretty(manifest)
                .map_err(|e| AgentError::config(format!("serialize manifest: {e}")))?;
            std::fs::write(dir.join("manifest.json"), text)
                .map_err(|e| AgentError::config(format!("write manifest: {e}")))
        }
        ManifestTarget::Http(base) => {
            let client = reqwest::Client::new();
            let mut request = client
                .post(format!("{base}/v1/skills-sync/manifest"))
                .timeout(std::time::Duration::from_secs(30))
                .json(manifest);
            if let Some(token) = bearer(cfg, oauth, home).await {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = request
                .send()
                .await
                .map_err(|e| AgentError::Tool(format!("sync manifest POST: {e}")))?;
            if !response.status().is_success() {
                return Err(AgentError::Tool(format!(
                    "sync manifest POST: {}",
                    response.status()
                )));
            }
            Ok(())
        }
    }
}

async fn bearer(
    cfg: &SyncConfig,
    oauth: &crate::oauth::OAuthConfig,
    home: &Path,
) -> Option<String> {
    if !cfg.api_key.trim().is_empty() {
        return Some(cfg.api_key.trim().to_string());
    }
    crate::oauth::access_token(oauth, home).await
}

/// Push every opted-in skill (hermes `sync push`). Returns names pushed.
pub async fn push(
    cfg: &SyncConfig,
    oauth: &crate::oauth::OAuthConfig,
    home: &Path,
) -> Result<Vec<String>> {
    if let Some(reason) = inert_reason(cfg) {
        return Err(AgentError::config(reason));
    }
    let state = load_state(home);
    if state.enabled.is_empty() {
        return Ok(Vec::new());
    }
    let skills_dir = home.join("skills");
    let device = if !state.device_name.trim().is_empty() {
        state.device_name.trim().to_string()
    } else if !cfg.device_name.trim().is_empty() {
        cfg.device_name.trim().to_string()
    } else {
        hostname_fallback()
    };
    let mut manifest = read_manifest(cfg, oauth, home).await.unwrap_or_default();
    let mut pushed = Vec::new();
    for name in &state.enabled {
        let skill = collect_skill(&skills_dir, name, &device)?;
        manifest.skills.insert(name.clone(), skill);
        pushed.push(name.clone());
    }
    write_manifest(cfg, oauth, home, &manifest).await?;
    Ok(pushed)
}

/// Pull remote skills into the local skills dir (hermes `sync pull`).
/// Existing local skills are never clobbered; returns names materialized.
pub async fn pull(
    cfg: &SyncConfig,
    oauth: &crate::oauth::OAuthConfig,
    home: &Path,
) -> Result<Vec<String>> {
    if let Some(reason) = inert_reason(cfg) {
        return Err(AgentError::config(reason));
    }
    let manifest = read_manifest(cfg, oauth, home).await?;
    let skills_dir = home.join("skills");
    std::fs::create_dir_all(&skills_dir)
        .map_err(|e| AgentError::config(format!("create skills dir: {e}")))?;
    let mut materialized = Vec::new();
    for (name, skill) in &manifest.skills {
        let target = skills_dir.join(name);
        if target.exists() {
            continue; // never clobber local state (hermes materialize policy)
        }
        std::fs::create_dir_all(&target)
            .map_err(|e| AgentError::config(format!("create skill dir: {e}")))?;
        for (rel, content) in &skill.files {
            let path = target.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, content)
                .map_err(|e| AgentError::config(format!("write {}: {e}", path.display())))?;
        }
        materialized.push(name.clone());
    }
    Ok(materialized)
}

fn hostname_fallback() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ulnclaw-device".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ulnclaw-sync-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn inert_without_base_url() {
        let cfg = SyncConfig::default();
        assert!(inert_reason(&cfg).is_some());
        let cfg = SyncConfig {
            base_url: "/tmp/somewhere".into(),
            ..Default::default()
        };
        assert!(inert_reason(&cfg).is_none());
    }

    #[test]
    fn stable_device_id_persists() {
        let home = tmp_home("device");
        let first = stable_device_id(&home);
        assert!(!first.is_empty());
        assert_eq!(first, stable_device_id(&home));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn state_opt_in_out() {
        let home = tmp_home("optin");
        let mut state = load_state(&home);
        state.enabled.push("my-skill".into());
        save_state(&home, &state).unwrap();
        assert!(load_state(&home).enabled.contains(&"my-skill".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn collect_skill_captures_files() {
        let home = tmp_home("collect");
        let skills = home.join("skills");
        let skill = skills.join("demo");
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody").unwrap();
        std::fs::write(skill.join("references").join("note.md"), "ref").unwrap();
        let payload = collect_skill(&skills, "demo", "dev1").unwrap();
        assert_eq!(payload.device, "dev1");
        assert!(payload.files.contains_key("SKILL.md"));
        assert!(payload.files.contains_key("references/note.md"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn dir_transport_roundtrip() {
        let home = tmp_home("roundtrip-home");
        let remote = tmp_home("roundtrip-remote");
        // Local skill
        let skills = home.join("skills");
        let skill = skills.join("demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody").unwrap();
        let mut state = load_state(&home);
        state.enabled.push("demo".into());
        save_state(&home, &state).unwrap();

        let cfg = SyncConfig {
            base_url: remote.display().to_string(),
            ..Default::default()
        };
        let oauth = crate::oauth::OAuthConfig::default();
        let pushed = push(&cfg, &oauth, &home).await.unwrap();
        assert_eq!(pushed, vec!["demo".to_string()]);

        // Pull into a fresh home
        let home2 = tmp_home("roundtrip-home2");
        let pulled = pull(&cfg, &oauth, &home2).await.unwrap();
        assert_eq!(pulled, vec!["demo".to_string()]);
        assert!(home2.join("skills/demo/SKILL.md").is_file());
        // Second pull never clobbers
        let pulled = pull(&cfg, &oauth, &home2).await.unwrap();
        assert!(pulled.is_empty());
        for dir in [home, remote, home2] {
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
