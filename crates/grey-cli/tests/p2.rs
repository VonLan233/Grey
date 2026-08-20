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
fn auth_help_lists_the_three_openai_oauth_actions() {
    let output = grey().args(["auth", "--help"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: grey auth [OPTIONS] <COMMAND>"));
    for action in ["login", "status", "logout"] {
        assert!(stdout.contains(action), "missing {action} in {stdout}");
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
fn providers_list_includes_provider_plugins() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
default_provider = "mock"
default_model = "m"

[[plugins]]
id = "provider-plugin"
kind = "provider"
command = "printf"
args = ["{\"schema_version\":1,\"text\":\"provider plugin\"}"]
enabled = true
"#,
    )
    .unwrap();

    let output = env.command().args(["providers", "list"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("provider-plugin"));
    assert!(stdout.contains("protocol=plugin"));
}

#[test]
fn providers_show_supports_provider_plugin_entry() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
default_provider = "provider-plugin"
default_model = "m"

[[plugins]]
id = "provider-plugin"
kind = "provider"
command = "printf"
args = ["{\"schema_version\":1,\"text\":\"provider plugin\"}"]
enabled = true
version = "0.1.0"
"#,
    )
    .unwrap();

    let show = env
        .command()
        .args(["providers", "show", "provider-plugin"])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(stdout.contains("protocol: plugin"));
    assert!(stdout.contains("version: 0.1.0"));
}

#[test]
fn provider_plugin_entry_works_in_headless_flow() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
default_provider = "provider-plugin"
default_model = "m"

[[plugins]]
id = "provider-plugin"
kind = "provider"
command = "printf"
args = ["{\"schema_version\":1,\"text\":\"plugin response\",\"usage\":{\"input_tokens\":7,\"output_tokens\":9}}"]
enabled = true
"#,
    )
    .unwrap();

    let output = env
        .command()
        .args([
            "--provider",
            "provider-plugin",
            "--no-save",
            "--format",
            "json",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let response = value["response"].as_str().unwrap();
    assert!(response.contains("plugin response"));
    assert_eq!(value["provider"].as_str().unwrap(), "provider-plugin");
}

#[test]
fn provider_plugin_list_and_show_do_not_expose_command_or_args() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
default_provider = "provider-plugin"
default_model = "m"

[[plugins]]
id = "provider-plugin"
kind = "provider"
command = "secret-command"
args = ["secret-arg"]
enabled = true
"#,
    )
    .unwrap();
    for args in [
        &["providers", "list"][..],
        &["providers", "show", "provider-plugin"][..],
    ] {
        let output = env.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("secret-command"));
        assert!(!stdout.contains("secret-arg"));
    }
}

#[test]
fn providers_list_uses_the_same_provider_plugin_eligibility_as_router() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
[[plugins]]
id = "valid"
kind = "provider"
command = "printf"
enabled = true

[[plugins]]
id = "empty-command"
kind = "provider"
enabled = true
"#,
    )
    .unwrap();
    let output = env.command().args(["providers", "list"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid"));
    assert!(!stdout.contains("empty-command"));
}

#[test]
fn providers_show_uses_the_provider_eligibility_set_on_id_collision() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
[[plugins]]
id = "collision"
kind = "theme"
command = "theme-command"
enabled = true
version = "theme-version"

[[plugins]]
id = "collision"
kind = "provider"
command = "provider-command"
enabled = true
version = "provider-version"
"#,
    )
    .unwrap();
    let output = env
        .command()
        .args(["providers", "show", "collision"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("protocol: plugin"));
    assert!(stdout.contains("version: provider-version"));
}

