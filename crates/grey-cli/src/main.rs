//! Grey composition root: headless agent, interactive TUI, spikes and config.

use std::collections::HashSet;
use std::env;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::future::join_all;
use grey_core::{
    config, Agent, AgentEvent, AgentOptions, AgentOutcome, CharApproxCounter, ChatMessage,
    ChatRequest, ContextManager, GreyConfig, Provider, Role, Session, SessionStore, SummaryEngine,
    ToolExecutor,
};
use grey_provider::router::ProviderRouter;
use grey_tools::{AlwaysApprove, Approver, BuiltinTools, DenySideEffects, StdioApprover};
use grey_tools::{CombinedTools, HookedTools, McpTools};
use serde::Serialize;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tokio::time::Duration;

const SYSTEM_PROMPT: &str = r#"You are Grey, a careful coding agent working inside one workspace.
Inspect before changing anything. Use read_file, glob, and grep to gather evidence. Use edit_file
only with an exact old_string that occurs once. After edits, run the relevant tests with bash.
Keep changes scoped to the user's request, report tool failures honestly, and never claim success
without verification evidence."#;
const DEFAULT_HOOK_TIMEOUT_MS: u64 = 10_000;

#[derive(Parser, Clone)]
#[command(
    name = "grey",
    version,
    about = "A lightweight, high-performance, extensible code agent harness"
)]
struct Cli {
    /// One-shot prompt. Omit it to start the interactive TUI.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Provider override: any configured provider id.
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Model override for the selected provider.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Workspace root available to built-in tools.
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,

    /// Resume an existing session id.
    #[arg(long, global = true, conflicts_with = "continue_session")]
    session: Option<String>,

    /// Resume the latest session for this workspace.
    #[arg(long = "continue", global = true)]
    continue_session: bool,

    /// Do not save the resulting conversation.
    #[arg(long, global = true)]
    no_save: bool,

    /// Approve edit_file and bash without prompting.
    #[arg(long, global = true, conflicts_with = "read_only")]
    auto_approve: bool,

    /// Deny edit_file and bash while keeping read/search tools available.
    #[arg(long, global = true)]
    read_only: bool,

    /// Maximum provider/tool rounds per prompt.
    #[arg(long, global = true, default_value_t = 12)]
    max_steps: usize,

    /// Output format for one-shot prompts.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    /// Task kind for routing: planning, coding, fast, or default.
    #[arg(long, global = true, value_enum)]
    task: Option<TaskKindArg>,

    /// Disable request caching.
    #[arg(long, global = true)]
    no_cache: bool,

