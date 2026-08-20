//! Grey composition root: headless agent, interactive TUI, spikes and config.

use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::future::join_all;
use grey_core::{
    config, Agent, AgentEvent, AgentOptions, AgentOutcome, CharApproxCounter, ChatMessage,
    ChatRequest, ContextManager, GreyConfig, HookEvent, HookPayload, HookRunner, PluginConfig,
    PluginKind, Provider, Role, Session, SessionStore, SummaryEngine, ToolExecutor,
};
use grey_provider::chatgpt_oauth::ChatgptOauth;
use grey_provider::router::ProviderRouter;
use grey_tools::{
    AlwaysApprove, Approver, BuiltinTools, DenySideEffects, HookedApprover, LspTools, McpTools,
    PluginTools, StdioApprover,
};
use grey_tools::{CombinedTools, HookedTools};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Duration;

const SYSTEM_PROMPT: &str = r#"You are Grey, a careful coding agent working inside one workspace.
Inspect before changing anything. Use read_file, glob, and grep to gather evidence. Use edit_file
only with an exact old_string that occurs once. After edits, run the relevant tests with bash.
Keep changes scoped to the user's request, report tool failures honestly, and never claim success
without verification evidence."#;
const TUI_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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
enum PluginKindArg {
    Tool,
    Provider,
    Hook,
    Theme,
}

