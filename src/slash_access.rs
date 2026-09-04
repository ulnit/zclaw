//! Per-platform slash command access control — port of hermes
//! `gateway/slash_access.py`.
//!
//! This sits beside the existing per-platform allowlists and adds a
//! second axis: of the users who are *allowed to talk to the gateway*,
//! which ones can run *which slash commands*.
//!
//! Two lists per platform scope (DM vs group):
//! - `allow_admin_from` / `group_allow_admin_from` — user IDs that get
//!   every registered slash command.
//! - `user_allowed_commands` / `group_user_allowed_commands` — slash
//!   command names non-admin users may run. Empty → non-admins get
//!   only the implicit floor ([`ALWAYS_ALLOWED_FOR_USERS`]).
//!
//! Backward compatibility: if `allow_admin_from` is not set for a
//! scope, gating is disabled entirely for that scope — every allowed
//! user can run every slash command, exactly like before. Existing
//! installs are unaffected until an operator opts in by listing at
//! least one admin.
//!
//! Gating slash commands does not affect plain chat — non-admin users
//! can still talk to the agent normally; they just can't trigger
//! commands outside their allowlist. Unknown `/…` text (paths, etc.)
//! is never gated.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Slash commands that MUST stay reachable for any allowed user, even
/// when gating is enabled and the user has no commands listed — a
/// guest-sized read-only floor (hermes `_ALWAYS_ALLOWED_FOR_USERS`).
/// `user_allowed_commands` extends this set additively, never
/// restrictively.
pub const ALWAYS_ALLOWED_FOR_USERS: &[&str] = &["help", "whoami"];

/// Built-in messaging slash commands the gate knows about (platform
/// slash arms + the dispatcher intercepts). Skill/bundle slashes are
/// agent-turn expansions, not command dispatch, and stay ungated.
pub const KNOWN_COMMANDS: &[&str] = &[
    "help",
    "whoami",
    "skills",
    "tools",
    "recap",
    "title",
    "resume",
    "new",
    "reset",
    "learn",
    "sethome",
    "set-home",
    "usage",
    "context",
    "insights",
    "footer",
    "reload-mcp",
    "approve",
    "deny",
];

/// Per-platform scope config (hermes platform `extra` keys:
/// `allow_admin_from` / `user_allowed_commands` for DMs,
/// `group_allow_admin_from` / `group_user_allowed_commands` for
/// groups, channels, threads and any other multi-user context).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SlashAccessScopeConfig {
    /// DM scope: user IDs that can run every slash command.
    pub allow_admin_from: Vec<String>,
    /// DM scope: slash commands non-admin users may run.
    pub user_allowed_commands: Vec<String>,
    /// Group scope: user IDs that can run every slash command.
    pub group_allow_admin_from: Vec<String>,
    /// Group scope: slash commands non-admin users may run.
    pub group_user_allowed_commands: Vec<String>,
}

/// Resolved access policy for a single (platform, scope) pair.
#[derive(Debug, Clone)]
pub struct SlashAccessPolicy {
    /// Gating active for this scope?
    pub enabled: bool,
    pub admin_user_ids: HashSet<String>,
    pub user_allowed_commands: HashSet<String>,
}

impl SlashAccessPolicy {
    pub fn is_admin(&self, user_id: Option<&str>) -> bool {
        if !self.enabled {
            // Gating disabled → treat every allowed user as admin so
            // downstream code can keep using is_admin / can_run
            // uniformly.
            return true;
        }
        match user_id {
            Some(id) if !id.is_empty() => self.admin_user_ids.contains(id),
            _ => false,
        }
    }

    pub fn can_run(&self, user_id: Option<&str>, canonical_cmd: &str) -> bool {
        if !self.enabled {
            return true;
        }
        if self.is_admin(user_id) {
            return true;
        }
        if canonical_cmd.is_empty() {
            return false;
        }
        if ALWAYS_ALLOWED_FOR_USERS.contains(&canonical_cmd) {
            return true;
        }
        self.user_allowed_commands.contains(canonical_cmd)
    }
}