    /// Disable provider fallback on failure.
    #[arg(long, global = true)]
    no_fallback: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum TaskKindArg {
    Planning,
    Coding,
    Fast,
    Default,
}

impl TaskKindArg {
    fn to_core(self) -> grey_core::TaskKind {
        match self {
            TaskKindArg::Planning => grey_core::TaskKind::Planning,
            TaskKindArg::Coding => grey_core::TaskKind::Coding,
            TaskKindArg::Fast => grey_core::TaskKind::Fast,
            TaskKindArg::Default => grey_core::TaskKind::Default,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// P0 Spike A: streaming-render benchmark in a minimal TUI.
    #[command(name = "spike-a")]
    SpikeA,
    /// P0 Spike B: LSP diagnostics and definition integration.
    #[command(name = "spike-b")]
    SpikeB {
        /// Source file to open.
        file: Option<PathBuf>,
        /// Path to the language server binary.
        #[arg(long)]
        lsp: Option<PathBuf>,
    },
    /// P0 Spike C: raw provider streaming without the agent loop.
    #[command(name = "spike-c")]
    SpikeC {
        prompt: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Configuration management.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Persisted conversation management.
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Provider and model management.
    Providers {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Request cache management.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Usage and cost tracking.
    Usage {
        #[command(subcommand)]
        action: UsageAction,
    },
    /// Multi-agent orchestration: run sub-agents in parallel and synthesize.
    Orchestrate {
        prompt: String,
        /// Add as `name:task` pairs, e.g. `--agent coder:给出 patch 方案`.
        #[arg(long, value_name = "name:task")]
        agent: Vec<String>,
    },
}

#[derive(Subcommand, Clone)]
enum ConfigAction {
    /// Print effective configuration with secrets masked.
    Show,
    /// Write a default configuration file.
    Init {
        /// Replace an existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Print the resolved configuration path.
    Path,
}

#[derive(Subcommand, Clone)]
enum SessionAction {
    /// List recent sessions.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print one complete session as JSON.
    Show { id: String },
}

#[derive(Subcommand, Clone)]
enum ProviderAction {
    /// List all configured providers and their models.
    List,
    /// Show details for a specific provider.
    Show { id: String },
}

#[derive(Subcommand, Clone)]
enum CacheAction {
    /// Remove all cached responses.
    Clear,
    /// Print cache statistics.
    Stats,
}

#[derive(Subcommand, Clone)]
enum UsageAction {
    /// Show token usage and cost for a session.
    Show { id: String },
    /// Show aggregate usage across all sessions.
    Summary,
}

#[derive(Serialize)]
struct HeadlessOutput {
    response: String,
    session_id: Option<String>,
    usage: HeadlessUsage,
    steps: usize,
    cached: bool,
    provider: String,
    model: String,
}

#[derive(Serialize)]
struct HeadlessUsage {
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    cached: bool,
    provider: String,
    model: String,
}

#[derive(Debug, Clone)]
struct OrchestrateAgent {
    name: String,
    task: String,
    system_prompt: String,
}

#[derive(Serialize)]
struct OrchestrateAgentResult {
    name: String,
    task: String,
    response: String,
    provider: String,
    model: String,
    steps: usize,
    cached: bool,
    success: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct OrchestrateOutput {
    task: String,
    subagents: Vec<OrchestrateAgentResult>,
    synthesis: AgentOutcomeSummary,
}

#[derive(Serialize)]
struct AgentOutcomeSummary {
    response: String,
    provider: String,
    model: String,
    steps: usize,
    cached: bool,
}

const DEFAULT_ORCHESTRATE_AGENTS: &[(&str, &str)] = &[
    ("researcher", "研究主任务并给出关键点、边界与风险。"),
    ("coder", "给出最小可执行实现路径和文件级改动建议。"),
    ("reviewer", "评估方案可测性、回归风险与遗漏项。"),
];

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(command) = cli.command.clone() {
        return run_command(&cli, command).await;
    }
    let config = config::load()?;
    let workspace = resolve_workspace(cli.workspace.as_deref())?;
    if let Some(prompt) = &cli.prompt {
        return run_headless(&cli, &config, &workspace, prompt).await;
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("interactive TUI requires a terminal; use `grey \"prompt\"` for headless mode");
    }
    run_tui(&cli, &config, &workspace).await
}

async fn run_command(cli: &Cli, command: Command) -> Result<()> {
    match command {
        Command::SpikeA => grey_tui::run_stream_demo().await,
        Command::SpikeB { file, lsp } => {
            let file = file.unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../grey-core/src/config.rs")
            });
            grey_lsp::run_lsp_spike(&file, lsp.as_deref()).await
        }
        Command::SpikeC {
            prompt,
            provider,
            model,
        } => {
            let config = config::load()?;
            run_spike_c(&config, &prompt, provider.as_deref(), model.as_deref()).await
        }
        Command::Config { action } => run_config(action),
        Command::Sessions { action } => run_sessions(action),
        Command::Providers { action } => run_providers(action),
        Command::Cache { action } => run_cache(action),
        Command::Usage { action } => run_usage(action),
        Command::Orchestrate { prompt, agent } => run_orchestrate(cli, prompt, agent).await,
    }
}

const ORCHESTRATE_AGENT_TIMEOUT_SECS: u64 = 120;

async fn run_orchestrate(cli: &Cli, task: String, raw_specs: Vec<String>) -> Result<()> {
    let config = config::load()?;
    let workspace = resolve_workspace(cli.workspace.as_deref())?;
    let config = Arc::new(config);
    let subagents = parse_orchestrate_agents(raw_specs)?;

    let mut futures = Vec::with_capacity(subagents.len());
    for agent in subagents {
        let child_cli = cli.clone();
        let config = config.clone();
        let workspace = workspace.clone();
        let task = task.clone();
        futures.push(run_orchestrate_subagent(
            child_cli, config, workspace, agent, task,
        ));
    }

    let subagent_results = join_all(futures).await;

    let mut coordinator_cli = cli.clone();
    coordinator_cli.no_save = true;
    coordinator_cli.read_only = true;
    coordinator_cli.auto_approve = false;
    let (coordinator, _, _) =
        build_agent_and_session(&coordinator_cli, &config, &workspace, false)?;
    let synthesis = coordinator
        .run_new(
            "你是任务协调子代理，负责把子代理结论合成为可执行计划。",
            build_coordinator_prompt(&task, &subagent_results),
            None,
        )
        .await?;

    if cli.format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::to_string(&OrchestrateOutput {
                task,
                subagents: subagent_results,
                synthesis: AgentOutcomeSummary {
                    response: synthesis.response,
                    provider: synthesis.provider_id,
                    model: synthesis.model,
                    steps: synthesis.steps,
                    cached: synthesis.cached,
                },
            })?
        );
        return Ok(());
    }

    println!("task: {}", task);
    for result in &subagent_results {
        println!(
            "\n[{name}] provider={provider} model={model} steps={steps} cached={cached}",
            name = result.name,
            provider = result.provider,
            model = result.model,
            steps = result.steps,
            cached = result.cached
        );
        println!("{}", result.response);
    }
    println!("\n== Synthesis ==\n{}", synthesis.response);
    Ok(())
}

fn parse_orchestrate_agents(raw_specs: Vec<String>) -> Result<Vec<OrchestrateAgent>> {
    if raw_specs.is_empty() {
        return Ok(default_orchestrate_agents());
    }
    raw_specs
        .into_iter()
        .map(|raw| {
            let (name, task) = raw.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("invalid --agent spec `{raw}`, expected name:task")
            })?;
            Ok(OrchestrateAgent {
                name: name.to_string(),
                task: task.to_string(),
                system_prompt: format!("你是{name}子代理，直接输出结论即可。"),
            })
        })
        .collect()
}

