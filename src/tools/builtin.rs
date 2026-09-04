//! Built-in tool registration — the ulnclaw equivalent of hermes importing
//! every tools/*.py module (each of which self-registers).

pub mod clarify;
pub mod cronjob;
pub mod delegate;
pub mod desktop;
pub mod execute_code;
pub mod files;
pub mod media;
pub mod memory;
pub mod platform;
pub mod project;
pub mod session_search;
pub mod skills;
pub mod terminal;
pub mod todo;
pub mod tool_search;
pub mod video;
pub mod web;
pub mod x_search;

use crate::tools::ToolRegistry;

/// Register every built-in tool. Toolsets are enabled/disabled afterwards
/// via `ToolRegistry::disable_toolset` (see `toolsets` module).
pub fn register_builtin_tools(registry: &mut ToolRegistry) {
    files::register(registry);
    terminal::register(registry);
    web::register(registry);
    memory::register(registry);
    todo::register(registry);
    clarify::register(registry);
    session_search::register(registry);
    skills::register(registry);
    delegate::register(registry);
    execute_code::register(registry);
    cronjob::register(registry);
    media::register(registry);
    platform::register(registry);
    tool_search::register(registry);
    desktop::register(registry);
    project::register(registry);
    x_search::register(registry);
    video::register(registry);
}

/// Names of all built-in tools (for tests/docs).
pub fn builtin_tool_names() -> Vec<&'static str> {
    vec![
        "read_file",
        "write_file",
        "patch",
        "search_files",
        "terminal",
        "process",
        "web_search",
        "web_extract",
        "memory",
        "todo",
        "clarify",
        "session_search",
        "skills_list",
        "skill_view",
        "skill_manage",
        "delegate_task",
        "execute_code",
        "cronjob",
        "vision_analyze",
        "video_analyze",
        "image_generate",
        "text_to_speech",
        "ha_list_entities",
        "ha_get_state",
        "ha_list_services",
        "ha_call_service",
        "kanban_create",
        "kanban_list",
        "kanban_show",
        "kanban_complete",
        "kanban_block",
        "kanban_unblock",
        "kanban_comment",
        "kanban_heartbeat",
        "kanban_link",
        "kanban_attach",
        "kanban_attach_url",
        "kanban_attachments",
        "browser_navigate",
        "browser_snapshot",
        "browser_click",
        "browser_type",
        "browser_scroll",
        "browser_back",
        "browser_press",
        "browser_get_images",
        "browser_vision",
        "browser_console",
        "browser_cdp",
        "browser_dialog",
        "computer_use",
        "tool_search",
        "discord",
        "discord_admin",
        "feishu_doc_read",
        "spotify_playback",
        "close_terminal",
        "read_terminal",
        "focus_pane",
        "open_preview",
        "x_search",
        "video_generate",
        "xai_video_edit",
        "xai_video_extend",
        "project_list",
        "project_create",
        "project_switch",
        "bfl_flux3_text_to_video",
        "bfl_flux3_image_to_video",
        "bfl_flux3_keyframes_to_video",
        "bfl_flux3_video_continuation",
        "bfl_flux3_get_result",
        "bfl_flux3_prompting_guide",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_builtins_register() {
        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        assert!(
            registry.len() >= 45,
            "expected 45+ tools, got {}",
            registry.len()
        );
        for name in builtin_tool_names() {
            assert!(registry.has(name), "missing built-in tool: {}", name);
        }
    }
}