#[cfg(unix)]
#[test]
fn provider_plugin_runs_from_resolved_workspace() {
    use std::os::unix::fs::PermissionsExt;

    let env = temp_home();
    let workspace = tempfile::tempdir().unwrap();
    let plugin = workspace.path().join("plugin");
    std::fs::write(
        &plugin,
        "#!/bin/sh\nprintf '%s' '{\"schema_version\":1,\"text\":\"workspace plugin\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
default_provider = "workspace-plugin"
default_model = "m"

[[plugins]]
id = "workspace-plugin"
kind = "provider"
command = "./plugin"
enabled = true
"#,
    )
    .unwrap();
    let output = env
        .command()
        .args([
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--provider",
            "workspace-plugin",
            "--no-save",
            "workspace",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("workspace plugin"));
}

#[cfg(unix)]
#[test]
fn spike_c_provider_plugin_runs_from_resolved_workspace() {
    use std::os::unix::fs::PermissionsExt;

    let env = temp_home();
    let workspace = tempfile::tempdir().unwrap();
    let plugin = workspace.path().join("plugin");
    std::fs::write(
        &plugin,
        "#!/bin/sh\nprintf '%s' '{\"schema_version\":1,\"text\":\"spike workspace\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
default_provider = "workspace-plugin"
default_model = "m"

[[plugins]]
id = "workspace-plugin"
kind = "provider"
command = "./plugin"
enabled = true
"#,
    )
    .unwrap();
    let output = env
        .command()
        .args([
            "--workspace",
            workspace.path().to_str().unwrap(),
            "spike-c",
            "--provider",
            "workspace-plugin",
            "workspace",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("spike workspace"));
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
fn pre_message_send_hook_rewrites_prompt_before_agent_request() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"[hooks]
pre_message_send = ["printf '{\"prompt\":\"from hook pre message\"}'"]
"#,
    )
    .unwrap();
    let output = env
        .command()
        .args(["--no-save", "--format", "json", "should be replaced"])
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
        .contains("from hook pre message"));
}

#[test]
fn session_start_completion_and_end_hooks_run_in_headless_mode() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    let start_marker = env.home.path().join("session_start.marker");
    let completion_marker = env.home.path().join("session_completion.marker");
    let end_marker = env.home.path().join("session_end.marker");
    std::fs::write(&start_marker, "").unwrap();
    std::fs::write(&completion_marker, "").unwrap();
    std::fs::write(&end_marker, "").unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        format!(
            "[hooks]\nsession_start = [\"printf start > '{}'\"]\ncompletion = [\"printf completion > '{}'\"]\nsession_end = [\"printf end > '{}'\"]\n",
            start_marker.display(),
            completion_marker.display(),
            end_marker.display(),
        ),
    )
    .unwrap();
    let output = env
        .command()
        .args(["--no-save", "--format", "json", "hello hooks"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["response"].as_str().is_some());
    assert_eq!(std::fs::read_to_string(start_marker).unwrap(), "start");
    assert_eq!(
        std::fs::read_to_string(completion_marker).unwrap(),
        "completion"
    );
    assert_eq!(std::fs::read_to_string(end_marker).unwrap(), "end");
}

#[test]
fn plugin_pre_prompt_hook_rewrites_prompt() {
    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        r#"
[[plugins]]
id = "rewrite-prompt"
kind = "hook"
command = "printf"
args = ["from plugin hook"]
enabled = true
hook_event = "pre_prompt"
"#,
    )
    .unwrap();

    let output = env
        .command()
        .args(["--no-save", "--format", "json", "placeholder"])
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
        .contains("from plugin hook"));
}

#[test]
fn plugins_add_list_show_remove_workflow() {
    let env = temp_home();
    let output = env
        .command()
        .args([
            "plugins",
            "add",
            "p-runner",
            "--kind",
            "tool",
            "--command",
            "printf",
            "--arg",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "added plugin"
    );

    let show = env
        .command()
        .args(["plugins", "show", "p-runner"])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show_value: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(show_value["id"].as_str().unwrap(), "p-runner");
    assert_eq!(show_value["kind"].as_str().unwrap(), "tool");

    let list = env.command().args(["plugins", "list"]).output().unwrap();
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_output = String::from_utf8_lossy(&list.stdout);
    assert!(list_output.contains("p-runner"));
    assert!(list_output.contains("tool"));

    let remove = env
        .command()
        .args(["plugins", "remove", "p-runner"])
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&remove.stdout).trim(),
        "removed plugin p-runner"
    );

    let list_after = env.command().args(["plugins", "list"]).output().unwrap();
    assert!(
        list_after.status.success(),
        "{}",
        String::from_utf8_lossy(&list_after.stderr)
    );
    assert!(String::from_utf8_lossy(&list_after.stdout).contains("(no plugins configured)"));
}

