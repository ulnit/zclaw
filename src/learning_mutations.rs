//! Journey node mutations — port of hermes `agent/learning_mutations.py`
//! (v2026.8.3).
//!
//! The journey graph (`learning_graph`) gives every node a stable id:
//!
//! - **skills** → the skill name (e.g. `"debugging-hermes-desktop"`)
//! - **memories** → `memory:<source>:<index>` where `source` is `memory`
//!   (`MEMORY.md`) or `profile` (`USER.md`) and `index` is the node's
//!   position in the combined card list (`MEMORY.md` cards first, then
//!   `USER.md`).
//!
//! This module maps a node id back to its on-disk home and performs the
//! mutation. Deleting a skill *archives* it (recoverable via
//! `skill_usage::restore_skill`); deleting a memory rewrites its file.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const MEMORY_FILES: &[(&str, &str)] = &[("memory", "MEMORY.md"), ("profile", "USER.md")];

pub fn parse_node_kind(node_id: &str) -> &'static str {
    if node_id.starts_with("memory:") {
        "memory"
    } else {
        "skill"
    }
}

fn memory_file_name(source: &str) -> Option<&'static str> {
    MEMORY_FILES
        .iter()
        .find(|(key, _)| *key == source)
        .map(|(_, file)| *file)
}

fn parse_memory_id(node_id: &str) -> Result<(String, usize), String> {
    let parts: Vec<&str> = node_id.splitn(3, ':').collect();
    if parts.len() != 3 || parts[0] != "memory" || memory_file_name(parts[1]).is_none() {
        return Err(format!("bad memory node id: '{}'", node_id));
    }
    let index: usize = parts[2]
        .parse()
        .map_err(|_| format!("bad memory node id: '{}'", node_id))?;
    Ok((parts[1].to_string(), index))
}

/// Global card index → position within the source's own file.
///
/// `memory_cards` emits all `MEMORY.md` cards before `USER.md` cards, so a
/// profile card's local index is its global index minus the memory count.
fn memory_local_index(home: &Path, source: &str, global_index: usize) -> Result<usize, String> {
    let cards = crate::learning_graph::memory_cards(home);
    if global_index >= cards.len() {
        return Err(format!("memory index {} out of range", global_index));
    }
    let card_source = cards[global_index]
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if card_source != source {
        return Err("memory node id is stale — refresh the graph".to_string());
    }
    if source == "memory" {
        return Ok(global_index);
    }
    let memory_count = cards
        .iter()
        .filter(|c| c.get("source").and_then(|v| v.as_str()) == Some("memory"))
        .count();
    Ok(global_index - memory_count)
}

