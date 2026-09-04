//! Profile-based routing for gateway messaging with hierarchical
//! matching (hermes `gateway/profile_routing.py` parity).
//!
//! Allows a single ulnclaw instance to route specific platform chats
//! to different profiles — each with its own model, tools, memory,
//! and persona (the `[profiles.<name>]` override + a profile-scoped
//! home under `<home>/profiles/<name>`).
//!
//! Matching priority (most specific first, first match wins):
//!
//! ```text
//! 1. platform + chat_id + thread_id   specificity 12
//! 2. platform + chat_id               specificity  4
//! 3. platform + guild_id              specificity  2
//! ```
//!
//! All discriminators a route declares must hold (AND). `chat_id`
//! supports hierarchical matching for thread-bearing platforms: a
//! route keyed on a channel matches both direct messages in that
//! channel and messages in any thread/post whose parent is that
//! channel (hermes `parent_chat_id` rule).
//!
//! Note: current ulnclaw `MessageEvent`s carry platform + chat_id
//! only, so in practice chat-level routes are the ones that match;
//! guild/thread discriminators parse and match faithfully for the
//! day events gain those fields (and stay parity with hermes rules).

use serde::{Deserialize, Serialize};

/// One routing rule mapping a platform scope to a profile (hermes
/// `ProfileRoute`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileRoute {
    pub name: String,
    pub platform: String,
    pub profile: String,
    pub guild_id: Option<String>,
    pub chat_id: Option<String>,
    pub thread_id: Option<String>,
    pub enabled: bool,
}

/// Config-file shape of a route (`[[gateway.profile_routes]]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileRouteSpec {
    pub name: String,
    pub platform: String,
    pub profile: String,
    pub guild_id: Option<String>,
    pub chat_id: Option<String>,
    pub thread_id: Option<String>,
    pub enabled: bool,
}

impl Default for ProfileRouteSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            platform: String::new(),
            profile: String::new(),
            guild_id: None,
            chat_id: None,
            thread_id: None,
            enabled: true,
        }
    }
}

impl ProfileRoute {
    /// Higher value = more specific match (hermes `specificity`).
    pub fn specificity(&self) -> u32 {
        let mut s = 0;
        if self.guild_id.is_some() {
            s += 2;
        }
        if self.chat_id.is_some() {
            s += 4;
        }
        if self.thread_id.is_some() {
            s += 8;
        }
        s
    }

    /// Whether this route matches the given source fields (hermes
    /// `matches`): every declared discriminator must hold; `chat_id`
    /// also matches the direct parent of a thread/post.
    pub fn matches(
        &self,
        platform: &str,
        guild_id: Option<&str>,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
        parent_chat_id: Option<&str>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        if self.platform != platform {
            return false;
        }
        if let Some(wanted) = &self.thread_id {
            if thread_id != Some(wanted.as_str()) {
                return false;
            }
        }
        if let Some(wanted) = &self.chat_id {
            let direct = chat_id == Some(wanted.as_str());
            let parent = parent_chat_id == Some(wanted.as_str());
            if !direct && !parent {
                return false;
            }
        }
        if let Some(wanted) = &self.guild_id {
            if guild_id != Some(wanted.as_str()) {
                return false;
            }
        }
        true
    }
}

/// Parse configured route specs into routes, most specific first
/// (hermes `parse_profile_routes`). Entries missing platform/profile
/// are skipped with a warning.
pub fn parse_profile_routes(specs: &[ProfileRouteSpec]) -> Vec<ProfileRoute> {
    let mut routes: Vec<ProfileRoute> = Vec::new();
    for spec in specs {
        if spec.platform.trim().is_empty() || spec.profile.trim().is_empty() {
            tracing::warn!(
                "[profile_routing] skipping route '{}': missing platform or profile",
                spec.name
            );
            continue;
        }
        routes.push(ProfileRoute {
            name: spec.name.clone(),
            platform: spec.platform.trim().to_lowercase(),
            profile: spec.profile.trim().to_string(),
            guild_id: spec.guild_id.clone(),
            chat_id: spec.chat_id.clone(),
            thread_id: spec.thread_id.clone(),
            enabled: spec.enabled,
        });
    }
    // Most specific first so the first match wins (stable sort keeps
    // config order among equal-specificity routes).
    routes.sort_by_key(|route| std::cmp::Reverse(route.specificity()));
    routes
}

