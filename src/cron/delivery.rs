//! Cron delivery targets — port of hermes `cron/scheduler.py` delivery
//! resolution (`_KNOWN_DELIVERY_PLATFORMS`, `_HOME_TARGET_ENV_VARS`,
//! `_get_home_target_chat_id`, `_get_home_target_thread_id`,
//! `cron_delivery_targets`, `_resolve_delivery_targets`,
//! `_normalize_deliver_value`, `SILENT_MARKER` suppression) plus the
//! `gateway.response_filters.is_autonomous_silence_response` matcher.
//!
//! Delivery routing intent tokens (`origin`, `all`, `platform`,
//! `platform:chat[:thread]`) are resolved at fire time, not create time,
//! so a job created before a platform was wired up picks it up once it
//! comes online.

use crate::cron::{CronJob, JobOrigin};

/// Sentinel: when a cron agent has nothing new to report, it can start
/// its response with this marker to suppress delivery. Output is still
/// saved locally for audit (hermes `SILENT_MARKER`).
pub const SILENT_MARKER: &str = "[SILENT]";

/// Exact whole-line markers that mean "the agent intentionally chose not
/// to reply" (hermes `LIVE_GATEWAY_SILENT_MARKERS` — canonical set
/// lives in `crate::response_filters`).

/// Valid delivery platforms — used to validate user-supplied platform
/// names in cron delivery targets, preventing env var enumeration via
/// crafted names (hermes `_KNOWN_DELIVERY_PLATFORMS`).
pub const KNOWN_DELIVERY_PLATFORMS: &[&str] = &[
    "telegram",
    "discord",
    "slack",
    "whatsapp",
    "signal",
    "matrix",
    "mattermost",
    "homeassistant",
    "dingtalk",
    "feishu",
    "wecom",
    "wecom_callback",
    "weixin",
    "sms",
    "email",
    "webhook",
    "bluebubbles",
    "qqbot",
    "yuanbao",
];

/// Platforms that support a configured cron/notification home target,
/// mapped to the environment variable used by gateway setup/runtime
/// config (hermes `_HOME_TARGET_ENV_VARS`, declaration order preserved).
pub const HOME_TARGET_ENV_VARS: &[(&str, &str)] = &[
    ("matrix", "MATRIX_HOME_ROOM"),
    ("telegram", "TELEGRAM_HOME_CHANNEL"),
    ("discord", "DISCORD_HOME_CHANNEL"),
    ("slack", "SLACK_HOME_CHANNEL"),
    ("signal", "SIGNAL_HOME_CHANNEL"),
    ("mattermost", "MATTERMOST_HOME_CHANNEL"),
    ("sms", "SMS_HOME_CHANNEL"),
    ("email", "EMAIL_HOME_ADDRESS"),
    ("dingtalk", "DINGTALK_HOME_CHANNEL"),
    ("feishu", "FEISHU_HOME_CHANNEL"),
    ("wecom", "WECOM_HOME_CHANNEL"),
    ("weixin", "WEIXIN_HOME_CHANNEL"),
    ("bluebubbles", "BLUEBUBBLES_HOME_CHANNEL"),
    ("qqbot", "QQBOT_HOME_CHANNEL"),
    ("whatsapp", "WHATSAPP_HOME_CHANNEL"),
    ("whatsapp_cloud", "WHATSAPP_CLOUD_HOME_CHANNEL"),
];

/// Legacy env var names kept for back-compat: current primary env var →
/// the previous name (hermes `_LEGACY_HOME_TARGET_ENV_VARS`).
const LEGACY_HOME_TARGET_ENV_VARS: &[(&str, &str)] = &[("QQBOT_HOME_CHANNEL", "QQ_HOME_CHANNEL")];

/// Routing intent tokens — resolved at fire time (hermes
/// `_ROUTING_TOKENS`). `all` expands into the set of platforms with a
/// configured home chat id.
const ROUTING_TOKENS: &[&str] = &["all"];

/// One concrete delivery target (hermes target dict `{platform, chat_id,
/// thread_id}`).
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryTarget {
    pub platform: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
}

/// Whether `platform_name` is a valid cron delivery target (hermes
/// `_is_known_delivery_platform`; plugin platforms are not a thing in
/// ulnclaw, so only the built-in set is checked).
pub fn is_known_delivery_platform(platform_name: &str) -> bool {
    KNOWN_DELIVERY_PLATFORMS.contains(&platform_name.to_lowercase().as_str())
}

