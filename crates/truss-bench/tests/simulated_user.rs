use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use truss_bench::{SimulatedUser, SimulatedUserConfig};
use truss_core::ctx::Ctx;
use truss_core::error::ProviderError;
use truss_core::message::Block;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};

#[derive(Clone)]
struct ScriptedUser {
    messages: Arc<Mutex<VecDeque<String>>>,
}

impl ScriptedUser {
    fn new(messages: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            messages: Arc::new(Mutex::new(messages.into_iter().map(Into::into).collect())),
        }
    }
}

#[async_trait]
impl Provider for ScriptedUser {
    fn name(&self) -> &str {
        "scripted-user"
    }

    fn default_model(&self) -> &str {
        "gpt-4o-mini"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let system = req.system.clone().unwrap_or_default();
        let text = if system.starts_with("Rate whether") {
            let transcript = req
                .messages
                .iter()
                .map(|message| message.text())
                .collect::<Vec<_>>()
                .join("\n");
            let goal_met = transcript.contains("reset link sent");
            format!(
                r#"{{"goal_met": {goal_met}, "legibility_score": 0.88, "score": 0.91, "reason": "scripted rating"}}"#
            )
        } else {
            self.messages
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "Can you be clearer?".into())
        };
        Ok(text_response(&text, 7, 5))
    }
}

struct ScriptedAgent;

#[async_trait]
impl Provider for ScriptedAgent {
    fn name(&self) -> &str {
        "scripted-agent"
    }

    fn default_model(&self) -> &str {
        "gpt-4o-mini"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let prompt = req
            .messages
            .last()
            .map(|message| message.text())
            .unwrap_or_default();
        let response = if prompt.contains("password") {
            "I found the account and sent a reset link. reset link sent"
        } else {
            "I can help with the account once you describe the issue."
        };
        Ok(text_response(response, 9, 8))
    }
}

fn text_response(text: &str, input_tokens: u64, output_tokens: u64) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(text.into())],
        stop_reason: StopReason::EndTurn,
        model: "gpt-4o-mini".into(),
        usage: Usage {
            input_tokens,
            output_tokens,
        },
    }
}

#[tokio::test]
async fn simulated_user_drives_scripted_multi_turn_session() {
    let user = Arc::new(ScriptedUser::new([
        "Hi, I need help with my account.",
        "I forgot my password.",
    ]));
    let config = SimulatedUserConfig {
        persona: "Impatient but polite SaaS admin".into(),
        goal: "Recover account access with a clear next step".into(),
        max_turns: 3,
        max_tokens: 256,
    };
    let harness = SimulatedUser::new(user, config);

    let report = harness
        .run(&Ctx::new(), Arc::new(ScriptedAgent))
        .await
        .unwrap();

    assert_eq!(report.turns.len(), 2);
    assert_eq!(report.metrics.turns_to_success, Some(2));
    assert!(report.metrics.goal_met);
    assert!(report.metrics.legibility_score > 0.8);
    assert!(report.metrics.score > 0.9);
    assert_eq!(report.rating_reason, "scripted rating");
}
