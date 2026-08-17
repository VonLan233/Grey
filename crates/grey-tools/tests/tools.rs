use std::sync::Arc;
use std::time::Duration;

use grey_core::{McpToolConfig, ToolCall, ToolExecutor};
use grey_tools::{
    AlwaysApprove, BuiltinTools, CombinedTools, DenySideEffects, HookedTools, McpTools,
    BUILTIN_TOOL_NAMES,
};

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments,
    }
}

#[test]
fn exposes_exact_p1_tool_set() {
    let directory = tempfile::tempdir().unwrap();
    let tools = BuiltinTools::new(directory.path(), Arc::new(DenySideEffects)).unwrap();
    let names: Vec<_> = tools
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(names, BUILTIN_TOOL_NAMES);
}

#[tokio::test]
async fn read_file_is_scoped_and_line_bounded() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("sample.txt"), "one\ntwo\nthree\n").unwrap();
    let tools = BuiltinTools::new(directory.path(), Arc::new(DenySideEffects)).unwrap();

    let result = tools
        .execute(&call(
            "read_file",
            serde_json::json!({"path": "sample.txt", "offset": 2, "limit": 1}),
        ))
        .await;
    assert!(result.success);
    assert_eq!(result.output, "two\n");

    let escaped = tools
        .execute(&call(
            "read_file",
            serde_json::json!({"path": "../outside.txt"}),
        ))
        .await;
    assert!(!escaped.success);
    assert!(escaped.output.contains("workspace"));
}

#[tokio::test]
async fn edit_is_atomic_and_requires_one_exact_match() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("code.rs");
    std::fs::write(&path, "fn old() {}\n").unwrap();
    let tools = BuiltinTools::new(directory.path(), Arc::new(AlwaysApprove)).unwrap();

    let result = tools
        .execute(&call(
            "edit_file",
            serde_json::json!({
                "path": "code.rs",
                "old_string": "fn old() {}",
                "new_string": "fn new() {}"
            }),
        ))
        .await;
    assert!(result.success, "{}", result.output);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn new() {}\n");

    std::fs::write(&path, "same\nsame\n").unwrap();
    let duplicate = tools
        .execute(&call(
            "edit_file",
            serde_json::json!({
                "path": "code.rs",
                "old_string": "same",
                "new_string": "changed"
            }),
        ))
        .await;
    assert!(!duplicate.success);
    assert!(duplicate.output.contains("exactly once"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "same\nsame\n");
}

#[tokio::test]
async fn denied_side_effect_never_changes_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("code.rs");
    std::fs::write(&path, "old").unwrap();
    let tools = BuiltinTools::new(directory.path(), Arc::new(DenySideEffects)).unwrap();
    let result = tools
        .execute(&call(
            "edit_file",
            serde_json::json!({
                "path": "code.rs",
                "old_string": "old",
                "new_string": "new"
            }),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("denied"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "old");
}

#[tokio::test]
async fn glob_and_grep_respect_workspace_and_report_locations() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("src")).unwrap();
    std::fs::write(
        directory.path().join("src/lib.rs"),
        "fn alpha() {}\nfn beta() {}\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("src/skip.txt"), "alpha").unwrap();
    let tools = BuiltinTools::new(directory.path(), Arc::new(DenySideEffects)).unwrap();

    let glob = tools
        .execute(&call("glob", serde_json::json!({"pattern": "**/*.rs"})))
        .await;
    assert!(glob.success);
    assert_eq!(glob.output, "src/lib.rs\n");

    let grep = tools
        .execute(&call(
            "grep",
            serde_json::json!({"pattern": "alpha", "glob": "**/*.rs"}),
        ))
        .await;
    assert!(grep.success);
    assert_eq!(grep.output, "src/lib.rs:1:fn alpha() {}\n");
}

#[tokio::test]
async fn bash_captures_output_and_times_out() {
    let directory = tempfile::tempdir().unwrap();
    let mut tools = BuiltinTools::new(directory.path(), Arc::new(AlwaysApprove)).unwrap();
    tools.set_max_command_duration(Duration::from_millis(50));

    let output = tools
        .execute(&call("bash", serde_json::json!({"command": "printf grey"})))
        .await;
    assert!(output.success, "{}", output.output);
    assert_eq!(output.output, "grey");

    let timeout = tools
        .execute(&call("bash", serde_json::json!({"command": "sleep 1"})))
        .await;
    assert!(!timeout.success);
    assert!(timeout.output.contains("timed out"));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(outside.path(), workspace.path().join("link")).unwrap();
    let tools = BuiltinTools::new(workspace.path(), Arc::new(DenySideEffects)).unwrap();
    let result = tools
        .execute(&call(
            "read_file",
            serde_json::json!({"path": "link/secret.txt"}),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("workspace"));
}

#[tokio::test]
async fn combined_tools_resolves_builtins_and_mcp() {
    let workspace = tempfile::tempdir().unwrap();
    let builtin = BuiltinTools::new(workspace.path(), Arc::new(DenySideEffects)).unwrap();
    let mcp = McpTools::new(vec![McpToolConfig {
        name: "mock".into(),
        command: "sh".into(),
        args: vec![
            "-lc".into(),
            "printf '{\"success\":true,\"output\":\"from mcp\"}'".into(),
        ],
        ..Default::default()
    }]);
    let tools: std::sync::Arc<dyn grey_core::ToolExecutor> =
        Arc::new(CombinedTools::new(vec![Arc::new(builtin), Arc::new(mcp)]));

    let names = tools.definitions();
    assert!(names
        .iter()
        .any(|definition| definition.name == "read_file"));
    assert!(names.iter().any(|definition| definition.name == "mock"));

    let result = tools
        .execute(&crate::call("mock", serde_json::json!({})))
        .await;
    assert!(result.success);
    assert_eq!(result.output, "from mcp");
}

#[tokio::test]
async fn pre_tool_hook_blocks_and_post_tool_hook_runs_on_success() {
    let workspace = tempfile::tempdir().unwrap();
    let builtin = BuiltinTools::new(workspace.path(), Arc::new(DenySideEffects)).unwrap();
    let tools = HookedTools::new(Arc::new(builtin), vec!["false".into()], vec!["true".into()]);
    let result = tools
        .execute(&crate::call(
            "read_file",
            serde_json::json!({"path": "absent.txt"}),
        ))
        .await;
    assert!(!result.success, "{}", result.output);
    assert!(result.output.contains("denied"));
}