impl PluginKindArg {
    fn to_core(self) -> PluginKind {
        match self {
            PluginKindArg::Tool => PluginKind::Tool,
            PluginKindArg::Provider => PluginKind::Provider,
            PluginKindArg::Hook => PluginKind::Hook,
            PluginKindArg::Theme => PluginKind::Theme,
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
    /// Plugin and extension management (P6).
    Plugins {
        #[command(subcommand)]
        action: PluginAction,
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
    /// Sign in to the OpenAI ChatGPT subscription service.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Multi-agent orchestration: run sub-agents in parallel and synthesize.
    Orchestrate {
        prompt: String,
        /// Add as `name:task` pairs, e.g. `--agent coder:给出 patch 方案`.
        #[arg(long, value_name = "name:task")]
        agent: Vec<String>,
        /// Share selected context fields with every sub-agent (`task`, `summary`).
        #[arg(long, value_enum)]
        share_context: Vec<OrchestrateShareContext>,
    },
    /// Repeat a prompt for a bounded number of iterations.
    Loop {
        /// Base prompt for each iteration.
        prompt: String,
        /// Maximum number of iterations to run.
        #[arg(long, default_value_t = 3)]
        iterations: usize,
        /// Stop early when a response contains this token.
        #[arg(long)]
        until: Option<String>,
    },
    /// Iterative Goal mode: keep refining toward an explicit acceptance token.
    Goal {
        /// Goal statement to pursue.
        goal: String,
        /// Maximum number of refinement iterations.
        #[arg(long, default_value_t = 5)]
        iterations: usize,
        /// Consider the goal complete when a response contains this token.
        #[arg(long, value_name = "TOKEN", default_value = "DONE")]
        done_when: String,
    },
}

#[derive(Subcommand, Clone)]
enum AuthAction {
    /// Open the system browser and sign in with ChatGPT.
    Login { provider: AuthProvider },
    /// Print non-secret ChatGPT sign-in metadata.
    Status { provider: AuthProvider },
    /// Remove the saved ChatGPT sign-in from the OS keyring.
    Logout { provider: AuthProvider },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AuthProvider {
    Openai,
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
enum PluginAction {
    /// List configured plugins.
    List,
    /// Show plugin details by id.
    Show { id: String },
    /// Add or update a plugin entry.
    Add {
        id: String,
        #[arg(long, value_enum, default_value_t = PluginKindArg::Tool)]
        kind: PluginKindArg,
        /// Executable command for tool/hook plugins.
        #[arg(long)]
        command: Option<String>,
        /// Repeated arguments appended to the command.
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Human friendly plugin name.
        #[arg(long)]
        name: Option<String>,
        /// Optional description.
        #[arg(long)]
        description: Option<String>,
        /// Hook event for hook plugins (`pre_prompt`, `pre_tool_call`, ...).
        #[arg(long)]
        hook_event: Option<String>,
        /// Optional plugin semantic version.
        #[arg(long)]
        version: Option<String>,
        /// Command timeout override in milliseconds.
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// Enable plugin immediately.
        #[arg(long, default_value_t = true)]
        enabled: bool,
    },
    /// Remove a plugin by id.
    Remove { id: String },
    /// Enable an existing plugin.
    Enable { id: String },
    /// Disable an existing plugin.
    Disable { id: String },
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

#[derive(Serialize)]
struct RepeaterOutput {
    prompt: String,
    iterations: usize,
    completed: bool,
    response: String,
    session_id: Option<String>,
    usage: HeadlessUsage,
    steps: usize,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum OrchestrateShareContext {
    Task,
    Summary,
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
    status: String,
    summary: String,
    recommendations: Vec<String>,
    risks: Vec<String>,
    artifacts: Vec<String>,
    error: Option<String>,
}

const ORCHESTRATE_MAX_SUMMARY_CHARS: usize = 500;
const ORCHESTRATE_MAX_LIST_ITEMS: usize = 24;
const ORCHESTRATE_MAX_LIST_ITEM_CHARS: usize = 180;
const ORCHESTRATE_MAX_SYNTHESIS_RESPONSE_CHARS: usize = 1024;
const ORCHESTRATE_SESSION_MESSAGE_PREVIEW_CHARS: usize = 320;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OrchestrateAgentContract {
    #[serde(default = "default_orchestrate_status")]
    status: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    recommendations: Vec<String>,
    #[serde(default)]
    risks: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default)]
#[serde(deny_unknown_fields)]
struct OrchestrateCoordinatorContract {
    #[serde(default)]
    response: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    steps: usize,
    #[serde(default)]
    cached: bool,
}

fn default_orchestrate_status() -> String {
    "warn".to_string()
}

#[derive(Serialize)]
struct OrchestrateOutput {
    task: String,
    subagents: Vec<OrchestrateAgentResult>,
    synthesis: AgentOutcomeSummary,
    session_id: Option<String>,
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
        Command::Plugins { action } => run_plugins(action),
        Command::Cache { action } => run_cache(action),
        Command::Usage { action } => run_usage(action),
        Command::Auth { action } => run_auth(action).await,
        Command::Orchestrate {
            prompt,
            agent,
            share_context,
        } => run_orchestrate(cli, prompt, agent, share_context).await,
        Command::Loop {
            prompt,
            iterations,
            until,
        } => run_repeater(cli, RepeaterMode::Loop, prompt, iterations, until).await,
        Command::Goal {
            goal,
            iterations,
            done_when,
        } => run_repeater(cli, RepeaterMode::Goal, goal, iterations, Some(done_when)).await,
    }
}

async fn run_auth(action: AuthAction) -> Result<()> {
    let provider = match &action {
        AuthAction::Login { provider }
        | AuthAction::Status { provider }
        | AuthAction::Logout { provider } => provider,
    };
    match provider {
        AuthProvider::Openai => {}
    }

    let oauth = ChatgptOauth::new()?;
    match action {
        AuthAction::Login { .. } => {
            let pending = oauth.begin_login().await?;
            let url = pending.authorize_url();
            println!("Open this URL to sign in with ChatGPT:\n{url}");
            if let Err(error) = open_system_browser(url.as_str()).await {
                eprintln!("Could not open the system browser: {error}. Paste the URL above into a browser.");
            }
            oauth.complete_login(pending).await?;
            println!("ChatGPT subscription login complete.");
        }
        AuthAction::Status { .. } => {
            let status = oauth.status().await?;
            println!("logged_in: {}", status.logged_in);
            println!(
                "account_id: {}",
                status.account_id.as_deref().unwrap_or("(none)")
            );
            println!(
                "expires_at: {}",
                status
                    .expires_at
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "(none)".to_string())
            );
        }
        AuthAction::Logout { .. } => {
            oauth.logout().await?;
            println!("ChatGPT subscription login removed.");
        }
    }
    Ok(())
}

async fn open_system_browser(url: &str) -> Result<()> {
    use grey_core::process::run_bounded;

    let output = run_bounded(browser_opener_spec(url)?).await?;
    anyhow::ensure!(
        output.status.success(),
        "system browser opener exited with {}",
        output.status
    );
    Ok(())
}

fn browser_opener_spec(url: &str) -> Result<grey_core::process::CommandSpec> {
    use grey_core::process::CommandSpec;

    #[cfg(target_os = "macos")]
    let spec = CommandSpec::direct("/usr/bin/open", [OsString::from(url)]);
    #[cfg(target_os = "linux")]
    let spec = CommandSpec::direct("xdg-open", [OsString::from(url)])
        .env("PATH", env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    let spec = CommandSpec::direct("explorer.exe", [OsString::from(url)]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let spec = bail!("opening a browser is not supported on this platform");
    Ok(spec.timeout(Duration::from_secs(10)))
}

const ORCHESTRATE_AGENT_TIMEOUT_SECS: u64 = 120;
const ORCHESTRATE_AGENT_MAX_ATTEMPTS: usize = 2;
const ORCHESTRATE_AGENT_RETRY_DELAY_MS: u64 = 200;

async fn run_orchestrate(
    cli: &Cli,
    task: String,
    raw_specs: Vec<String>,
    share_context: Vec<OrchestrateShareContext>,
) -> Result<()> {
    let config = config::load()?;
    let workspace = resolve_workspace(cli.workspace.as_deref())?;
    let config = Arc::new(config);
    let hooks = Arc::new(HookRunner::new(
        &config.hooks,
        &config.plugins,
        &config.runtime,
    ));
    let provider = cli
        .provider
        .as_deref()
        .unwrap_or(&config.default_provider)
        .to_string();
    let model = cli
        .model
        .clone()
        .unwrap_or_else(|| default_model_for_provider(&config, &provider));
    run_best_effort_hook(
        &hooks,
        lifecycle_hook_payload(HookEvent::SessionStart, &workspace, &provider, &model),
    )
    .await;
    let mut task = task;
    let result = async {
        task = apply_prompt_hook(
            &hooks,
            HookEvent::PreMessageSend,
            &workspace,
            &provider,
            &model,
            &task,
        )
        .await?;
        task = apply_prompt_hook(
            &hooks,
            HookEvent::PrePrompt,
            &workspace,
            &provider,
            &model,
            &task,
        )
        .await?;
        run_orchestrate_session(
            cli,
            task.clone(),
            raw_specs,
            share_context,
            config,
            workspace.clone(),
            hooks.clone(),
        )
        .await
    }
    .await;
    let error = result.as_ref().err().map(|error| error.to_string());
    let mut completion =
        lifecycle_hook_payload(HookEvent::Completion, &workspace, &provider, &model);
    completion.prompt = Some(&task);
    completion.success = Some(result.is_ok());
    completion.error = error.as_deref();
    run_best_effort_hook(&hooks, completion).await;
    let mut session_end =
        lifecycle_hook_payload(HookEvent::SessionEnd, &workspace, &provider, &model);
    session_end.success = Some(result.is_ok());
    session_end.error = error.as_deref();
    run_best_effort_hook(&hooks, session_end).await;
    result
}

// Orchestration children are internal turns in one user-visible session. They share
// tool/permission hooks, while only this outer boundary emits lifecycle events.
async fn run_orchestrate_session(
    cli: &Cli,
    task: String,
    raw_specs: Vec<String>,
    share_context: Vec<OrchestrateShareContext>,
    config: Arc<GreyConfig>,
    workspace: PathBuf,
    hooks: Arc<HookRunner>,
) -> Result<()> {
    let subagents = parse_orchestrate_agents(raw_specs)?;
    let shared_context = build_orchestrate_shared_context(&task, &share_context, cli, &workspace)?;

    let mut futures = Vec::with_capacity(subagents.len());
    for agent in subagents {
        let child_cli = cli.clone();
        let config = config.clone();
        let workspace = workspace.clone();
        let task = task.clone();
        let shared_context = shared_context.clone();
        let hooks = hooks.clone();
        futures.push(run_orchestrate_subagent(
            child_cli,
            config,
            workspace,
            agent,
            task,
            shared_context,
            hooks,
        ));
    }

    let subagent_results = join_all(futures).await;

    let mut coordinator_cli = cli.clone();
    coordinator_cli.no_save = cli.no_save;
    coordinator_cli.read_only = true;
    coordinator_cli.auto_approve = false;
    let (coordinator, store, existing) =
        build_agent_and_session(&coordinator_cli, &config, &workspace, false, &hooks)?;
    let coordinator_prompt = build_coordinator_prompt(&task, &subagent_results);
    let synthesis_outcome = coordinator
        .run_new(
            "你是任务协调子代理，负责把子代理结论合成为可执行计划。",
            coordinator_prompt.as_str(),
            None,
        )
        .await?;
    let synthesis =
        parse_orchestrate_coordinator_contract(&synthesis_outcome.response, &synthesis_outcome);
    let usage_tracker = coordinator.usage_tracker();
    let existing_messages = existing
        .as_ref()
        .map(|session| session.messages.as_slice())
        .unwrap_or(&[]);
    let orchestration_messages = build_orchestrate_session_messages(
        existing_messages,
        &task,
        &subagent_results,
        &synthesis_outcome.response,
    );
    let session_id = persist_outcome(
        store.as_ref(),
        existing,
        &AgentOutcome {
            messages: orchestration_messages,
            response: synthesis_outcome.response.clone(),
            usage: synthesis_outcome.usage,
            steps: synthesis_outcome.steps,
            cached: synthesis_outcome.cached,
            provider_id: synthesis_outcome.provider_id,
            model: synthesis_outcome.model,
        },
        &task,
        &workspace,
        cli.no_save,
        usage_tracker.as_deref(),
    )?;

    if cli.format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::to_string(&OrchestrateOutput {
                task,
                subagents: subagent_results,
                synthesis: AgentOutcomeSummary {
                    response: synthesis.response,
                    provider: synthesis.provider,
                    model: synthesis.model,
                    steps: synthesis.steps,
                    cached: synthesis.cached,
                },
                session_id,
            })?
        );
        return Ok(());
    }

    for line in render_orchestrate_text_panels(
        &task,
        &subagent_results,
        &synthesis,
        &synthesis_outcome.response,
    ) {
        println!("{}", line);
    }
    if let Some(session_id) = &session_id {
        eprintln!("[session {session_id}]");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RepeaterMode {
    Loop,
    Goal,
}

async fn run_repeater(
    cli: &Cli,
    mode: RepeaterMode,
    prompt: String,
    iterations: usize,
    stop_when: Option<String>,
) -> Result<()> {
    if iterations == 0 {
        bail!("iterations must be greater than zero");
    }
    let config = config::load()?;
    let workspace = resolve_workspace(cli.workspace.as_deref())?;
    let workspace_text = workspace.to_string_lossy();
    let hooks = HookRunner::new(&config.hooks, &config.plugins, &config.runtime);
    let (agent, store, existing) =
        build_agent_and_session(cli, &config, &workspace, false, &hooks)?;
    let usage_tracker = agent.usage_tracker();
    let active_provider = agent.provider_id().to_string();
    let active_model = agent.model().to_string();

    let stop_token = stop_when.as_deref().unwrap_or("DONE");

    run_best_effort_hook(
        &hooks,
        lifecycle_hook_payload(
            HookEvent::SessionStart,
            &workspace,
            &active_provider,
            &active_model,
        ),
    )
    .await;

    let mut session = existing;
    let mut last_outcome: Option<AgentOutcome> = None;
    let mut loop_usage = grey_core::Usage::default();
    let mut prompt_count = 0usize;
    let mut stopped = false;
    let mut last_error: Option<String> = None;

    for iteration in 0..iterations {
        prompt_count += 1;
        let iteration_prompt = match mode {
            RepeaterMode::Loop => prompt.clone(),
            RepeaterMode::Goal => {
                if iteration == 0 {
                    prompt.clone()
                } else {
                    let previous = last_outcome
                        .as_ref()
                        .map(|outcome| outcome.response.as_str())
                        .unwrap_or("");
                    format!(
                        "{prompt}\n\nCurrent attempt: {previous}\n\n请继续改进，并在满足目标后返回包含 {stop_token} 的简短结论。"
                    )
                }
            }
        };
        let next_prompt = async {
            let next = apply_prompt_hook(
                &hooks,
                HookEvent::PreMessageSend,
                &workspace,
                &active_provider,
                &active_model,
                &iteration_prompt,
            )
            .await?;
            apply_prompt_hook(
                &hooks,
                HookEvent::PrePrompt,
                &workspace,
                &active_provider,
                &active_model,
                &next,
            )
            .await
        }
        .await;
        let next_prompt = match next_prompt {
            Ok(prompt) => prompt,
            Err(error) => {
                let error_text = error.to_string();
                let mut completion = lifecycle_hook_payload(
                    HookEvent::Completion,
                    &workspace,
                    &active_provider,
                    &active_model,
                );
                completion.prompt = Some(&iteration_prompt);
                completion.success = Some(false);
                completion.error = Some(&error_text);
                run_best_effort_hook(&hooks, completion).await;
                let mut session_end = lifecycle_hook_payload(
                    HookEvent::SessionEnd,
                    &workspace,
                    &active_provider,
                    &active_model,
                );
                session_end.success = Some(false);
                session_end.error = Some(&error_text);
                run_best_effort_hook(&hooks, session_end).await;
                return Err(error);
            }
        };

        let result = if let Some(session) = &session {
            agent
                .continue_messages(session.messages.clone(), next_prompt.as_str(), None)
                .await
        } else {
            agent
                .run_new(SYSTEM_PROMPT, next_prompt.as_str(), None)
                .await
        };

        match result {
            Ok(outcome) => {
                let mut completion = lifecycle_hook_payload(
                    HookEvent::Completion,
                    &workspace,
                    &outcome.provider_id,
                    &outcome.model,
                );
                completion.prompt = Some(&next_prompt);
                completion.success = Some(true);
                run_best_effort_hook(&hooks, completion).await;

                loop_usage.add_assign(&outcome.usage);

                let mut current = if let Some(mut previous) = session {
                    previous.messages.clone_from(&outcome.messages);
                    previous
                } else {
                    let title = next_prompt
                        .lines()
                        .next()
                        .unwrap_or(&next_prompt)
                        .chars()
                        .take(80)
                        .collect::<String>();
                    Session::new(title, workspace_text.as_ref(), outcome.messages.clone())
                };

                if let Some(store) = &store {
                    if let Err(error) = store.save(&mut current) {
                        last_error = Some(format!("saving session: {error:#}"));
                    }
                    if let Err(error) = persist_usage(store, &current.id, usage_tracker.as_deref())
                    {
                        last_error = Some(format!("saving usage: {error:#}"));
                    }
                }

                if let Some(last_error) = last_error.clone() {
                    let mut session_end = lifecycle_hook_payload(
                        HookEvent::SessionEnd,
                        &workspace,
                        &outcome.provider_id,
                        &outcome.model,
                    );
                    session_end.success = Some(false);
                    session_end.error = Some(&last_error);
                    run_best_effort_hook(&hooks, session_end).await;
                    return Err(anyhow::anyhow!(last_error));
                }

                let should_stop = stop_when
                    .as_deref()
                    .is_some_and(|token| outcome.response.contains(token));
                session = Some(current);
                last_outcome = Some(outcome);
                if should_stop {
                    stopped = true;
                    break;
                }
            }
            Err(error) => {
                let error_text = error.to_string();
                let mut completion = lifecycle_hook_payload(
                    HookEvent::Completion,
                    &workspace,
                    &active_provider,
                    &active_model,
                );
                completion.prompt = Some(&next_prompt);
                completion.success = Some(false);
                completion.error = Some(&error_text);
                run_best_effort_hook(&hooks, completion).await;
                let mut session_end = lifecycle_hook_payload(
                    HookEvent::SessionEnd,
                    &workspace,
                    &active_provider,
                    &active_model,
                );
                session_end.success = Some(false);
                session_end.error = Some(&error_text);
                run_best_effort_hook(&hooks, session_end).await;
                return Err(error);
            }
        }
    }

    let outcome = last_outcome
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no iterations executed"))?;
    let mut session_end = lifecycle_hook_payload(
        HookEvent::SessionEnd,
        &workspace,
        &outcome.provider_id,
        &outcome.model,
    );
    session_end.success = Some(last_error.is_none());
    session_end.error = last_error.as_deref();
    run_best_effort_hook(&hooks, session_end).await;

    let session_id = session.as_ref().map(|session| session.id.clone());
    let tracked_cost = session_id
        .as_deref()
        .and_then(|id| {
            usage_tracker
                .as_ref()
                .and_then(|tracker| tracker.session_usage(id))
        })
        .map(|usage| usage.total_cost_usd)
        .or_else(|| {
            usage_tracker
                .as_ref()
                .and_then(|tracker| tracker.session_usage("default"))
                .map(|usage| usage.total_cost_usd)
        })
        .unwrap_or(0.0);
    match cli.format {
        OutputFormat::Text => {
            println!("iterations: {prompt_count}");
            if stopped {
                println!("status: stopped by marker");
            }
            if !outcome.response.ends_with('\n') {
                println!();
            }
            println!("{}", outcome.response);
            if let Some(id) = &session_id {
                eprintln!("[session {id}]");
            }
        }
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&RepeaterOutput {
                prompt,
                iterations: prompt_count,
                completed: stopped,
                response: outcome.response.clone(),
                session_id,
                usage: HeadlessUsage {
                    input_tokens: loop_usage.input_tokens,
                    output_tokens: loop_usage.output_tokens,
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
            "[{}]\n子任务: {}\n状态: {}\n要点: {}\n建议: {:?}\n风险: {:?}\n",
            result.name,
            result.task,
            result.status,
            result.summary,
            result.recommendations,
            result.risks
        ));
    }
    chunks.push(
        "请输出：1)最终结论 2)最小落地步骤 3)测试检查清单（严格结构化 JSON 输出更佳）".to_string(),
    );
    chunks.join("\n")
}

async fn run_orchestrate_subagent(
    mut cli: Cli,
    config: Arc<GreyConfig>,
    workspace: PathBuf,
    agent: OrchestrateAgent,
    task: String,
    shared_context: String,
    hooks: Arc<HookRunner>,
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
        match build_agent_and_session(&cli, &config, &workspace, false, &hooks) {
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
                    status: "fail".to_string(),
                    summary: "sub-agent initialization failed".to_string(),
                    recommendations: Vec::new(),
                    risks: vec!["initialize".to_string()],
                    artifacts: Vec::new(),
                    error: Some(error.to_string()),
                };
            }
        };
    let agent_provider = agent_client.provider_id().to_string();
    let _ = existing;
    let context_line = if shared_context.is_empty() {
        String::new()
    } else {
        format!("\n共享上下文（白名单）:\n{shared_context}\n")
    };
    let prompt = format!(
        "{}\n{}主任务: {task}\n子任务: {}\n请按固定 JSON 输出。\n",
        build_orchestrate_subagent_system_prompt(&agent.name),
        context_line,
        agent.task
    );
    for attempt in 1..=ORCHESTRATE_AGENT_MAX_ATTEMPTS {
        let run = agent_client.run_new(agent.system_prompt.clone(), prompt.clone(), None);
        let outcome =
            tokio::time::timeout(Duration::from_secs(ORCHESTRATE_AGENT_TIMEOUT_SECS), run).await;

        match outcome {
            Ok(Ok(outcome)) => {
                let contract = parse_orchestrate_contract(&outcome.response);
                let success = contract.status == "ok";
                return OrchestrateAgentResult {
                    name: agent.name,
                    task: agent.task,
                    response: outcome.response,
                    provider: outcome.provider_id,
                    model: outcome.model,
                    steps: outcome.steps,
                    cached: outcome.cached,
                    success,
                    status: contract.status,
                    summary: contract.summary,
                    recommendations: contract.recommendations,
                    risks: contract.risks,
                    artifacts: contract.artifacts,
                    error: None,
                };
            }
            Ok(Err(error)) => {
                let error_text = error.to_string();
                let mut risk = "execution".to_string();
                let is_retriable = is_retriable_subagent_error(&error_text);
                if is_retriable {
                    risk = "transient_execution".to_string();
                }
                if is_retriable && attempt < ORCHESTRATE_AGENT_MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(
                        ORCHESTRATE_AGENT_RETRY_DELAY_MS * attempt as u64,
                    ))
                    .await;
                    continue;
                }
                return OrchestrateAgentResult {
                    name: agent.name,
                    task: agent.task,
                    response: if attempt == 1 {
                        "sub-agent execution failed".to_string()
                    } else {
                        format!("sub-agent execution failed after {attempt} attempts")
                    },
                    provider: agent_provider.clone(),
                    model: model_hint.clone(),
                    steps: 0,
                    cached: false,
                    success: false,
                    status: "fail".to_string(),
                    summary: if attempt == 1 {
                        "sub-agent execution failed".to_string()
                    } else {
                        format!("sub-agent execution failed after {attempt} attempts")
                    },
                    recommendations: vec![if attempt > 1 {
                        format!("已重试 {attempt} 次后仍失败")
                    } else {
                        "无需重试".to_string()
                    }],
                    risks: vec![risk],
                    artifacts: Vec::new(),
                    error: Some(error_text),
                };
            }
            Err(_) => {
                let error = "sub-agent execution timed out".to_string();
                if attempt < ORCHESTRATE_AGENT_MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(
                        ORCHESTRATE_AGENT_RETRY_DELAY_MS * attempt as u64,
                    ))
                    .await;
                    continue;
                }
                return OrchestrateAgentResult {
                    name: agent.name,
                    task: agent.task,
                    response: if attempt == 1 {
                        "sub-agent execution timed out".to_string()
                    } else {
                        format!("sub-agent execution timed out after {attempt} attempts")
                    },
                    provider: agent_provider,
                    model: model_hint,
                    steps: 0,
                    cached: false,
                    success: false,
                    status: "fail".to_string(),
                    summary: if attempt == 1 {
                        "sub-agent execution timed out".to_string()
                    } else {
                        format!("sub-agent execution timed out after {attempt} attempts")
                    },
                    recommendations: vec![format!("已重试 {attempt} 次后仍失败")],
                    risks: vec!["timeout".to_string()],
                    artifacts: Vec::new(),
                    error: Some(error),
                };
            }
        }
    }

