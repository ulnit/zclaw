//! Toolsets — port of hermes' toolsets.py
//!
//! Toolsets group tools for specific scenarios and can compose other
//! toolsets. `resolve_toolset` expands a name into the full tool list.

use std::collections::HashSet;

/// Static toolset definitions (tools + includes). Mirrors hermes TOOLSETS.
pub struct ToolsetDef {
    pub description: &'static str,
    pub tools: &'static [&'static str],
    pub includes: &'static [&'static str],
}

pub fn toolsets() -> &'static std::collections::HashMap<&'static str, ToolsetDef> {
    use std::collections::HashMap;
    static MAP: std::sync::OnceLock<HashMap<&'static str, ToolsetDef>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut map: HashMap<&str, ToolsetDef> = HashMap::new();
        map.insert("web", ToolsetDef {
            description: "Web research and content extraction tools",
            tools: &["web_search", "web_extract"],
            includes: &[],
        });
        map.insert("search", ToolsetDef {
            description: "Web search only (no content extraction)",
            tools: &["web_search"],
            includes: &[],
        });
        map.insert("x_search", ToolsetDef {
            description: "Search X (Twitter) posts and threads via xAI's built-in x_search                           Responses tool. Read-only public X discovery. Available when xAI                           credentials are configured (XAI_API_KEY). Off by default: add                           x_search to enabled_toolsets (hermes parity).",
            tools: &["x_search"],
            includes: &[],
        });
        map.insert("vision", ToolsetDef {
            description: "Image analysis and vision tools",
            tools: &["vision_analyze"],
            includes: &[],
        });
        map.insert("video", ToolsetDef {
            description: "Video analysis and understanding tools (opt-in, not in default toolset)",
            tools: &["video_analyze"],
            includes: &[],
        });
        map.insert("image_gen", ToolsetDef {
            description: "Creative generation tools (images)",
            tools: &["image_generate"],
            includes: &[],
        });
        map.insert("video_gen", ToolsetDef {
            description:
                "Video generation tools. Single video_generate tool covers text-to-video \
                 (prompt only), image-to-video (prompt + image_url), and reference-to-video. \
                 Provider-specific edit/extend tools ship alongside (hermes video_gen)",
            tools: &["video_generate", "xai_video_edit", "xai_video_extend"],
            includes: &[],
        });
        map.insert("bfl", ToolsetDef {
            description:
                "Black Forest Labs FLUX 3 video generation through the Nous tool gateway: \
                 per-mode submit tools (text, image, keyframes, continuation), a poll tool, \
                 and a prompting guide. Generations take minutes, so submit returns a job id \
                 and the model polls for the result.",
            tools: &[
                "bfl_flux3_text_to_video",
                "bfl_flux3_image_to_video",
                "bfl_flux3_keyframes_to_video",
                "bfl_flux3_video_continuation",
                "bfl_flux3_get_result",
                "bfl_flux3_prompting_guide",
            ],
            includes: &[],
        });
        map.insert("computer_use", ToolsetDef {
            description: "Desktop control via a computer-use driver",
            tools: &["computer_use"],
            includes: &[],
        });
        map.insert("terminal", ToolsetDef {
            description: "Shell command execution and background process management",
            tools: &["terminal", "process"],
            includes: &[],
        });
        map.insert("skills", ToolsetDef {
            description: "Skill discovery, viewing, and management",
            tools: &["skills_list", "skill_view", "skill_manage"],
            includes: &[],
        });
        map.insert("browser", ToolsetDef {
            description: "Browser automation (CDP-backed)",
            tools: &[
                "browser_navigate", "browser_snapshot", "browser_click", "browser_type",
                "browser_scroll", "browser_back", "browser_press", "browser_get_images",
                "browser_vision", "browser_console", "browser_cdp", "browser_dialog",
                "web_search",
            ],
            includes: &[],
        });
        map.insert("cronjob", ToolsetDef {
            description: "Scheduled job management",
            tools: &["cronjob"],
            includes: &[],
        });
        map.insert("file", ToolsetDef {
            description: "File read/write/patch/search",
            tools: &["read_file", "write_file", "patch", "search_files"],
            includes: &[],
        });
        map.insert("tts", ToolsetDef {
            description: "Text-to-speech synthesis",
            tools: &["text_to_speech"],
            includes: &[],
        });
        map.insert("stt", ToolsetDef {
            description:
                "Speech-to-text transcription (voice notes, recordings). Opt-in: add stt \
                 to enabled_toolsets; gateway voice messages are transcribed via [stt] \
                 config regardless of toolsets (hermes parity)",
            tools: &["transcribe_audio"],
            includes: &[],
        });
        map.insert("todo", ToolsetDef {
            description: "Session task list",
            tools: &["todo"],
            includes: &[],
        });
        map.insert("memory", ToolsetDef {
            description: "Persistent memory across sessions",
            tools: &["memory"],
            includes: &[],
        });
        map.insert("session_search", ToolsetDef {
            description: "Search past session history",
            tools: &["session_search"],
            includes: &[],
        });
        map.insert("clarify", ToolsetDef {
            description: "Ask the user clarifying questions",
            tools: &["clarify"],
            includes: &[],
        });
        map.insert("code_execution", ToolsetDef {
            description: "Python code execution sandbox",
            tools: &["execute_code"],
            includes: &[],
        });
        map.insert("delegation", ToolsetDef {
            description: "Spawn sub-agents for parallel work",
            tools: &["delegate_task"],
            includes: &[],
        });
        map.insert("homeassistant", ToolsetDef {
            description: "Home Assistant smart home control",
            tools: &["ha_list_entities", "ha_get_state", "ha_list_services", "ha_call_service"],
            includes: &[],
        });
        map.insert("kanban", ToolsetDef {
            description: "Kanban multi-agent coordination board",
            tools: &[
                "kanban_show", "kanban_list", "kanban_complete", "kanban_block",
                "kanban_heartbeat", "kanban_comment", "kanban_create", "kanban_link",
                "kanban_unblock", "kanban_attach", "kanban_attach_url", "kanban_attachments",
            ],
            includes: &[],
        });
        map.insert("project", ToolsetDef {
            description: "Desktop Projects — create/switch named workspaces (GUI sessions only)",
            tools: &["project_list", "project_create", "project_switch"],
            includes: &[],
        });
        map.insert("discord", ToolsetDef {
            description: "Discord messaging",
            tools: &["discord"],
            includes: &[],
        });
        map.insert("discord_admin", ToolsetDef {
            description: "Discord server administration",
            tools: &["discord_admin"],
            includes: &[],
        });
        map.insert("feishu_doc", ToolsetDef {
            description: "Feishu/Lark document reading",
            tools: &["feishu_doc_read"],
            includes: &[],
        });
        map.insert("spotify", ToolsetDef {
            description: "Spotify playback control",
            tools: &["spotify_playback"],
            includes: &[],
        });
        map.insert("debugging", ToolsetDef {
            description: "Terminal + web + file tools for debugging sessions",
            tools: &["terminal", "process"],
            includes: &["web", "file"],
        });
        map.insert("safe", ToolsetDef {
            description: "Read-only research set (no shell/file writes)",
            tools: &[],
            includes: &["web", "vision", "image_gen"],
        });
        map.insert("coding", ToolsetDef {
            description: "The default coding-agent toolset (hermes _HERMES_CORE_TOOLS)",
            tools: &[
                "web_search", "web_extract",
                "terminal", "process",
                "read_file", "write_file", "patch", "search_files",
                "vision_analyze", "image_generate",
                "skills_list", "skill_view", "skill_manage",
                "browser_navigate", "browser_snapshot", "browser_click", "browser_type",
                "browser_scroll", "browser_back", "browser_press", "browser_get_images",
                "browser_vision", "browser_console", "browser_cdp", "browser_dialog",
                "text_to_speech",
                "todo", "memory", "session_search",
                "execute_code", "delegate_task", "cronjob",
                "ha_list_entities", "ha_get_state", "ha_list_services", "ha_call_service",
                "kanban_show", "kanban_list", "kanban_complete", "kanban_block",
                "kanban_heartbeat", "kanban_comment", "kanban_create", "kanban_link",
                "kanban_unblock", "kanban_attach", "kanban_attach_url", "kanban_attachments",
                "computer_use",
                "bfl_flux3_text_to_video", "bfl_flux3_image_to_video",
                "bfl_flux3_keyframes_to_video", "bfl_flux3_video_continuation",
                "bfl_flux3_get_result", "bfl_flux3_prompting_guide",
            ],
            includes: &["clarify"],
        });
        map
    })
}