fn default_orchestrate_agents() -> Vec<OrchestrateAgent> {
    DEFAULT_ORCHESTRATE_AGENTS
        .iter()
        .map(|(name, task)| OrchestrateAgent {
            name: (*name).to_string(),
            task: (*task).to_string(),
            system_prompt: format!("你是{name}子代理，直接输出结论即可。"),
        })
        .collect()
}

fn build_coordinator_prompt(task: &str, subagents: &[OrchestrateAgentResult]) -> String {
    let mut chunks = Vec::with_capacity(subagents.len() + 2);
    chunks.push(format!("主任务: {task}"));
    for result in subagents {
        chunks.push(format!(
            "[{}]\n子任务: {}\n结论:\n{}\n",
            result.name, result.task, result.response
        ));
    }
    chunks.push("请输出：1)最终结论 2)最小落地步骤 3)测试检查清单".to_string());
    chunks.join("\n")
}

async fn run_orchestrate_subagent(
    mut cli: Cli,
    config: Arc<GreyConfig>,
    workspace: PathBuf,
    agent: OrchestrateAgent,
    task: String,
) -> OrchestrateAgentResult {
    cli.no_save = true;
    cli.read_only = true;
    cli.auto_approve = false;
    let model_hint = {
        let resolved_provider = cli
            .provider
            .as_deref()
            .unwrap_or(&config.default_provider)
            .to_string();
        cli.model
            .clone()
            .unwrap_or_else(|| default_model_for_provider(&config, &resolved_provider))
    };
    let (agent_client, _store, existing) =
        match build_agent_and_session(&cli, &config, &workspace, false) {
            Ok((agent_client, _store, existing)) => (agent_client, _store, existing),
            Err(error) => {
                return OrchestrateAgentResult {
                    name: agent.name,
                    task: agent.task,
                    response: "sub-agent initialization failed".to_string(),
                    provider: cli
                        .provider
                        .clone()
                        .unwrap_or_else(|| "unresolved".to_string()),
                    model: model_hint,
                    steps: 0,
                    cached: false,
                    success: false,
                    error: Some(error.to_string()),
                };
            }
        };
    let agent_provider = agent_client.provider_id().to_string();
    let _ = existing;
    let prompt = format!("主任务: {task}\n子任务: {}\n请直接输出结论。", agent.task);
    let run = agent_client.run_new(agent.system_prompt, prompt, None);
    let outcome = match tokio::time::timeout(
        Duration::from_secs(ORCHESTRATE_AGENT_TIMEOUT_SECS),
        run,
    )
    .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            return OrchestrateAgentResult {
                name: agent.name,
                task: agent.task,
                response: "sub-agent execution failed".to_string(),
                provider: agent_provider,
                model: model_hint,
                steps: 0,
                cached: false,
                success: false,
                error: Some(error.to_string()),
            };
        }
        Err(_) => {
            return OrchestrateAgentResult {
                name: agent.name,
                task: agent.task,
                response: "sub-agent execution timed out".to_string(),
                provider: agent_provider,
                model: model_hint,
                steps: 0,
                cached: false,
                success: false,
                error: Some("sub-agent execution timed out".to_string()),
            };
        }
    };
    OrchestrateAgentResult {
        name: agent.name,
        task: agent.task,
        response: outcome.response,
        provider: outcome.provider_id,
        model: outcome.model,
        steps: outcome.steps,
        cached: outcome.cached,
        success: true,
        error: None,
    }
}

