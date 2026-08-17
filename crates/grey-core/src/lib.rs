//! Grey core runtime: normalized protocols, agent loop, context and sessions.
//!
//! This crate holds the language-agnostic contracts of the harness. UI and
//! integrations depend on this crate, never the other way around.

pub mod agent;
pub mod cache;
pub mod config;
pub mod context;
pub mod provider;
pub mod session;
pub mod token;
pub mod tool;

pub use agent::{Agent, AgentEvent, AgentOptions, AgentOutcome};
pub use cache::{CacheStats, CachedResponse, RequestCache};
pub use config::{
    AnthropicConfig, CacheConfig, ContextConfig, FallbackConfig, GreyConfig, LspConfig, ModelEntry,
    OpenAiConfig, ProviderEntry, RouteRule, TaskKind, UsageConfig,
};
pub use context::{ContextAudit, ContextManager};
pub use provider::{
    collect, ChatMessage, ChatRequest, Provider, ProviderEvent, ProviderModelRef, Role, ToolCall,
    Usage,
};
pub use session::{Session, SessionStore, SessionSummary};
pub use token::{CharApproxCounter, TiktokenCounter, TokenCounter};
pub use tool::{ToolDefinition, ToolExecutor, ToolResult, ToolRisk};