#[test]
fn plugins_enable_disable_updates_state_without_reordering() {
    let env = temp_home();
    let add = env
        .command()
        .args([
            "plugins",
            "add",
            "p-guard",
            "--kind",
            "hook",
            "--command",
            "printf",
            "--arg",
            "ok",
            "--hook-event",
            "pre_prompt",
        ])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );

    let disable = env
        .command()
        .args(["plugins", "disable", "p-guard"])
        .output()
        .unwrap();
    assert!(
        disable.status.success(),
        "{}",
        String::from_utf8_lossy(&disable.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&disable.stdout).trim(),
        "disabled plugin p-guard"
    );

    let enabled_false = env
        .command()
        .args(["plugins", "show", "p-guard"])
        .output()
        .unwrap();
    assert!(enabled_false.status.success());
    let value: serde_json::Value = serde_json::from_slice(&enabled_false.stdout).unwrap();
    assert!(!value["enabled"].as_bool().unwrap());

    let enable = env
        .command()
        .args(["plugins", "enable", "p-guard"])
        .output()
        .unwrap();
    assert!(
        enable.status.success(),
        "{}",
        String::from_utf8_lossy(&enable.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&enable.stdout).trim(),
        "enabled plugin p-guard"
    );

    let enabled_true = env
        .command()
        .args(["plugins", "show", "p-guard"])
        .output()
        .unwrap();
    assert!(enabled_true.status.success());
    let value: serde_json::Value = serde_json::from_slice(&enabled_true.stdout).unwrap();
    assert!(value["enabled"].as_bool().unwrap());
}

