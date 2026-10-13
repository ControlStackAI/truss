use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use truss_core::agent::Agent;
use truss_core::ctx::Ctx;
use truss_core::error::ProviderError;
use truss_core::message::{Block, Role};
use truss_core::policy::Policy;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::tools::ToolRegistry;

const TEXT_EDITOR_TOOL_NAME: &str = "str_replace_based_edit_tool";

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "truss-provider-tools-{}-{name}",
        std::process::id()
    ))
}

#[tokio::test]
async fn anthropic_text_editor_alias_executes_local_file_tools() {
    let dir = temp_dir("direct");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("note.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    let path = path.to_string_lossy().to_string();

    let registry = ToolRegistry::with_defaults();
    let ctx = Ctx::new().with_policy(Policy::workdir(&dir));
    let result = registry
        .call(
            &ctx,
            TEXT_EDITOR_TOOL_NAME,
            json!({
                "command": "str_replace",
                "path": path,
                "old_str": "beta",
                "new_str": "gamma"
            }),
        )
        .await
        .unwrap();

    assert!(result.contains("wrote"));
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt")).unwrap(),
        "alpha\ngamma\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

struct TypedToolProvider {
    calls: AtomicUsize,
    path: String,
}

#[async_trait]
impl Provider for TypedToolProvider {
    fn name(&self) -> &str {
        "typed-tool-test"
    }

    fn default_model(&self) -> &str {
        "typed-tool-test"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(CompletionResponse {
                content: vec![Block::ToolUse {
                    id: "toolu_1".into(),
                    name: TEXT_EDITOR_TOOL_NAME.into(),
                    input: json!({
                        "command": "create",
                        "path": self.path,
                        "file_text": "created through typed editor"
                    }),
                }],
                stop_reason: StopReason::ToolUse,
                model: "typed-tool-test".into(),
                usage: Usage::default(),
            }),
            1 => {
                let tool_result = req
                    .messages
                    .iter()
                    .find(|message| message.role == Role::Tool)
                    .and_then(|message| message.content.first())
                    .expect("agent should send a tool result back");
                match tool_result {
                    Block::ToolResult {
                        id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(id, "toolu_1");
                        assert!(!is_error);
                        assert!(content.contains("wrote"));
                    }
                    other => panic!("expected tool result, got {other:?}"),
                }
                Ok(CompletionResponse {
                    content: vec![Block::Text("done".into())],
                    stop_reason: StopReason::EndTurn,
                    model: "typed-tool-test".into(),
                    usage: Usage::default(),
                })
            }
            _ => panic!("unexpected extra model call"),
        }
    }
}

#[tokio::test]
async fn typed_tool_use_round_trips_through_agent_loop() {
    let dir = temp_dir("roundtrip");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("created.txt");

    let provider = Arc::new(TypedToolProvider {
        calls: AtomicUsize::new(0),
        path: path.to_string_lossy().to_string(),
    });
    let mut agent = Agent::new(provider, Arc::new(ToolRegistry::with_defaults()));
    agent.max_steps = 4;
    let ctx = Ctx::new().with_policy(Policy::workdir(&dir));

    let outcome = agent.run(&ctx, "create the file").await.unwrap();

    assert_eq!(outcome.final_text, "done");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "created through typed editor"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