async fn run_headless(
    cli: &Cli,
    config: &GreyConfig,
    workspace: &Path,
    prompt: &str,
) -> Result<()> {
    let prompt = apply_pre_prompt_hooks(&config.hooks.pre_prompt, prompt).await?;
    let (agent, store, existing) = build_agent_and_session(cli, config, workspace, false)?;
    let usage_tracker = agent.usage_tracker();
    let outcome = if cli.format == OutputFormat::Text {
        run_with_text_events(&agent, existing.as_ref(), &prompt).await?
    } else {
        run_with_cancellation(&agent, existing.as_ref(), &prompt, None).await?
    };
    let session_id = persist_outcome(
        store.as_ref(),
        existing,
        &outcome,
        &prompt,
        workspace,
        cli.no_save,
        usage_tracker.as_deref(),
    )?;
    let tracked_cost = usage_tracker
        .as_ref()
        .and_then(|tracker| {
            session_id
                .as_deref()
                .and_then(|id| tracker.session_usage(id))
                .or_else(|| tracker.session_usage("default"))
        })
        .map(|usage| usage.total_cost_usd)
        .unwrap_or(0.0);

    match cli.format {
        OutputFormat::Text => {
            if !outcome.response.ends_with('\n') {
                println!();
            }
            if let Some(id) = &session_id {
                eprintln!("[session {id}]");
            }
        }
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&HeadlessOutput {
                response: outcome.response,
                session_id,
                usage: HeadlessUsage {
                    input_tokens: outcome.usage.input_tokens,
                    output_tokens: outcome.usage.output_tokens,
                    cost_usd: tracked_cost,
                    cached: outcome.cached,
                    provider: outcome.provider_id.clone(),
                    model: outcome.model.clone(),
                },
                steps: outcome.steps,
                cached: outcome.cached,
                provider: outcome.provider_id.clone(),
                model: outcome.model.clone(),
            })?
        ),
    }
    Ok(())
}

fn build_agent_and_session(
    cli: &Cli,
    config: &GreyConfig,
    workspace: &Path,
    tui_mode: bool,
) -> Result<(Agent, Option<SessionStore>, Option<Session>)> {
    anyhow::ensure!(
        (1..=100).contains(&cli.max_steps),
        "max-steps must be between 1 and 100"
    );
    let router = ProviderRouter::from_config(config)?;
    let resolved = if let Some(provider_id) = cli.provider.as_deref() {
        let model = cli
            .model
            .clone()
            .unwrap_or_else(|| default_model_for_provider(config, provider_id));
        router
            .resolve_explicit(provider_id, &model)
            .with_context(|| format!("unknown provider `{provider_id}`"))?
    } else {
        let task = cli.task.map(|t| t.to_core()).unwrap_or_default();
        router.resolve(&task)?
    };
    let fallback_providers = if cli.no_fallback {
        Vec::new()
    } else {
        let health = router.fallback_handle();
        router
            .resolve_candidates(&resolved.fallback_chain)?
            .into_iter()
            .map(|candidate| candidate.with_health(health.clone()))
            .collect()
    };
    let resolved_provider_id = resolved.provider_id.clone();
    let provider: Arc<dyn Provider> = resolved.provider;
    let model = resolved.model;
    let approver: Arc<dyn Approver> = if cli.auto_approve {
        Arc::new(AlwaysApprove)
    } else if cli.read_only || tui_mode {
        Arc::new(DenySideEffects)
    } else {
        Arc::new(StdioApprover)
    };
    let builtin = Arc::new(BuiltinTools::new(workspace, approver)?);
    let mut executors: Vec<Arc<dyn ToolExecutor>> = vec![builtin];
    let mcp = McpTools::new(config.mcp_tools.clone());
    if !mcp.is_empty() {
        executors.push(Arc::new(mcp));
    }
    let duplicated = duplicate_tool_names(&executors);
    if !duplicated.is_empty() {
        bail!("duplicate tool name(s) detected across tool providers: {duplicated:?}");
    }
    let tools: Arc<dyn ToolExecutor> = Arc::new(HookedTools::new(
        Arc::new(CombinedTools::new(executors)),
        config.hooks.pre_tool_call.clone(),
        config.hooks.post_tool_call.clone(),
    ));
    let mut options = AgentOptions::new(model.clone());
    options.max_steps = cli.max_steps;
    let context = ContextManager::with_budget(
        config.context.clone(),
        Arc::new(CharApproxCounter),
        Some(SummaryEngine::new(
            provider.clone(),
            model.clone(),
            config.context.summary_max_messages,
        )),
        model.clone(),
    );
    let mut agent =
        Agent::new(provider, tools, context, options).with_provider_id(resolved_provider_id);

    if !cli.no_cache && config.cache.enabled {
        let cache_path = cache_database_path();
        let cache = std::sync::Arc::new(
            grey_core::RequestCache::open(
                &cache_path,
                grey_core::cache::RequestCacheConfig {
                    enabled: config.cache.enabled,
                    max_entries: config.cache.max_entries,
                    ttl_hours: config.cache.ttl_hours,
                },
            )
            .context("opening request cache")?,
        );
        agent = agent.with_cache(cache);
    }

    let usage_tracker = std::sync::Arc::new(grey_core::UsageTracker::new(&config.usage));
    agent = agent.with_usage(usage_tracker.clone());

    if !cli.no_fallback {
        agent = agent
            .with_fallback_chain(resolved.fallback_chain)
            .with_fallback_providers(fallback_providers)
            .with_fallback_health(router.fallback_handle());
    }

    let needs_store = !cli.no_save || cli.session.is_some() || cli.continue_session;
    let store = needs_store
        .then(|| SessionStore::open(&session_database_path()))
        .transpose()?;
    let workspace_text = workspace.to_string_lossy();
    let existing = if let Some(id) = &cli.session {
        Some(
            store
                .as_ref()
                .context("session store was not initialized")?
                .load(id)?
                .with_context(|| format!("session not found: {id}"))?,
        )
    } else if cli.continue_session {
        Some(
            store
                .as_ref()
                .context("session store was not initialized")?
                .latest_for_workspace(&workspace_text)?
                .context("no session found for this workspace")?,
        )
    } else {
        None
    };
    if let Some(session) = &existing {
        anyhow::ensure!(
            session.workspace == workspace_text,
            "session workspace mismatch: stored {}, current {}",
            session.workspace,
            workspace.display()
        );
        if let Some(store) = &store {
            if let Some(usage_json) = store.load_usage(&session.id)? {
                usage_tracker
                    .load_json(&session.id, &usage_json)
                    .map_err(|error| anyhow::anyhow!(error))?;
            }
        }
        agent = agent.with_usage_session_id(session.id.clone());
    }
    Ok((agent, store, existing))
}

