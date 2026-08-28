// Provider/model/API configuration for the standalone tool.
//
// Two independent sources, tried in this order (first non-empty wins per field):
//   1. CLI flags (--provider/--endpoint/--model/--api-key)
//   2. Environment variables (CS_IMAGEINDEX_PROVIDER/_ENDPOINT/_MODEL/_API_KEY)
//   3. --config <file>  (own simple "key = value" format, see CONFIG_EXAMPLE below)
//   4. --legacy-cfg <file>  (reads endpoint2/model2/api_key2 from an existing
//      napp-it cs-aihelp.cfg, for convenience when running inside a csweb-gui
//      install that already has DeepSeek-Vision configured there)
//
// If nothing resolves an endpoint/model, vision description is skipped (same
// "--no-vision" fallback the original Python prototype used), not a hard error.

use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone, Default)]
pub struct VisionConfig {
    pub provider: String, // informational only ("openai-compatible" | "ollama")
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub max_tokens: u32,
}

pub const CONFIG_EXAMPLE: &str = r#"# cs-imageindex.cfg -- own standalone config, independent of any other tool.
provider   = openai-compatible
endpoint   = https://api.deepseek.com/chat/completions
model      = deepseek-v4-flash-vision-exp
api_key    = sk-...
max_tokens = 600
"#;

fn parse_kv_file(path: &str) -> std::io::Result<HashMap<String, String>> {
    let content = fs::read_to_string(path)?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(map)
}

impl VisionConfig {
    /// Load own-format config file (provider/endpoint/model/api_key/max_tokens).
    pub fn from_own_file(path: &str) -> std::io::Result<Self> {
        let m = parse_kv_file(path)?;
        Ok(VisionConfig {
            provider: m.get("provider").cloned().unwrap_or_default(),
            endpoint: m.get("endpoint").cloned().unwrap_or_default(),
            model: m.get("model").cloned().unwrap_or_default(),
            api_key: m.get("api_key").cloned().unwrap_or_default(),
            max_tokens: m
                .get("max_tokens")
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        })
    }

    /// Load legacy cs-aihelp.cfg format (endpoint2/model2/api_key2 -- the
    /// "mode2"/vision slot napp-it CS's AI Helpdesk already uses).
    pub fn from_legacy_cs_aihelp(path: &str) -> std::io::Result<Self> {
        let m = parse_kv_file(path)?;
        Ok(VisionConfig {
            provider: "openai-compatible".to_string(),
            endpoint: m.get("endpoint2").cloned().unwrap_or_default(),
            model: m.get("model2").cloned().unwrap_or_default(),
            api_key: m.get("api_key2").cloned().unwrap_or_default(),
            max_tokens: 600,
        })
    }

    pub fn from_env() -> Self {
        VisionConfig {
            provider: std::env::var("CS_IMAGEINDEX_PROVIDER").unwrap_or_default(),
            endpoint: std::env::var("CS_IMAGEINDEX_ENDPOINT").unwrap_or_default(),
            model: std::env::var("CS_IMAGEINDEX_MODEL").unwrap_or_default(),
            api_key: std::env::var("CS_IMAGEINDEX_API_KEY").unwrap_or_default(),
            max_tokens: std::env::var("CS_IMAGEINDEX_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }
    }

    /// Merge: `self` fields win when non-empty, else fall back to `other`.
    pub fn merged_over(self, other: VisionConfig) -> VisionConfig {
        VisionConfig {
            provider: if self.provider.is_empty() { other.provider } else { self.provider },
            endpoint: if self.endpoint.is_empty() { other.endpoint } else { self.endpoint },
            model: if self.model.is_empty() { other.model } else { self.model },
            api_key: if self.api_key.is_empty() { other.api_key } else { self.api_key },
            max_tokens: if self.max_tokens == 0 { other.max_tokens } else { self.max_tokens },
        }
    }

    pub fn is_usable(&self) -> bool {
        !self.endpoint.is_empty() && !self.model.is_empty()
    }
}
