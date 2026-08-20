use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

use grey_core::{
    HookRunner, HooksConfig, McpServerConfig, McpToolConfig, PluginConfig, PluginKind,
    RuntimeConfig, ToolCall, ToolExecutor,
};
use grey_tools::{
    AlwaysApprove, BuiltinTools, CombinedTools, DenySideEffects, HookedApprover, HookedTools,
    LspTools, McpServers, McpTools, PluginTools, BUILTIN_TOOL_NAMES,
};

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments,
    }
}

fn hook_runner(hooks: HooksConfig) -> HookRunner {
    HookRunner::new(&hooks, &[], &RuntimeConfig::default())
}

#[cfg(unix)]
fn mcp_mock(workspace: &std::path::Path) -> std::path::PathBuf {
    let script = workspace.join("mcp-mock.sh");
    std::fs::write(&script, r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}' ;;
    *'"method":"tools/list"'*'"cursor":null'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"first","description":"first tool","inputSchema":{"type":"object"}}],"nextCursor":"page-2"}}' ;;
    *'"method":"tools/list"'*) echo '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"second","inputSchema":{"type":"object"}}]}}' ;;
    *'"method":"tools/call"'*) echo '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"ok"}]}}' ;;
  esac
done
"#).unwrap();
    script
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_mcp_initializes_paginates_and_calls() {
    let workspace = tempfile::tempdir().unwrap();
    let server = McpServers::connect(
        workspace.path(),
        vec![McpServerConfig {
            id: "mock".into(),
            command: "sh".into(),
            args: vec![mcp_mock(workspace.path()).display().to_string()],
            timeout_ms: Some(1_000),
            ..Default::default()
        }],
    )
    .await
    .unwrap();
    let names: Vec<_> = server
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(names, ["mock__first", "mock__second"]);
    let result = server
        .execute(&call("mock__second", serde_json::json!({"value": 1})))
        .await;
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("ok"));
}

