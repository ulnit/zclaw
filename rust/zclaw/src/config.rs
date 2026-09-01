use serde::Deserialize;

fn default_temp() -> f32 { 0.7 }
fn default_max_iter() -> u32 { 10 }
fn default_prompt() -> String {
    "You are ZClaw, a helpful pocket AI assistant.".to_string()
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentCfg {
    #[serde(default = "default_max_iter")]
    pub max_iterations: u32,
    #[serde(default = "default_prompt")]
    pub system_prompt: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub api_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default = "default_temp")]
    pub temperature: f32,
    #[serde(default)]
    pub workspace_dir: String,
    #[serde(default)]
    pub agent: AgentCfg,
}

impl Config {
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.api_url.trim_end_matches('/'))
    }
}