    unreachable!()
}

fn build_orchestrate_subagent_system_prompt(agent_name: &str) -> String {
    format!(
        "你是{agent_name}子代理。你必须只输出 JSON，不要输出额外解释：\
{{\"status\":\"ok|warn|fail\",\"summary\":\"...\",\"recommendations\":[\"...\"],\"risks\":[\"...\"],\"artifacts\":[\"...\"]}}"
    )
}

fn parse_orchestrate_contract(raw: &str) -> OrchestrateAgentContract {
    if let Some(json_blob) = extract_orchestrate_json(raw) {
        if let Ok(contract) = serde_json::from_str::<OrchestrateAgentContract>(&json_blob) {
            return normalize_orchestrate_contract(contract);
        }
    }
    if let Ok(contract) = serde_json::from_str::<OrchestrateAgentContract>(raw.trim()) {
        return normalize_orchestrate_contract(contract);
    }
    fallback_orchestrate_contract(raw)
}

fn parse_orchestrate_coordinator_contract(
    raw: &str,
    fallback: &AgentOutcome,
) -> OrchestrateCoordinatorContract {
    if let Some(json_blob) = extract_orchestrate_json(raw) {
        if let Ok(contract) = serde_json::from_str::<OrchestrateCoordinatorContract>(&json_blob) {
            return normalize_orchestrate_coordinator_contract(contract, fallback);
        }
    }
    if let Ok(contract) = serde_json::from_str::<OrchestrateCoordinatorContract>(raw.trim()) {
        return normalize_orchestrate_coordinator_contract(contract, fallback);
    }
    fallback_orchestrate_coordinator_contract(raw, fallback)
}