/// Best-matching route for a source, or None (hermes
/// `match_profile_route`). Expects routes pre-sorted by specificity
/// (see [`parse_profile_routes`]).
pub fn match_profile_route<'a>(
    routes: &'a [ProfileRoute],
    platform: &str,
    guild_id: Option<&str>,
    chat_id: Option<&str>,
    thread_id: Option<&str>,
    parent_chat_id: Option<&str>,
) -> Option<&'a ProfileRoute> {
    let platform = platform.trim().to_lowercase();
    routes
        .iter()
        .find(|route| route.matches(&platform, guild_id, chat_id, thread_id, parent_chat_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, platform: &str, profile: &str) -> ProfileRouteSpec {
        ProfileRouteSpec {
            name: name.into(),
            platform: platform.into(),
            profile: profile.into(),
            ..Default::default()
        }
    }

    #[test]
    fn specificity_orders_thread_over_chat_over_guild() {
        let mut s = spec("guild", "discord", "p");
        s.guild_id = Some("g1".into());
        let mut c = spec("chat", "discord", "p");
        c.chat_id = Some("c1".into());
        let mut t = spec("thread", "discord", "p");
        t.chat_id = Some("c1".into());
        t.thread_id = Some("t1".into());
        let routes = parse_profile_routes(&[s, c, t]);
        let names: Vec<&str> = routes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["thread", "chat", "guild"]);
    }

    #[test]
    fn parse_skips_incomplete_entries() {
        let mut no_platform = spec("a", "", "p");
        no_platform.chat_id = Some("c".into());
        let mut no_profile = spec("b", "telegram", "");
        no_profile.chat_id = Some("c".into());
        let ok = {
            let mut s = spec("c", "telegram", "p");
            s.chat_id = Some("c".into());
            s
        };
        let routes = parse_profile_routes(&[no_platform, no_profile, ok]);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].name, "c");
    }

    #[test]
    fn matching_is_conjunctive_and_hierarchical() {
        let mut chat_route = spec("chan", "discord", "chan-profile");
        chat_route.chat_id = Some("C1".into());
        let mut thread_route = spec("thread", "discord", "thread-profile");
        thread_route.chat_id = Some("C1".into());
        thread_route.thread_id = Some("T9".into());
        let mut guild_route = spec("guild", "discord", "guild-profile");
        guild_route.guild_id = Some("G1".into());
        let routes = parse_profile_routes(&[chat_route, thread_route, guild_route]);

        // Exact thread beats its parent channel route.
        let hit = match_profile_route(&routes, "discord", Some("G1"), Some("C1"), Some("T9"), None)
            .unwrap();
        assert_eq!(hit.profile, "thread-profile");
        // A different thread in the channel falls to the channel route.
        let hit = match_profile_route(
            &routes,
            "discord",
            Some("G1"),
            Some("C1"),
            Some("T10"),
            None,
        )
        .unwrap();
        assert_eq!(hit.profile, "chan-profile");
        // Thread-bearing message whose PARENT is the routed channel
        // (forum post semantics) still matches the channel route.
        let hit = match_profile_route(
            &routes,
            "discord",
            Some("G1"),
            Some("POST1"),
            Some("T1"),
            Some("C1"),
        )
        .unwrap();
        assert_eq!(hit.profile, "chan-profile");
        // Another guild's chat with the same id: guild route needs G1.
        let hit = match_profile_route(&routes, "discord", Some("G2"), Some("OTHER"), None, None);
        assert!(hit.is_none());
        let hit =
            match_profile_route(&routes, "discord", Some("G1"), Some("OTHER"), None, None).unwrap();
        assert_eq!(hit.profile, "guild-profile");
        // Platform mismatch never matches.
        assert!(match_profile_route(
            &routes,
            "telegram",
            Some("G1"),
            Some("C1"),
            Some("T9"),
            None
        )
        .is_none());
    }

    #[test]
    fn disabled_routes_never_match() {
        let mut s = spec("off", "telegram", "p");
        s.chat_id = Some("42".into());
        s.enabled = false;
        let routes = parse_profile_routes(&[s]);
        assert!(match_profile_route(&routes, "telegram", None, Some("42"), None, None).is_none());
    }

    #[test]
    fn platform_names_are_case_insensitive() {
        let mut s = spec("r", "Telegram", "p");
        s.chat_id = Some("42".into());
        let routes = parse_profile_routes(&[s]);
        assert!(match_profile_route(&routes, "TELEGRAM", None, Some("42"), None, None).is_some());
    }

    #[test]
    fn chat_level_route_matches_without_thread_fields() {
        // ulnclaw MessageEvents carry platform + chat_id only — the
        // common case must match with all other fields None.
        let mut s = spec("r", "telegram", "work");
        s.chat_id = Some("-100123".into());
        let routes = parse_profile_routes(&[s]);
        let hit =
            match_profile_route(&routes, "telegram", None, Some("-100123"), None, None).unwrap();
        assert_eq!(hit.profile, "work");
        assert!(
            match_profile_route(&routes, "telegram", None, Some("-100999"), None, None).is_none()
        );
    }
}