async fn run_with_text_events(
    agent: &Agent,
    existing: Option<&Session>,
    prompt: &str,
) -> Result<AgentOutcome> {
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                AgentEvent::Delta(delta) => {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::ToolStarted(call) => {
                    eprintln!("\n[tool {}] {}", call.name, call.arguments)
                }
                AgentEvent::ToolFinished(result) => eprintln!(
                    "[tool {}] {}: {}",
                    result.name,
                    if result.success { "ok" } else { "failed" },
                    result.output
                ),
                AgentEvent::Retry { attempt, error } => {
                    eprintln!("[retry {attempt}] {error}")
                }
                AgentEvent::ContextTrimmed(audit) => eprintln!(
                    "[context] dropped {} old message(s), retained {} chars",
                    audit.dropped_messages, audit.retained_chars
                ),
                AgentEvent::Completed { .. } => {}
                AgentEvent::Failed(error) => eprintln!("[agent failed] {error}"),
                AgentEvent::ProviderSwitched { from, to, reason } => {
                    eprintln!("[switch] {from} → {to}: {reason}")
                }
                AgentEvent::CacheHit { model } => eprintln!("[cache] hit for {model}"),
            }
        }
    });
    let result = run_with_cancellation(agent, existing, prompt, Some(&events_tx)).await;
    drop(events_tx);
    let _ = printer.await;
    result
}

async fn run_with_cancellation(
    agent: &Agent,
    existing: Option<&Session>,
    prompt: &str,
    events: Option<&mpsc::UnboundedSender<AgentEvent>>,
) -> Result<AgentOutcome> {
    let run = async {
        if let Some(session) = existing {
            agent
                .continue_messages(session.messages.clone(), prompt, events)
                .await
        } else {
            agent.run_new(SYSTEM_PROMPT, prompt, events).await
        }
    };
    tokio::select! {
        result = run => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("installing Ctrl-C handler")?;
            bail!("interrupted by Ctrl-C")
        }
    }
}