/// Resolve a memory card to its file, all entries, and local index.
///
/// Entries come from the memory tool's parser so journey indices stay
/// aligned with what the graph renders.
fn locate_memory(
    home: &Path,
    source: &str,
    gidx: usize,
) -> Result<(PathBuf, Vec<String>, usize), String> {
    let file = memory_file_name(source).expect("validated source");
    let path = home.join("memory").join(file);
    if !path.exists() {
        return Err(format!("{} not found", file));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let chunks = crate::tools::builtin::memory::read_entries(&content);
    let local = memory_local_index(home, source, gidx)?;
    if local >= chunks.len() {
        return Err("memory node id is stale — refresh the graph".to_string());
    }
    Ok((path, chunks, local))
}

fn write_memory(path: &Path, chunks: &[String]) -> Result<(), String> {
    let kept: Vec<String> = chunks
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let content = crate::tools::builtin::memory::entries_to_content(&kept);
    std::fs::write(path, content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Inspect (edit prefill)
// ---------------------------------------------------------------------------

/// Current content for an edit prefill. `content` is the full SKILL.md
/// (skills) or the raw memory entry (memories).
pub fn node_detail(home: &Path, node_id: &str) -> Value {
    match parse_node_kind(node_id) {
        "memory" => memory_detail(home, node_id),
        _ => skill_detail(home, node_id),
    }
}

fn memory_detail(home: &Path, node_id: &str) -> Value {
    let result = (|| -> Result<Value, String> {
        let (source, gidx) = parse_memory_id(node_id)?;
        let (_, chunks, local) = locate_memory(home, &source, gidx)?;
        let body = chunks[local].trim().to_string();
        let label: String = body.lines().next().unwrap_or("").chars().take(80).collect();
        Ok(json!({"ok": true, "kind": "memory", "id": node_id, "label": label, "content": body}))
    })();
    result.unwrap_or_else(|message| json!({"ok": false, "message": message}))
}

fn skill_detail(home: &Path, node_id: &str) -> Value {
    let skills_dir = home.join("skills");
    let Some(skill) = crate::skills::find_skill(&skills_dir, node_id) else {
        return json!({"ok": false, "message": format!("skill '{}' not found", node_id)});
    };
    let skill_md = skill.path.join("SKILL.md");
    let Ok(content) = std::fs::read_to_string(&skill_md) else {
        return json!({"ok": false, "message": format!("SKILL.md missing for '{}'", node_id)});
    };
    json!({"ok": true, "kind": "skill", "id": node_id, "label": node_id, "content": content})
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Delete a node: skills are archived (pinned skills refused), memories are
/// removed from their file.
pub fn delete_node(home: &Path, node_id: &str) -> Value {
    let result = match parse_node_kind(node_id) {
        "memory" => delete_memory(home, node_id),
        _ => delete_skill(home, node_id),
    };
    result.unwrap_or_else(|message| json!({"ok": false, "message": message}))
}

fn delete_skill(home: &Path, name: &str) -> Result<Value, String> {
    let (ok, message) = crate::skill_usage::archive_skill(home, name);
    if ok {
        Ok(json!({
            "ok": true,
            "message": format!("archived '{}' — {}", name, message),
        }))
    } else {
        Ok(json!({"ok": false, "message": message}))
    }
}

fn delete_memory(home: &Path, node_id: &str) -> Result<Value, String> {
    let (source, gidx) = parse_memory_id(node_id)?;
    let (path, mut chunks, local) = locate_memory(home, &source, gidx)?;
    chunks.remove(local);
    write_memory(&path, &chunks)?;
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(json!({"ok": true, "message": format!("deleted memory from {}", file)}))
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

/// Replace a node's content: full SKILL.md rewrite (skills) or entry
/// replacement (memories).
pub fn edit_node(home: &Path, node_id: &str, content: &str) -> Value {
    let result = match parse_node_kind(node_id) {
        "memory" => edit_memory(home, node_id, content),
        _ => edit_skill(home, node_id, content),
    };
    result.unwrap_or_else(|message| json!({"ok": false, "message": message}))
}

fn edit_skill(home: &Path, name: &str, content: &str) -> Result<Value, String> {
    let skills_dir = home.join("skills");
    let Some(skill) = crate::skills::find_skill(&skills_dir, name) else {
        return Ok(json!({"ok": false, "message": format!("skill '{}' not found", name)}));
    };
    std::fs::write(skill.path.join("SKILL.md"), content).map_err(|e| e.to_string())?;
    crate::skill_usage::bump_patch(home, &skill.name);
    Ok(json!({"ok": true, "message": format!("updated '{}'", name)}))
}

fn edit_memory(home: &Path, node_id: &str, content: &str) -> Result<Value, String> {
    let (source, gidx) = parse_memory_id(node_id)?;
    let body = content.trim().to_string();
    if body.is_empty() {
        return Ok(json!({"ok": false, "message": "empty memory — use delete to remove it"}));
    }
    let (path, mut chunks, local) = locate_memory(home, &source, gidx)?;
    chunks[local] = body;
    write_memory(&path, &chunks)?;
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(json!({"ok": true, "message": format!("updated memory in {}", file)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    static HOME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_home() -> PathBuf {
        let n = HOME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-learning-mutations-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        dir
    }

    fn make_skill(home: &Path, name: &str) {
        let dir = home.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {}\ndescription: test\n---\n\nbody\n", name),
        )
        .unwrap();
    }

    #[test]
    fn node_kind_and_bad_ids() {
        assert_eq!(parse_node_kind("memory:memory:0"), "memory");
        assert_eq!(parse_node_kind("some-skill"), "skill");
        let detail = node_detail(&std::env::temp_dir(), "memory:bogus:0");
        assert_eq!(detail["ok"], false);
        let detail = node_detail(&std::env::temp_dir(), "memory:memory:x");
        assert_eq!(detail["ok"], false);
    }

    #[test]
    fn skill_detail_edit_delete() {
        let home = temp_home();
        make_skill(&home, "demo");

        let detail = node_detail(&home, "demo");
        assert_eq!(detail["ok"], true);
        assert_eq!(detail["kind"], "skill");
        assert!(detail["content"].as_str().unwrap().contains("body"));

        // Edit rewrites SKILL.md and bumps patch telemetry.
        let edited = edit_node(&home, "demo", "---\nname: demo\n---\nnew body");
        assert_eq!(edited["ok"], true);
        let content = std::fs::read_to_string(home.join("skills/demo/SKILL.md")).unwrap();
        assert!(content.contains("new body"));
        assert_eq!(
            crate::skill_usage::get_record(&home, "demo")["patch_count"],
            1
        );

        // Delete archives (skill dir disappears, archive holds it).
        let deleted = delete_node(&home, "demo");
        assert_eq!(deleted["ok"], true);
        assert!(!home.join("skills/demo").exists());
        assert!(home.join("skills/.archive/demo").exists());

        // Missing skill reports cleanly.
        assert_eq!(node_detail(&home, "ghost")["ok"], false);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn pinned_skill_delete_refused() {
        let home = temp_home();
        make_skill(&home, "demo");
        crate::skill_usage::set_pinned(&home, "demo", true);
        let deleted = delete_node(&home, "demo");
        assert_eq!(deleted["ok"], false);
        assert!(home.join("skills/demo").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn memory_detail_edit_delete() {
        let home = temp_home();
        std::fs::write(
            home.join("memory/MEMORY.md"),
            "- first memory\n- second memory\n",
        )
        .unwrap();
        std::fs::write(home.join("memory/USER.md"), "- profile note\n").unwrap();

        // Global indices: memory:0, memory:1, then profile:2.
        let detail = node_detail(&home, "memory:memory:1");
        assert_eq!(detail["ok"], true);
        assert_eq!(detail["content"], "second memory");
        let detail = node_detail(&home, "memory:profile:2");
        assert_eq!(detail["content"], "profile note");

        // Edit replaces the entry in place.
        let edited = edit_node(&home, "memory:memory:0", "first memory (updated)");
        assert_eq!(edited["ok"], true);
        let content = std::fs::read_to_string(home.join("memory/MEMORY.md")).unwrap();
        assert!(content.contains("- first memory (updated)"));

        // Empty edit is refused.
        let edited = edit_node(&home, "memory:memory:0", "   ");
        assert_eq!(edited["ok"], false);

        // Delete removes the entry; indices shift afterwards (stale id guard).
        let deleted = delete_node(&home, "memory:memory:0");
        assert_eq!(deleted["ok"], true);
        let content = std::fs::read_to_string(home.join("memory/MEMORY.md")).unwrap();
        assert!(!content.contains("updated"));
        // Index 2 now belongs to the profile card; the old memory:memory:2
        // id is stale (only one memory card left).
        let stale = delete_node(&home, "memory:memory:2");
        assert_eq!(stale["ok"], false);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
