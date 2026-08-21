//! Configuration system: defaults < TOML file < environment < CLI overrides.
//!
//! P2: dynamic `[[providers]]` table with backward-compatible legacy
//! `[openai]`/`[anthropic]` sections auto-migrated at load time.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

/// Secret-ish fields are masked in `config show` output.
const SECRET_FIELDS: &[&str] = &["api_key", "token", "secret", "authorization", "password"];

// ---------------------------------------------------------------------------
// P2: dynamic provider registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuth {
    #[default]
    ApiKey,
    ChatgptOauth,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderEntry {
    pub id: String,
    pub protocol: String,
    #[serde(default)]
    pub auth: ProviderAuth,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_prompt: Vec<String>,
    #[serde(default)]
    pub pre_message_send: Vec<String>,
    #[serde(default)]
    pub session_start: Vec<String>,
    #[serde(default)]
    pub session_end: Vec<String>,
    #[serde(default)]
    pub permission_decision: Vec<String>,
    #[serde(default)]
    pub pre_tool_call: Vec<String>,
    #[serde(default)]
    pub post_tool_call: Vec<String>,
    #[serde(default)]
    pub completion: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    #[default]
    Tool,
    Provider,
    Hook,
    Theme,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PluginRuntime {
    #[default]
    Command,
    Wasm,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: PluginKind,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub hook_event: Option<String>,
    #[serde(default)]
    pub runtime: PluginRuntime,
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub manifest_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillConfig {
    pub id: String,
    #[serde(default = "default_skill_enabled")]
    pub enabled: bool,
}

fn default_skill_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpToolConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// A persistent MCP protocol server. `mcp_tools` remains the legacy
/// one-command compatibility format and is not treated as MCP transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: String,
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn default_mcp_transport() -> String {
    "stdio".into()
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            transport: default_mcp_transport(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            timeout_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Planning,
    Coding,
    Fast,
    #[default]
    Default,
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
    #[serde(default = "default_context_max_tokens")]
    pub max_tokens: u64,
    #[serde(default = "default_context_system_budget")]
    pub system_budget: u64,
    #[serde(default = "default_context_history_budget")]
    pub history_budget: u64,
    #[serde(default = "default_context_tool_output_budget")]
    pub tool_output_budget: u64,
    #[serde(default = "default_context_input_budget")]
    pub input_budget: u64,
    #[serde(default = "default_context_summary_threshold")]
    pub summary_threshold: usize,
    #[serde(default = "default_context_summary_max_messages")]
    pub summary_max_messages: usize,
}

fn default_context_max_tokens() -> u64 {
    128_000
}
fn default_context_system_budget() -> u64 {
    4_096
}
fn default_context_history_budget() -> u64 {
    65_536
}
fn default_context_tool_output_budget() -> u64 {
    16_384
}
fn default_context_input_budget() -> u64 {
    32_768
}
fn default_context_summary_threshold() -> usize {
    20
}
fn default_context_summary_max_messages() -> usize {
    5
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: default_context_max_tokens(),
            system_budget: default_context_system_budget(),
            history_budget: default_context_history_budget(),
            tool_output_budget: default_context_tool_output_budget(),
            input_budget: default_context_input_budget(),
            summary_threshold: default_context_summary_threshold(),
            summary_max_messages: default_context_summary_max_messages(),
        }
    }
}

const RUNTIME_QUEUE_MIN: usize = 1;
const RUNTIME_QUEUE_MAX: usize = 65_536;
const RUNTIME_BYTES_MIN: usize = 1024;
const RUNTIME_BYTES_MAX: usize = 64 * 1024 * 1024;
pub const RUNTIME_WASM_MEMORY_MIN: usize = 64 * 1024;
pub const RUNTIME_WASM_MEMORY_MAX: usize = 256 * 1024 * 1024;
pub const RUNTIME_WASM_FUEL_MIN: u64 = 1;
pub const RUNTIME_WASM_FUEL_MAX: u64 = 100_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_event_queue_capacity")]
    pub event_queue_capacity: usize,
    #[serde(default = "default_input_queue_capacity")]
    pub input_queue_capacity: usize,
    #[serde(default = "default_prompt_queue_capacity")]
    pub prompt_queue_capacity: usize,
    #[serde(default = "default_transcript_max_bytes")]
    pub transcript_max_bytes: usize,
    #[serde(default = "default_response_max_bytes")]
    pub response_max_bytes: usize,
    #[serde(default = "default_command_stdout_max_bytes")]
    pub command_stdout_max_bytes: usize,
    #[serde(default = "default_command_stderr_max_bytes")]
    pub command_stderr_max_bytes: usize,
    #[serde(default = "default_skill_max_bytes")]
    pub skill_max_bytes: usize,
    #[serde(default = "default_wasm_memory_bytes")]
    pub wasm_memory_bytes: usize,
    #[serde(default = "default_wasm_fuel")]
    pub wasm_fuel: u64,
}

fn default_event_queue_capacity() -> usize {
    256
}
fn default_input_queue_capacity() -> usize {
    256
}
fn default_prompt_queue_capacity() -> usize {
    1
}
fn default_transcript_max_bytes() -> usize {
    1024 * 1024
}
fn default_response_max_bytes() -> usize {
    4 * 1024 * 1024
}
fn default_command_stdout_max_bytes() -> usize {
    64 * 1024
}
fn default_command_stderr_max_bytes() -> usize {
    64 * 1024
}
fn default_skill_max_bytes() -> usize {
    1024 * 1024
}
fn default_wasm_memory_bytes() -> usize {
    64 * 1024 * 1024
}
fn default_wasm_fuel() -> u64 {
    10_000_000
}

