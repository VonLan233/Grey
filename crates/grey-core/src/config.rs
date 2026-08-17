//! Configuration system: defaults < TOML file < environment < CLI overrides.
//!
//! P2: dynamic `[[providers]]` table with backward-compatible legacy
//! `[openai]`/`[anthropic]` sections auto-migrated at load time.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

/// Secret-ish fields are masked in `config show` output.
const SECRET_FIELDS: &[&str] = &["api_key", "token", "secret"];

// ---------------------------------------------------------------------------
// P2: dynamic provider registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderEntry {
    pub id: String,
    pub protocol: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub max_tokens: u32,
    #[serde(default)]
    pub include_usage: bool,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_limit: u64,
    #[serde(default)]
    pub output_limit: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Planning,
    Coding,
    Fast,
    Default,
}

impl Default for TaskKind {
    fn default() -> Self {
        Self::Default
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouteRule {
    #[serde(rename = "match")]
    pub match_kind: TaskKind,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FallbackConfig {
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub models: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub max_tokens: u64,
    pub system_budget: u64,
    pub history_budget: u64,
    pub tool_output_budget: u64,
    pub input_budget: u64,
    pub summary_threshold: usize,
    pub summary_max_messages: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            system_budget: 4_096,
            history_budget: 65_536,
            tool_output_budget: 16_384,
            input_budget: 32_768,
            summary_threshold: 20,
            summary_max_messages: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    pub ttl_hours: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 1_000,
            ttl_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageConfig {
    pub track: bool,
    #[serde(default)]
    pub cost_per_1m_input: HashMap<String, f64>,
    #[serde(default)]
    pub cost_per_1m_output: HashMap<String, f64>,
}

// ---------------------------------------------------------------------------
// Legacy config structs (kept for backward compatibility)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub include_usage: bool,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            include_usage: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub version: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    pub rust_analyzer: String,
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GreyConfig {
    /// P2: default provider id for dynamic registry.
    pub default_provider: String,
    /// P2: default model name.
    pub default_model: String,
    /// P2: dynamic provider table.
    pub providers: Vec<ProviderEntry>,
    /// P2: task routing rules.
    pub routes: Vec<RouteRule>,
    /// P2: fallback configuration.
    pub fallback: FallbackConfig,
    /// P2: context / token budget configuration.
    pub context: ContextConfig,
    /// P2: request cache configuration.
    pub cache: CacheConfig,
    /// P2: usage tracking configuration.
    pub usage: UsageConfig,

    // Legacy fields (kept for backward compat; migrated into `providers`).
    pub provider: String,
    pub model: String,
    pub openai: OpenAiConfig,
    pub anthropic: AnthropicConfig,
    pub lsp: LspConfig,
}

impl Default for GreyConfig {
    fn default() -> Self {
        Self {
            default_provider: "mock".into(),
            default_model: "grey-default".into(),
            providers: vec![ProviderEntry {
                id: "mock".into(),
                protocol: "mock".into(),
                ..Default::default()
            }],
            routes: Vec::new(),
            fallback: FallbackConfig::default(),
            context: ContextConfig::default(),
            cache: CacheConfig::default(),
            usage: UsageConfig::default(),
            provider: "mock".into(),
            model: "grey-default".into(),
            openai: OpenAiConfig {
                base_url: "http://localhost:11434/v1".into(),
                api_key: String::new(),
                model: "qwen2.5:7b".into(),
                include_usage: true,
            },
            anthropic: AnthropicConfig {
                base_url: "https://api.anthropic.com/v1".into(),
                api_key: String::new(),
                model: "claude-sonnet-4-5".into(),
                version: "2023-06-01".into(),
                max_tokens: 4096,
            },
            lsp: LspConfig {
                rust_analyzer: "rust-analyzer".into(),
            },
        }
    }
}

impl GreyConfig {
    /// Mask secret fields for display.
    pub fn redacted(&self) -> Self {
        let mut c = self.clone();
        if !c.openai.api_key.is_empty() {
            c.openai.api_key = "***".into();
        }
        if !c.anthropic.api_key.is_empty() {
            c.anthropic.api_key = "***".into();
        }
        for p in c.providers.iter_mut() {
            if !p.api_key.is_empty() {
                p.api_key = "***".into();
            }
        }
        c
    }

    /// Look up a provider entry by id.
    pub fn provider(&self, id: &str) -> Option<&ProviderEntry> {
        self.providers.iter().find(|p| p.id == id)
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

pub fn config_path() -> Option<PathBuf> {
    if let Ok(p) = env::var("GREY_CONFIG") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let local = PathBuf::from("grey.toml");
    if local.exists() {
        return Some(local);
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        let p = home.join(".config/grey/grey.toml");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = env::var_os("GREY_CONFIG") {
        return PathBuf::from(path);
    }
    match env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/grey/grey.toml"),
        None => PathBuf::from("grey.toml"),
    }
}

pub fn load() -> Result<GreyConfig> {
    let mut cfg = GreyConfig::default();
    if let Some(path) = config_path() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let file_cfg: GreyConfig =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        cfg = merge(cfg, file_cfg);
    }
    migrate_legacy(&mut cfg);
    apply_env(&mut cfg)?;
    Ok(cfg)
}

fn merge(mut base: GreyConfig, over: GreyConfig) -> GreyConfig {
    if !over.default_provider.is_empty() {
        base.default_provider = over.default_provider;
    }
    if !over.default_model.is_empty() {
        base.default_model = over.default_model;
    }
    if !over.providers.is_empty() {
        base.providers = over.providers;
    }
    if !over.routes.is_empty() {
        base.routes = over.routes;
    }
    if !over.fallback.providers.is_empty() {
        base.fallback.providers = over.fallback.providers;
    }
    if !over.fallback.models.is_empty() {
        base.fallback.models = over.fallback.models;
    }
    // Legacy fields
    if !over.provider.is_empty() {
        base.provider = over.provider;
    }
    if !over.model.is_empty() {
        base.model = over.model;
    }
    if !over.openai.base_url.is_empty() {
        base.openai.base_url = over.openai.base_url;
    }
    if !over.openai.api_key.is_empty() {
        base.openai.api_key = over.openai.api_key;
    }
    if !over.openai.model.is_empty() {
        base.openai.model = over.openai.model;
    }
    base.openai.include_usage = over.openai.include_usage;
    if !over.anthropic.base_url.is_empty() {
        base.anthropic.base_url = over.anthropic.base_url;
    }
    if !over.anthropic.api_key.is_empty() {
        base.anthropic.api_key = over.anthropic.api_key;
    }
    if !over.anthropic.model.is_empty() {
        base.anthropic.model = over.anthropic.model;
    }
    if !over.anthropic.version.is_empty() {
        base.anthropic.version = over.anthropic.version;
    }
    if over.anthropic.max_tokens != 0 {
        base.anthropic.max_tokens = over.anthropic.max_tokens;
    }
    if !over.lsp.rust_analyzer.is_empty() {
        base.lsp.rust_analyzer = over.lsp.rust_analyzer;
    }
    base
}

fn migrate_legacy(cfg: &mut GreyConfig) {
    let has_openai = cfg.providers.iter().any(|p| p.id == "openai");
    let has_anthropic = cfg.providers.iter().any(|p| p.id == "anthropic");

    if !has_openai && (!cfg.openai.base_url.is_empty() || !cfg.openai.api_key.is_empty()) {
        cfg.providers.push(ProviderEntry {
            id: "openai".into(),
            protocol: "openai".into(),
            base_url: cfg.openai.base_url.clone(),
            api_key: cfg.openai.api_key.clone(),
            include_usage: cfg.openai.include_usage,
            models: if !cfg.openai.model.is_empty() {
                vec![ModelEntry {
                    id: cfg.openai.model.clone(),
                    name: cfg.openai.model.clone(),
                    ..Default::default()
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        });
    }

    if !has_anthropic && (!cfg.anthropic.base_url.is_empty() || !cfg.anthropic.api_key.is_empty()) {
        cfg.providers.push(ProviderEntry {
            id: "anthropic".into(),
            protocol: "anthropic".into(),
            base_url: cfg.anthropic.base_url.clone(),
            api_key: cfg.anthropic.api_key.clone(),
            version: cfg.anthropic.version.clone(),
            max_tokens: cfg.anthropic.max_tokens,
            models: if !cfg.anthropic.model.is_empty() {
                vec![ModelEntry {
                    id: cfg.anthropic.model.clone(),
                    name: cfg.anthropic.model.clone(),
                    ..Default::default()
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        });
    }

    if !cfg.provider.is_empty() {
        cfg.default_provider = cfg.provider.clone();
    }
    if !cfg.model.is_empty() {
        cfg.default_model = cfg.model.clone();
    }
}

fn apply_env(cfg: &mut GreyConfig) -> Result<()> {
    if let Ok(v) = env::var("GREY_PROVIDER") {
        cfg.provider = v;
    }
    if let Ok(v) = env::var("GREY_MODEL") {
        cfg.model = v;
    }
    if let Ok(v) = env::var("GREY_OPENAI_BASE_URL") {
        cfg.openai.base_url = v;
    }
    if let Ok(v) = env::var("GREY_OPENAI_API_KEY") {
        cfg.openai.api_key = v;
    }
    if let Ok(v) = env::var("GREY_OPENAI_MODEL") {
        cfg.openai.model = v;
    }
    if let Ok(v) = env::var("GREY_OPENAI_INCLUDE_USAGE") {
        cfg.openai.include_usage = v.parse::<bool>().with_context(|| {
            format!("GREY_OPENAI_INCLUDE_USAGE must be true or false, got `{v}`")
        })?;
    }
    if let Ok(v) = env::var("GREY_ANTHROPIC_BASE_URL") {
        cfg.anthropic.base_url = v;
    }
    if let Ok(v) = env::var("GREY_ANTHROPIC_API_KEY") {
        cfg.anthropic.api_key = v;
    }
    if let Ok(v) = env::var("GREY_ANTHROPIC_MODEL") {
        cfg.anthropic.model = v;
    }
    if let Ok(v) = env::var("GREY_ANTHROPIC_VERSION") {
        cfg.anthropic.version = v;
    }
    if let Ok(v) = env::var("GREY_ANTHROPIC_MAX_TOKENS") {
        let max_tokens = v.parse::<u32>().with_context(|| {
            format!("GREY_ANTHROPIC_MAX_TOKENS must be a positive integer, got `{v}`")
        })?;
        anyhow::ensure!(
            max_tokens != 0,
            "GREY_ANTHROPIC_MAX_TOKENS must be greater than zero"
        );
        cfg.anthropic.max_tokens = max_tokens;
    }
    if let Ok(v) = env::var("GREY_RUST_ANALYZER") {
        cfg.lsp.rust_analyzer = v;
    }
    cfg.openai.api_key = expand_env_refs(&cfg.openai.api_key)?;
    cfg.anthropic.api_key = expand_env_refs(&cfg.anthropic.api_key)?;
    for p in cfg.providers.iter_mut() {
        p.api_key = expand_env_refs(&p.api_key)?;
    }
    Ok(())
}

fn expand_env_refs(s: &str) -> Result<String> {
    if let Some(rest) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        env::var(rest)
            .with_context(|| format!("referenced environment variable `{rest}` is not set"))
    } else {
        Ok(s.to_string())
    }
}

pub fn write_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let default = GreyConfig::default();
    let toml = toml::to_string_pretty(&default)?;
    std::fs::write(path, toml).with_context(|| format!("writing {}", path.display()))
}

pub fn is_secret_field(name: &str) -> bool {
    SECRET_FIELDS.iter().any(|s| name.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn test_dir() -> &'static Path {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let d = std::env::temp_dir().join(format!("grey-config-test-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            d
        })
    }

    #[test]
    fn defaults_apply() {
        let cfg = GreyConfig::default();
        assert_eq!(cfg.provider, "mock");
        assert_eq!(cfg.openai.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.default_provider, "mock");
        assert!(!cfg.providers.is_empty());
    }

    #[test]
    fn file_overrides_defaults() {
        let dir = test_dir().join("file-overrides");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grey.toml");
        std::fs::write(
            &path,
            "[openai]\nbase_url = \"http://127.0.0.1:9999/v1\"\napi_key = \"sk-test\"\n",
        )
        .unwrap();
        let mut base = GreyConfig::default();
        let file: GreyConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        base = merge(base, file);
        assert_eq!(base.provider, "mock");
        assert_eq!(base.openai.base_url, "http://127.0.0.1:9999/v1");
        assert_eq!(base.openai.api_key, "sk-test");
    }

    #[test]
    fn env_override_file() {
        let dir = test_dir().join("env-override");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("grey.toml"),
            "[openai]\nbase_url = \"http://x/v1\"\n",
        )
        .unwrap();
        let mut base = GreyConfig::default();
        let file: GreyConfig =
            toml::from_str(&std::fs::read_to_string(dir.join("grey.toml")).unwrap()).unwrap();
        base = merge(base, file);
        unsafe { env::set_var("GREY_OPENAI_BASE_URL", "http://env/v1") };
        apply_env(&mut base).unwrap();
        unsafe { env::remove_var("GREY_OPENAI_BASE_URL") };
        assert_eq!(base.openai.base_url, "http://env/v1");
    }

    #[test]
    fn redacted_masks_key() {
        let mut cfg = GreyConfig::default();
        cfg.openai.api_key = "sk-super-secret".into();
        cfg.anthropic.api_key = "anthropic-secret".into();
        assert_eq!(cfg.redacted().openai.api_key, "***");
        assert_eq!(cfg.redacted().anthropic.api_key, "***");
    }

    #[test]
    fn anthropic_defaults_are_complete() {
        let cfg = GreyConfig::default();
        assert_eq!(cfg.anthropic.base_url, "https://api.anthropic.com/v1");
        assert_eq!(cfg.anthropic.version, "2023-06-01");
        assert_eq!(cfg.anthropic.max_tokens, 4096);
        assert!(!cfg.anthropic.model.is_empty());
    }

    #[test]
    fn partial_anthropic_section_preserves_unspecified_defaults() {
        let base = GreyConfig::default();
        let file: GreyConfig =
            toml::from_str("[anthropic]\nmodel = \"claude-test\"\nmax_tokens = 1024\n").unwrap();
        let merged = merge(base, file);

        assert_eq!(merged.anthropic.model, "claude-test");
        assert_eq!(merged.anthropic.max_tokens, 1024);
        assert_eq!(merged.anthropic.base_url, "https://api.anthropic.com/v1");
        assert_eq!(merged.anthropic.version, "2023-06-01");
    }

    #[test]
    fn expand_refs() {
        unsafe { env::set_var("GREY_TEST_REF", "hello") };
        assert_eq!(expand_env_refs("${GREY_TEST_REF}").unwrap(), "hello");
        assert_eq!(expand_env_refs("plain").unwrap(), "plain");
        unsafe { env::remove_var("GREY_TEST_REF") };
    }

    #[test]
    fn invalid_anthropic_token_limit_and_missing_secret_reference_are_errors() {
        let mut cfg = GreyConfig::default();
        let previous_tokens = env::var_os("GREY_ANTHROPIC_MAX_TOKENS");
        unsafe { env::set_var("GREY_ANTHROPIC_MAX_TOKENS", "zero-ish") };
        let error = apply_env(&mut cfg).unwrap_err();
        unsafe {
            match previous_tokens {
                Some(value) => env::set_var("GREY_ANTHROPIC_MAX_TOKENS", value),
                None => env::remove_var("GREY_ANTHROPIC_MAX_TOKENS"),
            }
        }
        assert!(error.to_string().contains("positive integer"));

        let previous_key = env::var_os("GREY_ANTHROPIC_API_KEY");
        let previous_missing = env::var_os("GREY_MISSING_ANTHROPIC_KEY");
        unsafe {
            env::remove_var("GREY_ANTHROPIC_API_KEY");
            env::remove_var("GREY_MISSING_ANTHROPIC_KEY");
        }
        cfg.anthropic.api_key = "${GREY_MISSING_ANTHROPIC_KEY}".into();
        let error = apply_env(&mut cfg).unwrap_err();
        unsafe {
            if let Some(value) = previous_key {
                env::set_var("GREY_ANTHROPIC_API_KEY", value);
            }
            if let Some(value) = previous_missing {
                env::set_var("GREY_MISSING_ANTHROPIC_KEY", value);
            }
        }
        assert!(error.to_string().contains("GREY_MISSING_ANTHROPIC_KEY"));
    }

    #[test]
    fn parses_dynamic_providers_table() {
        let toml_str = r#"
default_provider = "astrdark"
default_model = "glm-5.2"

[[providers]]
id = "mock"
protocol = "mock"

[[providers]]
id = "astrdark"
protocol = "openai"
base_url = "https://api.astrdark.cyou/v1"
api_key = "sk-test"
models = [
  { id = "glm-5.2", name = "GLM 5.2" },
  { id = "claude-opus-4-7", name = "Claude Opus 4.7" },
]

[[routes]]
match = "planning"
provider = "astrdark"
model = "claude-opus-4-7"
"#;
        let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.default_provider, "astrdark");
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[1].id, "astrdark");
        assert_eq!(cfg.providers[1].models.len(), 2);
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].match_kind, TaskKind::Planning);
    }

    #[test]
    fn migrates_legacy_openai_section() {
        let toml_str = r#"
provider = "openai"
model = "gpt-4o"

[openai]
base_url = "https://api.openai.com/v1"
api_key = "sk-test"
model = "gpt-4o"
"#;
        let mut cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        migrate_legacy(&mut cfg);
        assert!(cfg.providers.iter().any(|p| p.id == "openai"));
        assert_eq!(cfg.default_provider, "openai");
    }

    #[test]
    fn parses_fallback_and_context_config() {
        let toml_str = r#"
default_provider = "mock"
default_model = "m"

[[providers]]
id = "mock"
protocol = "mock"

[fallback]
providers = ["mock"]

[context]
max_tokens = 65536
system_budget = 2048
history_budget = 32768
tool_output_budget = 8192
input_budget = 16384
summary_threshold = 10
summary_max_messages = 3

[cache]
enabled = true
max_entries = 500
ttl_hours = 12
"#;
        let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.fallback.providers, vec!["mock"]);
        assert_eq!(cfg.context.max_tokens, 65536);
        assert_eq!(cfg.cache.enabled, true);
    }

    #[test]
    fn redacted_masks_provider_api_keys() {
        let mut cfg = GreyConfig::default();
        cfg.providers.push(ProviderEntry {
            id: "test".into(),
            protocol: "openai".into(),
            api_key: "sk-secret".into(),
            ..Default::default()
        });
        let redacted = cfg.redacted();
        let test_provider = redacted.provider("test").unwrap();
        assert_eq!(test_provider.api_key, "***");
    }

    #[test]
    fn provider_lookup_by_id() {
        let cfg = GreyConfig::default();
        assert!(cfg.provider("mock").is_some());
        assert!(cfg.provider("nonexistent").is_none());
    }
}
