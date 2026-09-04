//! Per-platform display/verbosity configuration resolver — port of
//! hermes `gateway/display_config.py`.
//!
//! Provides [`resolve`] — the single entry-point for reading display
//! settings with platform-specific overrides and sensible defaults.
//!
//! Resolution order (first hit wins):
//! 1. `[display.platforms.<platform>.<key>]` — explicit per-platform override
//! 2. `[display.tool_progress_overrides.<platform>]` — legacy fallback,
//!    `tool_progress` only
//! 3. `[display.<key>]` — global user setting (except `streaming`, which is
//!    CLI-only; gateway streaming follows the top-level streaming config
//!    unless a per-platform override is set)
//! 4. Built-in per-platform defaults (tiered by platform capability)
//! 5. Built-in global defaults
//!
//! Backward compatibility: `[display.tool_progress_overrides]` is still
//! read as a fallback for `tool_progress` when no `display.platforms`
//! entry exists (hermes config-migration parity).

use crate::config::{DisplayConfig, PlatformDisplayOverride};

/// Overridable display settings (hermes `_GLOBAL_DEFAULTS` keys).
///
/// Other display settings (compact, personality, skin, …) are CLI-only
/// and don't participate in per-platform resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplaySetting {
    /// Tool-progress bubble mode: `off`|`new`|`all`|`verbose`|`log`.
    ToolProgress,
    /// `accumulate` (edit one bubble) or `separate` (one msg per tool).
    ToolProgressGrouping,
    /// Whether reasoning/thinking summaries are rendered.
    ShowReasoning,
    /// Reasoning render style: `code`|`blockquote`|`subtext`.
    ReasoningStyle,
    /// Tool argument preview length (0 = no limit).
    ToolPreviewLength,
    /// Gateway streaming override (`None` = follow top-level streaming).
    Streaming,
    /// Surface real mid-turn assistant commentary.
    InterimAssistantMessages,
    /// Send periodic "still working" heartbeats on long turns.
    LongRunningNotifications,
    /// Include queue-depth/iteration detail in busy acknowledgments.
    BusyAckDetail,
    /// Send a visible ack after a successful mid-turn steer.
    BusySteerAckEnabled,
    /// Delete progress bubbles after the final response lands.
    CleanupProgress,
    /// Live working-state status text: `full`|`verb`|`off`.
    LiveStatus,
}

impl DisplaySetting {
    /// TOML/config key name.
    pub fn key(self) -> &'static str {
        match self {
            DisplaySetting::ToolProgress => "tool_progress",
            DisplaySetting::ToolProgressGrouping => "tool_progress_grouping",
            DisplaySetting::ShowReasoning => "show_reasoning",
            DisplaySetting::ReasoningStyle => "reasoning_style",
            DisplaySetting::ToolPreviewLength => "tool_preview_length",
            DisplaySetting::Streaming => "streaming",
            DisplaySetting::InterimAssistantMessages => "interim_assistant_messages",
            DisplaySetting::LongRunningNotifications => "long_running_notifications",
            DisplaySetting::BusyAckDetail => "busy_ack_detail",
            DisplaySetting::BusySteerAckEnabled => "busy_steer_ack_enabled",
            DisplaySetting::CleanupProgress => "cleanup_progress",
            DisplaySetting::LiveStatus => "live_status",
        }
    }

    /// Canonical set of per-platform overrideable keys (hermes
    /// `OVERRIDEABLE_KEYS`).
    pub fn all() -> &'static [DisplaySetting] {
        &[
            DisplaySetting::ToolProgress,
            DisplaySetting::ToolProgressGrouping,
            DisplaySetting::ShowReasoning,
            DisplaySetting::ReasoningStyle,
            DisplaySetting::ToolPreviewLength,
            DisplaySetting::Streaming,
            DisplaySetting::InterimAssistantMessages,
            DisplaySetting::LongRunningNotifications,
            DisplaySetting::BusyAckDetail,
            DisplaySetting::BusySteerAckEnabled,
            DisplaySetting::CleanupProgress,
            DisplaySetting::LiveStatus,
        ]
    }
}

