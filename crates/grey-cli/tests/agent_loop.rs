use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::{self, BoxStream};
use grey_core::{
    Agent, AgentOptions, ChatRequest, ContextManager, Provider, ProviderEvent, Role, ToolCall,
    Usage,
};
use grey_tools::{AlwaysApprove, BuiltinTools};

struct CodingProvider {
    turns: Mutex<VecDeque<Vec<ProviderEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl Provider for CodingProvider {
    fn id(&self) -> &str {
        "coding-fixture"
    }

    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Result<BoxStream<'a, ProviderEvent>> {
        self.requests.lock().unwrap().push(request.clone());
        let events = self.turns.lock().unwrap().pop_front().unwrap();
        Ok(Box::pin(stream::iter(events)))
    }
}

fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }),
        ProviderEvent::Done(Usage::default()),
    ]
}

#[tokio::test]
async fn finds_edits_and_verifies_a_bug_in_one_real_tool_loop() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("answer.txt"), "status=broken\n").unwrap();
    let provider = Arc::new(CodingProvider {
        turns: Mutex::new(VecDeque::from([
            tool_turn(
                "read",
                "read_file",
                serde_json::json!({"path": "answer.txt"}),
            ),
            tool_turn(
                "edit",
                "edit_file",
                serde_json::json!({
                    "path": "answer.txt",
                    "old_string": "status=broken",
                    "new_string": "status=fixed"
                }),
            ),
            tool_turn(
                "test",
                "bash",
                serde_json::json!({
                    "command": "test \"$(cat answer.txt)\" = status=fixed"
                }),
            ),
            vec![
                ProviderEvent::Delta("Fixed and verified.".into()),
                ProviderEvent::Done(Usage {
                    input_tokens: 20,
                    output_tokens: 4,
                }),
            ],
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let tools = Arc::new(BuiltinTools::new(workspace.path(), Arc::new(AlwaysApprove)).unwrap());
    let mut options = AgentOptions::new("fixture");
    options.max_steps = 6;
    options.retries = 0;
    options.retry_delay = Duration::ZERO;
    let agent = Agent::new(provider.clone(), tools, ContextManager::default(), options);

    let outcome = agent
        .run_new("system", "fix the broken status and verify it", None)
        .await
        .unwrap();

    assert_eq!(outcome.response, "Fixed and verified.");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("answer.txt")).unwrap(),
        "status=fixed\n"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter().skip(1) {
        let result = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(result.content.contains("\"success\":true"));
    }
}