fn build_orchestrate_shared_context(
    task: &str,
    share_context: &[OrchestrateShareContext],
    cli: &Cli,
    workspace: &Path,
) -> Result<String> {
    let mut sections = Vec::new();
    let include_task = share_context
        .iter()
        .any(|context| matches!(context, OrchestrateShareContext::Task));
    if include_task {
        sections.push(format!("主任务: {task}"));
    }

    let include_summary = share_context
        .iter()
        .any(|context| matches!(context, OrchestrateShareContext::Summary));
    if include_summary {
        if let Some(session) = load_orchestrate_session(cli, workspace)? {
            let summary = compact_session_tail(&session.messages, 6);
            if !summary.is_empty() {
                sections.push(format!("会话摘要:\n{summary}"));
            }
        }
    }

    Ok(sections.join("\n"))
}

fn load_orchestrate_session(cli: &Cli, workspace: &Path) -> Result<Option<grey_core::Session>> {
    if cli.session.is_none() && !cli.continue_session {
        return Ok(None);
    }

    let store = SessionStore::open(&session_database_path())?;
    let workspace_text = workspace.to_string_lossy();
    let session = if let Some(id) = &cli.session {
        Some(
            store
                .load(id)?
                .context(format!("session not found: {id}"))?,
        )
    } else {
        store.latest_for_workspace(&workspace_text)?
    };

    match session {
        Some(session) if session.workspace == workspace_text => Ok(Some(session)),
        Some(session) => {
            bail!(
                "session workspace mismatch: stored {}, current {}",
                session.workspace,
                workspace.display()
            )
        }
        None => Ok(None),
    }
}

fn compact_session_tail(messages: &[grey_core::ChatMessage], max_messages: usize) -> String {
    let visible: Vec<&grey_core::ChatMessage> = messages
        .iter()
        .filter(|message| message.role != Role::Tool)
        .rev()
        .take(max_messages)
        .collect();
    visible
        .into_iter()
        .rev()
        .map(|message| {
            format!(
                "[{}] {}",
                match message.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                },
                compact_message_preview(&message.content)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_message_preview(content: &str) -> String {
    let max_chars = 180;
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= max_chars {
        content.to_string()
    } else {
        chars
            .into_iter()
            .take(max_chars)
            .chain("...".chars())
            .collect()
    }
}

fn compact_message_preview_with_limit(content: &str, max_chars: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= max_chars {
        content.to_string()
    } else {
        chars
            .into_iter()
            .take(max_chars)
            .chain("...".chars())
            .collect()
    }
}

fn sanitize_contract_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| {
            compact_message_preview_with_limit(value.trim(), ORCHESTRATE_MAX_LIST_ITEM_CHARS)
        })
        .filter(|value| !value.is_empty())
        .take(ORCHESTRATE_MAX_LIST_ITEMS)
        .collect()
}

fn is_retriable_subagent_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    const TRANSIENT_HINTS: &[&str] = &[
        "timeout",
        "timed out",
        "connection",
        "rate limit",
        "temporarily",
        "temporary",
        "500",
        "502",
        "503",
        "504",
        "service unavailable",
        "network",
        "econn",
    ];
    TRANSIENT_HINTS.iter().any(|hint| lower.contains(hint))
}

fn extract_orchestrate_json(raw: &str) -> Option<String> {
    let fence_start = raw.find("```json");
    let fence_end = raw.rfind("```");
    if let Some(start) = fence_start {
        if let Some(end) = fence_end {
            if end > start + 6 {
                let inner = raw[start + 6..end].trim();
                if inner.starts_with('{') && inner.ends_with('}') {
                    return Some(inner.to_string());
                }
            }
        }
    }
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end > start {
        Some(raw[start..=end].to_string())
    } else {
        None
    }
}

fn fallback_orchestrate_contract(raw: &str) -> OrchestrateAgentContract {
    let summary = raw.lines().next().unwrap_or_default().trim().to_string();
    let summary = if summary.is_empty() {
        raw.to_string()
    } else {
        summary
    };
    OrchestrateAgentContract {
        status: "warn".to_string(),
        summary,
        recommendations: vec!["response not in schema".to_string()],
        risks: vec!["requires normalization".to_string()],
        artifacts: Vec::new(),
    }
}

fn normalize_orchestrate_contract(
    mut contract: OrchestrateAgentContract,
) -> OrchestrateAgentContract {
    contract.status = match contract.status.to_lowercase().as_str() {
        "ok" => "ok".to_string(),
        "warn" => "warn".to_string(),
        "fail" => "fail".to_string(),
        _ => default_orchestrate_status(),
    };
    if contract.summary.is_empty() {
        contract.summary = "no summary provided".to_string();
    }
    contract.summary =
        compact_message_preview_with_limit(&contract.summary, ORCHESTRATE_MAX_SUMMARY_CHARS);
    contract.recommendations = sanitize_contract_list(contract.recommendations);
    contract.risks = sanitize_contract_list(contract.risks);
    contract.artifacts = sanitize_contract_list(contract.artifacts);
    contract
}

fn normalize_orchestrate_coordinator_contract(
    mut contract: OrchestrateCoordinatorContract,
    fallback: &AgentOutcome,
) -> OrchestrateCoordinatorContract {
    if contract.response.is_empty() {
        contract.response = fallback.response.clone();
    }
    if contract.response.is_empty() {
        contract.response = "no synthesis output".to_string();
    }
    if contract.provider.is_empty() {
        contract.provider = fallback.provider_id.clone();
    }
    if contract.model.is_empty() {
        contract.model = fallback.model.clone();
    }
    if contract.steps == 0 {
        contract.steps = fallback.steps;
    }
    contract.cached |= fallback.cached;
    contract.response = compact_message_preview_with_limit(
        &contract.response,
        ORCHESTRATE_MAX_SYNTHESIS_RESPONSE_CHARS,
    );
    contract
}

fn fallback_orchestrate_coordinator_contract(
    raw: &str,
    fallback: &AgentOutcome,
) -> OrchestrateCoordinatorContract {
    let response = raw.lines().next().unwrap_or_default().trim();
    let response = if response.is_empty() {
        fallback.response.clone()
    } else {
        response.to_string()
    };
    OrchestrateCoordinatorContract {
        response: if response.is_empty() {
            "no synthesis output".to_string()
        } else {
            response
        },
        provider: fallback.provider_id.clone(),
        model: fallback.model.clone(),
        steps: fallback.steps,
        cached: fallback.cached,
    }
}

fn build_orchestrate_session_messages(
    existing_messages: &[ChatMessage],
    task: &str,
    subagents: &[OrchestrateAgentResult],
    synthesis: &str,
) -> Vec<ChatMessage> {
    let mut messages = existing_messages.to_vec();
    messages.push(ChatMessage::new(
        Role::User,
        format!("[orchestrate] task: {task}"),
    ));

    for result in subagents {
        let mut lines = vec![
            format!(
                "子代理: {} | status={} | provider={} model={} steps={} cached={}",
                result.name,
                result.status,
                result.provider,
                result.model,
                result.steps,
                result.cached
            ),
            format!("任务: {}", result.task),
            format!(
                "摘要: {}",
                compact_message_preview_with_limit(&result.summary, 160)
            ),
        ];
        if !result.recommendations.is_empty() {
            lines.push(format!("建议: {}", result.recommendations.join("，")));
        }
        if !result.risks.is_empty() {
            lines.push(format!("风险: {}", result.risks.join("，")));
        }
        if !result.artifacts.is_empty() {
            lines.push(format!("产物: {}", result.artifacts.join("，")));
        }
        if let Some(error) = &result.error {
            lines.push(format!("错误: {error}"));
        }
        if !result.response.is_empty() {
            lines.push(format!(
                "原文: {}",
                compact_message_preview_with_limit(
                    &result.response,
                    ORCHESTRATE_SESSION_MESSAGE_PREVIEW_CHARS,
                )
            ));
        }
        messages.push(ChatMessage::new(Role::Assistant, lines.join("\n")));
    }

    messages.push(ChatMessage::new(
        Role::Assistant,
        format!(
            "协调总结: {}\n{}",
            compact_message_preview_with_limit(synthesis, ORCHESTRATE_MAX_SUMMARY_CHARS),
            compact_message_preview_with_limit(synthesis, ORCHESTRATE_MAX_SUMMARY_CHARS * 2)
        ),
    ));
    messages
}