#[tokio::test]
async fn stdio_mcp_rejects_non_stdio_transport() {
    let workspace = tempfile::tempdir().unwrap();
    let error = McpServers::connect(
        workspace.path(),
        vec![McpServerConfig {
            id: "bad".into(),
            transport: "sse".into(),
            command: "https://example.test".into(),
            ..Default::default()
        }],
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("only stdio"));
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_mcp_rejects_malformed_jsonl() {
    let workspace = tempfile::tempdir().unwrap();
    let script = workspace.path().join("mcp-malformed.sh");
    std::fs::write(&script, "#!/bin/sh\nread _\nprintf 'not-json\\n'\n").unwrap();
    let error = match McpServers::connect(
        workspace.path(),
        vec![McpServerConfig {
            id: "bad".into(),
            command: "sh".into(),
            args: vec![script.display().to_string()],
            ..Default::default()
        }],
    )
    .await
    {
        Ok(_) => panic!("malformed MCP JSONL was accepted"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("malformed MCP JSONL"),
        "{error:#}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_mcp_rejects_oversize_jsonl_before_buffering_a_full_line() {
    let workspace = tempfile::tempdir().unwrap();
    let script = workspace.path().join("mcp-oversize.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nread _\nhead -c 1048577 /dev/zero | tr '\\0' x\nprintf '\\n'\n",
    )
    .unwrap();
    let error = match McpServers::connect(
        workspace.path(),
        vec![McpServerConfig {
            id: "large".into(),
            command: "sh".into(),
            args: vec![script.display().to_string()],
            ..Default::default()
        }],
    )
    .await
    {
        Ok(_) => panic!("oversize MCP JSONL was accepted"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("MCP response exceeds"),
        "{error:#}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_mcp_timeout_terminates_and_reaps_its_process_group() {
    let workspace = tempfile::tempdir().unwrap();
    let pid_file = workspace.path().join("mcp-pids");
    let script = workspace.path().join("mcp-timeout.sh");
    std::fs::write(&script, r#"#!/bin/sh
sleep 30 & child=$!
printf '%s %s\n' "$$" "$child" > "$PID_FILE"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}' ;;
    *'"method":"tools/list"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"hang","inputSchema":{"type":"object"}}]}}' ;;
    *'"method":"tools/call"'*) sleep 30 ;;
  esac
done
"#).unwrap();
    let server = McpServers::connect(
        workspace.path(),
        vec![McpServerConfig {
            id: "timeout".into(),
            command: "sh".into(),
            args: vec![script.display().to_string()],
            env: std::collections::HashMap::from([(
                "PID_FILE".into(),
                pid_file.display().to_string(),
            )]),
            timeout_ms: Some(50),
            ..Default::default()
        }],
    )
    .await
    .unwrap();
    let result = server
        .execute(&call("timeout__hang", serde_json::json!({})))
        .await;
    assert!(!result.success, "{result:?}");
    let pids = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Ok(pids) = std::fs::read_to_string(&pid_file) {
                break pids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for pid in pids.split_whitespace() {
        let status = std::process::Command::new("/bin/kill")
            .args(["-0", pid])
            .status()
            .unwrap();
        assert!(!status.success(), "MCP descendant {pid} survived timeout");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_mcp_cancellation_reaps_its_process_group() {
    let workspace = tempfile::tempdir().unwrap();
    let pid_file = workspace.path().join("mcp-pids");
    let script = workspace.path().join("mcp-cancel.sh");
    std::fs::write(&script, r#"#!/bin/sh
sleep 30 & child=$!
printf '%s %s\n' "$$" "$child" > "$PID_FILE"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}' ;;
    *'"method":"tools/list"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"hang","inputSchema":{"type":"object"}}]}}' ;;
    *'"method":"tools/call"'*) sleep 30 ;;
  esac
done
"#).unwrap();
    let server = Arc::new(
        McpServers::connect(
            workspace.path(),
            vec![McpServerConfig {
                id: "cancel".into(),
                command: "sh".into(),
                args: vec![script.display().to_string()],
                env: std::collections::HashMap::from([(
                    "PID_FILE".into(),
                    pid_file.display().to_string(),
                )]),
                timeout_ms: Some(5_000),
                ..Default::default()
            }],
        )
        .await
        .unwrap(),
    );
    let pending = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            server
                .execute(&call("cancel__hang", serde_json::json!({})))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    pending.abort();
    let _ = pending.await;
    drop(server);
    let pids = std::fs::read_to_string(&pid_file).unwrap();
    for pid in pids.split_whitespace() {
        for _ in 0..100 {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", pid])
                .status()
                .unwrap()
                .success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !std::process::Command::new("/bin/kill")
                .args(["-0", pid])
                .status()
                .unwrap()
                .success(),
            "MCP descendant {pid} survived cancellation"
        );
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
async fn permission_decision_hook_can_block_when_inner_approves() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("code.rs");
    std::fs::write(&path, "old").unwrap();
    let approver = Arc::new(HookedApprover::new(
        Arc::new(AlwaysApprove),
        hook_runner(HooksConfig {
            permission_decision: vec!["printf '{\"approved\": false}'".into()],
            ..Default::default()
        }),
        directory.path(),
        "provider",
        "model",
    ));
    let tools = BuiltinTools::new(directory.path(), approver).unwrap();

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
async fn permission_decision_hook_false_plain_output_defaults_to_denied() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("code.rs");
    std::fs::write(&path, "old").unwrap();
    let approver = Arc::new(HookedApprover::new(
        Arc::new(AlwaysApprove),
        hook_runner(HooksConfig {
            permission_decision: vec!["printf false".into()],
            ..Default::default()
        }),
        directory.path(),
        "provider",
        "model",
    ));
    let tools = BuiltinTools::new(directory.path(), approver).unwrap();

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
async fn permission_hook_cannot_upgrade_a_base_denial() {
    let workspace = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("permission.marker");
    let hooks = HooksConfig {
        permission_decision: vec![format!(
            "printf config >> '{}'; printf true",
            marker.display()
        )],
        ..Default::default()
    };
    let plugins = vec![PluginConfig {
        id: "permission-plugin".into(),
        kind: PluginKind::Hook,
        enabled: true,
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            format!("printf plugin >> '{}'; printf true", marker.display()),
        ],
        hook_event: Some("permission_decision".into()),
        ..Default::default()
    }];
    let approver = HookedApprover::new(
        Arc::new(DenySideEffects),
        HookRunner::new(&hooks, &plugins, &RuntimeConfig::default()),
        workspace.path(),
        "provider",
        "model",
    );
    assert!(
        !grey_tools::Approver::approve(
            &approver,
            &call("bash", serde_json::json!({"command": "true"})),
            grey_core::ToolRisk::Execute,
        )
        .await
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "configplugin");
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
    let tools = HookedTools::new(
        Arc::new(builtin),
        Arc::new(AlwaysApprove),
        hook_runner(HooksConfig {
            pre_tool_call: vec!["printf false".into()],
            ..Default::default()
        }),
        workspace.path(),
        "provider",
        "model",
    );
    let result = tools
        .execute(&crate::call(
            "read_file",
            serde_json::json!({"path": "absent.txt"}),
        ))
        .await;
    assert!(!result.success, "{}", result.output);
    assert!(result.output.contains("denied"));
}

#[tokio::test]
async fn post_tool_hook_error_preserves_tool_success() {
    let workspace = tempfile::tempdir().unwrap();
    let file_path = workspace.path().join("code.rs");
    std::fs::write(&file_path, "hello").unwrap();

    let builtin = BuiltinTools::new(workspace.path(), Arc::new(AlwaysApprove)).unwrap();
    let tools = HookedTools::new(
        Arc::new(builtin),
        Arc::new(AlwaysApprove),
        hook_runner(HooksConfig {
            post_tool_call: vec!["false".into(), "echo unreachable".into()],
            ..Default::default()
        }),
        workspace.path(),
        "provider",
        "model",
    );
    let result = tools
        .execute(&crate::call(
            "read_file",
            serde_json::json!({"path": "code.rs"}),
        ))
        .await;
    assert!(result.success, "{}", result.output);
    assert_eq!(result.output, "hello");
}

#[tokio::test]
async fn plugin_tool_executes_command_in_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("plugin_marker"), "").unwrap();
    let tools = PluginTools::new(
        workspace.path(),
        vec![PluginConfig {
            id: "mark".into(),
            kind: PluginKind::Tool,
            enabled: true,
            command: "printf".into(),
            args: vec![r#"{"success":true,"output":"ok"}"#.into()],
            ..Default::default()
        }],
        Arc::new(AlwaysApprove),
    );

    let result = tools
        .execute(&call(
            "mark",
            serde_json::json!({
                "path": "plugin_marker",
            }),
        ))
        .await;
    assert!(result.success, "{}", result.output);
    assert_eq!(result.output, "ok");
}

#[tokio::test]
async fn sealed_wasm_tool_executes_without_command_fallback() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("plugins/tool");
    std::fs::create_dir_all(&dir).unwrap();
    let output = br#"{"success":true,"output":"wasm-tool"}"#;
    let mut bytes = Vec::from([8, 0, 0, 0]);
    bytes.extend_from_slice(&(output.len() as u32).to_le_bytes());
    bytes.extend_from_slice(output);
    let data = bytes
        .iter()
        .map(|byte| format!("\\{:02x}", byte))
        .collect::<String>();
    let module = wat::parse_str(format!(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func $w (param i32 i32 i32 i32) (result i32))) (memory 1) (export "memory" (memory 0)) (data (i32.const 0) "{data}") (func (export "_start") i32.const 1 i32.const 0 i32.const 1 i32.const 64 call $w drop))"#)).unwrap();
    std::fs::write(dir.join("module.wasm"), &module).unwrap();
    let manifest = format!(
        r#"{{"schema_version":1,"id":"sealed-tool","kind":"tool","protocol":"grey.wasm-plugin.v1","wasi":"preview1-stdio","module":"module.wasm","module_sha256":"{}"}}"#,
        hex::encode(Sha256::digest(&module))
    );
    std::fs::write(dir.join("plugin.json"), &manifest).unwrap();
    let mut plugin = PluginConfig {
        id: "sealed-tool".into(),
        kind: PluginKind::Tool,
        enabled: true,
        runtime: grey_core::PluginRuntime::Wasm,
        manifest: Some("plugins/tool/plugin.json".into()),
        manifest_sha256: Some(hex::encode(Sha256::digest(manifest.as_bytes()))),
        ..Default::default()
    };
    plugin.command = "false".into();
    assert!(PluginTools::new_with_runtime(
        root.path(),
        vec![plugin.clone()],
        Arc::new(AlwaysApprove),
        &RuntimeConfig::default(),
        root.path()
    )
    .is_err());
    plugin.command.clear();
    let tools = PluginTools::new_with_runtime(
        root.path(),
        vec![plugin],
        Arc::new(AlwaysApprove),
        &RuntimeConfig::default(),
        root.path(),
    )
    .unwrap();
    assert!(tools
        .definitions()
        .iter()
        .any(|definition| definition.name == "sealed-tool"));
    let result = tools
        .execute(&call("sealed-tool", serde_json::json!({})))
        .await;
    assert!(result.success, "{}", result.output);
    assert_eq!(result.output, "wasm-tool");
}

#[tokio::test]
async fn plugin_tool_unknown_name_fails() {
    let workspace = tempfile::tempdir().unwrap();
    let tools = PluginTools::new(
        workspace.path(),
        vec![PluginConfig {
            id: "mark".into(),
            kind: PluginKind::Tool,
            enabled: true,
            command: "printf".into(),
            args: vec![r#"{"success":true}"#.into()],
            ..Default::default()
        }],
        Arc::new(AlwaysApprove),
    );

    let result = tools.execute(&call("missing", serde_json::json!({}))).await;
    assert!(!result.success);
    assert!(result.output.contains("unknown plugin tool"));
}

#[test]
fn command_only_plugin_tools_never_panics_on_wasm_configuration() {
    let workspace = tempfile::tempdir().unwrap();
    let tools = PluginTools::new(
        workspace.path(),
        vec![PluginConfig {
            id: "sealed".into(),
            kind: PluginKind::Tool,
            enabled: true,
            runtime: grey_core::PluginRuntime::Wasm,
            manifest: Some("missing.json".into()),
            manifest_sha256: Some("0".repeat(64)),
            ..Default::default()
        }],
        Arc::new(AlwaysApprove),
    );
    assert!(tools.is_empty());
}

#[tokio::test]
async fn lsp_diagnostics_is_defined_and_readonly() {
    let workspace = tempfile::tempdir().unwrap();
    let lsp = LspTools::new(workspace.path(), "rust-analyzer".into()).unwrap();
    let names: Vec<_> = lsp
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(names.contains(&"lsp_diagnostics".to_string()));
}

#[tokio::test]
async fn lsp_diagnostics_fails_with_invalid_server_path() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
    let lsp = LspTools::new(workspace.path(), "non-existent-lsp-server".into()).unwrap();
    let result = lsp
        .execute(&crate::call(
            "lsp_diagnostics",
            serde_json::json!({"path":"main.rs"}),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("running LSP diagnostics"));
}

#[tokio::test]
async fn lsp_definition_is_defined_and_readonly() {
    let workspace = tempfile::tempdir().unwrap();
    let lsp = LspTools::new(workspace.path(), "rust-analyzer".into()).unwrap();
    let names: Vec<_> = lsp
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(names.contains(&"lsp_definition".to_string()));
}

#[tokio::test]
async fn lsp_definition_fails_with_invalid_server_path() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
    let lsp = LspTools::new(workspace.path(), "non-existent-lsp-server".into()).unwrap();
    let result = lsp
        .execute(&crate::call(
            "lsp_definition",
            serde_json::json!({"path":"main.rs"}),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("running LSP definition"));
}

#[tokio::test]
async fn lsp_references_is_defined_and_readonly() {
    let workspace = tempfile::tempdir().unwrap();
    let lsp = LspTools::new(workspace.path(), "rust-analyzer".into()).unwrap();
    let names: Vec<_> = lsp
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(names.contains(&"lsp_references".to_string()));
}

#[tokio::test]
async fn lsp_references_fails_with_invalid_server_path() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
    let lsp = LspTools::new(workspace.path(), "non-existent-lsp-server".into()).unwrap();
    let result = lsp
        .execute(&crate::call(
            "lsp_references",
            serde_json::json!({"path":"main.rs"}),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("running LSP references"));
}

#[tokio::test]
async fn lsp_hover_is_defined_and_readonly() {
    let workspace = tempfile::tempdir().unwrap();
    let lsp = LspTools::new(workspace.path(), "rust-analyzer".into()).unwrap();
    let names: Vec<_> = lsp
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(names.contains(&"lsp_hover".to_string()));
}

#[tokio::test]
async fn lsp_hover_fails_with_invalid_server_path() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
    let lsp = LspTools::new(workspace.path(), "non-existent-lsp-server".into()).unwrap();
    let result = lsp
        .execute(&crate::call(
            "lsp_hover",
            serde_json::json!({"path":"main.rs"}),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("running LSP hover"));
}

#[tokio::test]
async fn lsp_rename_is_defined_and_readonly() {
    let workspace = tempfile::tempdir().unwrap();
    let lsp = LspTools::new(workspace.path(), "rust-analyzer".into()).unwrap();
    let names: Vec<_> = lsp
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(names.contains(&"lsp_rename".to_string()));
}

#[tokio::test]
async fn lsp_rename_fails_with_invalid_server_path() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
    let lsp = LspTools::new(workspace.path(), "non-existent-lsp-server".into()).unwrap();
    let result = lsp
        .execute(&crate::call(
            "lsp_rename",
            serde_json::json!({"path":"main.rs","new_name":"main"}),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("running LSP rename"));
}

#[tokio::test]
async fn lsp_symbols_is_defined_and_readonly() {
    let workspace = tempfile::tempdir().unwrap();
    let lsp = LspTools::new(workspace.path(), "rust-analyzer".into()).unwrap();
    let names: Vec<_> = lsp
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(names.contains(&"lsp_symbols".to_string()));
}

#[tokio::test]
async fn lsp_symbols_fails_with_invalid_server_path() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
    let lsp = LspTools::new(workspace.path(), "non-existent-lsp-server".into()).unwrap();
    let result = lsp
        .execute(&crate::call(
            "lsp_symbols",
            serde_json::json!({"path":"main.rs"}),
        ))
        .await;
    assert!(!result.success);
    assert!(result.output.contains("running LSP symbols"));
}