#[test]
fn loop_mode_runs_and_reports_iteration_count() {
    let env = temp_home();
    let output = env
        .command()
        .args([
            "--no-save",
            "--format",
            "json",
            "loop",
            "check this task",
            "--iterations",
            "2",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["prompt"].as_str().unwrap(), "check this task");
    assert_eq!(value["iterations"].as_u64().unwrap(), 2);
    assert!(value["response"].as_str().unwrap().contains("（mock"));
}

#[test]
fn repeater_prompt_hook_failure_emits_completion_before_session_end() {
    let env = temp_home();
    let marker = env.home.path().join("repeater-hooks.log");
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("grey.toml"),
        format!(
            r#"[hooks]
pre_message_send = ["exit 7"]
completion = ["printf 'completion\\n' >> '{}'"]
session_end = ["printf 'session_end\\n' >> '{}'"]
"#,
            marker.display(),
            marker.display()
        ),
    )
    .unwrap();

    let output = env
        .command()
        .args([
            "--no-save",
            "loop",
            "fails before provider",
            "--iterations",
            "1",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "completion\nsession_end\n"
    );
}

#[test]
fn goal_mode_outputs_json_and_respects_no_stop_token_by_default() {
    let env = temp_home();
    let output = env
        .command()
        .args([
            "--no-save",
            "--format",
            "json",
            "goal",
            "fix the issue",
            "--iterations",
            "1",
            "--done-when",
            "DONE",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["prompt"].as_str().unwrap(), "fix the issue");
    assert_eq!(value["iterations"].as_u64().unwrap(), 1);
    assert!(value["response"].as_str().unwrap().contains("（mock"));
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
        assert!(item["status"].as_str().is_some());
        assert!(item["summary"].as_str().is_some());
        assert!(item["recommendations"].as_array().is_some());
        assert!(item["risks"].as_array().is_some());
        assert!(item["artifacts"].as_array().is_some());
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
    assert!(!subagents[0]["status"].as_str().unwrap().is_empty());
    assert!(subagents[0]["summary"].as_str().is_some());
}

#[test]
fn orchestrate_share_context_summary_injects_session_tail() {
    let env = temp_home();

    let first = env
        .command()
        .args(["--format", "json", "前置上下文：会话共享标记-abc-123"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let session_id = first_json["session_id"].as_str().unwrap();

    let output = env
        .command()
        .args([
            "orchestrate",
            "请复用上下文做一次建议",
            "--session",
            session_id,
            "--share-context",
            "summary",
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
    assert_eq!(subagents.len(), 3);

    let contains_marker = subagents.iter().all(|agent| {
        agent["response"]
            .as_str()
            .unwrap()
            .contains("会话共享标记-abc-123")
    });
    assert!(contains_marker);
}

#[test]
fn orchestrate_session_is_persisted_by_default() {
    let env = temp_home();
    let output = env
        .command()
        .args([
            "orchestrate",
            "给我一份这个项目的协作说明",
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
    let session_id = value["session_id"].as_str().unwrap();

    let show = env
        .command()
        .args(["sessions", "show", session_id])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let session: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    let messages = session["messages"].as_array().unwrap();
    assert!(!messages.is_empty());
    assert!(messages.iter().any(|message| message["content"]
        .as_str()
        .unwrap_or("")
        .contains("[orchestrate] task")));
}

#[test]
fn orchestrate_subagents_do_not_leak_other_agents_context() {
    let env = temp_home();
    let marker_a = "planner only";
    let marker_b = "coder only";
    let output = env
        .command()
        .args([
            "orchestrate",
            "统一任务：重构日志模块",
            "--agent",
            &format!("planner:{marker_a}"),
            "--agent",
            &format!("coder:{marker_b}"),
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
    assert_eq!(subagents.len(), 2);

    let planner = subagents
        .iter()
        .find(|agent| agent["name"] == "planner")
        .expect("planner missing");
    let coder = subagents
        .iter()
        .find(|agent| agent["name"] == "coder")
        .expect("coder missing");

    let planner_resp = planner["response"].as_str().unwrap();
    let coder_resp = coder["response"].as_str().unwrap();

    assert!(planner_resp.contains(marker_a));
    assert!(!planner_resp.contains(marker_b));

    assert!(coder_resp.contains(marker_b));
    assert!(!coder_resp.contains(marker_a));
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

#[test]
fn plugin_config_edit_preserves_comments_placeholders_and_unknown_fields() {
    let env = temp_home();
    let path = env.home.path().join("explicit.toml");
    let original = r#"# keep
[[plugins]]
id = "old"
command = "printf"
args = ["${PLUGIN_TOKEN}"]
extra = "keep"
"#;
    std::fs::write(&path, original).unwrap();

    let output = env
        .command()
        .env("GREY_CONFIG", &path)
        .env("PLUGIN_TOKEN", "expanded-only-at-runtime")
        .args(["plugins", "disable", "old"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let edited = std::fs::read_to_string(&path).unwrap();
    assert!(edited.contains("# keep"));
    assert!(edited.contains("${PLUGIN_TOKEN}"));
    assert!(edited.contains("extra = \"keep\""));
    assert!(edited.contains("enabled = false"));
}

#[test]
fn plugin_config_edit_prefers_explicit_then_project_target() {
    let env = temp_home();
    let explicit = env.home.path().join("explicit.toml");
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("grey.toml"), "# project\n").unwrap();

    let explicit_output = env
        .command()
        .current_dir(project.path())
        .env("GREY_CONFIG", &explicit)
        .args(["plugins", "add", "explicit", "--command", "printf"])
        .output()
        .unwrap();
    assert!(
        explicit_output.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit_output.stderr)
    );
    assert!(std::fs::read_to_string(&explicit)
        .unwrap()
        .contains("id = \"explicit\""));
    assert_eq!(
        std::fs::read_to_string(project.path().join("grey.toml")).unwrap(),
        "# project\n"
    );

    let project_output = env
        .command()
        .current_dir(project.path())
        .args(["plugins", "add", "project", "--command", "printf"])
        .output()
        .unwrap();
    assert!(
        project_output.status.success(),
        "{}",
        String::from_utf8_lossy(&project_output.stderr)
    );
    let project_config = std::fs::read_to_string(project.path().join("grey.toml")).unwrap();
    assert!(project_config.contains("id = \"project\""));
}

#[cfg(unix)]
#[test]
fn plugin_config_edit_rejects_symlink_target() {
    use std::os::unix::fs::symlink;

    let env = temp_home();
    let target = env.home.path().join("target.toml");
    let link = env.home.path().join("config-link.toml");
    std::fs::write(&target, "# target\n").unwrap();
    symlink(&target, &link).unwrap();

    let output = env
        .command()
        .env("GREY_CONFIG", &link)
        .args(["plugins", "add", "blocked", "--command", "printf"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("symlink"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "# target\n");
}

#[test]
fn plugin_config_concurrent_edits_do_not_lose_entries() {
    let env = temp_home();
    let path = env.home.path().join("concurrent.toml");
    std::fs::write(&path, "# concurrent\n").unwrap();

    let mut first = env
        .command()
        .env("GREY_CONFIG", &path)
        .args(["plugins", "add", "first", "--command", "printf"])
        .spawn()
        .unwrap();
    let mut second = env
        .command()
        .env("GREY_CONFIG", &path)
        .args(["plugins", "add", "second", "--command", "printf"])
        .spawn()
        .unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    let edited = std::fs::read_to_string(&path).unwrap();
    assert!(edited.contains("id = \"first\""));
    assert!(edited.contains("id = \"second\""));
}

#[test]
fn plugin_config_show_recursively_redacts_secret_fields() {
    let env = temp_home();
    let path = env.home.path().join("secrets.toml");
    std::fs::write(
        &path,
        r#"[[plugins]]
id = "secret-plugin"
command = "printf"
api_key = "api-value"
token = "token-value"
secret = "secret-value"
authorization = "authorization-value"
password = "password-value"
nested = { token = "nested-token" }
args = ["--token", "argument-secret"]
"#,
    )
    .unwrap();

    let output = env
        .command()
        .env("GREY_CONFIG", &path)
        .args(["plugins", "show", "secret-plugin"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for secret in [
        "api-value",
        "token-value",
        "secret-value",
        "authorization-value",
        "password-value",
        "nested-token",
        "argument-secret",
    ] {
        assert!(!stdout.contains(secret), "secret leaked: {secret}");
    }
    assert_eq!(stdout.matches("***").count(), 7);
}

#[test]
fn plugin_config_raw_editor_preserves_text_and_redacts_json() {
    let original =
        "# keep\n[[plugins]]\nid = \"old\"\nargs = [\"${PLUGIN_TOKEN}\"]\nextra = \"keep\"\n";
    let edited = grey_core::raw_config::edit_text(original, |doc| {
        grey_core::raw_config::set_enabled(doc, "plugins", "old", false)
    })
    .unwrap();
    assert!(edited.contains("# keep"));
    assert!(edited.contains("${PLUGIN_TOKEN}"));
    assert!(edited.contains("extra = \"keep\""));
    assert!(edited.contains("enabled = false"));

    let mut value = serde_json::json!({"nested": {"authorization": "hidden"}});
    grey_core::raw_config::redact(&mut value);
    assert_eq!(value["nested"]["authorization"], "***");
}

#[cfg(unix)]
fn serve_openai_tool_turns() -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responses = [
        concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-private-id\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"printf tool-argument-secret\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        ),
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"finished\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        ),
    ];
    let task = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response_body in responses {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).unwrap();
                assert!(read > 0, "client closed before request body completed");
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let content_length = String::from_utf8_lossy(&request[..header_end])
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            requests.push(request);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

#[cfg(unix)]
#[test]
fn hook_plugin_all_events_are_ordered_typed_and_sanitized() {
    use std::os::unix::fs::PermissionsExt;

    let env = temp_home();
    let config_dir = env.home.path().join(".config/grey");
    std::fs::create_dir_all(&config_dir).unwrap();
    let history = env
        .command()
        .current_dir(env.home.path())
        .args(["--format", "json", "history-secret-must-not-leak"])
        .output()
        .unwrap();
    assert!(history.status.success());
    let history_json: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    let session_id = history_json["session_id"].as_str().unwrap().to_string();
    let hook_log = env.home.path().join("hook-events.jsonl");
    let hook_script = env.home.path().join("hook.sh");
    std::fs::write(
        &hook_script,
        r#"#!/bin/sh
kind="$1"
event="$2"
log="$3"
payload=$(cat)
printf '%s:%s\t%s\n' "$kind" "$event" "$payload" >> "$log"
case "$event" in
  pre_message_send)
    if [ "$kind" = plugin ]; then
      printf '{"prompt":"rewritten prompt"}'
    fi
    ;;
  permission_decision) printf true ;;
  post_tool_call)
    if [ "$kind" = plugin ]; then
      exit 19
    fi
    ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&hook_script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook_script, permissions).unwrap();
    let (base_url, server) = serve_openai_tool_turns();
    let script = hook_script.display();
    let log = hook_log.display();
    let events = [
        "pre_message_send",
        "pre_prompt",
        "permission_decision",
        "pre_tool_call",
        "post_tool_call",
        "session_start",
        "completion",
        "session_end",
    ];
    let mut config = format!(
        r#"default_provider = "fixture"
default_model = "fixture-model"

[[providers]]
id = "fixture"
protocol = "openai"
base_url = "{base_url}"
api_key = "api-secret-must-not-leak"
include_usage = true

[hooks]
"#
    );
    for event in events {
        config.push_str(&format!(
            "{event} = [\"'{}' config {event} '{}'\"]\n",
            script, log
        ));
    }
    for event in events {
        config.push_str(&format!(
            r#"
[[plugins]]
id = "plugin-{event}"
kind = "hook"
command = "{script}"
args = ["plugin", "{event}", "{log}"]
enabled = true
hook_event = "{event}"
"#
        ));
    }
    std::fs::write(config_dir.join("grey.toml"), config).unwrap();

    let output = env
        .command()
        .current_dir(env.home.path())
        .args([
            "--auto-approve",
            "--no-save",
            "--session",
            &session_id,
            "--format",
            "json",
            "history-free prompt",
        ])
        .output()
        .unwrap();
    let early_records = std::fs::read_to_string(&hook_log).unwrap_or_default();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}\nhooks={early_records}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let second_request = String::from_utf8_lossy(&requests[1]);
    assert!(
        second_request.contains(r#"\"success\":true"#),
        "post hook failure changed tool result: {second_request}"
    );

    let records = std::fs::read_to_string(&hook_log).unwrap();
    let parsed = records
        .lines()
        .map(|line| {
            let (label, payload) = line.split_once('\t').unwrap();
            (
                label.to_string(),
                serde_json::from_str::<serde_json::Value>(payload).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let expected_events = [
        "session_start",
        "pre_message_send",
        "pre_prompt",
        "permission_decision",
        "pre_tool_call",
        "post_tool_call",
        "completion",
        "session_end",
    ];
    let expected = expected_events
        .into_iter()
        .flat_map(|event| [format!("config:{event}"), format!("plugin:{event}")])
        .collect::<Vec<_>>();
    assert_eq!(
        parsed.iter().map(|(label, _)| label).collect::<Vec<_>>(),
        expected.iter().collect::<Vec<_>>()
    );
    for (label, payload) in &parsed {
        assert_eq!(payload["schema_version"], 1, "{label}: {payload}");
        let event = label.split_once(':').unwrap().1;
        assert_eq!(payload["event"], event);
        assert!(payload["workspace"].as_str().is_some());
        let mut expected_fields = vec!["event", "schema_version", "workspace"];
        match event {
            "session_start" | "session_end" => {
                expected_fields.extend(["model", "provider"]);
                if event == "session_end" {
                    expected_fields.push("success");
                }
            }
            "pre_message_send" | "pre_prompt" => {
                expected_fields.extend(["model", "prompt", "provider"]);
            }
            "permission_decision" | "pre_tool_call" => {
                expected_fields.extend(["model", "provider", "tool"]);
            }
            "post_tool_call" => {
                expected_fields.extend(["model", "provider", "success", "tool"]);
            }
            "completion" => {
                expected_fields.extend(["model", "prompt", "provider", "success"]);
            }
            other => panic!("unexpected hook event {other}"),
        }
        expected_fields.sort_unstable();
        let mut actual_fields = payload
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        actual_fields.sort_unstable();
        assert_eq!(actual_fields, expected_fields, "{label}: {payload}");
        for forbidden in [
            "api-secret-must-not-leak",
            "history-secret-must-not-leak",
            "call-private-id",
            "tool-argument-secret",
            "\"arguments\"",
            "\"messages\"",
            "\"history\"",
            "\"id\"",
        ] {
            assert!(
                !payload.to_string().contains(forbidden),
                "{label} leaked {forbidden}: {payload}"
            );
        }
    }
}