fn persist_outcome(
    store: Option<&SessionStore>,
    existing: Option<Session>,
    outcome: &AgentOutcome,
    prompt: &str,
    workspace: &Path,
    no_save: bool,
    usage_tracker: Option<&grey_core::UsageTracker>,
) -> Result<Option<String>> {
    if no_save {
        return Ok(existing.map(|session| session.id));
    }
    let store = store.context("session store was not initialized")?;
    let mut session = match existing {
        Some(mut session) => {
            session.messages.clone_from(&outcome.messages);
            session
        }
        None => Session::new(
            prompt
                .lines()
                .next()
                .unwrap_or(prompt)
                .chars()
                .take(80)
                .collect::<String>(),
            workspace.to_string_lossy(),
            outcome.messages.clone(),
        ),
    };
    store.save(&mut session)?;
    persist_usage(store, &session.id, usage_tracker)?;
    Ok(Some(session.id))
}

fn persist_usage(
    store: &SessionStore,
    session_id: &str,
    usage_tracker: Option<&grey_core::UsageTracker>,
) -> Result<()> {
    let Some(tracker) = usage_tracker else {
        return Ok(());
    };
    let usage_json = tracker
        .persist_json(session_id)
        .or_else(|| tracker.persist_json("default"));
    if let Some(usage_json) = usage_json {
        store.save_usage(session_id, &usage_json)?;
    }
    Ok(())
}

async fn run_tui(cli: &Cli, config: &GreyConfig, workspace: &Path) -> Result<()> {
    let (agent, store, existing) = build_agent_and_session(cli, config, workspace, true)?;
    let usage_tracker = agent.usage_tracker();
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (prompts_tx, mut prompts_rx) = mpsc::unbounded_channel::<String>();
    let workspace = workspace.to_path_buf();
    let no_save = cli.no_save;
    let pre_prompt_hooks = config.hooks.pre_prompt.clone();
    let worker = tokio::spawn(async move {
        let mut session = existing;
        while let Some(prompt) = prompts_rx.recv().await {
            let prompt = match apply_pre_prompt_hooks(&pre_prompt_hooks, &prompt).await {
                Ok(prompt) => prompt,
                Err(error) => {
                    let _ = events_tx.send(AgentEvent::Failed(format!("{error:#}")));
                    continue;
                }
            };
            let result = match &session {
                Some(session) => {
                    agent
                        .continue_messages(session.messages.clone(), &prompt, Some(&events_tx))
                        .await
                }
                None => {
                    agent
                        .run_new(SYSTEM_PROMPT, &prompt, Some(&events_tx))
                        .await
                }
            };
            match result {
                Ok(outcome) => {
                    let mut current = match session.take() {
                        Some(mut current) => {
                            current.messages.clone_from(&outcome.messages);
                            current
                        }
                        None => Session::new(
                            prompt
                                .lines()
                                .next()
                                .unwrap_or(&prompt)
                                .chars()
                                .take(80)
                                .collect::<String>(),
                            workspace.to_string_lossy(),
                            outcome.messages.clone(),
                        ),
                    };
                    if !no_save {
                        if let Some(store) = &store {
                            if let Err(error) = store.save(&mut current) {
                                let _ = events_tx
                                    .send(AgentEvent::Failed(format!("saving session: {error:#}")));
                                session = Some(current);
                                continue;
                            }
                            if let Err(error) =
                                persist_usage(store, &current.id, usage_tracker.as_deref())
                            {
                                let _ = events_tx
                                    .send(AgentEvent::Failed(format!("saving usage: {error:#}")));
                            }
                        }
                    }
                    session = Some(current);
                    let _ = events_tx.send(AgentEvent::Completed {
                        usage: outcome.usage,
                        steps: outcome.steps,
                    });
                }
                Err(error) => {
                    let _ = events_tx.send(AgentEvent::Failed(format!("{error:#}")));
                }
            }
        }
    });
    let ui_result = grey_tui::run_agent_tui(events_rx, prompts_tx).await;
    worker.abort();
    let _ = worker.await;
    ui_result
}

async fn apply_pre_prompt_hooks(commands: &[String], prompt: &str) -> Result<String> {
    if commands.is_empty() {
        return Ok(prompt.to_string());
    }
    let mut current = prompt.to_string();
    for command in commands {
        let payload = json!({
            "event": "pre_prompt",
            "prompt": current,
        })
        .to_string();
        let output = run_shell_command(command, Some(&payload), DEFAULT_HOOK_TIMEOUT_MS).await?;
        if let Some(next) = extract_prompt_from_hook_output(&output) {
            current = next;
        }
    }
    Ok(current)
}

