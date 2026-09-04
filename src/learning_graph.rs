//! Learning graph assembly — port of hermes `agent/learning_graph.py`
//! (v2026.8.3).
//!
//! The graph is intentionally scoped to what a user actually learns over
//! time:
//! - non-base, learned/profile skills (agent-created or used),
//! - memory entries from `MEMORY.md` / `USER.md` as first-class nodes.
//!
//! Skill links come from declared `related_skills`. Memory-to-skill links
//! are derived from lexical overlap so the graph can answer "which learned
//! skills are connected to the things I remember?".
//!
//! ulnclaw adaptation: profile skills live flat under `<home>/skills`,
//! memory entries are `- ` bullets (the memory tool's format), and there
//! is no bundled base-skill repo — the `source` field stays for payload
//! parity.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct SkillNode {
    pub name: String,
    pub category: String,
    pub source: String,
    pub timestamp: Option<i64>,
    pub use_count: u64,
    pub state: String,
    pub created_by: Option<String>,
    pub pinned: bool,
    pub related: Vec<String>,
}

const EXCLUDED_DIRS: &[&str] = &[".archive", ".hub", "node_modules", ".git"];

// ---------------------------------------------------------------------------
// Frontmatter (name / category / related_skills)
// ---------------------------------------------------------------------------