/// A resolved display-setting value (typed TOML input, normalized).
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayValue {
    Text(String),
    Flag(bool),
    Number(i64),
}

impl DisplayValue {
    pub fn as_flag(&self) -> Option<bool> {
        match self {
            DisplayValue::Flag(flag) => Some(*flag),
            DisplayValue::Text(text) => parse_flag_word(text),
            DisplayValue::Number(num) => Some(*num != 0),
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            DisplayValue::Text(text) => text.clone(),
            DisplayValue::Flag(flag) => flag.to_string(),
            DisplayValue::Number(num) => num.to_string(),
        }
    }

    pub fn as_number(&self) -> Option<i64> {
        match self {
            DisplayValue::Number(num) => Some(*num),
            DisplayValue::Flag(flag) => Some(i64::from(*flag)),
            DisplayValue::Text(text) => text.trim().parse().ok(),
        }
    }
}

fn parse_flag_word(text: &str) -> Option<bool> {
    match text.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "raw" | "verbose" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Built-in global defaults (hermes `_GLOBAL_DEFAULTS`).
pub fn global_default(setting: DisplaySetting) -> Option<DisplayValue> {
    Some(match setting {
        DisplaySetting::ToolProgress => DisplayValue::Text("all".to_string()),
        DisplaySetting::ToolProgressGrouping => DisplayValue::Text("accumulate".to_string()),
        DisplaySetting::ShowReasoning => DisplayValue::Flag(false),
        DisplaySetting::ReasoningStyle => DisplayValue::Text("code".to_string()),
        DisplaySetting::ToolPreviewLength => DisplayValue::Number(0),
        // None = follow top-level streaming config.
        DisplaySetting::Streaming => return None,
        DisplaySetting::InterimAssistantMessages => DisplayValue::Flag(true),
        DisplaySetting::LongRunningNotifications => DisplayValue::Flag(true),
        DisplaySetting::BusyAckDetail => DisplayValue::Flag(true),
        DisplaySetting::BusySteerAckEnabled => DisplayValue::Flag(true),
        DisplaySetting::CleanupProgress => DisplayValue::Flag(false),
        DisplaySetting::LiveStatus => DisplayValue::Text("full".to_string()),
    })
}

/// Built-in per-platform defaults, tiered by platform capability
/// (hermes `_PLATFORM_DEFAULTS`).
///
/// Tier 1 (high): supports message editing, personal/team use.
/// Tier 2 (medium): supports editing, often workspace/customer-facing.
/// Tier 3 (low): no edit support — each progress msg is permanent.
/// Tier 4 (minimal): batch/non-interactive delivery.
pub fn platform_default(platform: &str, setting: DisplaySetting) -> Option<DisplayValue> {
    let key = platform_key(platform);
    match key.as_str() {
        // Tier 1 — full edit support, personal/team use.
        // Telegram is usually a mobile inbox: keep tool_progress quiet
        // and skip the verbose busy-ack counter, but DO surface real
        // mid-turn assistant commentary and periodic heartbeats so the
        // user has signal between turn start and final answer.
        "telegram" => match setting {
            DisplaySetting::ToolProgress => return Some(DisplayValue::Text("off".into())),
            DisplaySetting::BusyAckDetail => return Some(DisplayValue::Flag(false)),
            other => Tier::High.defaults(other),
        },
        // Discord has a native "subtext" primitive (-# small grey
        // text) that reads as metadata rather than content, so
        // reasoning summaries default to it here.
        "discord" => match setting {
            DisplaySetting::ReasoningStyle => return Some(DisplayValue::Text("subtext".into())),
            other => Tier::High.defaults(other),
        },
        // Tier 2 — Slack: tool_progress off by default — Bolt posts
        // cannot be edited like CLI; "new"/"all" spam permanent lines
        // in channels (hermes-agent#14663).
        "slack" => match setting {
            DisplaySetting::ToolProgress => return Some(DisplayValue::Text("off".into())),
            DisplaySetting::LongRunningNotifications => return Some(DisplayValue::Flag(false)),
            DisplaySetting::BusyAckDetail => return Some(DisplayValue::Flag(false)),
            other => Tier::Medium.defaults(other),
        },
        "mattermost" | "matrix" | "feishu" => Tier::Medium.defaults(setting),
        // WhatsApp's Baileys bridge supports /edit → tier 2.
        "whatsapp" => Tier::Medium.defaults(setting),
        // Tier 3 — no edit support, progress messages are permanent.
        "signal" => Tier::Low.defaults(setting),
        "weixin" | "wecom" | "wecom_callback" | "dingtalk" => Tier::Low.defaults(setting),
        // Tier 4 — batch or non-interactive delivery.
        "email" | "sms" | "webhook" | "homeassistant" => Tier::Minimal.defaults(setting),
        // OpenAI-compatible API surface: full features, no previews.
        "api_server" => match setting {
            DisplaySetting::ToolPreviewLength => return Some(DisplayValue::Number(0)),
            other => Tier::High.defaults(other),
        },
        // Unlisted platforms inherit the global defaults (hermes
        // parity).
        _ => None,
    }
}

enum Tier {
    High,
    Medium,
    Low,
    Minimal,
}

impl Tier {
    fn defaults(self, setting: DisplaySetting) -> Option<DisplayValue> {
        match self {
            Tier::High => Some(match setting {
                DisplaySetting::ToolProgress => DisplayValue::Text("all".into()),
                DisplaySetting::ShowReasoning => DisplayValue::Flag(false),
                DisplaySetting::ToolPreviewLength => DisplayValue::Number(40),
                DisplaySetting::Streaming => return None, // follow global
                DisplaySetting::InterimAssistantMessages => DisplayValue::Flag(true),
                DisplaySetting::LongRunningNotifications => DisplayValue::Flag(true),
                DisplaySetting::BusyAckDetail => DisplayValue::Flag(true),
                other => return global_default(other),
            }),
            Tier::Medium => Some(match setting {
                DisplaySetting::ToolProgress => DisplayValue::Text("new".into()),
                DisplaySetting::ShowReasoning => DisplayValue::Flag(false),
                DisplaySetting::ToolPreviewLength => DisplayValue::Number(40),
                DisplaySetting::Streaming => return None,
                DisplaySetting::InterimAssistantMessages => DisplayValue::Flag(true),
                DisplaySetting::LongRunningNotifications => DisplayValue::Flag(true),
                DisplaySetting::BusyAckDetail => DisplayValue::Flag(true),
                other => return global_default(other),
            }),
            Tier::Low => Some(match setting {
                DisplaySetting::ToolProgress => DisplayValue::Text("off".into()),
                DisplaySetting::ShowReasoning => DisplayValue::Flag(false),
                DisplaySetting::ToolPreviewLength => DisplayValue::Number(40),
                DisplaySetting::Streaming => DisplayValue::Flag(false),
                DisplaySetting::InterimAssistantMessages => DisplayValue::Flag(false),
                DisplaySetting::LongRunningNotifications => DisplayValue::Flag(false),
                DisplaySetting::BusyAckDetail => DisplayValue::Flag(false),
                other => return global_default(other),
            }),
            Tier::Minimal => Some(match setting {
                DisplaySetting::ToolProgress => DisplayValue::Text("off".into()),
                DisplaySetting::ShowReasoning => DisplayValue::Flag(false),
                DisplaySetting::ToolPreviewLength => DisplayValue::Number(0),
                DisplaySetting::Streaming => DisplayValue::Flag(false),
                DisplaySetting::InterimAssistantMessages => DisplayValue::Flag(false),
                DisplaySetting::LongRunningNotifications => DisplayValue::Flag(false),
                DisplaySetting::BusyAckDetail => DisplayValue::Flag(false),
                other => return global_default(other),
            }),
        }
    }
}

/// Normalize a platform key the way hermes `_platform_config_key` does.
pub fn platform_key(platform: &str) -> String {
    platform.trim().to_lowercase()
}

fn override_value(
    override_cfg: &PlatformDisplayOverride,
    setting: DisplaySetting,
) -> Option<DisplayValue> {
    Some(match setting {
        DisplaySetting::ToolProgress => DisplayValue::Text(override_cfg.tool_progress.clone()?),
        DisplaySetting::ToolProgressGrouping => {
            DisplayValue::Text(override_cfg.tool_progress_grouping.clone()?)
        }
        DisplaySetting::ShowReasoning => DisplayValue::Flag(override_cfg.show_reasoning?),
        DisplaySetting::ReasoningStyle => DisplayValue::Text(override_cfg.reasoning_style.clone()?),
        DisplaySetting::ToolPreviewLength => {
            DisplayValue::Number(override_cfg.tool_preview_length?)
        }
        DisplaySetting::Streaming => DisplayValue::Flag(override_cfg.streaming?),
        DisplaySetting::InterimAssistantMessages => {
            DisplayValue::Flag(override_cfg.interim_assistant_messages?)
        }
        DisplaySetting::LongRunningNotifications => {
            match override_cfg.long_running_notifications.as_ref()? {
                crate::config::BoolOrMode::Flag(flag) => DisplayValue::Flag(*flag),
                crate::config::BoolOrMode::Mode(mode) => DisplayValue::Text(mode.clone()),
            }
        }
        DisplaySetting::BusyAckDetail => DisplayValue::Flag(override_cfg.busy_ack_detail?),
        DisplaySetting::BusySteerAckEnabled => {
            DisplayValue::Flag(override_cfg.busy_steer_ack_enabled?)
        }
        DisplaySetting::CleanupProgress => DisplayValue::Flag(override_cfg.cleanup_progress?),
        DisplaySetting::LiveStatus => DisplayValue::Text(override_cfg.live_status.clone()?),
    })
}

fn global_value(display: &DisplayConfig, setting: DisplaySetting) -> Option<DisplayValue> {
    Some(match setting {
        DisplaySetting::ToolProgress => DisplayValue::Text(display.tool_progress.clone()?),
        DisplaySetting::ToolProgressGrouping => {
            DisplayValue::Text(display.tool_progress_grouping.clone()?)
        }
        DisplaySetting::ShowReasoning => DisplayValue::Flag(display.show_reasoning?),
        DisplaySetting::ReasoningStyle => DisplayValue::Text(display.reasoning_style.clone()?),
        DisplaySetting::ToolPreviewLength => DisplayValue::Number(display.tool_preview_length?),
        DisplaySetting::Streaming => DisplayValue::Flag(display.streaming?),
        DisplaySetting::InterimAssistantMessages => {
            DisplayValue::Flag(display.interim_assistant_messages?)
        }
        DisplaySetting::LongRunningNotifications => {
            match display.long_running_notifications.as_ref()? {
                crate::config::BoolOrMode::Flag(flag) => DisplayValue::Flag(*flag),
                crate::config::BoolOrMode::Mode(mode) => DisplayValue::Text(mode.clone()),
            }
        }
        DisplaySetting::BusyAckDetail => DisplayValue::Flag(display.busy_ack_detail?),
        DisplaySetting::BusySteerAckEnabled => DisplayValue::Flag(display.busy_steer_ack_enabled?),
        DisplaySetting::CleanupProgress => DisplayValue::Flag(display.cleanup_progress?),
        DisplaySetting::LiveStatus => DisplayValue::Text(display.live_status.clone()?),
    })
}

/// Normalize config quirks (hermes `_normalise`).
pub fn normalise(setting: DisplaySetting, value: DisplayValue) -> DisplayValue {
    match setting {
        DisplaySetting::ToolProgress => {
            let text = match &value {
                DisplayValue::Flag(false) => return DisplayValue::Text("off".into()),
                DisplayValue::Flag(true) => return DisplayValue::Text("all".into()),
                DisplayValue::Number(num) => {
                    if *num == 0 {
                        return DisplayValue::Text("off".into());
                    }
                    return DisplayValue::Text("all".into());
                }
                DisplayValue::Text(text) => text.trim().to_lowercase(),
            };
            let mode = match text.as_str() {
                "false" | "0" | "no" => "off",
                "true" | "1" | "yes" | "on" => "all",
                "off" | "new" | "all" | "verbose" | "log" => return DisplayValue::Text(text),
                _ => "all",
            };
            DisplayValue::Text(mode.into())
        }
        DisplaySetting::ShowReasoning
        | DisplaySetting::Streaming
        | DisplaySetting::InterimAssistantMessages
        | DisplaySetting::LongRunningNotifications
        | DisplaySetting::BusyAckDetail
        | DisplaySetting::BusySteerAckEnabled => {
            if let DisplayValue::Text(text) = &value {
                let word = text.trim().to_lowercase();
                // "generic" is a visibility mode for long-running
                // notifications (hermes parity) — keep it textual.
                if word == "generic" && setting == DisplaySetting::LongRunningNotifications {
                    return DisplayValue::Text("generic".into());
                }
            }
            DisplayValue::Flag(value.as_flag().unwrap_or(false))
        }
        DisplaySetting::CleanupProgress => DisplayValue::Flag(value.as_flag().unwrap_or(false)),
        DisplaySetting::LiveStatus => {
            let text = match &value {
                DisplayValue::Flag(true) => return DisplayValue::Text("full".into()),
                DisplayValue::Flag(false) => return DisplayValue::Text("off".into()),
                DisplayValue::Number(num) => {
                    return DisplayValue::Text(if *num == 0 { "off" } else { "full" }.into())
                }
                DisplayValue::Text(text) => text.trim().to_lowercase(),
            };
            let mode = match text.as_str() {
                "true" | "1" | "yes" | "on" | "all" => "full",
                "false" | "0" | "no" => "off",
                "full" | "verb" | "off" => return DisplayValue::Text(text),
                _ => "full",
            };
            DisplayValue::Text(mode.into())
        }
        DisplaySetting::ToolProgressGrouping => {
            let text = value.as_text().to_lowercase();
            DisplayValue::Text(match text.as_str() {
                "accumulate" | "separate" => text,
                _ => "accumulate".into(),
            })
        }
        DisplaySetting::ReasoningStyle => {
            let text = value.as_text().to_lowercase();
            DisplayValue::Text(match text.as_str() {
                "code" | "blockquote" | "subtext" => text,
                _ => "code".into(),
            })
        }
        DisplaySetting::ToolPreviewLength => DisplayValue::Number(value.as_number().unwrap_or(0)),
    }
}

/// Resolve a display setting with per-platform override support (hermes
/// `resolve_display_setting`).
///
/// `platform` is the platform config key (e.g. `"telegram"`, `"slack"`);
/// `None` skips the per-platform layers.
pub fn resolve(
    display: &DisplayConfig,
    platform: Option<&str>,
    setting: DisplaySetting,
) -> Option<DisplayValue> {
    let platform = platform.map(platform_key);
    // 1. Explicit per-platform override.
    if let Some(key) = platform.as_deref() {
        if let Some(override_cfg) = display.platforms.get(key) {
            if let Some(value) = override_value(override_cfg, setting) {
                return Some(normalise(setting, value));
            }
        }
        // 1b. Backward compat: display.tool_progress_overrides.<platform>.
        if setting == DisplaySetting::ToolProgress {
            if let Some(value) = display.tool_progress_overrides.get(key) {
                return Some(normalise(setting, DisplayValue::Text(value.clone())));
            }
        }
    }
    // 2. Global user setting. Skip `streaming` — that key controls only
    // CLI terminal streaming; gateway token streaming is governed by the
    // top-level streaming config plus per-platform overrides.
    if setting != DisplaySetting::Streaming {
        if let Some(value) = global_value(display, setting) {
            return Some(normalise(setting, value));
        }
    }
    // 3. Built-in platform default.
    if let Some(key) = platform.as_deref() {
        if let Some(value) = platform_default(key, setting) {
            return Some(value);
        }
    }
    // 4. Built-in global default.
    global_default(setting)
}

/// Resolve with an explicit fallback for settings with no built-in
/// default (`streaming`) — hermes `fallback` parameter parity.
pub fn resolve_or(
    display: &DisplayConfig,
    platform: Option<&str>,
    setting: DisplaySetting,
    fallback: DisplayValue,
) -> DisplayValue {
    resolve(display, platform, setting).unwrap_or(fallback)
}

/// Boolean convenience accessor (normalized; missing = built-in default,
/// unknown platform = global default).
pub fn resolve_flag(
    display: &DisplayConfig,
    platform: Option<&str>,
    setting: DisplaySetting,
) -> bool {
    resolve(display, platform, setting)
        .and_then(|value| value.as_flag())
        .unwrap_or(false)
}

/// Text convenience accessor.
pub fn resolve_text(
    display: &DisplayConfig,
    platform: Option<&str>,
    setting: DisplaySetting,
) -> String {
    resolve(display, platform, setting)
        .map(|value| value.as_text())
        .unwrap_or_default()
}

/// Integer convenience accessor.
pub fn resolve_int(
    display: &DisplayConfig,
    platform: Option<&str>,
    setting: DisplaySetting,
) -> i64 {
    resolve(display, platform, setting)
        .and_then(|value| value.as_number())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display_from_toml(toml_text: &str) -> DisplayConfig {
        let config: crate::config::UlncLawConfig = toml::from_str(toml_text).unwrap();
        config.display
    }

    #[test]
    fn global_defaults_match_hermes() {
        let display = DisplayConfig::default();
        assert_eq!(
            resolve(&display, None, DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("all".into()))
        );
        assert_eq!(
            resolve(&display, None, DisplaySetting::ToolProgressGrouping),
            Some(DisplayValue::Text("accumulate".into()))
        );
        assert_eq!(
            resolve(&display, None, DisplaySetting::ShowReasoning),
            Some(DisplayValue::Flag(false))
        );
        assert_eq!(
            resolve(&display, None, DisplaySetting::ReasoningStyle),
            Some(DisplayValue::Text("code".into()))
        );
        assert_eq!(
            resolve(&display, None, DisplaySetting::ToolPreviewLength),
            Some(DisplayValue::Number(0))
        );
        // streaming has no built-in default (follows top-level config).
        assert_eq!(resolve(&display, None, DisplaySetting::Streaming), None);
        assert_eq!(
            resolve(&display, None, DisplaySetting::InterimAssistantMessages),
            Some(DisplayValue::Flag(true))
        );
        assert_eq!(
            resolve(&display, None, DisplaySetting::BusyAckDetail),
            Some(DisplayValue::Flag(true))
        );
        assert_eq!(
            resolve(&display, None, DisplaySetting::CleanupProgress),
            Some(DisplayValue::Flag(false))
        );
        assert_eq!(
            resolve(&display, None, DisplaySetting::LiveStatus),
            Some(DisplayValue::Text("full".into()))
        );
    }

    #[test]
    fn tiered_platform_defaults() {
        let display = DisplayConfig::default();
        // Telegram: tier-1 base but tool_progress off + no busy detail.
        assert_eq!(
            resolve(&display, Some("telegram"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("off".into()))
        );
        assert!(!resolve_flag(
            &display,
            Some("telegram"),
            DisplaySetting::BusyAckDetail
        ));
        assert!(resolve_flag(
            &display,
            Some("telegram"),
            DisplaySetting::LongRunningNotifications
        ));
        assert_eq!(
            resolve(
                &display,
                Some("telegram"),
                DisplaySetting::ToolPreviewLength
            ),
            Some(DisplayValue::Number(40))
        );
        // Discord: reasoning_style subtext.
        assert_eq!(
            resolve(&display, Some("discord"), DisplaySetting::ReasoningStyle),
            Some(DisplayValue::Text("subtext".into()))
        );
        // Slack: medium tier with progress/heartbeats/busy-detail off.
        assert_eq!(
            resolve(&display, Some("slack"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("off".into()))
        );
        assert!(!resolve_flag(
            &display,
            Some("slack"),
            DisplaySetting::LongRunningNotifications
        ));
        // Mattermost: tier-2 "new" progress.
        assert_eq!(
            resolve(&display, Some("mattermost"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("new".into()))
        );
        // Signal: tier-3 — streaming explicitly off.
        assert_eq!(
            resolve(&display, Some("signal"), DisplaySetting::Streaming),
            Some(DisplayValue::Flag(false))
        );
        assert!(!resolve_flag(
            &display,
            Some("signal"),
            DisplaySetting::InterimAssistantMessages
        ));
        // Email: minimal tier — preview length 0.
        assert_eq!(
            resolve(&display, Some("email"), DisplaySetting::ToolPreviewLength),
            Some(DisplayValue::Number(0))
        );
        // api_server: high tier, previews off.
        assert_eq!(
            resolve(
                &display,
                Some("api_server"),
                DisplaySetting::ToolPreviewLength
            ),
            Some(DisplayValue::Number(0))
        );
        assert_eq!(
            resolve(&display, Some("api_server"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("all".into()))
        );
        // Unlisted platform inherits global defaults.
        assert_eq!(
            resolve(&display, Some("irc"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("all".into()))
        );
    }

    #[test]
    fn platform_override_wins_over_global() {
        let display = display_from_toml(
            r#"
[display]
tool_progress = "new"
busy_ack_detail = false

[display.platforms.telegram]
tool_progress = "verbose"
busy_ack_detail = true
"#,
        );
        assert_eq!(
            resolve(&display, Some("telegram"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("verbose".into()))
        );
        assert!(resolve_flag(
            &display,
            Some("telegram"),
            DisplaySetting::BusyAckDetail
        ));
        // Global applies where no platform override exists.
        assert_eq!(
            resolve(&display, Some("discord"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("new".into()))
        );
        assert!(!resolve_flag(
            &display,
            Some("discord"),
            DisplaySetting::BusyAckDetail
        ));
        // No platform → global.
        assert_eq!(
            resolve(&display, None, DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("new".into()))
        );
    }

    #[test]
    fn legacy_tool_progress_overrides_fallback() {
        let display = display_from_toml(
            r#"
[display.tool_progress_overrides]
slack = "all"
telegram = "off"
"#,
        );
        assert_eq!(
            resolve(&display, Some("slack"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("all".into()))
        );
        // Explicit platforms entry beats the legacy map.
        let display2 = display_from_toml(
            r#"
[display.tool_progress_overrides]
slack = "all"

[display.platforms.slack]
tool_progress = "new"
"#,
        );
        assert_eq!(
            resolve(&display2, Some("slack"), DisplaySetting::ToolProgress),
            Some(DisplayValue::Text("new".into()))
        );
    }

    #[test]
    fn streaming_global_is_skipped() {
        // display.streaming is CLI-only; the gateway ignores the global
        // key and keeps following the top-level streaming config.
        let display = display_from_toml(
            r#"
[display]
streaming = true

[display.platforms.signal]
streaming = true
"#,
        );
        assert_eq!(resolve(&display, None, DisplaySetting::Streaming), None);
        assert_eq!(
            resolve(&display, Some("discord"), DisplaySetting::Streaming),
            None
        );
        // Explicit per-platform override still wins.
        assert_eq!(
            resolve(&display, Some("signal"), DisplaySetting::Streaming),
            Some(DisplayValue::Flag(true))
        );
    }

    #[test]
    fn normalisation_rules() {
        // tool_progress: bool/word quirks → modes.
        assert_eq!(
            normalise(DisplaySetting::ToolProgress, DisplayValue::Flag(false)),
            DisplayValue::Text("off".into())
        );
        assert_eq!(
            normalise(DisplaySetting::ToolProgress, DisplayValue::Flag(true)),
            DisplayValue::Text("all".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::ToolProgress,
                DisplayValue::Text("NO".into())
            ),
            DisplayValue::Text("off".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::ToolProgress,
                DisplayValue::Text("bogus".into())
            ),
            DisplayValue::Text("all".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::ToolProgress,
                DisplayValue::Text("log".into())
            ),
            DisplayValue::Text("log".into())
        );
        // live_status tri-state.
        assert_eq!(
            normalise(DisplaySetting::LiveStatus, DisplayValue::Flag(true)),
            DisplayValue::Text("full".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::LiveStatus,
                DisplayValue::Text("verb".into())
            ),
            DisplayValue::Text("verb".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::LiveStatus,
                DisplayValue::Text("weird".into())
            ),
            DisplayValue::Text("full".into())
        );
        // long_running_notifications keeps the "generic" visibility mode.
        assert_eq!(
            normalise(
                DisplaySetting::LongRunningNotifications,
                DisplayValue::Text("generic".into())
            ),
            DisplayValue::Text("generic".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::LongRunningNotifications,
                DisplayValue::Text("on".into())
            ),
            DisplayValue::Flag(true)
        );
        // reasoning_style + grouping fall back on unknown values.
        assert_eq!(
            normalise(
                DisplaySetting::ReasoningStyle,
                DisplayValue::Text("fancy".into())
            ),
            DisplayValue::Text("code".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::ToolProgressGrouping,
                DisplayValue::Text("separate".into())
            ),
            DisplayValue::Text("separate".into())
        );
        assert_eq!(
            normalise(
                DisplaySetting::ToolProgressGrouping,
                DisplayValue::Text("nope".into())
            ),
            DisplayValue::Text("accumulate".into())
        );
        // tool_preview_length parses text.
        assert_eq!(
            normalise(
                DisplaySetting::ToolPreviewLength,
                DisplayValue::Text("64".into())
            ),
            DisplayValue::Number(64)
        );
        assert_eq!(
            normalise(
                DisplaySetting::ToolPreviewLength,
                DisplayValue::Text("x".into())
            ),
            DisplayValue::Number(0)
        );
    }

    #[test]
    fn platform_keys_are_case_insensitive() {
        let display = display_from_toml(
            r#"
[display.platforms.telegram]
busy_ack_detail = true
"#,
        );
        assert!(resolve_flag(
            &display,
            Some("Telegram"),
            DisplaySetting::BusyAckDetail
        ));
        assert!(resolve_flag(
            &display,
            Some(" TELEGRAM "),
            DisplaySetting::BusyAckDetail
        ));
    }

    #[test]
    fn resolve_or_fallback_for_streaming() {
        let display = DisplayConfig::default();
        assert_eq!(
            resolve_or(
                &display,
                Some("discord"),
                DisplaySetting::Streaming,
                DisplayValue::Flag(true)
            ),
            DisplayValue::Flag(true)
        );
    }
}