/// Canonicalize a slash command name: strip the leading slash,
/// lowercase (hermes `_coerce_command_list` semantics).
pub fn normalize_command(raw: &str) -> String {
    raw.trim().trim_start_matches('/').to_lowercase()
}

/// True when the canonical name is a registered command the gate
/// knows about (unknown `/…` text stays an ordinary agent message).
pub fn is_known_command(canonical_cmd: &str) -> bool {
    KNOWN_COMMANDS.contains(&canonical_cmd)
}

/// Map a recorded chat type to a scope (hermes `_scope_for_chat_type`):
/// dm/direct/private/unknown → `"dm"`, anything else → `"group"`.
pub fn scope_for_chat_type(chat_type: Option<&str>) -> &'static str {
    match chat_type.map(|t| t.trim().to_lowercase()) {
        None => "dm",
        Some(t) if matches!(t.as_str(), "dm" | "direct" | "private" | "") => "dm",
        Some(_) => "group",
    }
}

fn coerce_ids(raw: &[String]) -> HashSet<String> {
    raw.iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn coerce_commands(raw: &[String]) -> HashSet<String> {
    raw.iter()
        .map(|s| normalize_command(s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Build a policy from one platform's scope config (hermes
/// `policy_from_extra`).
///
/// DM scope falls back to the group `user_allowed_commands` ONLY when
/// the DM scope didn't specify its own — operators list the same
/// command set once. Admin lists are NOT cross-scope: an admin in DMs
/// is not implicitly an admin in a group.
pub fn policy_for(config: Option<&SlashAccessScopeConfig>, scope: &str) -> SlashAccessPolicy {
    let Some(config) = config else {
        return SlashAccessPolicy {
            enabled: false,
            admin_user_ids: HashSet::new(),
            user_allowed_commands: HashSet::new(),
        };
    };
    let (admin_ids, mut cmds) = if scope == "group" {
        (
            coerce_ids(&config.group_allow_admin_from),
            coerce_commands(&config.group_user_allowed_commands),
        )
    } else {
        (
            coerce_ids(&config.allow_admin_from),
            coerce_commands(&config.user_allowed_commands),
        )
    };
    if scope == "dm" && cmds.is_empty() {
        cmds = coerce_commands(&config.group_user_allowed_commands);
    }
    SlashAccessPolicy {
        enabled: !admin_ids.is_empty(),
        admin_user_ids: admin_ids,
        user_allowed_commands: cmds,
    }
}

/// Denial reply for a gated command (hermes `_check_slash_access`
/// copy): the allowed preview is sorted, capped at 12 entries.
pub fn denial_message(canonical_cmd: &str, policy: &SlashAccessPolicy) -> String {
    let mut allowed: Vec<&String> = policy.user_allowed_commands.iter().collect();
    allowed.sort();
    let suffix = if allowed.is_empty() {
        "No slash commands are enabled for non-admins on this platform. \
         Ask an admin to add you to allow_admin_from or to set \
         user_allowed_commands."
            .to_string()
    } else {
        let preview: Vec<String> = allowed.iter().take(12).map(|c| format!("/{c}")).collect();
        let ellipsis = if allowed.len() > 12 { "\u{2026}" } else { "" };
        format!(
            "You can run: {}{ellipsis}. Use /whoami for the full list.",
            preview.join(", ")
        )
    };
    format!("\u{26d4} /{canonical_cmd} is admin-only here. {suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SlashAccessScopeConfig {
        SlashAccessScopeConfig {
            allow_admin_from: vec!["boss".into()],
            user_allowed_commands: vec!["/status".into(), "recap".into()],
            group_allow_admin_from: vec!["gadmin".into()],
            group_user_allowed_commands: vec!["help".into(), "title".into()],
        }
    }

    #[test]
    fn disabled_policy_allows_everything() {
        // No config at all → gating off (backward compat).
        let policy = policy_for(None, "dm");
        assert!(!policy.enabled);
        assert!(policy.is_admin(None));
        assert!(policy.can_run(Some("anyone"), "anything"));
        // Config present but empty admin list → still off.
        let policy = policy_for(Some(&SlashAccessScopeConfig::default()), "dm");
        assert!(!policy.enabled);
        assert!(policy.can_run(Some("anyone"), "anything"));
    }

    #[test]
    fn dm_scope_gates_admins_users_and_floor() {
        let policy = policy_for(Some(&cfg()), "dm");
        assert!(policy.enabled);
        // Admin runs anything.
        assert!(policy.can_run(Some("boss"), "reload-mcp"));
        // Non-admin: listed commands + the floor pass…
        assert!(policy.can_run(Some("pleb"), "status"));
        assert!(policy.can_run(Some("pleb"), "recap"));
        assert!(policy.can_run(Some("pleb"), "help"));
        assert!(policy.can_run(Some("pleb"), "whoami"));
        // …everything else is denied, including empty commands and
        // missing user ids.
        assert!(!policy.can_run(Some("pleb"), "reload-mcp"));
        assert!(!policy.can_run(Some("pleb"), ""));
        assert!(!policy.can_run(None, "reload-mcp"));
    }

    #[test]
    fn group_scope_uses_group_lists_without_admin_fallthrough() {
        let policy = policy_for(Some(&cfg()), "group");
        assert!(policy.enabled);
        // DM admin is NOT an admin in group scope.
        assert!(!policy.is_admin(Some("boss")));
        assert!(policy.is_admin(Some("gadmin")));
        assert!(policy.can_run(Some("gadmin"), "reload-mcp"));
        assert!(policy.can_run(Some("pleb"), "title"));
        assert!(
            !policy.can_run(Some("pleb"), "status"),
            "dm list must not leak into groups"
        );
    }

    #[test]
    fn dm_commands_fall_back_to_group_list() {
        let mut config = SlashAccessScopeConfig {
            allow_admin_from: vec!["boss".into()],
            ..Default::default()
        };
        config.group_user_allowed_commands = vec!["usage".into()];
        let policy = policy_for(Some(&config), "dm");
        assert!(policy.can_run(Some("pleb"), "usage"));
        // Explicit dm list wins and stops the fallthrough.
        config.user_allowed_commands = vec!["recap".into()];
        let policy = policy_for(Some(&config), "dm");
        assert!(policy.can_run(Some("pleb"), "recap"));
        assert!(!policy.can_run(Some("pleb"), "usage"));
    }

    #[test]
    fn scope_mapping_matches_hermes() {
        assert_eq!(scope_for_chat_type(None), "dm");
        assert_eq!(scope_for_chat_type(Some("")), "dm");
        assert_eq!(scope_for_chat_type(Some("DM")), "dm");
        assert_eq!(scope_for_chat_type(Some("direct")), "dm");
        assert_eq!(scope_for_chat_type(Some("private")), "dm");
        assert_eq!(scope_for_chat_type(Some("group")), "group");
        assert_eq!(scope_for_chat_type(Some("channel")), "group");
    }

    #[test]
    fn normalize_strips_slash_and_lowercases() {
        assert_eq!(normalize_command("/Help"), "help");
        assert_eq!(normalize_command("STATUS"), "status");
        assert!(is_known_command("help"));
        assert!(is_known_command("reload-mcp"));
        assert!(!is_known_command("my-skill-bundle"));
    }

    #[test]
    fn denial_message_shapes() {
        let policy = policy_for(Some(&cfg()), "dm");
        let msg = denial_message("reload-mcp", &policy);
        assert!(
            msg.starts_with("\u{26d4} /reload-mcp is admin-only here."),
            "{msg}"
        );
        assert!(msg.contains("/recap") && msg.contains("/status"), "{msg}");
        assert!(msg.contains("/whoami for the full list"), "{msg}");
        // No user_allowed_commands → the ask-an-admin suffix.
        let bare = SlashAccessScopeConfig {
            allow_admin_from: vec!["boss".into()],
            ..Default::default()
        };
        let policy = policy_for(Some(&bare), "dm");
        let msg = denial_message("status", &policy);
        assert!(msg.contains("No slash commands are enabled"), "{msg}");
    }
}