fn render_orchestrate_text_panels(
    task: &str,
    subagents: &[OrchestrateAgentResult],
    summary: &OrchestrateCoordinatorContract,
    synthesis_raw: &str,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("╭─ Orchestrate 子 Agent 面板 ─╮".to_string());
    lines.push(format!("│ 任务：{task}"));
    lines.push("├───────────────────────────".to_string());

    for result in subagents {
        lines.push(format!(
            "│ {:<7}: {:<6} | {:<14}@{:<12}",
            result.name, result.status, result.provider, result.model
        ));
        lines.push(format!("│ 任务：{}", result.task));
        if !result.summary.is_empty() {
            lines.push(format!("│ 摘要：{}", result.summary));
        }
        if !result.recommendations.is_empty() {
            lines.push(format!("│ 建议：{}", result.recommendations.join("，")));
        }
        if !result.risks.is_empty() {
            lines.push(format!("│ 风险：{}", result.risks.join("，")));
        }
        if !result.artifacts.is_empty() {
            lines.push(format!("│ 产物：{}", result.artifacts.join("，")));
        }
        if result.response.is_empty() {
            continue;
        }
        lines.push(format!(
            "│ 响应：{}",
            compact_message_preview_with_limit(&result.response, 120)
        ));
        if let Some(error) = &result.error {
            lines.push(format!("│ 错误：{error}"));
        }
        lines.push("│".to_string());
    }

    lines.push("├─ Synthesis ──────────────".to_string());
    lines.push(format!(
        "│ {}",
        compact_message_preview_with_limit(synthesis_raw, ORCHESTRATE_MAX_SUMMARY_CHARS)
    ));
    lines.push(format!(
        "│ 归一化: {}",
        compact_message_preview_with_limit(&summary.response, ORCHESTRATE_MAX_SUMMARY_CHARS)
    ));
    lines.push(format!(
        "│ provider: {} model: {}",
        summary.provider, summary.model
    ));
    lines.push(format!(
        "│ steps: {} cached: {}",
        summary.steps,
        if summary.cached { "true" } else { "false" }
    ));
    lines.push("╰───────────────────────────".to_string());
    lines
}

async fn run_headless(
    cli: &Cli,
    config: &GreyConfig,
    workspace: &Path,
    prompt: &str,
) -> Result<()> {
    let hooks = HookRunner::new(&config.hooks, &config.plugins, &config.runtime);
    let (agent, store, existing) = build_agent_and_session(cli, config, workspace, false, &hooks)?;
    let usage_tracker = agent.usage_tracker();
    let active_provider = agent.provider_id().to_string();
    let active_model = agent.model().to_string();
    run_best_effort_hook(
        &hooks,
        lifecycle_hook_payload(
            HookEvent::SessionStart,
            workspace,
            &active_provider,
            &active_model,
        ),
    )
    .await;

    let mut prompt = prompt.to_string();
    let run_result = async {
        prompt = apply_prompt_hook(
            &hooks,
            HookEvent::PreMessageSend,
            workspace,
            &active_provider,
            &active_model,
            &prompt,
        )
        .await?;
        prompt = apply_prompt_hook(
            &hooks,
            HookEvent::PrePrompt,
            workspace,
            &active_provider,
            &active_model,
            &prompt,
        )
        .await?;
        if cli.format == OutputFormat::Text {
            run_with_text_events(
                &agent,
                existing.as_ref(),
                &prompt,
                config.runtime.event_queue_capacity,
            )
            .await
        } else {
            run_with_cancellation(&agent, existing.as_ref(), &prompt, None).await
        }
    }
    .await;
    let result = match run_result {
        Ok(outcome) => outcome,
        Err(error) => {
            let error_text = error.to_string();
            let mut completion = lifecycle_hook_payload(
                HookEvent::Completion,
                workspace,
                &active_provider,
                &active_model,
            );
            completion.prompt = Some(&prompt);
            completion.success = Some(false);
            completion.error = Some(&error_text);
            run_best_effort_hook(&hooks, completion).await;
            let mut session_end = lifecycle_hook_payload(
                HookEvent::SessionEnd,
                workspace,
                &active_provider,
                &active_model,
            );
            session_end.success = Some(false);
            session_end.error = Some(&error_text);
            run_best_effort_hook(&hooks, session_end).await;
            return Err(error);
        }
    };

    let session_id = match persist_outcome(
        store.as_ref(),
        existing,
        &result,
        &prompt,
        workspace,
        cli.no_save,
        usage_tracker.as_deref(),
    ) {
        Ok(session_id) => session_id,
        Err(error) => {
            let error_text = error.to_string();
            let mut completion = lifecycle_hook_payload(
                HookEvent::Completion,
                workspace,
                &result.provider_id,
                &result.model,
            );
            completion.prompt = Some(&prompt);
            completion.success = Some(true);
            run_best_effort_hook(&hooks, completion).await;
            let mut session_end = lifecycle_hook_payload(
                HookEvent::SessionEnd,
                workspace,
                &result.provider_id,
                &result.model,
            );
            session_end.success = Some(false);
            session_end.error = Some(&error_text);
            run_best_effort_hook(&hooks, session_end).await;
            return Err(error);
        }
    };
    let mut completion = lifecycle_hook_payload(
        HookEvent::Completion,
        workspace,
        &result.provider_id,
        &result.model,
    );
    completion.prompt = Some(&prompt);
    completion.success = Some(true);
    run_best_effort_hook(&hooks, completion).await;
    let mut session_end = lifecycle_hook_payload(
        HookEvent::SessionEnd,
        workspace,
        &result.provider_id,
        &result.model,
    );
    session_end.success = Some(true);
    run_best_effort_hook(&hooks, session_end).await;
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
            if !result.response.ends_with('\n') {
                println!();
            }
            if let Some(id) = &session_id {
                eprintln!("[session {id}]");
            }
        }
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&HeadlessOutput {
                response: result.response,
                session_id,
                usage: HeadlessUsage {
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                    cost_usd: tracked_cost,
                    cached: result.cached,
                    provider: result.provider_id.clone(),
                    model: result.model.clone(),
                },
                steps: result.steps,
                cached: result.cached,
                provider: result.provider_id.clone(),
                model: result.model.clone(),
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
    hooks: &HookRunner,
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
    let base_approver: Arc<dyn Approver> = if cli.auto_approve {
        Arc::new(AlwaysApprove)
    } else if cli.read_only || tui_mode {
        Arc::new(DenySideEffects)
    } else {
        Arc::new(StdioApprover)
    };
    let approver = Arc::new(HookedApprover::new(
        base_approver,
        hooks.clone(),
        workspace,
        &resolved_provider_id,
        &model,
    ));
    let builtin = Arc::new(BuiltinTools::new(workspace, Arc::new(AlwaysApprove))?);
    let mut executors: Vec<Arc<dyn ToolExecutor>> = vec![builtin];
    let lsp_binary = config.lsp.rust_analyzer.clone();
    if !lsp_binary.trim().is_empty() {
        executors.push(Arc::new(LspTools::new(workspace, lsp_binary)?));
    }
    let mcp = McpTools::new(config.mcp_tools.clone());
    if !mcp.is_empty() {
        executors.push(Arc::new(mcp));
    }
    let plugin_tools = PluginTools::new(workspace, config.plugins.clone(), Arc::new(AlwaysApprove));
    if !plugin_tools.is_empty() {
        executors.push(Arc::new(plugin_tools));
    }
    let duplicated = duplicate_tool_names(&executors);
    if !duplicated.is_empty() {
        bail!("duplicate tool name(s) detected across tool providers: {duplicated:?}");
    }
    let tools: Arc<dyn ToolExecutor> = Arc::new(HookedTools::new(
        Arc::new(CombinedTools::new(executors)),
        approver,
        hooks.clone(),
        workspace,
        &resolved_provider_id,
        &model,
    ));
    let mut options = AgentOptions::new(model.clone());
    options.max_steps = cli.max_steps;
    options.response_max_bytes = config.runtime.response_max_bytes;
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
    event_queue_capacity: usize,
) -> Result<AgentOutcome> {
    let (events_tx, mut events_rx) = mpsc::channel(event_queue_capacity);
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
                AgentEvent::Warning(warning) => eprintln!("[agent warning] {warning}"),
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
    events: Option<&mpsc::Sender<AgentEvent>>,
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

#[derive(Debug)]
struct TuiWorkerSummary {
    prompt_count: usize,
    last_error: Option<String>,
    provider: String,
    model: String,
}

impl TuiWorkerSummary {
    fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            prompt_count: 0,
            last_error: None,
            provider: provider.into(),
            model: model.into(),
        }
    }
}

