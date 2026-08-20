use std::process::Command;

fn grey() -> Command {
    Command::new(env!("CARGO_BIN_EXE_grey"))
}

#[test]
fn single_prompt_has_scriptable_json_output_without_network() {
    let workspace = tempfile::tempdir().unwrap();
    let output = grey()
        .current_dir(workspace.path())
        .env("HOME", workspace.path())
        .args(["--no-save", "--format", "json", "hello"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["response"].as_str().unwrap().contains("hello"));
    assert!(value["session_id"].is_null());
    assert_eq!(value["steps"], 1);
}

#[test]
fn unknown_provider_is_an_actionable_error() {
    let workspace = tempfile::tempdir().unwrap();
    let output = grey()
        .current_dir(workspace.path())
        .args(["--no-save", "--provider", "missing", "hello"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown provider"));
}

#[test]
fn sessions_can_be_saved_listed_shown_and_resumed() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sessions.db");
    let first = grey()
        .current_dir(directory.path())
        .env("HOME", directory.path())
        .env("GREY_SESSION_DB", &database)
        .args(["--format", "json", "first prompt"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let session_id = first_json["session_id"].as_str().unwrap();

    let list = grey()
        .env("GREY_SESSION_DB", &database)
        .args(["sessions", "list"])
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains(session_id));

    let show = grey()
        .env("GREY_SESSION_DB", &database)
        .args(["sessions", "show", session_id])
        .output()
        .unwrap();
    assert!(show.status.success());
    assert!(String::from_utf8_lossy(&show.stdout).contains("first prompt"));

    let second = grey()
        .current_dir(directory.path())
        .env("HOME", directory.path())
        .env("GREY_SESSION_DB", &database)
        .args(["--session", session_id, "--format", "json", "second prompt"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["session_id"], session_id);
    assert!(second_json["response"]
        .as_str()
        .unwrap()
        .contains("second prompt"));
}

#[test]
fn nonexistent_resume_id_fails_without_creating_a_session() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sessions.db");
    let output = grey()
        .current_dir(directory.path())
        .env("GREY_SESSION_DB", &database)
        .args(["--session", "does-not-exist", "hello"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session not found"));
}

#[test]
fn config_init_refuses_to_overwrite_without_force() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("grey.toml");
    std::fs::write(&config, "sentinel = true\n").unwrap();
    let output = grey()
        .env("GREY_CONFIG", &config)
        .args(["config", "init"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert_eq!(
        std::fs::read_to_string(config).unwrap(),
        "sentinel = true\n"
    );
}
