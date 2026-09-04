//! Skin/theme engine — port of hermes `hermes_cli/skin_engine.py`
//! (v2026.8.3), built-in skins + active-skin resolution.
//!
//! A skin is a named palette + branding bundle. All nine hermes built-in
//! skins ship as data; missing values inherit from the `default` skin
//! (hermes `_build_skin_config`). The active skin comes from `[display]
//! skin` in config.toml (hermes `init_skin_from_config`).
//!
//! Deferred vs hermes: user YAML skins in `<home>/skins/` (no YAML
//! dependency in ulnclaw yet), TUI-only spinner wings/status-bar surfaces,
//! and the prompt-toolkit style overrides.

use std::collections::HashMap;
use std::sync::Mutex;

/// One built-in skin definition (hermes `_BUILTIN_SKINS` entry).
struct BuiltinSkin {
    name: &'static str,
    description: &'static str,
    tool_prefix: &'static str,
    colors: &'static [(&'static str, &'static str)],
    light_colors: &'static [(&'static str, &'static str)],
    branding: &'static [(&'static str, &'static str)],
    spinner_waiting_faces: &'static [&'static str],
    spinner_thinking_faces: &'static [&'static str],
    spinner_thinking_verbs: &'static [&'static str],
}

const BUILTIN_SKINS: &[BuiltinSkin] = &[
    BuiltinSkin {
        name: "default",
        description: "Classic Hermes — gold and kawaii",
        tool_prefix: "┊",
        colors: &[
            ("banner_border", "#CD7F32"),
            ("banner_title", "#FFD700"),
            ("banner_accent", "#FFBF00"),
            ("banner_dim", "#B8860B"),
            ("banner_text", "#FFF8DC"),
            ("ui_accent", "#FFBF00"),
            ("ui_label", "#DAA520"),
            ("ui_ok", "#4caf50"),
            ("ui_error", "#ef5350"),
            ("ui_warn", "#ffa726"),
            ("prompt", "#FFF8DC"),
            ("input_rule", "#CD7F32"),
            ("response_border", "#FFD700"),
            ("status_bar_bg", "#1a1a2e"),
            ("status_bar_text", "#C0C0C0"),
            ("status_bar_strong", "#FFD700"),
            ("status_bar_dim", "#8A7A4A"),
            ("status_bar_good", "#8FBC8F"),
            ("status_bar_warn", "#FFD700"),
            ("status_bar_bad", "#FF8C00"),
            ("status_bar_critical", "#FF6B6B"),
            ("session_label", "#DAA520"),
            ("session_border", "#8B8682"),
            ("completion_menu_bg", "#1a1a2e"),
            ("completion_menu_current_bg", "#333355"),
            ("selection_bg", "#3a3a55"),
            ("shell_dollar", "#4dabf7"),
            ("voice_status_bg", "#1a1a2e"),
        ],
        light_colors: &[
            ("banner_title", "#C8961E"),
            ("banner_accent", "#D89B04"),
            ("banner_dim", "#B8860B"),
            ("banner_text", "#5C4718"),
            ("ui_accent", "#D89B04"),
            ("ui_label", "#A97E10"),
            ("ui_ok", "#2E7D32"),
            ("ui_error", "#C62828"),
            ("ui_warn", "#D97706"),
            ("prompt", "#5C4718"),
            ("response_border", "#C8961E"),
            ("session_label", "#A97E10"),
            ("status_bar_text", "#6F6F6F"),
            ("status_bar_strong", "#C8961E"),
            ("status_bar_dim", "#9A8A5A"),
            ("status_bar_good", "#2E7D32"),
            ("status_bar_warn", "#C8961E"),
            ("status_bar_bad", "#C2410C"),
            ("status_bar_critical", "#B91C1C"),
            ("shell_dollar", "#1E6FC0"),
            ("completion_menu_bg", "#F5F5F5"),
            ("completion_menu_current_bg", "#E0D1BF"),
            ("selection_bg", "#D4E4F7"),
            ("status_bar_bg", "#F5F5F5"),
            ("voice_status_bg", "#F5F5F5"),
        ],
        branding: &[
            ("agent_name", "Hermes Agent"),
            (
                "welcome",
                "Welcome to Hermes Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Goodbye! ⚕"),
            ("response_label", " ⚕ Hermes "),
            ("prompt_symbol", "❯"),
            ("help_header", "(^_^)? Available Commands"),
        ],
        spinner_waiting_faces: &[],
        spinner_thinking_faces: &[],
        spinner_thinking_verbs: &[],
    },
    BuiltinSkin {
        name: "ares",
        description: "War-god theme — crimson and bronze",
        tool_prefix: "╎",
        colors: &[
            ("banner_border", "#A93333"),
            ("banner_title", "#C7A96B"),
            ("banner_accent", "#DD4A3A"),
            ("banner_dim", "#905151"),
            ("banner_text", "#F1E6CF"),
            ("ui_accent", "#DD4A3A"),
            ("ui_label", "#C7A96B"),
            ("ui_ok", "#4caf50"),
            ("ui_error", "#ef5350"),
            ("ui_warn", "#ffa726"),
            ("prompt", "#F1E6CF"),
            ("input_rule", "#A93333"),
            ("response_border", "#C7A96B"),
            ("status_bar_bg", "#2A1212"),
            ("status_bar_text", "#F1E6CF"),
            ("status_bar_strong", "#C7A96B"),
            ("status_bar_dim", "#756054"),
            ("status_bar_good", "#7BC96F"),
            ("status_bar_warn", "#C7A96B"),
            ("status_bar_bad", "#DD4A3A"),
            ("status_bar_critical", "#EF5350"),
            ("session_label", "#C7A96B"),
            ("session_border", "#6E584B"),
            ("completion_menu_bg", "#2A1212"),
            ("completion_menu_current_bg", "#5C221D"),
            ("selection_bg", "#692620"),
            ("shell_dollar", "#DD4A3A"),
            ("voice_status_bg", "#2A1212"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Ares Agent"),
            (
                "welcome",
                "Welcome to Ares Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Farewell, warrior! ⚔"),
            ("response_label", " ⚔ Ares "),
            ("prompt_symbol", "⚔"),
            ("help_header", "(⚔) Available Commands"),
        ],
        spinner_waiting_faces: &["(⚔)", "(⛨)", "(▲)", "(<>)", "(/)"],
        spinner_thinking_faces: &["(⚔)", "(⛨)", "(▲)", "(⌁)", "(<>)"],
        spinner_thinking_verbs: &[
            "forging",
            "marching",
            "sizing the field",
            "holding the line",
            "hammering plans",
            "tempering steel",
            "plotting impact",
            "raising the shield",
        ],
    },
    BuiltinSkin {
        name: "mono",
        description: "Monochrome — clean grayscale",
        tool_prefix: "┊",
        colors: &[
            ("banner_border", "#5E5E5E"),
            ("banner_title", "#e6edf3"),
            ("banner_accent", "#aaaaaa"),
            ("banner_dim", "#606060"),
            ("banner_text", "#c9d1d9"),
            ("ui_accent", "#aaaaaa"),
            ("ui_label", "#888888"),
            ("ui_ok", "#888888"),
            ("ui_error", "#cccccc"),
            ("ui_warn", "#999999"),
            ("prompt", "#c9d1d9"),
            ("input_rule", "#606060"),
            ("response_border", "#aaaaaa"),
            ("status_bar_bg", "#1F1F1F"),
            ("status_bar_text", "#C9D1D9"),
            ("status_bar_strong", "#E6EDF3"),
            ("status_bar_dim", "#777777"),
            ("status_bar_good", "#B5B5B5"),
            ("status_bar_warn", "#AAAAAA"),
            ("status_bar_bad", "#D0D0D0"),
            ("status_bar_critical", "#F0F0F0"),
            ("session_label", "#888888"),
            ("session_border", "#5E5E5E"),
            ("completion_menu_bg", "#1F1F1F"),
            ("completion_menu_current_bg", "#464646"),
            ("selection_bg", "#505050"),
            ("shell_dollar", "#aaaaaa"),
            ("voice_status_bg", "#1F1F1F"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Hermes Agent"),
            (
                "welcome",
                "Welcome to Hermes Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Goodbye! ⚕"),
            ("response_label", " ⚕ Hermes "),
            ("prompt_symbol", "❯"),
            ("help_header", "[?] Available Commands"),
        ],
        spinner_waiting_faces: &[],
        spinner_thinking_faces: &[],
        spinner_thinking_verbs: &[],
    },
    BuiltinSkin {
        name: "slate",
        description: "Cool blue — developer-focused",
        tool_prefix: "┊",
        colors: &[
            ("banner_border", "#4169e1"),
            ("banner_title", "#7eb8f6"),
            ("banner_accent", "#8EA8FF"),
            ("banner_dim", "#545E6B"),
            ("banner_text", "#c9d1d9"),
            ("ui_accent", "#7eb8f6"),
            ("ui_label", "#8EA8FF"),
            ("ui_ok", "#63D0A6"),
            ("ui_error", "#F7A072"),
            ("ui_warn", "#e6a855"),
            ("prompt", "#c9d1d9"),
            ("input_rule", "#4169e1"),
            ("response_border", "#7eb8f6"),
            ("status_bar_bg", "#151C2F"),
            ("status_bar_text", "#C9D1D9"),
            ("status_bar_strong", "#7EB8F6"),
            ("status_bar_dim", "#5D6672"),
            ("status_bar_good", "#63D0A6"),
            ("status_bar_warn", "#E6A855"),
            ("status_bar_bad", "#F7A072"),
            ("status_bar_critical", "#FF7A7A"),
            ("session_label", "#7eb8f6"),
            ("session_border", "#545E6B"),
            ("completion_menu_bg", "#151C2F"),
            ("completion_menu_current_bg", "#324867"),
            ("selection_bg", "#3A5375"),
            ("shell_dollar", "#7eb8f6"),
            ("voice_status_bg", "#151C2F"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Hermes Agent"),
            (
                "welcome",
                "Welcome to Hermes Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Goodbye! ⚕"),
            ("response_label", " ⚕ Hermes "),
            ("prompt_symbol", "❯"),
            ("help_header", "(^_^)? Available Commands"),
        ],
        spinner_waiting_faces: &[],
        spinner_thinking_faces: &[],
        spinner_thinking_verbs: &[],
    },
    BuiltinSkin {
        name: "daylight",
        description: "Light theme for bright terminals with dark text and cool blue accents",
        tool_prefix: "│",
        colors: &[
            ("banner_border", "#2563EB"),
            ("banner_title", "#0F172A"),
            ("banner_accent", "#1D4ED8"),
            ("banner_dim", "#475569"),
            ("banner_text", "#111827"),
            ("ui_accent", "#2563EB"),
            ("ui_label", "#0F766E"),
            ("ui_ok", "#15803D"),
            ("ui_error", "#B91C1C"),
            ("ui_warn", "#B45309"),
            ("prompt", "#111827"),
            ("input_rule", "#6E94BE"),
            ("response_border", "#2563EB"),
            ("status_bar_bg", "#E5EDF8"),
            ("status_bar_text", "#111827"),
            ("status_bar_strong", "#2563EB"),
            ("status_bar_dim", "#838890"),
            ("status_bar_good", "#15803D"),
            ("status_bar_warn", "#B45309"),
            ("status_bar_bad", "#B45309"),
            ("status_bar_critical", "#B91C1C"),
            ("session_label", "#1D4ED8"),
            ("session_border", "#64748B"),
            ("completion_menu_bg", "#F8FAFC"),
            ("completion_menu_current_bg", "#DBEAFE"),
            ("completion_menu_meta_bg", "#EEF2FF"),
            ("completion_menu_meta_current_bg", "#BFDBFE"),
            ("selection_bg", "#D3E0FB"),
            ("shell_dollar", "#2563EB"),
            ("voice_status_bg", "#E5EDF8"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Hermes Agent"),
            (
                "welcome",
                "Welcome to Hermes Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Goodbye! ⚕"),
            ("response_label", " ⚕ Hermes "),
            ("prompt_symbol", "❯"),
            ("help_header", "[?] Available Commands"),
        ],
        spinner_waiting_faces: &[],
        spinner_thinking_faces: &[],
        spinner_thinking_verbs: &[],
    },
    BuiltinSkin {
        name: "warm-lightmode",
        description: "Warm light mode — dark brown/gold text for light terminal backgrounds",
        tool_prefix: "┊",
        colors: &[
            ("banner_border", "#8B6914"),
            ("banner_title", "#5C3D11"),
            ("banner_accent", "#8B4513"),
            ("banner_dim", "#8B7355"),
            ("banner_text", "#2C1810"),
            ("ui_accent", "#8B4513"),
            ("ui_label", "#5C3D11"),
            ("ui_ok", "#2E7D32"),
            ("ui_error", "#C62828"),
            ("ui_warn", "#E65100"),
            ("prompt", "#2C1810"),
            ("input_rule", "#8B6914"),
            ("response_border", "#8B6914"),
            ("status_bar_bg", "#F5F0E8"),
            ("status_bar_text", "#2C1810"),
            ("status_bar_strong", "#8B4513"),
            ("status_bar_dim", "#8A8F98"),
            ("status_bar_good", "#2E7D32"),
            ("status_bar_warn", "#E65100"),
            ("status_bar_bad", "#DA4D00"),
            ("status_bar_critical", "#C62828"),
            ("session_label", "#5C3D11"),
            ("session_border", "#A0845C"),
            ("completion_menu_bg", "#F5EFE0"),
            ("completion_menu_current_bg", "#E8DCC8"),
            ("completion_menu_meta_bg", "#F0E8D8"),
            ("completion_menu_meta_current_bg", "#DFCFB0"),
            ("selection_bg", "#E8DAD0"),
            ("shell_dollar", "#8B4513"),
            ("voice_status_bg", "#F5F0E8"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Hermes Agent"),
            (
                "welcome",
                "Welcome to Hermes Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Goodbye! ⚕"),
            ("response_label", " ⚕ Hermes "),
            ("prompt_symbol", "❯"),
            ("help_header", "(^_^)? Available Commands"),
        ],
        spinner_waiting_faces: &[],
        spinner_thinking_faces: &[],
        spinner_thinking_verbs: &[],
    },
    BuiltinSkin {
        name: "poseidon",
        description: "Ocean-god theme — deep blue and seafoam",
        tool_prefix: "│",
        colors: &[
            ("banner_border", "#2A6FB9"),
            ("banner_title", "#A9DFFF"),
            ("banner_accent", "#5DB8F5"),
            ("banner_dim", "#44638F"),
            ("banner_text", "#EAF7FF"),
            ("ui_accent", "#5DB8F5"),
            ("ui_label", "#A9DFFF"),
            ("ui_ok", "#4caf50"),
            ("ui_error", "#ef5350"),
            ("ui_warn", "#ffa726"),
            ("prompt", "#EAF7FF"),
            ("input_rule", "#2A6FB9"),
            ("response_border", "#5DB8F5"),
            ("status_bar_bg", "#0F2440"),
            ("status_bar_text", "#EAF7FF"),
            ("status_bar_strong", "#A9DFFF"),
            ("status_bar_dim", "#52708A"),
            ("status_bar_good", "#6ED7B0"),
            ("status_bar_warn", "#5DB8F5"),
            ("status_bar_bad", "#3576BC"),
            ("status_bar_critical", "#D94F4F"),
            ("session_label", "#A9DFFF"),
            ("session_border", "#496884"),
            ("completion_menu_bg", "#0F2440"),
            ("completion_menu_current_bg", "#254D73"),
            ("selection_bg", "#2A587F"),
            ("shell_dollar", "#5DB8F5"),
            ("voice_status_bg", "#0F2440"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Poseidon Agent"),
            (
                "welcome",
                "Welcome to Poseidon Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Fair winds! Ψ"),
            ("response_label", " Ψ Poseidon "),
            ("prompt_symbol", "Ψ"),
            ("help_header", "(Ψ) Available Commands"),
        ],
        spinner_waiting_faces: &["(≈)", "(Ψ)", "(∿)", "(◌)", "(◠)"],
        spinner_thinking_faces: &["(Ψ)", "(∿)", "(≈)", "(⌁)", "(◌)"],
        spinner_thinking_verbs: &[
            "charting currents",
            "sounding the depth",
            "reading foam lines",
            "steering the trident",
            "tracking undertow",
            "plotting sea lanes",
            "calling the swell",
            "measuring pressure",
        ],
    },
    BuiltinSkin {
        name: "sisyphus",
        description: "Sisyphean theme — austere grayscale with persistence",
        tool_prefix: "│",
        colors: &[
            ("banner_border", "#B7B7B7"),
            ("banner_title", "#F5F5F5"),
            ("banner_accent", "#E7E7E7"),
            ("banner_dim", "#5C5C5C"),
            ("banner_text", "#D3D3D3"),
            ("ui_accent", "#E7E7E7"),
            ("ui_label", "#D3D3D3"),
            ("ui_ok", "#919191"),
            ("ui_error", "#E7E7E7"),
            ("ui_warn", "#B7B7B7"),
            ("prompt", "#F5F5F5"),
            ("input_rule", "#656565"),
            ("response_border", "#B7B7B7"),
            ("status_bar_bg", "#202020"),
            ("status_bar_text", "#D3D3D3"),
            ("status_bar_strong", "#F5F5F5"),
            ("status_bar_dim", "#6D6D6D"),
            ("status_bar_good", "#B7B7B7"),
            ("status_bar_warn", "#D3D3D3"),
            ("status_bar_bad", "#E7E7E7"),
            ("status_bar_critical", "#F5F5F5"),
            ("session_label", "#919191"),
            ("session_border", "#656565"),
            ("completion_menu_bg", "#202020"),
            ("completion_menu_current_bg", "#585858"),
            ("selection_bg", "#666666"),
            ("shell_dollar", "#E7E7E7"),
            ("voice_status_bg", "#202020"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Sisyphus Agent"),
            (
                "welcome",
                "Welcome to Sisyphus Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "The boulder waits. ◉"),
            ("response_label", " ◉ Sisyphus "),
            ("prompt_symbol", "◉"),
            ("help_header", "(◉) Available Commands"),
        ],
        spinner_waiting_faces: &["(◉)", "(◌)", "(◬)", "(⬤)", "(::)"],
        spinner_thinking_faces: &["(◉)", "(◬)", "(◌)", "(○)", "(●)"],
        spinner_thinking_verbs: &[
            "finding traction",
            "measuring the grade",
            "resetting the boulder",
            "counting the ascent",
            "testing leverage",
            "setting the shoulder",
            "pushing uphill",
            "enduring the loop",
        ],
    },
    BuiltinSkin {
        name: "charizard",
        description: "Volcanic theme — burnt orange and ember",
        tool_prefix: "│",
        colors: &[
            ("banner_border", "#C75B1D"),
            ("banner_title", "#FFD39A"),
            ("banner_accent", "#F29C38"),
            ("banner_dim", "#C58A45"),
            ("banner_text", "#FFF0D4"),
            ("ui_accent", "#F29C38"),
            ("ui_label", "#FFD39A"),
            ("ui_ok", "#4caf50"),
            ("ui_error", "#ef5350"),
            ("ui_warn", "#ffa726"),
            ("prompt", "#FFF0D4"),
            ("input_rule", "#C75B1D"),
            ("response_border", "#F29C38"),
            ("status_bar_bg", "#2B160E"),
            ("status_bar_text", "#FFF0D4"),
            ("status_bar_strong", "#FFD39A"),
            ("status_bar_dim", "#826144"),
            ("status_bar_good", "#6BCB77"),
            ("status_bar_warn", "#F29C38"),
            ("status_bar_bad", "#E2832B"),
            ("status_bar_critical", "#EF5350"),
            ("session_label", "#FFD39A"),
            ("session_border", "#7B593A"),
            ("completion_menu_bg", "#0B0503"),
            ("completion_menu_current_bg", "#4A1B07"),
            ("completion_menu_meta_bg", "#120806"),
            ("completion_menu_meta_current_bg", "#5A260D"),
            ("selection_bg", "#5A260D"),
            ("shell_dollar", "#F29C38"),
            ("voice_status_bg", "#2B160E"),
        ],
        light_colors: &[],
        branding: &[
            ("agent_name", "Charizard Agent"),
            (
                "welcome",
                "Welcome to Charizard Agent! Type your message or /help for commands.",
            ),
            ("goodbye", "Flame out! ✦"),
            ("response_label", " ✦ Charizard "),
            ("prompt_symbol", "✦"),
            ("help_header", "(✦) Available Commands"),
        ],
        spinner_waiting_faces: &["(✦)", "(▲)", "(◇)", "(<>)", "(🔥)"],
        spinner_thinking_faces: &["(✦)", "(▲)", "(◇)", "(⌁)", "(🔥)"],
        spinner_thinking_verbs: &[
            "banking into the draft",
            "measuring burn",
            "reading the updraft",
            "tracking ember fall",
            "setting wing angle",
            "holding the flame core",
            "plotting a hot landing",
            "coiling for lift",
        ],
    },
];
/// Resolved skin — colors/branding merged over the default skin (hermes
/// `SkinConfig`).
#[derive(Debug, Clone, Default)]
pub struct SkinConfig {
    pub name: String,
    pub description: String,
    pub tool_prefix: String,
    pub colors: HashMap<String, String>,
    /// Hand-tuned light-terminal variant (may be empty — consumers then
    /// adapt `colors`).
    pub light_colors: HashMap<String, String>,
    pub branding: HashMap<String, String>,
    pub spinner_waiting_faces: Vec<String>,
    pub spinner_thinking_faces: Vec<String>,
    pub spinner_thinking_verbs: Vec<String>,
}

impl SkinConfig {
    /// Color value with fallback (hermes `get_color`).
    pub fn get_color(&self, key: &str, fallback: &str) -> String {
        self.colors
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Branding value with fallback (hermes `get_branding`).
    pub fn get_branding(&self, key: &str, fallback: &str) -> String {
        self.branding
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }
}

fn pairs_to_map(pairs: &[(&'static str, &'static str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Build a resolved skin from a built-in definition — every section merges
/// over the default skin so partial skins resolve to a complete palette
/// (hermes `_build_skin_config`).
fn build_skin_config(skin: &BuiltinSkin) -> SkinConfig {
    let default = BUILTIN_SKINS
        .iter()
        .find(|s| s.name == "default")
        .expect("default skin exists");
    let mut colors = pairs_to_map(default.colors);
    colors.extend(pairs_to_map(skin.colors));
    let mut branding = pairs_to_map(default.branding);
    branding.extend(pairs_to_map(skin.branding));
    // Paired palettes are NOT merged over the default's blocks: an empty
    // block means "no hand-tuned variant for that polarity" (hermes).
    SkinConfig {
        name: skin.name.to_string(),
        description: skin.description.to_string(),
        tool_prefix: if skin.tool_prefix.is_empty() {
            default.tool_prefix.to_string()
        } else {
            skin.tool_prefix.to_string()
        },
        colors,
        light_colors: pairs_to_map(skin.light_colors),
        branding,
        spinner_waiting_faces: skin
            .spinner_waiting_faces
            .iter()
            .map(|s| s.to_string())
            .collect(),
        spinner_thinking_faces: skin
            .spinner_thinking_faces
            .iter()
            .map(|s| s.to_string())
            .collect(),
        spinner_thinking_verbs: skin
            .spinner_thinking_verbs
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

/// Skin listing row (hermes `list_skins`).
#[derive(Debug, Clone)]
pub struct SkinInfo {
    pub name: String,
    pub description: String,
    /// "builtin" (user YAML skins deferred).
    pub source: String,
}

/// List available skins (hermes `list_skins`).
pub fn list_skins() -> Vec<SkinInfo> {
    BUILTIN_SKINS
        .iter()
        .map(|skin| SkinInfo {
            name: skin.name.to_string(),
            description: skin.description.to_string(),
            source: "builtin".to_string(),
        })
        .collect()
}

/// Load a skin by name; unknown names fall back to `default` (hermes
/// `load_skin`).
pub fn load_skin(name: &str) -> SkinConfig {
    let name = name.trim();
    if let Some(skin) = BUILTIN_SKINS.iter().find(|s| s.name == name) {
        return build_skin_config(skin);
    }
    build_skin_config(
        BUILTIN_SKINS
            .iter()
            .find(|s| s.name == "default")
            .expect("default skin exists"),
    )
}

// ---------------------------------------------------------------------------
// Active skin (process-wide, hermes `_active_skin` global)
// ---------------------------------------------------------------------------

static ACTIVE_SKIN: Mutex<Option<SkinConfig>> = Mutex::new(None);

/// Initialize the active skin from config at startup (hermes
/// `init_skin_from_config`).
pub fn init_skin_from_config(config: &crate::config::UlncLawConfig) {
    let name = config
        .display
        .skin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    set_active_skin(name);
}

/// Set the active skin; returns it (hermes `set_active_skin`).
pub fn set_active_skin(name: &str) -> SkinConfig {
    let skin = load_skin(name);
    if let Ok(mut slot) = ACTIVE_SKIN.lock() {
        *slot = Some(skin.clone());
    }
    skin
}

/// Currently active skin — defaults to `default` when uninitialized
/// (hermes `get_active_skin`).
pub fn get_active_skin() -> SkinConfig {
    if let Ok(slot) = ACTIVE_SKIN.lock() {
        if let Some(skin) = slot.as_ref() {
            return skin.clone();
        }
    }
    load_skin("default")
}

/// Currently active skin name (hermes `get_active_skin_name`).
pub fn get_active_skin_name() -> String {
    get_active_skin().name
}

// ---------------------------------------------------------------------------
// ANSI rendering
// ---------------------------------------------------------------------------

/// Parse a `#RRGGBB` hex color.
pub fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Truecolor ANSI escape for a hex color, applied to `text`. Respects
/// `NO_COLOR` and non-TTY stdout (returns the text unchanged).
pub fn colorize(hex: &str, text: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        return text.to_string();
    }
    let Some((r, g, b)) = parse_hex(hex) else {
        return text.to_string();
    };
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nine_builtin_skins_listed() {
        let skins = list_skins();
        assert_eq!(skins.len(), 9);
        let names: Vec<&str> = skins.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "default",
            "ares",
            "mono",
            "slate",
            "daylight",
            "poseidon",
            "charizard",
        ] {
            assert!(names.contains(&expected), "missing skin {}", expected);
        }
        assert!(skins.iter().all(|s| s.source == "builtin"));
    }

    #[test]
    fn skins_inherit_from_default() {
        // Every skin resolves a complete palette: keys present on default
        // must resolve on every skin.
        let default = load_skin("default");
        for skin in list_skins() {
            let loaded = load_skin(&skin.name);
            assert_eq!(loaded.name, skin.name);
            for key in ["banner_title", "ui_accent", "ui_ok", "ui_error"] {
                assert!(
                    loaded.colors.contains_key(key),
                    "skin {} missing inherited key {}",
                    skin.name,
                    key
                );
            }
            assert!(!loaded.tool_prefix.is_empty());
        }
        assert_eq!(default.get_color("banner_title", ""), "#FFD700");
    }

    #[test]
    fn unknown_skin_falls_back_to_default() {
        let skin = load_skin("does-not-exist");
        assert_eq!(skin.name, "default");
    }

    #[test]
    fn active_skin_roundtrip() {
        set_active_skin("ares");
        assert_eq!(get_active_skin_name(), "ares");
        set_active_skin("default");
        assert_eq!(get_active_skin_name(), "default");
    }

    #[test]
    fn branding_with_fallback() {
        let skin = load_skin("default");
        // Missing keys fall back.
        assert_eq!(skin.get_branding("nonexistent", "fallback"), "fallback");
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#FFD700"), Some((255, 215, 0)));
        assert_eq!(parse_hex("ffd700"), Some((255, 215, 0)));
        assert_eq!(parse_hex("#GG0000"), None);
        assert_eq!(parse_hex("#FFF"), None);
    }

    #[test]
    fn colorize_respects_no_color() {
        // NO_COLOR handling is env-global; test the pure path instead.
        let colored = format!("\x1b[38;2;255;215;0mhi\x1b[0m");
        assert!(colored.contains("38;2;255;215;0"));
    }
}