/// The env var holding a platform's configured cron home channel (hermes
/// `_resolve_home_env_var`).
pub fn resolve_home_env_var(platform_name: &str) -> Option<&'static str> {
    let name = platform_name.to_lowercase();
    HOME_TARGET_ENV_VARS
        .iter()
        .find(|(platform, _)| *platform == name)
        .map(|(_, env_var)| *env_var)
}

/// The configured home target chat/room ID for a delivery platform
/// (hermes `_get_home_target_chat_id`), with legacy env var fallback.
pub fn home_target_chat_id(platform_name: &str) -> Option<String> {
    let env_var = resolve_home_env_var(platform_name)?;
    let value = std::env::var(env_var).unwrap_or_default();
    if !value.is_empty() {
        return Some(value);
    }
    LEGACY_HOME_TARGET_ENV_VARS
        .iter()
        .find(|(primary, _)| *primary == env_var)
        .and_then(|(_, legacy)| std::env::var(legacy).ok())
        .filter(|value| !value.is_empty())
}

/// The optional thread/topic ID for a platform home target (hermes
/// `_get_home_target_thread_id`). Telegram-only override:
/// `TELEGRAM_CRON_THREAD_ID` takes precedence over
/// `TELEGRAM_HOME_CHANNEL_THREAD_ID` for cron delivery.
pub fn home_target_thread_id(platform_name: &str) -> Option<String> {
    let env_var = resolve_home_env_var(platform_name)?;
    if platform_name.to_lowercase() == "telegram" {
        if let Ok(value) = std::env::var("TELEGRAM_CRON_THREAD_ID") {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    let thread_var = format!("{}_THREAD_ID", env_var);
    let value = std::env::var(&thread_var).unwrap_or_default();
    if value.is_empty() {
        if let Some((_, legacy)) = LEGACY_HOME_TARGET_ENV_VARS
            .iter()
            .find(|(primary, _)| *primary == env_var)
        {
            let legacy_var = format!("{}_THREAD_ID", legacy);
            let legacy_value = std::env::var(&legacy_var).unwrap_or_default();
            if !legacy_value.is_empty() {
                return Some(legacy_value);
            }
        }
        return None;
    }
    Some(value)
}

/// Iterate built-in platform names that expose a home channel (hermes
/// `_iter_home_target_platforms`; ulnclaw has no plugin registry, so
/// only the built-ins).
pub fn iter_home_target_platforms() -> impl Iterator<Item = &'static str> {
    HOME_TARGET_ENV_VARS.iter().map(|(name, _)| *name)
}

/// Normalize a stored/submitted `deliver` value to its canonical string
/// form (hermes `_normalize_deliver_value`): lists flatten to comma
/// separators, falsy → `"local"`.
pub fn normalize_deliver_value(deliver: Option<&serde_json::Value>) -> String {
    match deliver {
        None | Some(serde_json::Value::Null) => "local".to_string(),
        Some(serde_json::Value::Array(parts)) => {
            let joined: Vec<String> = parts
                .iter()
                .filter_map(|part| part.as_str())
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect();
            if joined.is_empty() {
                "local".to_string()
            } else {
                joined.join(",")
            }
        }
        Some(other) => {
            let text = match other {
                serde_json::Value::String(s) => s.clone(),
                value => value.to_string(),
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                "local".to_string()
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// Extract origin info from a job (hermes `_resolve_origin`): requires
/// both a platform and a chat id.
pub fn resolve_origin(job: &CronJob) -> Option<&JobOrigin> {
    job.origin
        .as_ref()
        .filter(|origin| !origin.platform.is_empty() && !origin.chat_id.is_empty())
}

/// Whether a delivery target IS the job's own origin conversation
/// (hermes `_target_matches_origin`). Mirroring is scoped to the
/// origin session by design: a job created from a live gateway chat
/// stamps that chat as origin and that session is guaranteed to
/// exist. Fan-out targets (`deliver=all`, an explicit other chat, a
/// home-channel fallback) are broadcasts, not a continuation of a
/// conversation, and are deliberately NOT mirrored.
pub fn target_matches_origin(
    origin: Option<&JobOrigin>,
    platform: &str,
    chat_id: &str,
    thread_id: Option<&str>,
) -> bool {
    let Some(origin) = origin else {
        return false;
    };
    if !origin.platform.eq_ignore_ascii_case(platform) {
        return false;
    }
    if origin.chat_id != chat_id {
        return false;
    }
    // thread_id must match when the origin pins one (topic-scoped
    // chats); a target that lost the thread_id is not the same
    // conversation lane.
    if let Some(origin_thread) = origin.thread_id.as_deref() {
        if origin_thread != thread_id.unwrap_or("") {
            return false;
        }
    }
    true
}

/// Whether a job's deliveries should also be mirrored into the target
/// chat's session transcript (hermes `_cron_mirror_delivery_enabled`).
/// Default OFF — preserves the historical isolation guarantee (cron
/// output lives only in the job's own session) for everyone who does
/// not opt in. Precedence (first decisive value wins): per-job
/// `attach_to_session`, then global `cron.mirror_delivery`, then
/// false.
pub fn mirror_delivery_enabled(job: &CronJob, cron_config: &crate::config::CronConfig) -> bool {
    job.attach_to_session.unwrap_or(cron_config.mirror_delivery)
}

/// Parse an explicit `rest` after `platform:` (approximation of hermes
/// `tools.send_message_tool._parse_target_ref`): returns `(chat_id,
/// thread_id, is_explicit)`. Matrix room ids contain `:` themselves, so
/// the whole remainder stays the chat id there; everywhere else a `:`
/// separates the chat id from an explicit thread id.
fn parse_target_ref(platform: &str, rest: &str) -> (String, Option<String>, bool) {
    if platform.to_lowercase() == "matrix" {
        return (rest.to_string(), None, false);
    }
    match rest.split_once(':') {
        Some((chat_id, thread_id)) if !chat_id.is_empty() && !thread_id.is_empty() => {
            (chat_id.to_string(), Some(thread_id.to_string()), true)
        }
        _ => (rest.to_string(), None, false),
    }
}

/// Resolve one concrete auto-delivery target for a cron job (hermes
/// `_resolve_single_delivery_target`).
pub fn resolve_single_delivery_target(
    job: &CronJob,
    deliver_value: &str,
) -> Option<DeliveryTarget> {
    let origin = resolve_origin(job);

    if deliver_value == "local" {
        return None;
    }

    if deliver_value == "origin" {
        if let Some(origin) = origin {
            return Some(DeliveryTarget {
                platform: origin.platform.clone(),
                chat_id: origin.chat_id.to_string(),
                thread_id: origin.thread_id.clone(),
            });
        }
        // Origin missing (e.g. job created via API/script) — try each
        // platform's home channel as a fallback instead of silently
        // dropping.
        for platform_name in iter_home_target_platforms() {
            if let Some(chat_id) = home_target_chat_id(platform_name) {
                return Some(DeliveryTarget {
                    platform: platform_name.to_string(),
                    chat_id,
                    thread_id: home_target_thread_id(platform_name),
                });
            }
        }
        return None;
    }

    if deliver_value.contains(':') {
        let (platform_name, rest) = deliver_value.split_once(':').unwrap();
        let platform_key = platform_name.to_lowercase();
        let (parsed_chat_id, parsed_thread_id, is_explicit) = parse_target_ref(&platform_key, rest);
        let (mut chat_id, mut thread_id) = if is_explicit {
            (parsed_chat_id, parsed_thread_id)
        } else {
            (rest.to_string(), None)
        };
        // ulnclaw has no channel directory, so human-friendly label
        // resolution (hermes `resolve_channel_name`) is not available —
        // the explicit/verbatim ids are used as-is.
        let _ = &mut chat_id;
        if thread_id.is_none() && platform_key == "slack" {
            if let Some(origin) = origin {
                if origin.platform.to_lowercase() == platform_key
                    && origin.chat_id == chat_id
                    && origin.thread_id.is_some()
                {
                    thread_id = origin.thread_id.clone();
                }
            }
        }
        return Some(DeliveryTarget {
            platform: platform_name.to_string(),
            chat_id,
            thread_id,
        });
    }

    let platform_name = deliver_value;
    if let Some(origin) = origin {
        if origin.platform == platform_name {
            if let Some(chat_id) = home_target_chat_id(platform_name) {
                return Some(DeliveryTarget {
                    platform: platform_name.to_string(),
                    chat_id,
                    thread_id: home_target_thread_id(platform_name),
                });
            }
            return Some(DeliveryTarget {
                platform: platform_name.to_string(),
                chat_id: origin.chat_id.to_string(),
                thread_id: origin.thread_id.clone(),
            });
        }
    }

    if !is_known_delivery_platform(platform_name) {
        return None;
    }
    let chat_id = home_target_chat_id(platform_name)?;
    Some(DeliveryTarget {
        platform: platform_name.to_string(),
        chat_id,
        thread_id: home_target_thread_id(platform_name),
    })
}

/// Expand a routing-intent token to concrete platform names (hermes
/// `_expand_routing_tokens`). `all` expands to every home-target
/// platform with a configured home chat_id right now; anything else
/// passes through unchanged.
pub fn expand_routing_tokens(part: &str) -> Vec<String> {
    let token = part.to_lowercase();
    if !ROUTING_TOKENS.contains(&token.as_str()) {
        return vec![part.to_string()];
    }
    iter_home_target_platforms()
        .filter(|name| home_target_chat_id(name).is_some())
        .map(|name| name.to_string())
        .collect()
}

/// Resolve all concrete auto-delivery targets for a cron job (hermes
/// `_resolve_delivery_targets`): comma-separated deliver values, `all`
/// routing-token expansion, duplicate `(platform, chat_id, thread_id)`
/// tuples collapsed.
pub fn resolve_delivery_targets(job: &CronJob) -> Vec<DeliveryTarget> {
    let deliver = normalize_deliver_value(
        job.deliver
            .as_ref()
            .map(|deliver| serde_json::Value::String(deliver.clone()))
            .as_ref(),
    );
    let mut targets: Vec<DeliveryTarget> = Vec::new();
    for part in deliver.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        for expanded in expand_routing_tokens(part) {
            if let Some(target) = resolve_single_delivery_target(job, &expanded) {
                let duplicate = targets.iter().any(|existing| {
                    existing.platform.to_lowercase() == target.platform.to_lowercase()
                        && existing.chat_id == target.chat_id
                        && existing.thread_id == target.thread_id
                });
                if !duplicate {
                    targets.push(target);
                }
            }
        }
    }
    targets
}

/// Loose silence matcher for autonomous lanes (cron, webhook) —
/// delegates to the canonical gateway response filters (hermes
/// `gateway.response_filters.is_autonomous_silence_response`; shared
/// marker set so the interactive and autonomous rules never drift).
pub fn is_cron_silence_response(response: &str) -> bool {
    crate::response_filters::is_autonomous_silence_response(response)
}

/// Wrap delivered cron output with a job-name header/footer so the user
/// knows this is a cron delivery (hermes `wrap_response` block in
/// `_deliver_result`).
pub fn wrap_delivery_content(job: &CronJob, content: &str) -> String {
    let task_name = if job.name.is_empty() {
        &job.id
    } else {
        &job.name
    };
    format!(
        "Cronjob Response: {}\n(job_id: {})\n-------------\n\n{}\n\nTo stop or manage this job, send me a new message (e.g. \"stop reminder {}\").",
        task_name, job.id, content, task_name
    )
}

/// Compact one-line failure message for chat delivery (hermes
/// `_summarize_cron_failure_for_delivery`): full details stay in the
/// logs; chat shows the operator what broke without dumping provider
/// JSON or stack traces.
pub fn summarize_cron_failure_for_delivery(job: &CronJob, error: Option<&str>) -> String {
    let job_name = if job.name.is_empty() {
        &job.id
    } else {
        &job.name
    };
    let text = error.unwrap_or("unknown error").trim().to_string();
    let lower = text.to_lowercase();

    if text.contains("429") || lower.contains("rate limit") || lower.contains("usage limit") {
        let reason = if lower.contains("weekly usage limit") {
            "weekly usage limit"
        } else if lower.contains("quota") {
            "quota limit"
        } else {
            "rate limit"
        };
        return format!(
            "⚠️ Cron '{}' failed: provider {}. Fallback chain was exhausted or unavailable. Full details saved in cron output.",
            job_name, reason
        );
    }
    if lower.contains("readtimeout") || lower.contains("timed out") || lower.contains("timeout") {
        return format!(
            "⚠️ Cron '{}' failed: provider timeout. Fallback chain was exhausted or unavailable. Full details saved in cron output.",
            job_name
        );
    }
    if lower.contains("authenticat") || lower.contains("authoriz") {
        return format!(
            "⚠️ Cron '{}' failed: provider authentication error. Full details saved in cron output.",
            job_name
        );
    }
    // Whole-token 401/403 match so "oauth", "4015" etc. do not trip the
    // auth message (hermes word-boundary regex).
    let has_auth_code = text
        .split(|c: char| !c.is_ascii_digit())
        .any(|token| token == "401" || token == "403");
    if has_auth_code {
        return format!(
            "⚠️ Cron '{}' failed: provider authentication error. Full details saved in cron output.",
            job_name
        );
    }
    // Generic fallback: first line only, bounded.
    let first_line = text.lines().next().unwrap_or("unknown error").trim();
    let summary: String = first_line.chars().take(200).collect();
    format!(
        "⚠️ Cron '{}' failed: {}. Full details saved in cron output.",
        job_name, summary
    )
}

/// Map a hermes delivery-platform name to the ulnclaw sender-registry
/// key (QQ registers as "qq", WeCom callback mode shares the "wecom"
/// sender).
pub fn sender_key_for(platform_name: &str) -> String {
    match platform_name.to_lowercase().as_str() {
        "qqbot" => "qq".to_string(),
        "wecom_callback" => "wecom".to_string(),
        other => other.to_string(),
    }
}

/// The platforms a cron job can auto-deliver to (hermes
/// `cron_delivery_targets`), filtered to the gateway's connected
/// platforms. Callers prepend the implicit `local` option themselves.
pub fn cron_delivery_targets(connected: &[String]) -> Vec<serde_json::Value> {
    let mut targets = Vec::new();
    for name in iter_home_target_platforms() {
        if !connected.iter().any(|platform| platform == name) {
            continue;
        }
        if !is_known_delivery_platform(name) && name != "whatsapp_cloud" {
            continue;
        }
        let env_var = resolve_home_env_var(name);
        let display = name
            .replace('_', " ")
            .split(' ')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        targets.push(serde_json::json!({
            "id": name,
            "name": display,
            "home_target_set": home_target_chat_id(name).is_some(),
            "home_env_var": env_var,
        }));
    }
    targets
}

/// Derive the connected platform set from the messaging config (hermes
/// `gateway_config.get_connected_platforms()`): a platform counts as
/// connected when its `[messaging.*]` section is enabled.
pub fn connected_messaging_platforms(msg: &crate::messaging::MessagingConfig) -> Vec<String> {
    let mut connected: Vec<(&str, bool)> = vec![
        ("telegram", msg.telegram.enabled),
        ("discord", msg.discord.enabled),
        ("slack", msg.slack.enabled),
        ("signal", msg.signal.enabled),
        ("whatsapp", msg.whatsapp.enabled),
        ("whatsapp_cloud", msg.whatsapp_cloud.enabled),
        ("matrix", msg.matrix.enabled),
        ("mattermost", msg.mattermost.enabled),
        ("homeassistant", msg.homeassistant.enabled),
        ("dingtalk", msg.dingtalk.enabled),
        ("feishu", msg.feishu.enabled),
        ("wecom", msg.wecom.enabled),
        ("weixin", msg.weixin.enabled),
        ("sms", msg.sms.enabled),
        ("email", msg.email.enabled),
        ("bluebubbles", msg.bluebubbles.enabled),
        ("qqbot", msg.qq.enabled),
        ("yuanbao", msg.yuanbao.enabled),
    ];
    // Byte-stable ordering by platform name (hermes sorts by platform
    // value for prompt-cache stability).
    connected.sort_by(|a, b| a.0.cmp(b.0));
    connected
        .into_iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(name, _)| name.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_job(deliver: Option<&str>, origin: Option<JobOrigin>) -> CronJob {
        CronJob {
            id: "job-1".into(),
            name: "digest".into(),
            schedule: "30m".into(),
            prompt: "tick".into(),
            skills: vec![],
            enabled: true,
            repeat: None,
            next_run: None,
            created_at: 0.0,
            last_run: None,
            last_status: None,
            deliver: deliver.map(String::from),
            origin,
            last_delivery_error: None,
            attach_to_session: None,
        }
    }

    fn origin(platform: &str, chat_id: &str, thread_id: Option<&str>) -> JobOrigin {
        JobOrigin {
            platform: platform.into(),
            chat_id: chat_id.into(),
            thread_id: thread_id.map(String::from),
        }
    }

    fn cron_cfg(mirror_delivery: bool) -> crate::config::CronConfig {
        crate::config::CronConfig {
            mirror_delivery,
            ..Default::default()
        }
    }

    #[test]
    fn mirror_gate_precedence_matches_hermes() {
        // Per-job attach_to_session wins over the global setting in
        // both directions; None falls through to the global; global
        // default is off.
        let mut job = test_job(Some("origin"), None);
        assert!(!mirror_delivery_enabled(&job, &cron_cfg(false)));
        assert!(mirror_delivery_enabled(&job, &cron_cfg(true)));
        job.attach_to_session = Some(true);
        assert!(mirror_delivery_enabled(&job, &cron_cfg(false)));
        job.attach_to_session = Some(false);
        assert!(!mirror_delivery_enabled(&job, &cron_cfg(true)));
    }

    #[test]
    fn origin_matcher_scopes_mirroring_to_origin_chat() {
        let no_origin = test_job(Some("all"), None);
        assert!(!target_matches_origin(
            no_origin.origin.as_ref(),
            "telegram",
            "42",
            None
        ));

        let job = test_job(Some("origin"), Some(origin("telegram", "42", None)));
        let origin_ref = job.origin.as_ref();
        // Same chat (case-insensitive platform) matches.
        assert!(target_matches_origin(origin_ref, "Telegram", "42", None));
        // Fan-out targets never match.
        assert!(!target_matches_origin(origin_ref, "telegram", "999", None));
        assert!(!target_matches_origin(origin_ref, "discord", "42", None));

        // A thread-pinned origin requires the same lane.
        let threaded = test_job(Some("origin"), Some(origin("telegram", "42", Some("7"))));
        let origin_ref = threaded.origin.as_ref();
        assert!(target_matches_origin(
            origin_ref,
            "telegram",
            "42",
            Some("7")
        ));
        assert!(!target_matches_origin(
            origin_ref,
            "telegram",
            "42",
            Some("8")
        ));
        assert!(!target_matches_origin(origin_ref, "telegram", "42", None));
    }

    /// Env vars are process-global; tests that mutate them serialize on
    /// this lock (shared with other delivery tests).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_home_envs() {
        for (_, env_var) in HOME_TARGET_ENV_VARS {
            std::env::remove_var(env_var);
        }
        for (_, legacy) in LEGACY_HOME_TARGET_ENV_VARS {
            std::env::remove_var(legacy);
        }
        std::env::remove_var("TELEGRAM_CRON_THREAD_ID");
        std::env::remove_var("TELEGRAM_HOME_CHANNEL_THREAD_ID");
        std::env::remove_var("QQ_HOME_CHANNEL_THREAD_ID");
    }

    #[test]
    fn test_normalize_deliver_value() {
        assert_eq!(normalize_deliver_value(None), "local");
        assert_eq!(normalize_deliver_value(Some(&json!(null))), "local");
        assert_eq!(normalize_deliver_value(Some(&json!(""))), "local");
        assert_eq!(
            normalize_deliver_value(Some(&json!(" telegram "))),
            "telegram"
        );
        assert_eq!(
            normalize_deliver_value(Some(&json!(["telegram", "discord"]))),
            "telegram,discord"
        );
        assert_eq!(normalize_deliver_value(Some(&json!([]))), "local");
        assert_eq!(normalize_deliver_value(Some(&json!([" "]))), "local");
    }

    #[test]
    fn test_is_known_delivery_platform() {
        assert!(is_known_delivery_platform("telegram"));
        assert!(is_known_delivery_platform("QQBOT"));
        assert!(is_known_delivery_platform("wecom_callback"));
        assert!(!is_known_delivery_platform("not-a-platform"));
        assert!(!is_known_delivery_platform("PATH"));
    }

    #[test]
    fn test_home_target_env_resolution_with_legacy_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_home_envs();
        assert_eq!(home_target_chat_id("telegram"), None);
        std::env::set_var("TELEGRAM_HOME_CHANNEL", "-100123");
        assert_eq!(home_target_chat_id("telegram").as_deref(), Some("-100123"));
        // QQ legacy fallback: QQ_HOME_CHANNEL when QQBOT_HOME_CHANNEL unset.
        assert_eq!(home_target_chat_id("qqbot"), None);
        std::env::set_var("QQ_HOME_CHANNEL", "guild-1");
        assert_eq!(home_target_chat_id("qqbot").as_deref(), Some("guild-1"));
        std::env::set_var("QQBOT_HOME_CHANNEL", "guild-2");
        assert_eq!(home_target_chat_id("qqbot").as_deref(), Some("guild-2"));
        clear_home_envs();
    }

    #[test]
    fn test_home_target_thread_id_telegram_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_home_envs();
        std::env::set_var("TELEGRAM_HOME_CHANNEL", "-100123");
        std::env::set_var("TELEGRAM_HOME_CHANNEL_THREAD_ID", "11");
        assert_eq!(home_target_thread_id("telegram").as_deref(), Some("11"));
        std::env::set_var("TELEGRAM_CRON_THREAD_ID", "17");
        assert_eq!(home_target_thread_id("telegram").as_deref(), Some("17"));
        clear_home_envs();
    }

    #[test]
    fn test_resolve_local_and_unknown() {
        let job = test_job(Some("local"), None);
        assert!(resolve_single_delivery_target(&job, "local").is_none());
        let job = test_job(Some("not-a-platform"), None);
        assert!(resolve_single_delivery_target(&job, "not-a-platform").is_none());
    }

    #[test]
    fn test_resolve_origin_target() {
        let job = test_job(
            Some("origin"),
            Some(origin("telegram", "-100123", Some("5"))),
        );
        let target = resolve_single_delivery_target(&job, "origin").unwrap();
        assert_eq!(target.platform, "telegram");
        assert_eq!(target.chat_id, "-100123");
        assert_eq!(target.thread_id.as_deref(), Some("5"));
    }

    #[test]
    fn test_resolve_origin_falls_back_to_home_channel() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_home_envs();
        std::env::set_var("MATRIX_HOME_ROOM", "!room:example.org");
        let job = test_job(Some("origin"), None);
        let target = resolve_single_delivery_target(&job, "origin").unwrap();
        assert_eq!(target.platform, "matrix");
        assert_eq!(target.chat_id, "!room:example.org");
        clear_home_envs();
    }

    #[test]
    fn test_resolve_origin_missing_home_channel_uses_origin_chat() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_home_envs();
        let job = test_job(
            Some("telegram"),
            Some(origin("telegram", "-100123", Some("5"))),
        );
        // No TELEGRAM_HOME_CHANNEL set → falls back to the origin chat.
        let target = resolve_single_delivery_target(&job, "telegram").unwrap();
        assert_eq!(target.chat_id, "-100123");
        assert_eq!(target.thread_id.as_deref(), Some("5"));
        clear_home_envs();
    }

    #[test]
    fn test_resolve_explicit_target_with_thread() {
        let job = test_job(Some("telegram:-100123:17"), None);
        let target = resolve_single_delivery_target(&job, "telegram:-100123:17").unwrap();
        assert_eq!(target.platform, "telegram");
        assert_eq!(target.chat_id, "-100123");
        assert_eq!(target.thread_id.as_deref(), Some("17"));
    }

    #[test]
    fn test_resolve_matrix_explicit_target_keeps_colons() {
        let job = test_job(Some("matrix:!room:example.org"), None);
        let target = resolve_single_delivery_target(&job, "matrix:!room:example.org").unwrap();
        assert_eq!(target.chat_id, "!room:example.org");
        assert_eq!(target.thread_id, None);
    }

    #[test]
    fn test_slack_thread_inherited_from_origin() {
        let job = test_job(
            Some("slack:C123"),
            Some(origin("slack", "C123", Some("1699.42"))),
        );
        let target = resolve_single_delivery_target(&job, "slack:C123").unwrap();
        assert_eq!(target.chat_id, "C123");
        assert_eq!(target.thread_id.as_deref(), Some("1699.42"));
    }

    #[test]
    fn test_resolve_delivery_targets_comma_and_dedup() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_home_envs();
        std::env::set_var("TELEGRAM_HOME_CHANNEL", "-100123");
        let job = test_job(
            Some("origin,telegram,telegram:-100123"),
            Some(origin("telegram", "-100123", None)),
        );
        let targets = resolve_delivery_targets(&job);
        // All three resolve to (telegram, -100123, None) → deduped to 1.
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].chat_id, "-100123");
        clear_home_envs();
    }

    #[test]
    fn test_expand_routing_token_all() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_home_envs();
        std::env::set_var("TELEGRAM_HOME_CHANNEL", "-100123");
        std::env::set_var("DISCORD_HOME_CHANNEL", "456");
        let expanded = expand_routing_tokens("all");
        assert!(expanded.contains(&"telegram".to_string()));
        assert!(expanded.contains(&"discord".to_string()));
        assert_eq!(expanded.len(), 2);
        assert_eq!(
            expand_routing_tokens("telegram"),
            vec!["telegram".to_string()]
        );
        clear_home_envs();
    }

    #[test]
    fn test_silence_response_matching() {
        assert!(is_cron_silence_response("[SILENT]"));
        assert!(is_cron_silence_response("SILENT"));
        assert!(is_cron_silence_response("NO_REPLY"));
        assert!(is_cron_silence_response("no reply"));
        assert!(is_cron_silence_response("  [SILENT]  "));
        assert!(is_cron_silence_response("[SILENT] No changes detected"));
        assert!(is_cron_silence_response("2 deals filtered\n\n[SILENT]"));
        assert!(is_cron_silence_response("[silent] lower-case note"));
        // Mid-sentence tokens deliver.
        assert!(!is_cron_silence_response(
            "I considered staying [SILENT] but here is the summary"
        ));
        assert!(!is_cron_silence_response("Silent retry succeeded"));
        assert!(!is_cron_silence_response(""));
        assert!(!is_cron_silence_response("all good, no changes"));
    }

    #[test]
    fn test_wrap_delivery_content() {
        let job = test_job(Some("telegram"), None);
        let wrapped = wrap_delivery_content(&job, "hello");
        assert!(wrapped.starts_with("Cronjob Response: digest\n(job_id: job-1)"));
        assert!(wrapped.contains("hello"));
        assert!(wrapped.contains("stop reminder digest"));
    }

    #[test]
    fn test_failure_summary_classifies_errors() {
        let job = test_job(None, None);
        let summary = summarize_cron_failure_for_delivery(&job, Some("HTTP 429 too many requests"));
        assert!(summary.contains("rate limit"));
        let summary = summarize_cron_failure_for_delivery(&job, Some("ReadTimeout after 30s"));
        assert!(summary.contains("timeout"));
        let summary =
            summarize_cron_failure_for_delivery(&job, Some("authentication failed for provider"));
        assert!(summary.contains("authentication"));
        let summary = summarize_cron_failure_for_delivery(&job, Some("status 4015 not auth"));
        assert!(summary.contains("status 4015 not auth"));
        let summary = summarize_cron_failure_for_delivery(&job, Some("error 401 unauthorized"));
        assert!(summary.contains("authentication"));
    }

    #[test]
    fn test_sender_key_mapping() {
        assert_eq!(sender_key_for("qqbot"), "qq");
        assert_eq!(sender_key_for("wecom_callback"), "wecom");
        assert_eq!(sender_key_for("Telegram"), "telegram");
    }

    #[test]
    fn test_cron_delivery_targets_filters_connected() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_home_envs();
        std::env::set_var("TELEGRAM_HOME_CHANNEL", "-100123");
        let connected = vec!["telegram".to_string(), "matrix".to_string()];
        let targets = cron_delivery_targets(&connected);
        assert_eq!(targets.len(), 2);
        let ids: Vec<&str> = targets.iter().map(|t| t["id"].as_str().unwrap()).collect();
        // hermes `_HOME_TARGET_ENV_VARS` order: matrix before telegram.
        assert_eq!(ids, vec!["matrix", "telegram"]);
        assert_eq!(targets[1]["home_target_set"], true);
        assert_eq!(targets[0]["home_target_set"], false);
        assert_eq!(targets[1]["home_env_var"], "TELEGRAM_HOME_CHANNEL");
        assert_eq!(targets[1]["name"], "Telegram");
        clear_home_envs();
    }

    #[test]
    fn test_connected_platforms_sorted_and_filtered() {
        let mut msg = crate::messaging::MessagingConfig::default();
        msg.telegram.enabled = true;
        msg.matrix.enabled = true;
        msg.qq.enabled = true;
        let connected = connected_messaging_platforms(&msg);
        assert_eq!(connected, vec!["matrix", "qqbot", "telegram"]);
    }
}