/// Resolve a toolset name into the full set of tool names (expanding
/// `includes` recursively, cycle-safe).
pub fn resolve_toolset(name: &str) -> Vec<String> {
    let map = toolsets();
    let mut out = Vec::new();
    let mut seen_sets: HashSet<String> = HashSet::new();
    let mut stack = vec![name.to_string()];
    while let Some(current) = stack.pop() {
        if !seen_sets.insert(current.clone()) {
            continue;
        }
        if let Some(def) = map.get(current.as_str()) {
            for tool in def.tools {
                out.push(tool.to_string());
            }
            for include in def.includes {
                stack.push(include.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Resolve several toolset names at once.
pub fn resolve_toolsets(names: &[String]) -> Vec<String> {
    let mut all: Vec<String> = names.iter().flat_map(|n| resolve_toolset(n)).collect();
    all.sort();
    all.dedup();
    all
}

/// Apply config-driven toolset enable/disable to a registry: tools not in
/// the allowed set get their toolset disabled. When `enabled` is empty the
/// default "coding" toolset applies.
pub fn apply_toolset_policy(
    registry: &mut crate::tools::ToolRegistry,
    enabled: &[String],
    disabled: &[String],
) {
    let allowed: HashSet<String> = if enabled.is_empty() {
        resolve_toolset("coding").into_iter().collect()
    } else {
        resolve_toolsets(enabled).into_iter().collect()
    };
    let removed: HashSet<String> = resolve_toolsets(disabled).into_iter().collect();

    // Disable whole toolsets whose tools are all excluded. Dynamically
    // registered toolsets (mcp:*, plugin:*) are enabled by default and
    // only disabled through an explicit disabled_toolsets entry (hermes
    // MCP/plugin semantics — the "coding" default must not swallow them).
    for toolset in registry.toolset_names() {
        let dynamic = toolset.starts_with("mcp:") || toolset.starts_with("plugin:");
        if dynamic {
            let explicitly_removed = registry
                .toolset_tools(&toolset)
                .iter()
                .all(|t| removed.contains(&t.definition.name))
                && !registry.toolset_tools(&toolset).is_empty();
            if explicitly_removed {
                registry.disable_toolset(&toolset);
            }
            continue;
        }
        let tools = registry
            .toolset_tools(&toolset)
            .iter()
            .map(|t| t.definition.name.clone())
            .collect::<Vec<_>>();
        let any_allowed = tools
            .iter()
            .any(|t| allowed.contains(t) && !removed.contains(t));
        if !any_allowed {
            registry.disable_toolset(&toolset);
        }
    }
}

/// Text digest of the toolset catalog (P672 — the `/toolsets` slash,
/// hermes `/toolsets` parity). `enabled` marks toolsets registered with
/// the current agent; descriptions are clipped to one short line.
pub fn format_toolsets_digest(enabled: &[String]) -> String {
    let catalog = toolsets();
    let mut names: Vec<&str> = catalog.keys().copied().collect();
    names.sort_unstable();
    let mut out = format!("toolsets ({} defined):\n", names.len());
    for name in names {
        let def = &catalog[name];
        let mark = if enabled.iter().any(|toolset| toolset == name) {
            "\u{2713}"
        } else {
            " "
        };
        let description = def
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let description = if description.chars().count() > 64 {
            let clipped: String = description.chars().take(61).collect();
            format!("{clipped}...")
        } else {
            description
        };
        out.push_str(&format!(
            "  {mark} {:<14} {:>2} tools  {}\n",
            name,
            def.tools.len(),
            description
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_coding_includes_clarify() {
        let tools = resolve_toolset("coding");
        assert!(tools.contains(&"terminal".to_string()));
        assert!(tools.contains(&"clarify".to_string()));
        assert!(tools.contains(&"patch".to_string()));
    }

    #[test]
    fn test_resolve_safe() {
        let tools = resolve_toolset("safe");
        assert!(tools.contains(&"web_search".to_string()));
        assert!(tools.contains(&"vision_analyze".to_string()));
        assert!(!tools.contains(&"terminal".to_string()));
    }

    #[test]
    fn test_apply_policy() {
        let mut registry = crate::tools::ToolRegistry::new();
        crate::tools::builtin::register_builtin_tools(&mut registry);
        // register clarify manually (lives in the CLI layer in hermes too)
        apply_toolset_policy(&mut registry, &["web".to_string()], &[]);
        let defs = registry.definitions();
        let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        assert!(names.contains(&"web_search".to_string()));
        assert!(!names.iter().any(|n| n == "terminal"));
    }

    #[test]
    fn toolsets_digest_marks_enabled_and_lists_catalog() {
        // P672: /toolsets digest formatting.
        let out = format_toolsets_digest(&["web".to_string()]);
        assert!(out.contains("toolsets ("), "{out}");
        assert!(out.contains("\u{2713} web"), "{out}");
        assert!(out.contains("vision"), "{out}");
        // A long description gets clipped, not wrapped.
        assert!(
            !out.contains("Available when xAI                           credentials"),
            "{out}"
        );
    }
}