impl RuntimeConfig {
    pub fn normalized(&self) -> Self {
        self.clone().clamped()
    }

    fn clamped(mut self) -> Self {
        self.event_queue_capacity = self
            .event_queue_capacity
            .clamp(RUNTIME_QUEUE_MIN, RUNTIME_QUEUE_MAX);
        self.input_queue_capacity = self
            .input_queue_capacity
            .clamp(RUNTIME_QUEUE_MIN, RUNTIME_QUEUE_MAX);
        self.prompt_queue_capacity = self
            .prompt_queue_capacity
            .clamp(RUNTIME_QUEUE_MIN, RUNTIME_QUEUE_MAX);
        self.transcript_max_bytes = self
            .transcript_max_bytes
            .clamp(RUNTIME_BYTES_MIN, RUNTIME_BYTES_MAX);
        self.response_max_bytes = self
            .response_max_bytes
            .clamp(RUNTIME_BYTES_MIN, RUNTIME_BYTES_MAX);
        self.command_stdout_max_bytes = self
            .command_stdout_max_bytes
            .clamp(RUNTIME_BYTES_MIN, RUNTIME_BYTES_MAX);
        self.command_stderr_max_bytes = self
            .command_stderr_max_bytes
            .clamp(RUNTIME_BYTES_MIN, RUNTIME_BYTES_MAX);
        self.skill_max_bytes = self
            .skill_max_bytes
            .clamp(RUNTIME_BYTES_MIN, RUNTIME_BYTES_MAX);
        self.wasm_memory_bytes = self
            .wasm_memory_bytes
            .clamp(RUNTIME_WASM_MEMORY_MIN, RUNTIME_WASM_MEMORY_MAX);
        self.wasm_fuel = self
            .wasm_fuel
            .clamp(RUNTIME_WASM_FUEL_MIN, RUNTIME_WASM_FUEL_MAX);
        self
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            event_queue_capacity: default_event_queue_capacity(),
            input_queue_capacity: default_input_queue_capacity(),
            prompt_queue_capacity: default_prompt_queue_capacity(),
            transcript_max_bytes: default_transcript_max_bytes(),
            response_max_bytes: default_response_max_bytes(),
            command_stdout_max_bytes: default_command_stdout_max_bytes(),
            command_stderr_max_bytes: default_command_stderr_max_bytes(),
            skill_max_bytes: default_skill_max_bytes(),
            wasm_memory_bytes: default_wasm_memory_bytes(),
            wasm_fuel: default_wasm_fuel(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_cache_enabled")]
    pub enabled: bool,
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: usize,
    #[serde(default = "default_cache_ttl_hours")]
    pub ttl_hours: u64,
}

fn default_cache_enabled() -> bool {
    true
}
fn default_cache_max_entries() -> usize {
    1_000
}
fn default_cache_ttl_hours() -> u64 {
    24
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_cache_enabled(),
            max_entries: default_cache_max_entries(),
            ttl_hours: default_cache_ttl_hours(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageConfig {
    #[serde(default = "default_usage_track")]
    pub track: bool,
    #[serde(default)]
    pub cost_per_1m_input: HashMap<String, f64>,
    #[serde(default)]
    pub cost_per_1m_output: HashMap<String, f64>,
}

fn default_usage_track() -> bool {
    true
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            track: default_usage_track(),
            cost_per_1m_input: HashMap::new(),
            cost_per_1m_output: HashMap::new(),
        }
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct TuiColorOverrides {
    #[serde(default)]
    pub border: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub status_fg: Option<String>,
    #[serde(default)]
    pub status_bg: Option<String>,
    #[serde(default)]
    pub muted: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub success: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
}

fn default_tui_theme() -> String {
    "grey_storm".to_string()
}

fn default_tui_input_lines() -> u16 {
    3
}

fn default_tui_completion_long_running_steps() -> usize {
    4
}

fn default_tui_completion_long_running_secs() -> u64 {
    120
}

fn default_tui_completion_bell() -> bool {
    true
}

fn default_tui_completion_strong() -> bool {
    false
}

fn default_tui_completion_notify() -> bool {
    false
}

fn default_tui_completion_persistent() -> bool {
    false
}

fn default_tui_leader_key() -> String {
    "\\".to_string()
}

fn default_tui_help_key() -> String {
    "k".to_string()
}

fn default_tui_quit_key() -> String {
    "ctrl-c".to_string()
}

fn default_tui_clear_key() -> String {
    "ctrl-l".to_string()
}

fn default_tui_scroll_up_key() -> String {
    "pageup".to_string()
}

fn default_tui_scroll_down_key() -> String {
    "pagedown".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiThemeConfig {
    #[serde(default = "default_tui_theme")]
    pub preset: String,
    #[serde(default)]
    pub overrides: TuiColorOverrides,
    /// Exact id of an enabled `theme` plugin to invoke before starting the TUI.
    #[serde(default)]
    pub plugin: Option<String>,
}

impl Default for TuiThemeConfig {
    fn default() -> Self {
        Self {
            preset: default_tui_theme(),
            overrides: TuiColorOverrides::default(),
            plugin: None,
        }
    }
}

/// Deprecated since v0.1.1: `tui.layout.input_lines` is ignored at render time.
/// Input height is now content-driven and clamped to 40% of the frame.
/// Kept for config compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiLayoutConfig {
    #[serde(default = "default_tui_input_lines")]
    pub input_lines: u16,
}

impl Default for TuiLayoutConfig {
    fn default() -> Self {
        Self {
            input_lines: default_tui_input_lines(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiCompletionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tui_completion_long_running_steps")]
    pub long_running_steps: usize,
    #[serde(default = "default_tui_completion_long_running_secs")]
    pub long_running_seconds: u64,
    #[serde(default = "default_tui_completion_bell")]
    pub bell: bool,
    #[serde(default = "default_tui_completion_strong")]
    pub strong_bell: bool,
    #[serde(default = "default_tui_completion_notify")]
    pub notify: bool,
    #[serde(default = "default_tui_completion_persistent")]
    pub persistent: bool,
}

impl Default for TuiCompletionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            long_running_steps: default_tui_completion_long_running_steps(),
            long_running_seconds: default_tui_completion_long_running_secs(),
            bell: default_tui_completion_bell(),
            strong_bell: default_tui_completion_strong(),
            notify: default_tui_completion_notify(),
            persistent: default_tui_completion_persistent(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiKeysConfig {
    #[serde(default = "default_tui_leader_key")]
    pub leader: String,
    #[serde(default = "default_tui_help_key")]
    pub help: String,
    #[serde(default = "default_tui_quit_key")]
    pub quit: String,
    #[serde(default = "default_tui_clear_key")]
    pub clear: String,
    #[serde(default = "default_tui_scroll_up_key")]
    pub scroll_up: String,
    #[serde(default = "default_tui_scroll_down_key")]
    pub scroll_down: String,
}

impl Default for TuiKeysConfig {
    fn default() -> Self {
        Self {
            leader: default_tui_leader_key(),
            help: default_tui_help_key(),
            quit: default_tui_quit_key(),
            clear: default_tui_clear_key(),
            scroll_up: default_tui_scroll_up_key(),
            scroll_down: default_tui_scroll_down_key(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub theme: TuiThemeConfig,
    pub layout: TuiLayoutConfig,
    pub completion: TuiCompletionConfig,
    pub keys: TuiKeysConfig,
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
    /// Runtime memory and queue limits.
    pub runtime: RuntimeConfig,
    /// P2: request cache configuration.
    pub cache: CacheConfig,
    /// P2: usage tracking configuration.
    pub usage: UsageConfig,
    /// Hook configuration.
    pub hooks: HooksConfig,
    /// External command MCP tools.
    pub mcp_tools: Vec<McpToolConfig>,
    /// Persistent MCP protocol servers over stdio only.
    pub mcp_servers: Vec<McpServerConfig>,
    /// TUI preferences for theme/layout/reminder.
    pub tui: TuiConfig,
    /// Plugin registry for P6 extension points.
    pub plugins: Vec<PluginConfig>,
    /// Local skill registry. Skill files live under `skills/<id>/SKILL.md` beside this config.
    pub skills: Vec<SkillConfig>,
    /// Canonical base directory for relative plugin manifests. Not persisted.
    #[serde(skip, default = "default_plugin_config_dir")]
    pub plugin_config_dir: PathBuf,
    /// Canonical base directory for local skills. Not persisted.
    #[serde(skip, default = "default_plugin_config_dir")]
    pub skill_config_dir: PathBuf,

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
            runtime: RuntimeConfig::default(),
            cache: CacheConfig::default(),
            usage: UsageConfig::default(),
            hooks: HooksConfig::default(),
            mcp_tools: Vec::new(),
            mcp_servers: Vec::new(),
            plugins: Vec::new(),
            skills: Vec::new(),
            plugin_config_dir: default_plugin_config_dir(),
            skill_config_dir: default_plugin_config_dir(),
            tui: TuiConfig::default(),
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
    let local = PathBuf::from("grey.toml");
    if local.exists() {
        return local;
    }
    match env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/grey/grey.toml"),
        None => PathBuf::from("grey.toml"),
    }
}

pub fn load() -> Result<GreyConfig> {
    let path = config_path();
    load_from_path(path.as_deref())
}

fn load_from_path(path: Option<&Path>) -> Result<GreyConfig> {
    let mut cfg = GreyConfig::default();
    let mut legacy_file = false;
    if let Some(path) = path {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let raw_value: toml::Value =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        let file_cfg: GreyConfig =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        legacy_file = ["provider", "model", "openai", "anthropic"]
            .iter()
            .any(|key| raw_value.get(*key).is_some());
        cfg = merge_file(cfg, file_cfg, &raw_value);
    }
    apply_env(&mut cfg)?;
    cfg.runtime = cfg.runtime.clamped();
    cfg.plugin_config_dir = plugin_config_dir(path)?;
    cfg.skill_config_dir = plugin_config_dir(path)?;
    let legacy_env = [
        "GREY_PROVIDER",
        "GREY_MODEL",
        "GREY_OPENAI_BASE_URL",
        "GREY_OPENAI_API_KEY",
        "GREY_OPENAI_MODEL",
        "GREY_ANTHROPIC_BASE_URL",
        "GREY_ANTHROPIC_API_KEY",
        "GREY_ANTHROPIC_MODEL",
    ]
    .iter()
    .any(|key| env::var_os(key).is_some());
    migrate_legacy_with_presence(&mut cfg, legacy_file || legacy_env);
    if legacy_file || legacy_env {
        eprintln!("warning: legacy provider fields are deprecated; migrate to [[providers]]");
    }
    validate_plugins(&cfg.plugins)?;
    validate_skills(&cfg.skills)?;
    Ok(cfg)
}

fn default_plugin_config_dir() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn plugin_config_dir(path: Option<&Path>) -> Result<PathBuf> {
    let base = path
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    base.canonicalize()
        .with_context(|| format!("canonicalizing plugin config directory {}", base.display()))
}

pub fn validate_plugins(plugins: &[PluginConfig]) -> Result<()> {
    validate_unique_plugin_ids(plugins)?;
    for plugin in plugins {
        validate_plugin_config(plugin)?;
    }
    Ok(())
}

pub fn validate_skills(skills: &[SkillConfig]) -> Result<()> {
    let mut ids = HashSet::with_capacity(skills.len());
    for skill in skills {
        crate::skill::validate_id(&skill.id)?;
        anyhow::ensure!(
            ids.insert(skill.id.as_str()),
            "duplicate skill id: {}",
            skill.id
        );
    }
    Ok(())
}

/// Plugin IDs are exact and case-sensitive; surrounding whitespace is rejected.
pub fn validate_unique_plugin_ids(plugins: &[PluginConfig]) -> Result<()> {
    let mut ids = HashSet::with_capacity(plugins.len());
    for plugin in plugins {
        anyhow::ensure!(
            plugin.id == plugin.id.trim(),
            "plugin id must not have leading or trailing whitespace: {:?}",
            plugin.id
        );
        anyhow::ensure!(
            ids.insert(plugin.id.as_str()),
            "duplicate plugin id: {}",
            plugin.id
        );
    }
    Ok(())
}

pub fn validate_plugin_config(plugin: &PluginConfig) -> Result<()> {
    anyhow::ensure!(!plugin.id.trim().is_empty(), "plugin id must not be empty");
    match plugin.runtime {
        PluginRuntime::Command => {
            anyhow::ensure!(
                !plugin.command.trim().is_empty(),
                "command plugin `{}` must specify command",
                plugin.id
            );
            anyhow::ensure!(
                plugin.manifest.is_none() && plugin.manifest_sha256.is_none(),
                "command plugin `{}` must not specify a wasm manifest",
                plugin.id
            );
        }
        PluginRuntime::Wasm => {
            anyhow::ensure!(
                matches!(
                    plugin.kind,
                    PluginKind::Provider | PluginKind::Theme | PluginKind::Tool
                ),
                "wasm plugin `{}` must be provider, theme, or tool",
                plugin.id
            );
            anyhow::ensure!(
                plugin
                    .manifest
                    .as_deref()
                    .is_some_and(|path| !path.trim().is_empty()),
                "wasm plugin `{}` must specify manifest",
                plugin.id
            );
            anyhow::ensure!(
                plugin
                    .manifest_sha256
                    .as_deref()
                    .is_some_and(|hash| hash.len() == 64
                        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())),
                "wasm plugin `{}` must specify a SHA-256 manifest hash",
                plugin.id
            );
            anyhow::ensure!(
                plugin.command.trim().is_empty() && plugin.args.is_empty(),
                "wasm plugin `{}` must not specify command or args",
                plugin.id
            );
            anyhow::ensure!(
                plugin.hook_event.is_none(),
                "wasm plugin `{}` must not specify hook_event",
                plugin.id
            );
        }
    }
    Ok(())
}

fn merge_file(base: GreyConfig, over: GreyConfig, raw: &toml::Value) -> GreyConfig {
    let defaults = base.clone();
    let mut merged = merge(base, over);
    let Some(table) = raw.as_table() else {
        return merged;
    };
    if !table.contains_key("default_provider") {
        merged.default_provider = defaults.default_provider;
    }
    if !table.contains_key("default_model") {
        merged.default_model = defaults.default_model;
    }
    if !table.contains_key("providers") {
        merged.providers = defaults.providers;
    }
    if !table.contains_key("routes") {
        merged.routes = defaults.routes;
    }
    if !table.contains_key("fallback") {
        merged.fallback = defaults.fallback;
    }
    if !table.contains_key("context") {
        merged.context = defaults.context;
    }
    if !table.contains_key("runtime") {
        merged.runtime = defaults.runtime;
    }
    if !table.contains_key("cache") {
        merged.cache = defaults.cache;
    }
    if !table.contains_key("usage") {
        merged.usage = defaults.usage;
    }
    if !table.contains_key("hooks") {
        merged.hooks = defaults.hooks;
    }
    if !table.contains_key("mcp_tools") {
        merged.mcp_tools = defaults.mcp_tools;
    }
    if !table.contains_key("mcp_servers") {
        merged.mcp_servers = defaults.mcp_servers;
    }
    if !table.contains_key("plugins") {
        merged.plugins = defaults.plugins;
    }
    if !table.contains_key("skills") {
        merged.skills = defaults.skills;
    }
    if !table.contains_key("tui") {
        merged.tui = defaults.tui;
    }
    merged
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
    base.context = over.context;
    base.runtime = over.runtime;
    base.cache = over.cache;
    base.usage = over.usage;
    let has_hook_override = !over.hooks.pre_prompt.is_empty()
        || !over.hooks.pre_message_send.is_empty()
        || !over.hooks.session_start.is_empty()
        || !over.hooks.session_end.is_empty()
        || !over.hooks.permission_decision.is_empty()
        || !over.hooks.pre_tool_call.is_empty()
        || !over.hooks.post_tool_call.is_empty()
        || !over.hooks.completion.is_empty();

    if has_hook_override {
        base.hooks = over.hooks;
    }
    if !over.mcp_tools.is_empty() {
        base.mcp_tools = over.mcp_tools;
    }
    if !over.mcp_servers.is_empty() {
        base.mcp_servers = over.mcp_servers;
    }
    if !over.plugins.is_empty() {
        base.plugins = over.plugins;
    }
    if !over.skills.is_empty() {
        base.skills = over.skills;
    }
    if !over.tui.theme.preset.is_empty()
        || !over.tui.layout.input_lines.eq(&0)
        || over.tui.completion.enabled
    {
        base.tui = over.tui;
    } else {
        base.tui.theme = over.tui.theme;
        base.tui.layout = over.tui.layout;
        base.tui.completion = over.tui.completion;
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

#[cfg(test)]
fn migrate_legacy(cfg: &mut GreyConfig) {
    migrate_legacy_with_presence(cfg, true);
}

fn migrate_legacy_with_presence(cfg: &mut GreyConfig, legacy_present: bool) {
    if !legacy_present {
        return;
    }
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
        cfg.default_provider = cfg.provider.clone();
    }
    if let Ok(v) = env::var("GREY_MODEL") {
        cfg.model = v;
        cfg.default_model = cfg.model.clone();
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
    let fallback_ark_api_key =
        env::var_os("ARK_API_KEY").or_else(|| env::var_os("VOLCANO_API_KEY"));
    for provider in cfg.providers.iter_mut() {
        let prefix = format!(
            "GREY_PROVIDER_{}_",
            provider
                .id
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase()
                } else {
                    '_'
                })
                .collect::<String>()
        );
        if let Some(value) = env::var_os(format!("{prefix}BASE_URL")) {
            provider.base_url = value.to_string_lossy().into_owned();
        }
        if let Some(value) = env::var_os(format!("{prefix}API_KEY")) {
            provider.api_key = value.to_string_lossy().into_owned();
        }
        if let Some(value) = env::var_os(format!("{prefix}VERSION")) {
            provider.version = value.to_string_lossy().into_owned();
        }
        if let Some(value) = env::var_os(format!("{prefix}MAX_TOKENS")) {
            provider.max_tokens = value
                .to_string_lossy()
                .parse::<u32>()
                .with_context(|| format!("{prefix}MAX_TOKENS must be a positive integer"))?;
        }
        if let Some(value) = env::var_os(format!("{prefix}INCLUDE_USAGE")) {
            provider.include_usage = value
                .to_string_lossy()
                .parse::<bool>()
                .with_context(|| format!("{prefix}INCLUDE_USAGE must be true or false"))?;
        }
        if (provider.id == "volcano" || provider.id == "volcano-coding-plan")
            && provider.api_key.is_empty()
        {
            if let Some(value) = &fallback_ark_api_key {
                provider.api_key = value.to_string_lossy().into_owned();
            }
        }
    }
    if cfg.default_provider == "volcano-coding-plan" && env::var_os("GREY_MODEL").is_none() {
        if let Ok(model) = env::var("ARK_MODEL") {
            if !model.trim().is_empty() {
                cfg.model = model.clone();
                cfg.default_model = model;
            }
        }
    }
    if let Ok(v) = env::var("GREY_RUST_ANALYZER") {
        cfg.lsp.rust_analyzer = v;
    }
    cfg.openai.api_key = expand_env_refs(&cfg.openai.api_key)?;
    cfg.anthropic.api_key = expand_env_refs(&cfg.anthropic.api_key)?;
    for p in cfg.providers.iter_mut() {
        p.api_key = expand_env_refs(&p.api_key)?;
    }
    for hook in cfg.hooks.pre_prompt.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for hook in cfg.hooks.pre_message_send.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for hook in cfg.hooks.session_start.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for hook in cfg.hooks.session_end.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for hook in cfg.hooks.permission_decision.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for hook in cfg.hooks.pre_tool_call.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for hook in cfg.hooks.post_tool_call.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for hook in cfg.hooks.completion.iter_mut() {
        *hook = expand_env_refs(hook)?;
    }
    for tool in cfg.mcp_tools.iter_mut() {
        tool.command = expand_env_refs(&tool.command)?;
        for arg in tool.args.iter_mut() {
            *arg = expand_env_refs(arg)?;
        }
    }
    for server in cfg.mcp_servers.iter_mut() {
        server.command = expand_env_refs(&server.command)?;
        for arg in &mut server.args {
            *arg = expand_env_refs(arg)?;
        }
        for value in server.env.values_mut() {
            *value = expand_env_refs(value)?;
        }
    }
    for plugin in cfg.plugins.iter_mut() {
        plugin.command = expand_env_refs(&plugin.command)?;
        for arg in plugin.args.iter_mut() {
            *arg = expand_env_refs(arg)?;
        }
        if let Some(event) = &plugin.hook_event {
            plugin.hook_event = Some(expand_env_refs(event)?);
        }
        if let Some(version) = &plugin.version {
            plugin.version = Some(expand_env_refs(version)?);
        }
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
    let name = name.to_ascii_lowercase();
    SECRET_FIELDS.iter().any(|secret| name.contains(secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::OnceLock;

    struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

    impl EnvRestore {
        fn capture(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| (*name, env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                for (name, value) in &self.0 {
                    match value {
                        Some(value) => env::set_var(name, value),
                        None => env::remove_var(name),
                    }
                }
            }
        }
    }

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
    fn bounded_runtime_defaults_and_file_values_are_clamped() {
        let _lock = crate::test_support::test_env_lock();
        let defaults = GreyConfig::default();
        assert_eq!(defaults.runtime.event_queue_capacity, 256);
        assert_eq!(defaults.runtime.input_queue_capacity, 256);
        assert_eq!(defaults.runtime.prompt_queue_capacity, 1);
        assert_eq!(defaults.runtime.transcript_max_bytes, 1024 * 1024);
        assert_eq!(defaults.runtime.response_max_bytes, 4 * 1024 * 1024);
        assert_eq!(defaults.runtime.command_stdout_max_bytes, 64 * 1024);
        assert_eq!(defaults.runtime.command_stderr_max_bytes, 64 * 1024);
        assert_eq!(defaults.runtime.skill_max_bytes, 1024 * 1024);
        assert_eq!(defaults.runtime.wasm_memory_bytes, 64 * 1024 * 1024);
        assert_eq!(defaults.runtime.wasm_fuel, 10_000_000);

        let dir = test_dir().join("bounded-runtime");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grey.toml");
        std::fs::write(
            &path,
            format!(
                r#"[runtime]
event_queue_capacity = 0
input_queue_capacity = {max}
prompt_queue_capacity = 0
transcript_max_bytes = {max}
response_max_bytes = 0
command_stdout_max_bytes = {max}
command_stderr_max_bytes = 0
skill_max_bytes = {max}
wasm_memory_bytes = {max}
wasm_fuel = {max}
"#,
                max = i64::MAX
            ),
        )
        .unwrap();

        let cfg = load_from_path(Some(&path)).unwrap();
        assert_eq!(cfg.runtime.event_queue_capacity, 1);
        assert_eq!(cfg.runtime.input_queue_capacity, 65_536);
        assert_eq!(cfg.runtime.prompt_queue_capacity, 1);
        assert_eq!(cfg.runtime.transcript_max_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.runtime.response_max_bytes, 1024);
        assert_eq!(cfg.runtime.command_stdout_max_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.runtime.command_stderr_max_bytes, 1024);
        assert_eq!(cfg.runtime.skill_max_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.runtime.wasm_memory_bytes, 256 * 1024 * 1024);
        assert_eq!(cfg.runtime.wasm_fuel, 100_000_000);
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
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["GREY_OPENAI_BASE_URL"]);
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
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["GREY_TEST_REF"]);
        unsafe { env::set_var("GREY_TEST_REF", "hello") };
        assert_eq!(expand_env_refs("${GREY_TEST_REF}").unwrap(), "hello");
        assert_eq!(expand_env_refs("plain").unwrap(), "plain");
    }

    #[test]
    fn invalid_anthropic_token_limit_and_missing_secret_reference_are_errors() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&[
            "GREY_ANTHROPIC_MAX_TOKENS",
            "GREY_ANTHROPIC_API_KEY",
            "GREY_MISSING_ANTHROPIC_KEY",
        ]);
        let mut cfg = GreyConfig::default();
        unsafe { env::set_var("GREY_ANTHROPIC_MAX_TOKENS", "zero-ish") };
        let error = apply_env(&mut cfg).unwrap_err();
        assert!(error.to_string().contains("positive integer"));

        unsafe {
            env::remove_var("GREY_ANTHROPIC_MAX_TOKENS");
            env::remove_var("GREY_ANTHROPIC_API_KEY");
            env::remove_var("GREY_MISSING_ANTHROPIC_KEY");
        }
        cfg.anthropic.api_key = "${GREY_MISSING_ANTHROPIC_KEY}".into();
        let error = apply_env(&mut cfg).unwrap_err();
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
        assert!(cfg.cache.enabled);
    }

    #[test]
    fn parses_tui_config_section() {
        let toml_str = r##"
[tui]
theme = { preset = "slate", overrides = { border = "#1f2937", accent = "#60a5fa", error = "#ff7b72", success = "#00ff00", warning = "#ffff00" } }

[tui.layout]
input_lines = 6

[tui.completion]
enabled = false
long_running_steps = 8
long_running_seconds = 45
bell = true
strong_bell = true
notify = true
persistent = true
    [tui.keys]
leader = "\\"
help = "k"
quit = "ctrl-c"
clear = "ctrl-l"
scroll_up = "pageup"
scroll_down = "pagedown"
"##;
        let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.tui.theme.preset, "slate");
        assert_eq!(cfg.tui.theme.overrides.border.as_deref(), Some("#1f2937"));
        assert_eq!(cfg.tui.theme.overrides.accent.as_deref(), Some("#60a5fa"));
        assert_eq!(cfg.tui.theme.overrides.error.as_deref(), Some("#ff7b72"));
        assert_eq!(cfg.tui.theme.overrides.success.as_deref(), Some("#00ff00"));
        assert_eq!(cfg.tui.theme.overrides.warning.as_deref(), Some("#ffff00"));
        assert_eq!(cfg.tui.layout.input_lines, 6);
        assert!(!cfg.tui.completion.enabled);
        assert_eq!(cfg.tui.completion.long_running_steps, 8);
        assert_eq!(cfg.tui.completion.long_running_seconds, 45);
        assert!(cfg.tui.completion.bell);
        assert!(cfg.tui.completion.strong_bell);
        assert!(cfg.tui.completion.notify);
        assert!(cfg.tui.completion.persistent);
        assert_eq!(cfg.tui.keys.leader, "\\");
        assert_eq!(cfg.tui.keys.help, "k");
        assert_eq!(cfg.tui.keys.quit, "ctrl-c");
        assert_eq!(cfg.tui.keys.clear, "ctrl-l");
        assert_eq!(cfg.tui.keys.scroll_up, "pageup");
        assert_eq!(cfg.tui.keys.scroll_down, "pagedown");
    }

    #[test]
    fn tui_defaults_to_grey_storm() {
        let theme = TuiThemeConfig::default();
        assert_eq!(theme.preset, "grey_storm");
        assert_eq!(theme.overrides.error, None);
    }

    #[test]
    fn parses_plugins_and_expands_refs() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["GREY_TEST_PLUGIN_CMD", "GREY_TEST_PLUGIN_ARG"]);
        let toml_str = r#"
[[plugins]]
id = "word_count"
kind = "tool"
name = "Word Count"
command = "${GREY_TEST_PLUGIN_CMD}"
args = ["${GREY_TEST_PLUGIN_ARG}"]
enabled = true
description = "Count words"
timeout_ms = 5000

[[plugins]]
id = "hook-pre"
kind = "hook"
name = "Pre prompt plugin"
command = "echo hook"
hook_event = "pre_prompt"
"#;
        unsafe {
            env::set_var("GREY_TEST_PLUGIN_CMD", "printf");
            env::set_var("GREY_TEST_PLUGIN_ARG", "x");
        }
        let mut cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        apply_env(&mut cfg).unwrap();
        assert_eq!(cfg.plugins.len(), 2);
        assert_eq!(cfg.plugins[0].id, "word_count");
        assert_eq!(cfg.plugins[0].command, "printf");
        assert_eq!(cfg.plugins[0].args, vec!["x".to_string()]);
        assert!(cfg.plugins[0].enabled);
        assert_eq!(
            cfg.plugins[0].kind,
            PluginKind::Tool,
            "tool plugin kind should parse"
        );
    }

    #[test]
    fn parses_hook_provider_and_theme_plugin_kinds() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["PLUGIN_VERSION"]);
        let toml_str = r#"
[[plugins]]
id = "hook-test"
kind = "hook"
command = "echo"
hook_event = "pre_prompt"

[[plugins]]
id = "custom-provider"
kind = "provider"
command = "provider-proxy"
version = "${PLUGIN_VERSION}"

[[plugins]]
id = "custom-theme"
kind = "theme"
command = "theme-proxy"
"#;
        unsafe {
            env::set_var("PLUGIN_VERSION", "0.1.0");
        }
        let mut cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        apply_env(&mut cfg).unwrap();
        assert_eq!(cfg.plugins.len(), 3);
        assert_eq!(cfg.plugins[0].kind, PluginKind::Hook);
        assert_eq!(cfg.plugins[1].kind, PluginKind::Provider);
        assert_eq!(cfg.plugins[1].version.as_deref(), Some("0.1.0"));
        assert_eq!(cfg.plugins[2].kind, PluginKind::Theme);
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

    #[test]
    fn parses_hooks_and_mcp_tools() {
        let toml_str = r#"
hooks = { pre_prompt = ["echo before prompt"] }

[[mcp_tools]]
name = "echoer"
command = "sh"
args = ["-lc", "printf '{\"success\":true,\"output\":\"tool-ok\"}'"]
description = "echo test tool"
"#;
        let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.hooks.pre_prompt, vec!["echo before prompt".to_string()]);
        assert!(cfg.hooks.pre_message_send.is_empty());
        assert!(cfg.hooks.session_start.is_empty());
        assert!(cfg.hooks.session_end.is_empty());
        assert!(cfg.hooks.permission_decision.is_empty());
        assert_eq!(cfg.mcp_tools.len(), 1);
        assert_eq!(cfg.mcp_tools[0].name, "echoer");
    }

    #[test]
    fn parses_stdio_mcp_servers() {
        let cfg: GreyConfig = toml::from_str(
            r#"
[[mcp_servers]]
id = "demo"
command = "server"
args = ["--stdio"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].transport, "stdio");
        assert_eq!(cfg.mcp_servers[0].id, "demo");
    }

    #[test]
    fn parses_extended_hook_events() {
        let toml_str = r#"
[hooks]
pre_message_send = ["printf 'msg'"]
pre_prompt = ["printf 'pre'"]
post_tool_call = ["printf 'post'"]
session_start = ["printf 'start'"]
session_end = ["printf 'end'"]
permission_decision = ["printf 'decision'"]
completion = ["printf 'complete'"]
"#;
        let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.hooks.pre_message_send, vec!["printf 'msg'"]);
        assert_eq!(cfg.hooks.pre_prompt, vec!["printf 'pre'"]);
        assert_eq!(cfg.hooks.post_tool_call, vec!["printf 'post'"]);
        assert_eq!(cfg.hooks.session_start, vec!["printf 'start'"]);
        assert_eq!(cfg.hooks.session_end, vec!["printf 'end'"]);
        assert_eq!(cfg.hooks.permission_decision, vec!["printf 'decision'"]);
        assert_eq!(cfg.hooks.completion, vec!["printf 'complete'"]);
    }

    #[test]
    fn load_rejects_duplicate_plugin_ids_across_kinds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("grey.toml");
        std::fs::write(
            &path,
            r#"[[plugins]]
id = "shared"
kind = "tool"
command = "printf"

[[plugins]]
id = "shared"
kind = "provider"
command = "printf"
"#,
        )
        .unwrap();
        let error = load_from_path(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("duplicate plugin id: shared"));
    }

    #[test]
    fn mcp_tool_env_refs_are_expanded() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&[
            "GREY_ANTHROPIC_MAX_TOKENS",
            "GREY_TEST_MCP_CMD",
            "GREY_TEST_MCP_ARG",
        ]);
        let mut cfg = GreyConfig::default();
        cfg.mcp_tools.push(McpToolConfig {
            name: "tool".into(),
            command: "${GREY_TEST_MCP_CMD}".into(),
            args: vec!["${GREY_TEST_MCP_ARG}".into()],
            ..Default::default()
        });
        unsafe {
            env::remove_var("GREY_ANTHROPIC_MAX_TOKENS");
            env::set_var("GREY_TEST_MCP_CMD", "sh");
            env::set_var("GREY_TEST_MCP_ARG", "-lc");
        }
        let result = apply_env(&mut cfg);
        assert!(result.is_ok(), "{}", result.unwrap_err());
        assert_eq!(cfg.mcp_tools[0].command, "sh");
        assert_eq!(cfg.mcp_tools[0].args, vec!["-lc".to_string()]);
    }

    #[test]
    fn applies_ark_api_key_to_volcano_provider_when_unset() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["ARK_API_KEY"]);
        let mut cfg = GreyConfig::default();
        cfg.providers.push(ProviderEntry {
            id: "volcano".into(),
            protocol: "openai".into(),
            ..Default::default()
        });
        unsafe { env::set_var("ARK_API_KEY", "ark-demo-key") };
        let result = apply_env(&mut cfg);
        assert!(result.is_ok(), "{}", result.unwrap_err());
        let volcano = cfg
            .providers
            .iter()
            .find(|provider| provider.id == "volcano")
            .expect("volcano provider should exist");
        assert_eq!(volcano.api_key, "ark-demo-key");
    }

    #[test]
    fn applies_volcano_api_key_to_volcano_provider_when_unset() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["ARK_API_KEY", "VOLCANO_API_KEY"]);
        let mut cfg = GreyConfig::default();
        cfg.providers.push(ProviderEntry {
            id: "volcano".into(),
            protocol: "openai".into(),
            ..Default::default()
        });
        unsafe {
            env::remove_var("ARK_API_KEY");
            env::set_var("VOLCANO_API_KEY", "volcano-demo-key");
        }
        let result = apply_env(&mut cfg);
        assert!(result.is_ok(), "{}", result.unwrap_err());
        let volcano = cfg
            .providers
            .iter()
            .find(|provider| provider.id == "volcano")
            .expect("volcano provider should exist");
        assert_eq!(volcano.api_key, "volcano-demo-key");
    }

    #[test]
    fn applies_ark_api_key_to_coding_plan_provider_when_unset() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["ARK_API_KEY", "VOLCANO_API_KEY"]);
        let mut cfg = GreyConfig::default();
        cfg.providers.push(ProviderEntry {
            id: "volcano-coding-plan".into(),
            protocol: "openai".into(),
            ..Default::default()
        });
        unsafe {
            env::set_var("ARK_API_KEY", "ark-coding-plan-key");
            env::remove_var("VOLCANO_API_KEY");
        }

        let result = apply_env(&mut cfg);

        result.unwrap();
        let provider = cfg
            .providers
            .iter()
            .find(|provider| provider.id == "volcano-coding-plan")
            .expect("Coding Plan provider should exist");
        assert_eq!(provider.api_key, "ark-coding-plan-key");
    }

    #[test]
    fn applies_ark_model_to_coding_plan_default() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["ARK_MODEL", "GREY_MODEL"]);
        let mut cfg = GreyConfig {
            default_provider: "volcano-coding-plan".into(),
            ..Default::default()
        };
        unsafe {
            env::set_var("ARK_MODEL", "doubao-seed-2.0-code");
            env::remove_var("GREY_MODEL");
        }

        let result = apply_env(&mut cfg);

        result.unwrap();
        assert_eq!(cfg.default_model, "doubao-seed-2.0-code");
        assert_eq!(cfg.model, "doubao-seed-2.0-code");
    }

    #[test]
    fn grey_model_overrides_ark_model_for_coding_plan() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["ARK_MODEL", "GREY_MODEL"]);
        let mut cfg = GreyConfig {
            default_provider: "volcano-coding-plan".into(),
            ..Default::default()
        };
        unsafe {
            env::set_var("ARK_MODEL", "ark-model");
            env::set_var("GREY_MODEL", "grey-model");
        }

        let result = apply_env(&mut cfg);

        result.unwrap();
        assert_eq!(cfg.default_model, "grey-model");
        assert_eq!(cfg.model, "grey-model");
    }

    #[test]
    fn empty_ark_model_does_not_clear_coding_plan_default() {
        let _lock = crate::test_support::test_env_lock();
        let _restore = EnvRestore::capture(&["ARK_MODEL", "GREY_MODEL"]);
        let mut cfg = GreyConfig {
            default_provider: "volcano-coding-plan".into(),
            default_model: "ark-code-latest".into(),
            ..Default::default()
        };
        unsafe {
            env::set_var("ARK_MODEL", "");
            env::remove_var("GREY_MODEL");
        }

        let result = apply_env(&mut cfg);

        result.unwrap();
        assert_eq!(cfg.default_model, "ark-code-latest");
    }
}
