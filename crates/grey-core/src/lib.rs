//! Grey core runtime: normalized protocols, agent loop, context and sessions.
//!
//! This crate holds the language-agnostic contracts of the harness. UI and
//! integrations depend on this crate, never the other way around.

pub mod agent;
pub mod cache;
pub mod config;
pub mod context;
pub mod hook;
pub mod process;
pub mod provider;
pub mod raw_config;
pub mod session;
pub mod skill;
pub mod summary;
pub mod token;
pub mod tool;
pub mod usage;
pub mod wasm_plugin;

pub use agent::{Agent, AgentEvent, AgentOptions, AgentOutcome, ProviderCandidate, ProviderHealth};
pub use cache::{CacheStats, CachedResponse, RequestCache};
pub use config::{
    AnthropicConfig, CacheConfig, ContextConfig, FallbackConfig, GreyConfig, HooksConfig,
    LspConfig, McpServerConfig, McpToolConfig, ModelEntry, OpenAiConfig, PluginConfig, PluginKind,
    PluginRuntime, ProviderAuth, ProviderEntry, RouteRule, RuntimeConfig, SkillConfig, TaskKind,
    TuiColorOverrides, TuiCompletionConfig, TuiConfig, TuiKeysConfig, TuiLayoutConfig,
    TuiThemeConfig, UsageConfig,
};
pub use context::{ContextAudit, ContextManager};
pub use hook::{HookEvent, HookPayload, HookRunner, HookTool};
pub use provider::{
    checked_utf8_bytes, collect, redact_provider_secrets, ChatMessage, ChatRequest, Provider,
    ProviderEvent, ProviderFailure, ProviderFailureKind, ProviderModelRef, Role, ToolCall, Usage,
};
pub use session::{Session, SessionStore, SessionSummary};
pub use summary::SummaryEngine;
pub use token::{CharApproxCounter, TiktokenCounter, TokenCounter};
pub use tool::{ToolDefinition, ToolExecutor, ToolResult, ToolRisk};
pub use usage::{CostRate, SessionUsage, TurnUsage, UsageTracker};
pub use wasm_plugin::{WasmPlugin, WasmPluginError, WasmPluginErrorKind, WasmPluginOutput};

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn test_env_lock() -> MutexGuard<'static, ()> {
        TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
