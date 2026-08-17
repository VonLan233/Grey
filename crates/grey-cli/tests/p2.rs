use std::process::Command;

use std::path::PathBuf;

fn grey() -> Command {
    Command::new(env!("CARGO_BIN_EXE_grey"))
}

fn temp_home() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    let cache_db = dir.path().join("cache.db");
    std::env::set_var("GREY_CACHE_DB", &cache_db);
    let session_db = dir.path().join("sessions.db");
    std::env::set_var("GREY_SESSION_DB", &session_db);
    (dir, session_db)
}

#[test]
fn providers_list_shows_configured_providers() {
    let (_home, _session_db) = temp_home();
    let output = grey().args(["providers", "list"]).output().unwrap();
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
    let (_home, _session_db) = temp_home();
    let output = grey()
        .args(["providers", "show", "nonexistent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"));
}

#[test]
fn cache_stats_works_on_empty_cache() {
    let (_home, _session_db) = temp_home();
    let output = grey().args(["cache", "stats"]).output().unwrap();
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
    let (_home, _session_db) = temp_home();
    let output = grey().args(["cache", "clear"]).output().unwrap();
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
    let (_home, _session_db) = temp_home();
    let db = tempfile::tempdir().unwrap();
    let output = grey()
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
    let (_home, _session_db) = temp_home();
    let output = grey()
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
    let (_home, _session_db) = temp_home();
    let output = grey()
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
    let (_home, _session_db) = temp_home();
    let output = grey()
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
    let (_home, _session_db) = temp_home();
    let output = grey()
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
    let (_home, _session_db) = temp_home();
    let cache_db = tempfile::tempdir().unwrap();
    let cache_path = cache_db.path().join("cache.db");
    std::env::set_var("GREY_CACHE_DB", &cache_path);

    let first = grey()
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(first.status.success());

    let second = grey()
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(second.status.success());

    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["cached"], true);

    let stats = grey().args(["cache", "stats"]).output().unwrap();
    let stats_out = String::from_utf8_lossy(&stats.stdout);
    assert!(stats_out.contains("entries: 1"));
}

#[test]
fn no_cache_flag_bypasses_cache() {
    let (_home, _session_db) = temp_home();
    let cache_db = tempfile::tempdir().unwrap();
    let cache_path = cache_db.path().join("cache.db");
    std::env::set_var("GREY_CACHE_DB", &cache_path);

    let first = grey()
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(first.status.success());

    let second = grey()
        .args(["--no-save", "--no-cache", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(second.status.success());

    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["cached"], false);
}
