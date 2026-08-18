use std::process::Command;

use std::path::PathBuf;

fn grey() -> Command {
    Command::new(env!("CARGO_BIN_EXE_grey"))
}

struct TestEnv {
    home: tempfile::TempDir,
    cache_db: PathBuf,
    session_db: PathBuf,
}

impl TestEnv {
    fn command(&self) -> Command {
        let mut command = grey();
        command
            .env("HOME", self.home.path())
            .env("GREY_CACHE_DB", &self.cache_db)
            .env("GREY_SESSION_DB", &self.session_db)
            .env_remove("GREY_CONFIG")
            .env_remove("GREY_PROVIDER")
            .env_remove("GREY_MODEL")
            .env_remove("GREY_OPENAI_BASE_URL")
            .env_remove("GREY_OPENAI_API_KEY")
            .env_remove("GREY_OPENAI_MODEL")
            .env_remove("GREY_ANTHROPIC_BASE_URL")
            .env_remove("GREY_ANTHROPIC_API_KEY")
            .env_remove("GREY_ANTHROPIC_MODEL");
        command
    }
}

fn temp_home() -> TestEnv {
    let dir = tempfile::tempdir().unwrap();
    let cache_db = dir.path().join("cache.db");
    let session_db = dir.path().join("sessions.db");
    TestEnv {
        home: dir,
        cache_db,
        session_db,
    }
}

#[test]
fn providers_list_shows_configured_providers() {
    let env = temp_home();
    let output = env.command().args(["providers", "list"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mock") || stdout.contains("openai") || stdout.contains("anthropic"));
}

#[test]
fn providers_show_for_unknown_id_errors() {
    let env = temp_home();
    let output = env
        .command()
        .args(["providers", "show", "nonexistent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"));
}

#[test]
fn cache_stats_works_on_empty_cache() {
    let env = temp_home();
    let output = env.command().args(["cache", "stats"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hits:") || stdout.contains("entries:"));
}

#[test]
fn cache_clear_works() {
    let env = temp_home();
    let output = env.command().args(["cache", "clear"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cleared"));
}

#[test]
fn usage_summary_works_with_no_sessions() {
    let env = temp_home();
    let db = tempfile::tempdir().unwrap();
    let output = env
        .command()
        .env("GREY_SESSION_DB", db.path().join("sessions.db"))
        .args(["usage", "summary"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Tokens:") || stdout.contains("Turns:"));
}

#[test]
fn no_cache_flag_runs_without_error() {
    let env = temp_home();
    let output = env
        .command()
        .args(["--no-save", "--no-cache", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["response"].as_str().unwrap().contains("hello"));
    assert_eq!(value["cached"], false);
}

#[test]
fn no_fallback_flag_runs_without_error() {
    let env = temp_home();
    let output = env
        .command()
        .args(["--no-save", "--no-fallback", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["response"].as_str().unwrap().contains("hello"));
}

#[test]
fn task_flag_routes_correctly() {
    let env = temp_home();
    let output = env
        .command()
        .args(["--no-save", "--task", "coding", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["response"].as_str().unwrap().contains("hello"));
    assert!(value["provider"].as_str().is_some());
    assert!(value["model"].as_str().is_some());
}

#[test]
fn json_output_includes_provider_and_model_fields() {
    let env = temp_home();
    let output = env
        .command()
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["provider"].as_str().is_some());
    assert!(value["model"].as_str().is_some());
    assert!(value["cached"].as_bool().is_some());
}

#[test]
fn cache_hit_on_second_identical_request() {
    let env = temp_home();
    let cache_db = tempfile::tempdir().unwrap();
    let cache_path = cache_db.path().join("cache.db");

    let first = env
        .command()
        .env("GREY_CACHE_DB", &cache_path)
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(first.status.success());

    let second = env
        .command()
        .env("GREY_CACHE_DB", &cache_path)
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(second.status.success());

    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["cached"], true);

    let stats = env
        .command()
        .env("GREY_CACHE_DB", &cache_path)
        .args(["cache", "stats"])
        .output()
        .unwrap();
    let stats_out = String::from_utf8_lossy(&stats.stdout);
    assert!(stats_out.contains("entries: 1"));
}

#[test]
fn no_cache_flag_bypasses_cache() {
    let env = temp_home();
    let cache_db = tempfile::tempdir().unwrap();
    let cache_path = cache_db.path().join("cache.db");

    let first = env
        .command()
        .env("GREY_CACHE_DB", &cache_path)
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(first.status.success());

    let second = env
        .command()
        .env("GREY_CACHE_DB", &cache_path)
        .args(["--no-save", "--no-cache", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(second.status.success());

    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["cached"], false);
}

#[test]
fn saved_session_contains_usage_json() {
    let env = temp_home();
    let output = env
        .command()
        .args(["--format", "json", "persist usage"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let session_id = value["session_id"].as_str().unwrap();
    let usage = env
        .command()
        .args(["usage", "show", session_id])
        .output()
        .unwrap();
    assert!(
        usage.status.success(),
        "{}",
        String::from_utf8_lossy(&usage.stderr)
    );
    let stdout = String::from_utf8_lossy(&usage.stdout);
    assert!(stdout.contains("Tokens:") && stdout.contains("Turns: 1"));
}

#[test]
fn pre_prompt_hook_rewrites_prompt_before_agent_request() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"[hooks]
pre_prompt = ["printf 'from hook pre prompt'"]
"#,
    )
    .unwrap();
    let output = env
        .command()
        .args(["--no-save", "--format", "json", "should be ignored"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["response"]
        .as_str()
        .unwrap()
        .contains("from hook pre prompt"));
}

#[test]
fn orchestrate_runs_default_subagents_and_returns_json() {
    let env = temp_home();
    let output = env
        .command()
        .args(["orchestrate", "优化这个仓库的文档结构", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["task"].as_str().unwrap(), "优化这个仓库的文档结构");
    let subagents = value["subagents"].as_array().unwrap();
    assert_eq!(subagents.len(), 3);
    for item in subagents {
        assert!(item["response"].as_str().unwrap().contains("（mock"));
        assert!(item["name"].as_str().is_some());
    }
    let synthesis = &value["synthesis"];
    assert!(synthesis["response"].as_str().is_some());
    assert!(synthesis["provider"].as_str().is_some());
}

#[test]
fn orchestrate_custom_agent_spec_is_parsed() {
    let env = temp_home();
    let output = env
        .command()
        .args([
            "orchestrate",
            "帮我改一个 bug",
            "--agent",
            "planner:只给出排障顺序",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let subagents = value["subagents"].as_array().unwrap();
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0]["name"].as_str().unwrap(), "planner");
    assert!(subagents[0]["response"]
        .as_str()
        .unwrap()
        .contains("（mock"));
}

#[test]
fn orchestrate_rejects_invalid_agent_spec() {
    let env = temp_home();
    let output = env
        .command()
        .args([
            "orchestrate",
            "help",
            "--agent",
            "invalid_spec_without_colon",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid --agent spec"));
}