async fn close_tui_runtime<F, Fut>(
    prompts: mpsc::Sender<String>,
    shutdown: watch::Sender<bool>,
    mut worker: JoinHandle<TuiWorkerSummary>,
    timeout: Duration,
    mut fallback: TuiWorkerSummary,
    ui_error: Option<String>,
    session_end: F,
) -> Result<()>
where
    F: FnOnce(TuiWorkerSummary) -> Fut,
    Fut: Future<Output = ()>,
{
    drop(prompts);
    let _ = shutdown.send(true);
    let (mut summary, cleanup_error) = match tokio::time::timeout(timeout, &mut worker).await {
        Ok(Ok(summary)) => (summary, None),
        Ok(Err(error)) => {
            let message = format!("TUI worker failed: {error}");
            fallback.last_error = Some(message.clone());
            (fallback, Some(anyhow::anyhow!(message)))
        }
        Err(_) => {
            worker.abort();
            let _ = worker.await;
            let message = "TUI worker shutdown timed out".to_string();
            fallback.last_error = Some(message.clone());
            (fallback, Some(anyhow::anyhow!(message)))
        }
    };
    if let Some(ui_error) = ui_error {
        summary.last_error = Some(ui_error);
    }
    session_end(summary).await;
    cleanup_error.map_or(Ok(()), Err)
}

fn finish_tui_result(ui_result: Result<()>, cleanup_result: Result<()>) -> Result<()> {
    match (ui_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(ui_error), Err(cleanup_error)) => Err(anyhow::anyhow!(
            "{ui_error:#}; additionally, TUI cleanup failed: {cleanup_error:#}"
        )),
    }
}

async fn run_tui(cli: &Cli, config: &GreyConfig, workspace: &Path) -> Result<()> {
    let hooks = HookRunner::new(&config.hooks, &config.plugins, &config.runtime);
    let (agent, store, existing) = build_agent_and_session(cli, config, workspace, true, &hooks)?;
    let usage_tracker = agent.usage_tracker();

    run_best_effort_hook(
        &hooks,
        lifecycle_hook_payload(
            HookEvent::SessionStart,
            workspace,
            agent.provider_id(),
            agent.model(),
        ),
    )
    .await;

    let (events_tx, events_rx) = mpsc::channel(config.runtime.event_queue_capacity);
    let (prompts_tx, mut prompts_rx) =
        mpsc::channel::<String>(config.runtime.prompt_queue_capacity);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let workspace_for_worker = workspace.to_path_buf();
    let workspace_name_for_worker = workspace_for_worker.to_string_lossy().into_owned();
    let workspace_for_cleanup = workspace_for_worker.clone();
    let no_save = cli.no_save;
    let initial_provider = agent.provider_id().to_string();
    let initial_model = agent.model().to_string();
    let fallback_summary = TuiWorkerSummary::new(&initial_provider, &initial_model);
    let worker_hooks = hooks.clone();
    let hook_workspace_for_worker = workspace_for_worker.clone();
    let worker = tokio::spawn(async move {
        let mut session = existing;
        let mut summary = TuiWorkerSummary::new(initial_provider, initial_model);
        'worker: loop {
            let prompt = tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                    continue;
                }
                prompt = prompts_rx.recv() => match prompt {
                    Some(prompt) => prompt,
                    None => break,
                },
            };
            let prompt = match apply_prompt_hook(
                &worker_hooks,
                HookEvent::PreMessageSend,
                &hook_workspace_for_worker,
                &summary.provider,
                &summary.model,
                &prompt,
            )
            .await
            {
                Ok(prompt) => prompt,
                Err(error) => {
                    let error_text = error.to_string();
                    summary.last_error = Some(error_text.clone());
                    let mut payload = lifecycle_hook_payload(
                        HookEvent::Completion,
                        &hook_workspace_for_worker,
                        &summary.provider,
                        &summary.model,
                    );
                    payload.prompt = Some(&prompt);
                    payload.success = Some(false);
                    payload.error = Some(&error_text);
                    if let Err(hook_error) = worker_hooks.run_best_effort(payload).await {
                        let _ = events_tx
                            .send(AgentEvent::Warning(format!(
                                "completion hook failed: {hook_error:#}"
                            )))
                            .await;
                    }
                    let _ = events_tx
                        .send(AgentEvent::Failed(format!("{error:#}")))
                        .await;
                    continue;
                }
            };
            let prompt = match apply_prompt_hook(
                &worker_hooks,
                HookEvent::PrePrompt,
                &hook_workspace_for_worker,
                &summary.provider,
                &summary.model,
                &prompt,
            )
            .await
            {
                Ok(prompt) => prompt,
                Err(error) => {
                    let error_text = error.to_string();
                    summary.last_error = Some(error_text.clone());
                    let mut payload = lifecycle_hook_payload(
                        HookEvent::Completion,
                        &hook_workspace_for_worker,
                        &summary.provider,
                        &summary.model,
                    );
                    payload.prompt = Some(&prompt);
                    payload.success = Some(false);
                    payload.error = Some(&error_text);
                    if let Err(hook_error) = worker_hooks.run_best_effort(payload).await {
                        let _ = events_tx
                            .send(AgentEvent::Warning(format!(
                                "completion hook failed: {hook_error:#}"
                            )))
                            .await;
                    }
                    let _ = events_tx
                        .send(AgentEvent::Failed(format!("{error:#}")))
                        .await;
                    continue;
                }
            };
            let result = {
                let run = async {
                    match &session {
                        Some(session) => {
                            agent
                                .continue_messages(
                                    session.messages.clone(),
                                    &prompt,
                                    Some(&events_tx),
                                )
                                .await
                        }
                        None => {
                            agent
                                .run_new(SYSTEM_PROMPT, &prompt, Some(&events_tx))
                                .await
                        }
                    }
                };
                tokio::pin!(run);
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => None,
                    result = &mut run => Some(result),
                }
            };
            let Some(result) = result else {
                summary.last_error = Some("cancelled".into());
                break 'worker;
            };
            match result {
                Ok(outcome) => {
                    summary.prompt_count = summary.prompt_count.saturating_add(1);
                    summary.last_error = None;
                    let provider = outcome.provider_id.clone();
                    let model = outcome.model.clone();
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
                            &workspace_name_for_worker,
                            outcome.messages.clone(),
                        ),
                    };
                    if !no_save {
                        if let Some(store) = &store {
                            if let Err(error) = store.save(&mut current) {
                                let error_text = error.to_string();
                                summary.last_error = Some(error_text.clone());
                                let mut payload = lifecycle_hook_payload(
                                    HookEvent::Completion,
                                    &hook_workspace_for_worker,
                                    &provider,
                                    &model,
                                );
                                payload.prompt = Some(&prompt);
                                payload.success = Some(false);
                                payload.error = Some(&error_text);
                                if let Err(hook_error) = worker_hooks.run_best_effort(payload).await
                                {
                                    let _ = events_tx
                                        .send(AgentEvent::Warning(format!(
                                            "completion hook failed: {hook_error:#}"
                                        )))
                                        .await;
                                }
                                let _ = events_tx
                                    .send(AgentEvent::Failed(format!("saving session: {error:#}")))
                                    .await;
                                session = Some(current);
                                continue;
                            }
                            if let Err(error) =
                                persist_usage(store, &current.id, usage_tracker.as_deref())
                            {
                                let _ = events_tx
                                    .send(AgentEvent::Warning(format!("saving usage: {error:#}")))
                                    .await;
                            }
                        }
                    }
                    session = Some(current);
                    summary.provider = outcome.provider_id.clone();
                    summary.model = outcome.model.clone();
                    let mut payload = lifecycle_hook_payload(
                        HookEvent::Completion,
                        &hook_workspace_for_worker,
                        &outcome.provider_id,
                        &outcome.model,
                    );
                    payload.prompt = Some(&prompt);
                    payload.success = Some(true);
                    if let Err(error) = worker_hooks.run_best_effort(payload).await {
                        let _ = events_tx
                            .send(AgentEvent::Warning(format!(
                                "completion hook failed: {error:#}"
                            )))
                            .await;
                    }
                    let _ = events_tx
                        .send(AgentEvent::Completed {
                            usage: outcome.usage,
                            steps: outcome.steps,
                            provider,
                            model,
                        })
                        .await;
                }
                Err(error) => {
                    summary.last_error = Some(error.to_string());
                    let error_text = error.to_string();
                    let mut payload = lifecycle_hook_payload(
                        HookEvent::Completion,
                        &hook_workspace_for_worker,
                        &summary.provider,
                        &summary.model,
                    );
                    payload.prompt = Some(&prompt);
                    payload.success = Some(false);
                    payload.error = Some(&error_text);
                    if let Err(error) = worker_hooks.run_best_effort(payload).await {
                        let _ = events_tx
                            .send(AgentEvent::Warning(format!(
                                "completion hook failed: {error:#}"
                            )))
                            .await;
                    }
                    let _ = events_tx
                        .send(AgentEvent::Failed(format!("{error:#}")))
                        .await;
                }
            }
        }
        summary
    });
    let branch = detect_git_branch(&workspace_for_worker);
    let ui_result = {
        let ui = grey_tui::run_agent_tui(
            events_rx,
            prompts_tx.clone(),
            &config.tui,
            &config.runtime,
            branch.as_deref(),
        );
        tokio::pin!(ui);
        tokio::select! {
            result = &mut ui => result,
            signal = tokio::signal::ctrl_c() => match signal {
                Ok(()) => Err(anyhow::anyhow!("interrupted")),
                Err(error) => Err(error).context("installing Ctrl-C handler"),
            },
        }
    };
    let ui_error = ui_result.as_ref().err().map(|error| format!("{error:#}"));
    let cleanup_hooks = hooks.clone();
    let cleanup_result = close_tui_runtime(
        prompts_tx,
        shutdown_tx,
        worker,
        TUI_SHUTDOWN_TIMEOUT,
        fallback_summary,
        ui_error,
        move |summary| async move {
            let mut payload = lifecycle_hook_payload(
                HookEvent::SessionEnd,
                &workspace_for_cleanup,
                &summary.provider,
                &summary.model,
            );
            payload.success = Some(summary.last_error.is_none());
            payload.error = summary.last_error.as_deref();
            run_best_effort_hook(&cleanup_hooks, payload).await;
        },
    )
    .await;
    finish_tui_result(ui_result, cleanup_result)
}

