//! Configuration system: defaults < TOML file < environment < CLI overrides.
//!
//! P0 scope: a small typed config merged from up to three sources. CLI
//! overrides live in the `grey-cli` crate and are applied on top of
//! `load()`'s result.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

/// Secret-ish fields are masked in `config show` output.
const SECRET_FIELDS: &[&str] = &["api_key", "token", "secret"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GreyConfig {
    /// Default provider id ("mock" | "openai" | "anthropic").
    pub provider: String,
    /// Default model name used by mock/fallback providers.
    pub model: String,
    /// OpenAI-compatible endpoint settings (covers Ollama, DeepSeek, vLLM...).
    pub openai: OpenAiConfig,
    /// Anthropic Messages API endpoint settings.
    pub anthropic: AnthropicConfig,
    /// LSP integration settings.
    pub lsp: LspConfig,
}

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

impl Default for GreyConfig {
    fn default() -> Self {
        Self {
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
        c
    }
}

/// Resolve the config file path: `GREY_CONFIG` env > `./grey.toml` > `~/.config/grey/grey.toml`.
/// Returns `None` when no config file exists (pure defaults + env remain in effect).
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

/// Default location for `grey config init`.
pub fn default_config_path() -> PathBuf {
    if let Some(path) = env::var_os("GREY_CONFIG") {
        return PathBuf::from(path);
    }
    match env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/grey/grey.toml"),
        None => PathBuf::from("grey.toml"),
    }
}

/// Load config: defaults, then optional TOML file, then environment variables.
pub fn load() -> Result<GreyConfig> {
    let mut cfg = GreyConfig::default();
    if let Some(path) = config_path() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let file_cfg: GreyConfig =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        cfg = merge(cfg, file_cfg);
    }
    apply_env(&mut cfg)?;
    Ok(cfg)
}

/// Left-biased field-by-field merge (file over defaults).
fn merge(mut base: GreyConfig, over: GreyConfig) -> GreyConfig {
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

/// Expand `${VAR}` references (e.g. api_key = "${GREY_OPENAI_API_KEY}").
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

/// Write the default config to `path` (used by `grey config init`).
pub fn write_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let default = GreyConfig::default();
    let toml = toml::to_string_pretty(&default)?;
    std::fs::write(path, toml).with_context(|| format!("writing {}", path.display()))
}

/// `true` when a field name should be masked in display output.
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
        assert_eq!(base.provider, "mock"); // untouched
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
}
