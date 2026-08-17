//! Grey composition root: headless agent, interactive TUI, spikes and config.

use std::env;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use grey_core::{
    config, Agent, AgentEvent, AgentOptions, AgentOutcome, ChatMessage, ChatRequest,
    ContextManager, GreyConfig, Provider, Role, Session, SessionStore, Usage,
};
use grey_provider::{build_provider, model_for_provider};
use grey_tools::{AlwaysApprove, Approver, BuiltinTools, DenySideEffects, StdioApprover};
use serde::Serialize;
use tokio::sync::mpsc;

const SYSTEM_PROMPT: &str = r#"You are Grey, a careful coding agent working inside one workspace.
Inspect before changing anything. Use read_file, glob, and grep to gather evidence. Use edit_file
only with an exact old_string that occurs once. After edits, run the relevant tests with bash.
Keep changes scoped to the user's request, report tool failures honestly, and never claim success
without verification evidence."#;

#[derive(Parser)]
#[command(
    name = "grey",
    version,
    about = "A lightweight, high-performance, extensible code agent harness"
)]
struct Cli {
    /// One-shot prompt. Omit it to start the interactive TUI.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Provider override: mock, openai, or anthropic.
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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
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
}

#[derive(Subcommand)]
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

#[derive(Subcommand)]
enum SessionAction {
    /// List recent sessions.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print one complete session as JSON.
    Show { id: String },
}

#[derive(Serialize)]
struct HeadlessOutput {
    response: String,
    session_id: Option<String>,
    usage: Usage,
    steps: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        return run_command(command).await;
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

async fn run_command(command: Command) -> Result<()> {
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
    }
}

async fn run_headless(
    cli: &Cli,
    config: &GreyConfig,
    workspace: &Path,
    prompt: &str,
) -> Result<()> {
    let (agent, store, existing) = build_agent_and_session(cli, config, workspace, false)?;
    let outcome = if cli.format == OutputFormat::Text {
        run_with_text_events(&agent, existing.as_ref(), prompt).await?
    } else {
        run_with_cancellation(&agent, existing.as_ref(), prompt, None).await?
    };
    let session_id = persist_outcome(
        store.as_ref(),
        existing,
        &outcome,
        prompt,
        workspace,
        cli.no_save,
    )?;

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
                usage: outcome.usage,
                steps: outcome.steps,
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
    let provider_box = build_provider(config, cli.provider.as_deref())?;
    let provider: Arc<dyn Provider> = Arc::from(provider_box);
    let model = model_for_provider(config, cli.provider.as_deref(), cli.model.as_deref())?;
    let approver: Arc<dyn Approver> = if cli.auto_approve {
        Arc::new(AlwaysApprove)
    } else if cli.read_only || tui_mode {
        Arc::new(DenySideEffects)
    } else {
        Arc::new(StdioApprover)
    };
    let tools = Arc::new(BuiltinTools::new(workspace, approver)?);
    let mut options = AgentOptions::new(model);
    options.max_steps = cli.max_steps;
    let agent = Agent::new(provider, tools, ContextManager::default(), options);

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
    Ok(Some(session.id))
}

async fn run_tui(cli: &Cli, config: &GreyConfig, workspace: &Path) -> Result<()> {
    let (agent, store, existing) = build_agent_and_session(cli, config, workspace, true)?;
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (prompts_tx, mut prompts_rx) = mpsc::unbounded_channel::<String>();
    let workspace = workspace.to_path_buf();
    let no_save = cli.no_save;
    let worker = tokio::spawn(async move {
        let mut session = existing;
        while let Some(prompt) = prompts_rx.recv().await {
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

async fn run_spike_c(
    config: &GreyConfig,
    prompt: &str,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<()> {
    let provider = build_provider(config, provider_override)?;
    let model = model_for_provider(config, provider_override, model_override)?;
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
