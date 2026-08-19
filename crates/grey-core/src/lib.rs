//! Grey core runtime: normalized protocols, agent loop, context and sessions.
//!
//! This crate holds the language-agnostic contracts of the harness. UI and
//! integrations depend on this crate, never the other way around.

pub mod agent;
pub mod cache;
pub mod config;
pub mod context;
pub mod process;
pub mod provider;
pub mod raw_config;
pub mod session;
pub mod summary;
pub mod token;
pub mod tool;
pub mod usage;

pub use agent::{Agent, AgentEvent, AgentOptions, AgentOutcome, ProviderCandidate, ProviderHealth};
pub use cache::{CacheStats, CachedResponse, RequestCache};
pub use config::{
    AnthropicConfig, CacheConfig, ContextConfig, FallbackConfig, GreyConfig, HooksConfig,
    LspConfig, McpToolConfig, ModelEntry, OpenAiConfig, PluginConfig, PluginKind, ProviderEntry,
    RouteRule, TaskKind, TuiColorOverrides, TuiCompletionConfig, TuiConfig, TuiKeysConfig,
    TuiLayoutConfig, TuiThemeConfig, UsageConfig,
};
pub use context::{ContextAudit, ContextManager};
pub use provider::{
    collect, ChatMessage, ChatRequest, Provider, ProviderEvent, ProviderModelRef, Role, ToolCall,
    Usage,
};
pub use session::{Session, SessionStore, SessionSummary};
pub use summary::SummaryEngine;
pub use token::{CharApproxCounter, TiktokenCounter, TokenCounter};
pub use tool::{ToolDefinition, ToolExecutor, ToolResult, ToolRisk};
pub use usage::{CostRate, SessionUsage, TurnUsage, UsageTracker};