fn extract_prompt_from_hook_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(prompt) = value.get("prompt").and_then(Value::as_str) {
            return Some(prompt.to_string());
        }
    }
    Some(trimmed.to_string())
}

async fn run_shell_command(command: &str, input: Option<&str>, timeout_ms: u64) -> Result<String> {
    let mut command_process = TokioCommand::new("sh");
    command_process
        .arg("-lc")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    let mut child = command_process.spawn().context("spawning command")?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().context("opening hook stdin")?;
        stdin
            .write_all(input.as_bytes())
            .await
            .context("writing hook input")?;
    }
    let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("command timed out after {timeout_ms}ms"))?
        .context("waiting for hook command")?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    if output.status.success() {
        Ok(text)
    } else {
        bail!("{}", text.trim());
    }
}

fn run_config(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let config = config::load()?;
            println!("{}", toml::to_string_pretty(&config.redacted())?);
        }
        ConfigAction::Init { force } => {
            let path = config::default_config_path();
            if path.exists() && !force {
                bail!(
                    "configuration already exists at {}; pass --force to replace it",
                    path.display()
                );
            }
            config::write_default_config(&path)?;
            println!("wrote {}", path.display());
        }
        ConfigAction::Path => match config::config_path() {
            Some(path) => println!("{}", path.display()),
            None => println!("(no config file; defaults + env are in effect)"),
        },
    }
    Ok(())
}

fn run_sessions(action: SessionAction) -> Result<()> {
    let store = SessionStore::open(&session_database_path())?;
    match action {
        SessionAction::List { limit } => {
            for session in store.list(limit)? {
                println!(
                    "{}\t{}\t{} messages\t{}",
                    session.id, session.updated_at, session.message_count, session.title
                );
            }
        }
        SessionAction::Show { id } => {
            let session = store
                .load(&id)?
                .with_context(|| format!("session not found: {id}"))?;
            println!("{}", serde_json::to_string_pretty(&session)?);
        }
    }
    Ok(())
}

fn run_providers(action: ProviderAction) -> Result<()> {
    let config = config::load()?;
    let router = ProviderRouter::from_config(&config)?;
    match action {
        ProviderAction::List => {
            for provider in &config.providers {
                let models = provider
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{}\tprotocol={}\tbase_url={}\tmodels=[{}]",
                    provider.id, provider.protocol, provider.base_url, models
                );
            }
        }
        ProviderAction::Show { id } => {
            let ids = router.list_provider_ids();
            if !ids.contains(&id) {
                bail!("provider not found: {id}");
            }
            let provider = config
                .provider(&id)
                .with_context(|| format!("provider not found: {id}"))?;
            println!("provider: {}", provider.id);
            println!("protocol: {}", provider.protocol);
            println!("base_url: {}", provider.base_url);
            println!(
                "api_key: {}",
                if provider.api_key.is_empty() {
                    "(none)"
                } else {
                    "***"
                }
            );
            println!("models:");
            for model in &provider.models {
                println!(
                    "  {}\t{}\tcontext={} output={}",
                    model.id, model.name, model.context_limit, model.output_limit
                );
            }
        }
    }
    Ok(())
}

fn run_cache(action: CacheAction) -> Result<()> {
    let cache = grey_core::RequestCache::open(
        &cache_database_path(),
        grey_core::cache::RequestCacheConfig::default(),
    )?;
    match action {
        CacheAction::Clear => {
            let count = cache.stats().entries;
            cache.clear()?;
            println!("cleared {count} cache entries");
        }
        CacheAction::Stats => {
            let stats = cache.stats();
            println!("hits: {}", stats.hits);
            println!("misses: {}", stats.misses);
            println!("entries: {}", stats.entries);
        }
    }
    Ok(())
}

fn run_usage(action: UsageAction) -> Result<()> {
    let store = SessionStore::open(&session_database_path())?;
    let config = config::load()?;
    let tracker = grey_core::UsageTracker::new(&config.usage);
    match action {
        UsageAction::Show { id } => {
            let json = store
                .load_usage(&id)?
                .with_context(|| format!("no usage data for session {id}"))?;
            tracker
                .load_json(&id, &json)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{}", tracker.format_panel(&id));
        }
        UsageAction::Summary => {
            let summaries = store.list(1000)?;
            for s in &summaries {
                if let Some(json) = store.load_usage(&s.id)? {
                    let _ = tracker.load_json(&s.id, &json);
                }
            }
            let agg = tracker.aggregate();
            println!(
                "Tokens: {} in / {} out\nCost: ${:.4}\nTurns: {}",
                agg.total_input_tokens,
                agg.total_output_tokens,
                agg.total_cost_usd,
                agg.turns.len()
            );
        }
    }
    Ok(())
}