fn detect_git_branch(workspace: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            workspace.to_string_lossy().as_ref(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch.to_string())
    }
}

fn lifecycle_hook_payload<'a>(
    event: HookEvent,
    workspace: &'a Path,
    provider: &'a str,
    model: &'a str,
) -> HookPayload<'a> {
    let mut payload = HookPayload::new(event, workspace);
    payload.provider = Some(provider);
    payload.model = Some(model);
    payload
}

async fn apply_prompt_hook(
    hooks: &HookRunner,
    event: HookEvent,
    workspace: &Path,
    provider: &str,
    model: &str,
    prompt: &str,
) -> Result<String> {
    let mut payload = lifecycle_hook_payload(event, workspace, provider, model);
    payload.prompt = Some(prompt);
    hooks.run_prompt(payload).await
}

async fn run_best_effort_hook(hooks: &HookRunner, payload: HookPayload<'_>) {
    if let Err(error) = hooks.run_best_effort(payload).await {
        eprintln!("{} hook failed: {error:#}", payload.event.as_str());
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

fn run_plugins(action: PluginAction) -> Result<()> {
    let config_path = grey_core::raw_config::mutation_target()?;

    match action {
        PluginAction::List => {
            let plugins = read_raw_plugins(&config_path)?;
            if plugins.is_empty() {
                println!("(no plugins configured)");
                return Ok(());
            }
            for plugin in &plugins {
                let kind = format_plugin_kind(plugin.kind);
                println!(
                    "{}\t{}\t{}\t{}",
                    plugin.id,
                    kind,
                    if plugin.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    plugin.hook_event.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
        PluginAction::Show { id } => {
            show_raw_plugin(&config_path, &id)?;
            Ok(())
        }
        PluginAction::Add {
            id,
            kind,
            command,
            args,
            name,
            description,
            hook_event,
            version,
            timeout_ms,
            enabled,
        } => {
            let kind = kind.to_core();
            if kind == PluginKind::Hook && hook_event.is_none() {
                bail!("--hook-event is required for hook plugins");
            }
            let mut updated = false;
            let output_id = id.clone();
            grey_core::raw_config::edit_file(&config_path, |doc| {
                let existing_command = grey_core::raw_config::plugin_command(doc, &id)?;
                updated = existing_command.is_some();
                let plugin = PluginConfig {
                    id: id.clone(),
                    name: name.clone(),
                    kind,
                    enabled,
                    description: description.clone(),
                    command: command.clone().or(existing_command).with_context(|| {
                        format!("--command is required when adding plugin {id}")
                    })?,
                    args: args.clone(),
                    timeout_ms,
                    version: version.clone(),
                    hook_event: if kind == PluginKind::Hook {
                        hook_event.clone()
                    } else {
                        None
                    },
                };
                grey_core::raw_config::upsert_plugin(doc, &plugin)
            })?;
            if updated {
                println!("updated plugin {output_id}");
            } else {
                println!("added plugin");
            }
            Ok(())
        }
        PluginAction::Remove { id } => {
            grey_core::raw_config::edit_file(&config_path, |doc| {
                grey_core::raw_config::remove_plugin(doc, &id)
            })?;
            println!("removed plugin {id}");
            Ok(())
        }
        PluginAction::Enable { id } => {
            grey_core::raw_config::edit_file(&config_path, |doc| {
                grey_core::raw_config::set_enabled(doc, "plugins", &id, true)
            })?;
            println!("enabled plugin {id}");
            Ok(())
        }
        PluginAction::Disable { id } => {
            grey_core::raw_config::edit_file(&config_path, |doc| {
                grey_core::raw_config::set_enabled(doc, "plugins", &id, false)
            })?;
            println!("disabled plugin {id}");
            Ok(())
        }
    }
}

fn format_plugin_kind(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Tool => "tool",
        PluginKind::Provider => "provider",
        PluginKind::Hook => "hook",
        PluginKind::Theme => "theme",
    }
}

#[derive(Deserialize, Default)]
struct RawPluginList {
    #[serde(default)]
    plugins: Vec<PluginConfig>,
}

fn read_raw_plugins(path: &Path) -> Result<Vec<PluginConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    toml::from_str::<RawPluginList>(
        &fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .map(|config| config.plugins)
    .with_context(|| format!("parsing {}", path.display()))
}

fn show_raw_plugin(path: &Path, id: &str) -> Result<()> {
    let source = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut config: toml::Value =
        toml::from_str(&source).with_context(|| format!("parsing {}", path.display()))?;
    let plugin = config
        .get_mut("plugins")
        .and_then(toml::Value::as_array_mut)
        .and_then(|plugins| {
            plugins
                .iter_mut()
                .find(|plugin| plugin.get("id").and_then(toml::Value::as_str) == Some(id))
        })
        .with_context(|| format!("plugin not found: {id}"))?;
    let mut output = serde_json::to_value(plugin)?;
    grey_core::raw_config::redact(&mut output);
    println!("{}", serde_json::to_string_pretty(&output)?);
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

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_opener_uses_the_native_binary_without_a_shell() {
        let spec =
            browser_opener_spec("https://auth.openai.com/oauth/authorize?state=test").unwrap();
        assert_eq!(spec.program, OsString::from("/usr/bin/open"));
        assert_eq!(
            spec.args,
            [OsString::from(
                "https://auth.openai.com/oauth/authorize?state=test"
            )]
        );
        assert!(spec.env.is_empty());
        assert_eq!(spec.timeout, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn tui_cleanup_merges_idle_cancel_and_ui_error_into_session_end_once() {
        use std::sync::Mutex;

        for expected in ["interrupted", "terminal input failed"] {
            let (prompts, _prompt_rx) = mpsc::channel(1);
            let (shutdown, _shutdown_rx) = tokio::sync::watch::channel(false);
            let worker = tokio::spawn(async { TuiWorkerSummary::new("provider", "model") });
            let ended = Arc::new(Mutex::new(Vec::new()));
            let ended_for_hook = Arc::clone(&ended);

            close_tui_runtime(
                prompts,
                shutdown,
                worker,
                Duration::from_secs(1),
                TuiWorkerSummary::new("fallback", "fallback"),
                Some(expected.to_string()),
                move |summary| async move {
                    ended_for_hook.lock().unwrap().push(summary.last_error);
                },
            )
            .await
            .unwrap();

            assert_eq!(
                ended.lock().unwrap().as_slice(),
                [Some(expected.to_string())]
            );
        }
    }

    #[tokio::test]
    async fn tui_cleanup_propagates_worker_panic_after_session_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (prompts, _prompt_rx) = mpsc::channel(1);
        let (shutdown, _shutdown_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(async { panic!("worker exploded") });
        let ended = Arc::new(AtomicUsize::new(0));
        let ended_for_hook = Arc::clone(&ended);

        let error = close_tui_runtime(
            prompts,
            shutdown,
            worker,
            Duration::from_secs(1),
            TuiWorkerSummary::new("provider", "model"),
            None,
            move |_| async move {
                ended_for_hook.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("TUI worker failed"));
        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tui_result_keeps_ui_error_primary_and_attaches_cleanup_error() {
        let error = finish_tui_result(
            Err(anyhow::anyhow!("UI failed")),
            Err(anyhow::anyhow!("cleanup failed")),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.starts_with("UI failed"));
        assert!(text.contains("cleanup failed"));

        assert!(
            finish_tui_result(Ok(()), Err(anyhow::anyhow!("cleanup only")))
                .unwrap_err()
                .to_string()
                .contains("cleanup only")
        );
    }

    #[tokio::test]
    async fn tui_cleanup_closes_prompts_then_signals_and_runs_session_end_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (prompts, mut prompt_rx) = mpsc::channel(1);
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(async move {
            assert!(prompt_rx.recv().await.is_none());
            shutdown_rx.changed().await.unwrap();
            assert!(*shutdown_rx.borrow());
            TuiWorkerSummary::new("provider", "model")
        });
        let ended = Arc::new(AtomicUsize::new(0));
        let ended_for_hook = Arc::clone(&ended);

        close_tui_runtime(
            prompts,
            shutdown,
            worker,
            Duration::from_secs(1),
            TuiWorkerSummary::new("fallback", "fallback"),
            None,
            move |_| async move {
                ended_for_hook.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();

        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tui_cleanup_aborts_only_after_timeout_and_still_runs_session_end_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (prompts, _prompt_rx) = mpsc::channel(1);
        let (shutdown, _shutdown_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(async {
            std::future::pending::<()>().await;
            unreachable!()
        });
        let ended = Arc::new(AtomicUsize::new(0));
        let ended_for_hook = Arc::clone(&ended);
        let started = tokio::time::Instant::now();

        let error = close_tui_runtime(
            prompts,
            shutdown,
            worker,
            Duration::from_millis(20),
            TuiWorkerSummary::new("provider", "model"),
            None,
            move |_| async move {
                ended_for_hook.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap_err();

        assert!(started.elapsed() >= Duration::from_millis(20));
        assert!(error.to_string().contains("shutdown timed out"));
        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_tui_runs_typed_session_end_hook_once() {
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("session-end.marker");
        let hooks = HookRunner::new(
            &grey_core::HooksConfig {
                session_end: vec![format!("printf x >> '{}'", marker.display())],
                ..Default::default()
            },
            &[],
            &grey_core::RuntimeConfig::default(),
        );
        let (prompts, _prompt_rx) = mpsc::channel(1);
        let (shutdown, _shutdown_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(async {
            std::future::pending::<()>().await;
            unreachable!()
        });
        let hook_workspace = workspace.path().to_path_buf();

        let error = close_tui_runtime(
            prompts,
            shutdown,
            worker,
            Duration::from_millis(20),
            TuiWorkerSummary::new("provider", "model"),
            Some("cancelled".into()),
            move |summary| async move {
                let mut payload = lifecycle_hook_payload(
                    HookEvent::SessionEnd,
                    &hook_workspace,
                    &summary.provider,
                    &summary.model,
                );
                payload.success = Some(false);
                payload.error = summary.last_error.as_deref();
                hooks.run_best_effort(payload).await.unwrap();
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("shutdown timed out"));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
    }

    #[test]
    fn parse_orchestrate_contract_parses_clean_json() {
        let raw = r#"{"status":"ok","summary":"done","recommendations":["ship"],"risks":["none"],"artifacts":["report.md"]}"#;
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "ok");
        assert_eq!(contract.summary, "done");
        assert_eq!(contract.recommendations, vec!["ship"]);
        assert_eq!(contract.risks, vec!["none"]);
        assert_eq!(contract.artifacts, vec!["report.md"]);
    }

    #[test]
    fn parse_orchestrate_contract_parses_with_prefixed_text() {
        let raw = r#"前面有说明，输出如下：{"status":"warn","summary":"partial","recommendations":["rerun"],"risks":["timeout"],"artifacts":[]}"#;
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "warn");
        assert_eq!(contract.summary, "partial");
        assert_eq!(contract.recommendations, vec!["rerun"]);
        assert_eq!(contract.risks, vec!["timeout"]);
    }

    #[test]
    fn parse_orchestrate_contract_parses_code_block_json() {
        let raw = r#"分析如下：
```json
{"status":"fail","summary":"found issue","recommendations":["retry"],"risks":["timeout"],"artifacts":["fix.patch"]}
```
"#;
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "fail");
        assert_eq!(contract.summary, "found issue");
        assert_eq!(contract.artifacts, vec!["fix.patch"]);
    }

    #[test]
    fn parse_orchestrate_contract_falls_back_on_plain_text() {
        let raw = "只给了自然语言，没有 JSON";
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "warn");
        assert_eq!(contract.summary, "只给了自然语言，没有 JSON");
        assert_eq!(contract.recommendations, vec!["response not in schema"]);
        assert_eq!(contract.risks, vec!["requires normalization"]);
        assert!(contract.artifacts.is_empty());
    }

    #[test]
    fn parse_orchestrate_contract_normalizes_unknown_status() {
        let raw =
            r#"{"status":"done","summary":"done","recommendations":[],"risks":[],"artifacts":[]}"#;
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "warn");
    }

    #[test]
    fn parse_orchestrate_contract_defaults_missing_fields() {
        let raw = r#"{"status":"OK"}"#;
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "ok");
        assert_eq!(contract.summary, "no summary provided");
        assert!(contract.recommendations.is_empty());
        assert!(contract.risks.is_empty());
        assert!(contract.artifacts.is_empty());
    }

    #[test]
    fn parse_orchestrate_contract_rejects_unknown_fields() {
        let raw = r#"{"status":"ok","summary":"done","recommendations":[],"risks":[],"artifacts":[],"unexpected":"bad"}"#;
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "warn");
        assert!(contract.summary.starts_with("{\"status\""));
        assert_eq!(contract.recommendations, vec!["response not in schema"]);
        assert_eq!(contract.risks, vec!["requires normalization"]);
        assert!(contract.artifacts.is_empty());
    }

    #[test]
    fn parse_orchestrate_contract_rejects_invalid_list_types() {
        let raw =
            r#"{"status":"ok","summary":"done","recommendations":"bad","risks":[],"artifacts":[]}"#;
        let contract = parse_orchestrate_contract(raw);
        assert_eq!(contract.status, "warn");
        assert!(contract.summary.starts_with("{\"status\""));
        assert_eq!(contract.recommendations, vec!["response not in schema"]);
        assert_eq!(contract.risks, vec!["requires normalization"]);
        assert!(contract.artifacts.is_empty());
    }

    #[test]
    fn sanitize_lists_caps_length() {
        let raw = format!(
            r#"{{"status":"ok","summary":"x","recommendations":[{}],"risks":[],"artifacts":[]}}"#,
            (0..30)
                .map(|i| format!(r#""r{i}""#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let contract = parse_orchestrate_contract(&raw);
        assert_eq!(contract.recommendations.len(), ORCHESTRATE_MAX_LIST_ITEMS);
    }

    #[test]
    fn parse_orchestrate_coordinator_contract_prefers_json_schema() {
        let fallback = AgentOutcome {
            messages: Vec::new(),
            response: "fallback response".to_string(),
            usage: grey_core::Usage::default(),
            steps: 99,
            cached: false,
            provider_id: "fallback-provider".to_string(),
            model: "fallback-model".to_string(),
        };
        let contract = parse_orchestrate_coordinator_contract(
            r#"{"response":"coordinator report","provider":"p","model":"m","steps":1,"cached":false}"#,
            &fallback,
        );
        assert_eq!(contract.response, "coordinator report");
        assert_eq!(contract.provider, "p");
        assert_eq!(contract.model, "m");
        assert_eq!(contract.steps, 1);
        assert!(!contract.cached);
    }

    #[test]
    fn parse_orchestrate_coordinator_contract_falls_back_to_outcome() {
        let fallback = AgentOutcome {
            messages: Vec::new(),
            response: "fallback raw".to_string(),
            usage: grey_core::Usage::default(),
            steps: 4,
            cached: true,
            provider_id: "fallback-provider".to_string(),
            model: "fallback-model".to_string(),
        };
        let contract = parse_orchestrate_coordinator_contract("plain text", &fallback);
        assert_eq!(contract.response, "plain text");
        assert_eq!(contract.provider, "fallback-provider");
        assert_eq!(contract.model, "fallback-model");
        assert_eq!(contract.steps, 4);
        assert!(contract.cached);
    }

    #[test]
    fn is_retriable_subagent_error_detects_hints() {
        assert!(is_retriable_subagent_error(
            "service temporarily unavailable"
        ));
        assert!(is_retriable_subagent_error("connection reset by peer"));
        assert!(is_retriable_subagent_error("HTTP 503 service unavailable"));

        assert!(!is_retriable_subagent_error("invalid api key"));
        assert!(!is_retriable_subagent_error("session not found"));
    }
}