fn frontmatter_block(content: &str) -> Option<&str> {
    let trimmed = content.trim_start();
    let rest = trimmed.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn fm_value(block: &str, key: &str) -> Option<String> {
    for line in block.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == key {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn fm_related(block: &str) -> Vec<String> {
    let Some(raw) = fm_value(block, "related_skills") else {
        return Vec::new();
    };
    let inner = raw
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(&raw);
    inner
        .split(',')
        .map(|item| {
            item.trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string()
        })
        .filter(|item| !item.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// Number-or-ISO-string → unix seconds (hermes `_to_int_ts`).
pub fn to_int_ts(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(n) = s.parse::<f64>() {
                return Some(n as i64);
            }
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.timestamp())
        }
        _ => None,
    }
}

/// Newest activity timestamp from a usage record (hermes
/// `_usage_timestamp`).
fn usage_timestamp(record: &Value) -> Option<i64> {
    for key in [
        "last_activity_at",
        "last_used_at",
        "last_viewed_at",
        "last_patched_at",
        "created_at",
    ] {
        if let Some(ts) = record.get(key).and_then(to_int_ts) {
            return Some(ts);
        }
    }
    None
}

fn file_mtime(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// Skill nodes
// ---------------------------------------------------------------------------

fn iter_skill_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !EXCLUDED_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push(path);
                }
            } else if name == "SKILL.md" {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Build skill nodes from `(source, root)` pairs (hermes
/// `build_skill_nodes`).
pub fn build_skill_nodes(home: &Path, skill_roots: &[(&str, PathBuf)]) -> Vec<SkillNode> {
    let usage = crate::skill_usage::load_usage(home);
    let mut nodes: Vec<SkillNode> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (source, root) in skill_roots {
        for skill_md in iter_skill_files(root) {
            let Ok(raw) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let head: String = raw.chars().take(4000).collect();
            let block = frontmatter_block(&head);
            let dir_name = skill_md
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let name = block
                .and_then(|b| fm_value(b, "name"))
                .unwrap_or(dir_name)
                .trim()
                .to_string();
            if name.is_empty() || seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            let record = usage.get(&name).cloned().unwrap_or(json!({}));
            let last_activity = usage_timestamp(&record);
            let file_ts = file_mtime(&skill_md);
            let category = block
                .and_then(|b| fm_value(b, "category"))
                .unwrap_or_else(|| "general".to_string());
            let related = block.map(fm_related).unwrap_or_default();
            nodes.push(SkillNode {
                name,
                category,
                source: source.to_string(),
                timestamp: last_activity.or(file_ts),
                use_count: record
                    .get("use_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                state: record
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("active")
                    .to_string(),
                created_by: record
                    .get("created_by")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                pinned: record
                    .get("pinned")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                related,
            });
        }
    }
    nodes
}

/// Undirected `related_skills` edges where BOTH endpoints exist (deduped) —
/// hermes `build_edges`.
pub fn build_edges(nodes: &[SkillNode]) -> Vec<(String, String)> {
    let names: std::collections::HashSet<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut edges = Vec::new();
    for node in nodes {
        for target in &node.related {
            if names.contains(target.as_str()) && target != &node.name {
                let (a, b) = if node.name < *target {
                    (node.name.clone(), target.clone())
                } else {
                    (target.clone(), node.name.clone())
                };
                if seen.insert((a.clone(), b.clone())) {
                    edges.push((a, b));
                }
            }
        }
    }
    edges
}

/// Graph density stats (hermes `density_stats`).
pub fn density_stats(nodes: &[SkillNode], edges: &[(String, String)]) -> Value {
    let mut linked: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (a, b) in edges {
        linked.insert(a);
        linked.insert(b);
    }
    let mut cats: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for node in nodes {
        *cats.entry(node.category.as_str()).or_insert(0) += 1;
    }
    let n = nodes.len().max(1) as f64;
    let mut top: Vec<(&&str, &usize)> = cats.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    json!({
        "nodes": nodes.len(),
        "related_edges": edges.len(),
        "edges_per_node": (edges.len() as f64 / n * 1000.0).round() / 1000.0,
        "linked_nodes": linked.len(),
        "isolated_pct": (100.0 * (nodes.len() as f64 - linked.len() as f64) / n * 10.0).round() / 10.0,
        "categories": cats.len(),
        "agent_created": nodes.iter().filter(|x| x.created_by.as_deref() == Some("agent")).count(),
        "used": nodes.iter().filter(|x| x.use_count > 0).count(),
        "top_categories": top.iter().take(8).map(|(cat, count)| json!([cat, count])).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Memory cards
// ---------------------------------------------------------------------------

fn memory_dir(home: &Path) -> PathBuf {
    home.join("memory")
}

/// Memory entries as readable cards (hermes `_memory_cards`, adapted to the
/// bullet-entry memory format). Each `- ` entry becomes one card; every
/// entry is surfaced — the graph shows everything.
pub fn memory_cards(home: &Path) -> Vec<Value> {
    let mut cards: Vec<Value> = Vec::new();
    for (fname, source) in [("MEMORY.md", "memory"), ("USER.md", "profile")] {
        let path = memory_dir(home).join(fname);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let file_ts = file_mtime(&path);
        for (idx, entry) in crate::tools::builtin::memory::read_entries(&text)
            .iter()
            .enumerate()
        {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let first = entry
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches('#')
                .trim()
                .to_string();
            let title: String = if first.chars().count() > 80 {
                first.chars().take(80).collect::<String>() + "…"
            } else {
                first
            };
            let body: String = entry.chars().take(1200).collect();
            cards.push(json!({
                "source": source,
                "timestamp": file_ts.map(|ts| ts + idx as i64),
                "title": title,
                "body": body,
            }));
        }
    }
    cards
}

fn tokenize(text: &str) -> std::collections::HashSet<String> {
    let lower = text.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_string())
        .collect()
}

/// Lexical-overlap memory→skill edges, top-4 per card (hermes
/// `_memory_skill_edges`).
pub fn memory_skill_edges(cards: &[Value], skills: &[SkillNode]) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    let skill_meta: Vec<(&SkillNode, std::collections::HashSet<String>, String)> = skills
        .iter()
        .map(|s| (s, tokenize(&s.name), s.name.to_ascii_lowercase()))
        .collect();
    for (idx, card) in cards.iter().enumerate() {
        let source = card
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("memory");
        let mem_id = format!("memory:{}:{}", source, idx);
        let text = format!(
            "{}\n{}",
            card.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            card.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        )
        .to_ascii_lowercase();
        let text_tokens = tokenize(&text);
        let mut scored: Vec<(i64, &str)> = Vec::new();
        for (skill, tokens, name_lower) in &skill_meta {
            let mut score: i64 = 0;
            if text.contains(name_lower.as_str()) {
                score += 6;
            }
            score += tokens.intersection(&text_tokens).count() as i64;
            if score > 0 {
                scored.push((score, skill.name.as_str()));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        for (_, name) in scored.into_iter().take(4) {
            edges.push((mem_id.clone(), name.to_string()));
        }
    }
    edges
}

// ---------------------------------------------------------------------------
// Full payload
// ---------------------------------------------------------------------------

fn skill_roots(home: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![("profile", home.join("skills"))]
}

/// Full payload for the learning panel (hermes `build_learning_graph`).
///
/// Focus on what is profile-learned and actionable: skills that are NOT
/// base-installed and show real learning signal (agent-created or used),
/// plus memory entries as first-class graph nodes connected to those
/// learned skills.
pub fn build_learning_graph(home: &Path) -> Value {
    let all_skills = build_skill_nodes(home, &skill_roots(home));
    let learned: Vec<SkillNode> = all_skills
        .into_iter()
        .filter(|n| {
            n.source != "base" && (n.created_by.as_deref() == Some("agent") || n.use_count > 0)
        })
        .collect();
    let skill_edges = build_edges(&learned);
    let cards = memory_cards(home);
    let mem_edges = memory_skill_edges(&cards, &learned);

    let mut clusters: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for node in &learned {
        *clusters.entry(node.category.clone()).or_insert(0) += 1;
    }
    if !cards.is_empty() {
        clusters.insert("memory".to_string(), cards.len());
    }

    let mut graph_nodes: Vec<Value> = learned
        .iter()
        .map(|n| {
            json!({
                "id": n.name,
                "label": n.name,
                "kind": "skill",
                "timestamp": n.timestamp,
                "category": n.category,
                "useCount": n.use_count,
                "state": n.state,
                "createdBy": n.created_by,
                "pinned": n.pinned,
            })
        })
        .collect();
    for (i, card) in cards.iter().enumerate() {
        graph_nodes.push(json!({
            "id": format!("memory:{}:{}", card["source"].as_str().unwrap_or("memory"), i),
            "label": card["title"],
            "kind": "memory",
            "memorySource": card["source"],
            "timestamp": card["timestamp"],
            "category": "memory",
            "useCount": 0,
            "state": "active",
            "createdBy": "memory",
            "pinned": false,
        }));
    }

    let mut edges: Vec<(String, String)> = skill_edges.clone();
    edges.extend(mem_edges.iter().cloned());

    let mut stats = density_stats(&learned, &skill_edges);
    if let Some(obj) = stats.as_object_mut() {
        obj.insert("memory_nodes".into(), json!(cards.len()));
        obj.insert("memory_skill_edges".into(), json!(mem_edges.len()));
        obj.insert("learned_skills".into(), json!(learned.len()));
    }

    let mut cluster_list: Vec<(&String, &usize)> = clusters.iter().collect();
    cluster_list.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));

    json!({
        "nodes": graph_nodes,
        "edges": edges.iter().map(|(a, b)| json!({"source": a, "target": b})).collect::<Vec<_>>(),
        "clusters": cluster_list.iter().map(|(c, n)| json!({"category": c, "count": n})).collect::<Vec<_>>(),
        "memory": cards,
        "stats": stats,
    })
}

/// Chronological learning-journey digest (P667 — the `/journey` slash,
/// hermes "open the learning journey timeline" in text form). Pure over
/// the [`build_learning_graph`] payload: newest `limit` events, one per
/// line, skills and memories intermixed.
pub fn format_journey_digest(payload: &Value, limit: usize) -> String {
    let nodes = payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if nodes.is_empty() {
        return "(o_o) no learning yet — skills you teach the agent and approved memory writes land here.\n"
            .to_string();
    }
    let skills = nodes
        .iter()
        .filter(|n| n.get("kind").and_then(|v| v.as_str()) == Some("skill"))
        .count();
    let memories = nodes.len() - skills;

    let mut sorted = nodes.clone();
    sorted.sort_by_key(|n| n.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0));
    let recent: Vec<&Value> = sorted.iter().rev().take(limit).collect();

    let mut out = String::new();
    out.push_str(&format!(
        "learning journey: {} skill(s), {} memory card(s) — newest first:\n",
        skills, memories
    ));
    for node in recent {
        let kind = node.get("kind").and_then(|v| v.as_str()).unwrap_or("skill");
        let glyph = if kind == "memory" {
            "\u{25c6}"
        } else {
            "\u{2726}"
        };
        let label = node.get("label").and_then(|v| v.as_str()).unwrap_or("?");
        let date = crate::learning_graph_render::format_date(
            node.get("timestamp").and_then(|v| v.as_f64()),
        );
        out.push_str(&format!("  {date}  {glyph} {label}\n"));
    }
    out
}

/// Convenience wrapper: build the graph from `home` and format it.
pub fn journey_digest(home: &Path, limit: usize) -> String {
    format_journey_digest(&build_learning_graph(home), limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    static HOME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_home() -> PathBuf {
        let n = HOME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-learning-graph-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        dir
    }

    fn make_skill(home: &Path, name: &str, frontmatter: &str) {
        let dir = home.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("{}\n\nbody\n", frontmatter)).unwrap();
    }

    fn seed_usage(home: &Path, name: &str, record: Value) {
        let mut data = crate::skill_usage::load_usage(home);
        data.insert(name.to_string(), record);
        crate::skill_usage::save_usage(home, &data);
    }

    #[test]
    fn skill_nodes_with_usage_and_related() {
        let home = temp_home();
        make_skill(
            &home,
            "alpha",
            "---\nname: alpha\ndescription: a\ncategory: devops\nrelated_skills: [beta]\n---",
        );
        make_skill(&home, "beta", "---\nname: beta\ndescription: b\n---");
        make_skill(&home, "ghost", "---\nname: ghost\ndescription: g\n---");
        seed_usage(
            &home,
            "alpha",
            json!({"created_by": "agent", "use_count": 3, "state": "active", "last_used_at": "2026-08-01T00:00:00+00:00"}),
        );
        seed_usage(&home, "beta", json!({"use_count": 1}));

        let roots = vec![("profile", home.join("skills"))];
        let nodes = build_skill_nodes(&home, &roots);
        assert_eq!(nodes.len(), 3);
        let alpha = nodes.iter().find(|n| n.name == "alpha").unwrap();
        assert_eq!(alpha.category, "devops");
        assert_eq!(alpha.created_by.as_deref(), Some("agent"));
        assert_eq!(alpha.use_count, 3);
        assert_eq!(alpha.related, vec!["beta"]);
        assert!(alpha.timestamp.is_some());

        // .archive is excluded from the walk.
        std::fs::create_dir_all(home.join("skills/.archive/old")).unwrap();
        std::fs::write(
            home.join("skills/.archive/old/SKILL.md"),
            "---\nname: old\n---\n",
        )
        .unwrap();
        let nodes = build_skill_nodes(&home, &roots);
        assert_eq!(nodes.len(), 3);

        // Edges: alpha<->beta exists; ghost is isolated.
        let edges = build_edges(&nodes);
        assert_eq!(edges, vec![("alpha".to_string(), "beta".to_string())]);

        let stats = density_stats(&nodes, &edges);
        assert_eq!(stats["nodes"], 3);
        assert_eq!(stats["agent_created"], 1);
        assert_eq!(stats["used"], 2);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn memory_cards_from_bullets() {
        let home = temp_home();
        std::fs::write(
            home.join("memory/MEMORY.md"),
            "- user prefers concise answers\n- deploy freezes on Fridays\n",
        )
        .unwrap();
        std::fs::write(home.join("memory/USER.md"), "- works on ulnclaw\n").unwrap();
        let cards = memory_cards(&home);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0]["source"], "memory");
        assert_eq!(cards[0]["title"], "user prefers concise answers");
        assert_eq!(cards[2]["source"], "profile");
        assert!(cards[0]["timestamp"].is_number());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn memory_skill_edges_lexical() {
        let home = temp_home();
        let skills = vec![
            SkillNode {
                name: "deploy-freeze".into(),
                category: "ops".into(),
                source: "profile".into(),
                timestamp: None,
                use_count: 1,
                state: "active".into(),
                created_by: None,
                pinned: false,
                related: vec![],
            },
            SkillNode {
                name: "unrelated-thing".into(),
                category: "x".into(),
                source: "profile".into(),
                timestamp: None,
                use_count: 1,
                state: "active".into(),
                created_by: None,
                pinned: false,
                related: vec![],
            },
        ];
        let cards = vec![json!({
            "source": "memory",
            "title": "deploy freezes on Fridays",
            "body": "deploy freezes on Fridays",
        })];
        let edges = memory_skill_edges(&cards, &skills);
        // "deploy" + "freezes" overlap beats nothing; the deploy-freeze name
        // tokens overlap too.
        assert!(edges.iter().any(|(_, s)| s == "deploy-freeze"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn full_payload_shape() {
        let home = temp_home();
        make_skill(&home, "alpha", "---\nname: alpha\ncategory: devops\n---");
        make_skill(&home, "idle", "---\nname: idle\n---");
        seed_usage(
            &home,
            "alpha",
            json!({"created_by": "agent", "use_count": 2}),
        );
        std::fs::write(home.join("memory/MEMORY.md"), "- alpha rollout notes\n").unwrap();

        let payload = build_learning_graph(&home);
        let node_ids: Vec<&str> = payload["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        // alpha is learned (agent-created); idle has no learning signal.
        assert!(node_ids.contains(&"alpha"));
        assert!(!node_ids.contains(&"idle"));
        assert!(node_ids.contains(&"memory:memory:0"));
        assert_eq!(payload["stats"]["learned_skills"], 1);
        assert_eq!(payload["stats"]["memory_nodes"], 1);
        assert!(payload["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| { e["source"] == "memory:memory:0" && e["target"] == "alpha" }));
        // Count tie — insertion order (skill category first, then memory).
        assert_eq!(payload["clusters"][0]["category"], "devops");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn to_int_ts_variants() {
        assert_eq!(to_int_ts(&json!(1720000000)), Some(1720000000));
        assert_eq!(
            to_int_ts(&json!("2026-08-01T00:00:00+00:00")),
            Some(1785542400)
        );
        assert_eq!(to_int_ts(&json!("1720000000")), Some(1720000000));
        assert_eq!(to_int_ts(&json!(null)), None);
        assert_eq!(to_int_ts(&json!("garbage")), None);
    }

    #[test]
    fn journey_digest_empty_and_populated() {
        let empty = serde_json::json!({"nodes": []});
        let out = format_journey_digest(&empty, 10);
        assert!(out.contains("no learning yet"), "{out}");

        let payload = serde_json::json!({"nodes": [
            {"id": "git-helper", "label": "git-helper", "kind": "skill", "timestamp": 1_700_000_000},
            {"id": "memory:notes:0", "label": "prefers tabs", "kind": "memory", "timestamp": 1_750_000_000},
            {"id": "old-skill", "label": "old-skill", "kind": "skill", "timestamp": 1_600_000_000},
        ]});
        let out = format_journey_digest(&payload, 2);
        assert!(out.contains("2 skill(s), 1 memory card(s)"), "{out}");
        // Newest first, limited to 2 entries.
        let first = out.lines().nth(1).unwrap();
        assert!(first.contains("prefers tabs"), "{out}");
        assert!(!out.contains("old-skill"), "{out}");
        assert!(out.contains("\u{25c6}"), "{out}");
        assert!(out.contains("\u{2726}"), "{out}");
    }
}