fn cache_database_path() -> PathBuf {
    if let Some(path) = env::var_os("GREY_CACHE_DB") {
        return PathBuf::from(path);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/grey/cache.db");
    }
    PathBuf::from(".grey-cache.db")
}

fn default_model_for_provider(config: &GreyConfig, provider_id: &str) -> String {
    if let Some(provider) = config.provider(provider_id) {
        if let Some(model) = provider.models.first().map(|model| model.id.clone()) {
            if !model.is_empty() {
                return model;
            }
        }
    }
    match provider_id {
        "mock" => config.model.clone(),
        "openai" => config.openai.model.clone(),
        "anthropic" => config.anthropic.model.clone(),
        _ => config.default_model.clone(),
    }
}

async fn run_spike_c(
    config: &GreyConfig,
    prompt: &str,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<()> {
    let router = ProviderRouter::from_config(config)?;
    let resolved = match (provider_override, model_override) {
        (Some(pid), Some(mid)) => router.resolve_explicit(pid, mid)?,
        (Some(pid), None) => {
            let model = default_model_for_provider(config, pid);
            router.resolve_explicit(pid, &model)?
        }
        (None, _) => router.resolve(&grey_core::TaskKind::Default)?,
    };
    let provider = &resolved.provider;
    let model = &resolved.model;
    let request = ChatRequest::new(
        model.clone(),
        vec![
            ChatMessage::new(Role::System, "You are Grey, a helpful coding assistant."),
            ChatMessage::new(Role::User, prompt),
        ],
    );
    println!("[spike-c] provider={} model={model}", provider.id());
    let started = std::time::Instant::now();
    let mut stream = provider.stream_chat(&request).await?;

    use futures_util::StreamExt;
    let mut text = String::new();
    let mut calls = 0_u32;
    let mut usage = None;
    while let Some(event) = stream.next().await {
        match event {
            grey_core::ProviderEvent::Delta(delta) => {
                text.push_str(&delta);
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            grey_core::ProviderEvent::ToolCall(call) => {
                calls += 1;
                println!("\n[tool] {} {} {}", call.id, call.name, call.arguments);
            }
            grey_core::ProviderEvent::Done(done_usage) => usage = Some(done_usage),
            grey_core::ProviderEvent::Error(error) => bail!("provider error: {error}"),
        }
    }
    println!(
        "\n[spike-c] done in {:?} ({} chars, {calls} tool calls, usage: {usage:?})",
        started.elapsed(),
        text.chars().count()
    );
    Ok(())
}

fn resolve_workspace(path: Option<&Path>) -> Result<PathBuf> {
    let path = path.map(Path::to_path_buf).unwrap_or(env::current_dir()?);
    let workspace = path
        .canonicalize()
        .with_context(|| format!("canonicalizing workspace {}", path.display()))?;
    anyhow::ensure!(workspace.is_dir(), "workspace must be a directory");
    Ok(workspace)
}

fn session_database_path() -> PathBuf {
    if let Some(path) = env::var_os("GREY_SESSION_DB") {
        return PathBuf::from(path);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/grey/sessions.db");
    }
    PathBuf::from(".grey-sessions.db")
}

fn duplicate_tool_names(tools: &[Arc<dyn ToolExecutor>]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    for definition in tools.iter().flat_map(|executor| executor.definitions()) {
        if !seen.insert(definition.name.clone()) {
            duplicates.push(definition.name);
        }
    }
    duplicates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_pre_prompt_hooks_can_rewrite_from_plain_output() {
        let commands = vec!["printf 'from plain hook'".into()];
        let prompt = apply_pre_prompt_hooks(&commands, "original").await.unwrap();
        assert_eq!(prompt, "from plain hook");
    }

    #[tokio::test]
    async fn apply_pre_prompt_hooks_accepts_json_payload() {
        let commands = vec!["printf '{\"prompt\":\"from json hook\"}'".into()];
        let prompt = apply_pre_prompt_hooks(&commands, "original").await.unwrap();
        assert_eq!(prompt, "from json hook");
    }

    #[tokio::test]
    async fn extract_prompt_from_hook_output_handles_empty_as_none() {
        assert_eq!(extract_prompt_from_hook_output(""), None);
        assert_eq!(
            extract_prompt_from_hook_output("{\"prompt\":\"x\"}"),
            Some("x".to_string())
        );
        assert_eq!(
            extract_prompt_from_hook_output("not-json"),
            Some("not-json".to_string())
        );
    }
}
