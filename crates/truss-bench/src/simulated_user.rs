use std::sync::Arc;

use serde::{Deserialize, Serialize};
use truss_core::ctx::Ctx;
use truss_core::error::HarnessError;
use truss_core::message::{Block, Message, Role};
use truss_core::provider::{CompletionRequest, Provider};

#[derive(Debug, Clone)]
pub struct SimulatedUserConfig {
    pub persona: String,
    pub goal: String,
    pub max_turns: usize,
    pub max_tokens: u32,
}

impl SimulatedUserConfig {
    pub fn new(persona: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            persona: persona.into(),
            goal: goal.into(),
            max_turns: 6,
            max_tokens: 512,
        }
    }
}

pub struct SimulatedUser {
    provider: Arc<dyn Provider>,
    config: SimulatedUserConfig,
}

impl SimulatedUser {
    pub fn new(provider: Arc<dyn Provider>, config: SimulatedUserConfig) -> Self {
        Self { provider, config }
    }

    pub async fn run(
        &self,
        ctx: &Ctx,
        agent: Arc<dyn Provider>,
    ) -> Result<InteractionReport, HarnessError> {
        let mut transcript = Vec::new();
        let mut turns = Vec::new();
        let mut last_rating = Rating::default();

        for turn_index in 1..=self.config.max_turns {
            let user_text = self.next_user_message(ctx, &transcript).await?;
            transcript.push(Message::user(user_text.clone()));

            let agent_text = complete_text(
                agent.as_ref(),
                ctx,
                CompletionRequest {
                    system: Some("You are the Truss agent under simulated UX test.".into()),
                    system_blocks: Vec::new(),
                    messages: transcript.clone(),
                    tools: Vec::new(),
                    max_tokens: self.config.max_tokens,
                    model: Some(agent.default_model().to_string()),
                    temperature: Some(0.0),
                    reasoning: None,
                },
            )
            .await?;
            transcript.push(Message {
                role: Role::Assistant,
                content: vec![Block::Text(agent_text.clone())],
            });
            turns.push(InteractionTurn {
                turn_index,
                user: user_text,
                agent: agent_text,
            });

            last_rating = self.rate(ctx, &transcript).await?;
            if last_rating.goal_met {
                return Ok(InteractionReport {
                    persona: self.config.persona.clone(),
                    goal: self.config.goal.clone(),
                    turns,
                    metrics: InteractionMetrics {
                        turns_to_success: Some(turn_index),
                        goal_met: true,
                        legibility_score: last_rating.legibility_score,
                        score: last_rating.score,
                    },
                    rating_reason: last_rating.reason,
                });
            }
        }

        Ok(InteractionReport {
            persona: self.config.persona.clone(),
            goal: self.config.goal.clone(),
            turns,
            metrics: InteractionMetrics {
                turns_to_success: None,
                goal_met: false,
                legibility_score: last_rating.legibility_score,
                score: last_rating.score,
            },
            rating_reason: last_rating.reason,
        })
    }

    async fn next_user_message(
        &self,
        ctx: &Ctx,
        transcript: &[Message],
    ) -> Result<String, HarnessError> {
        complete_text(
            self.provider.as_ref(),
            ctx,
            CompletionRequest {
                system: Some(format!(
                    "You are a simulated user.\nPersona: {}\nGoal: {}\n\
                     Continue the interaction with one concise user message.",
                    self.config.persona, self.config.goal
                )),
                system_blocks: Vec::new(),
                messages: transcript.to_vec(),
                tools: Vec::new(),
                max_tokens: self.config.max_tokens,
                model: Some(self.provider.default_model().to_string()),
                temperature: Some(0.0),
                reasoning: None,
            },
        )
        .await
    }

    async fn rate(&self, ctx: &Ctx, transcript: &[Message]) -> Result<Rating, HarnessError> {
        let raw = complete_text(
            self.provider.as_ref(),
            ctx,
            CompletionRequest {
                system: Some(format!(
                    "Rate whether this interaction met the simulated user's goal.\n\
                     Persona: {}\nGoal: {}\n\
                     Return only JSON with fields goal_met, legibility_score, score, reason.",
                    self.config.persona, self.config.goal
                )),
                system_blocks: Vec::new(),
                messages: transcript.to_vec(),
                tools: Vec::new(),
                max_tokens: self.config.max_tokens,
                model: Some(self.provider.default_model().to_string()),
                temperature: Some(0.0),
                reasoning: None,
            },
        )
        .await?;
        Ok(serde_json::from_str(&raw).unwrap_or_else(|_| Rating {
            goal_met: false,
            legibility_score: 0.0,
            score: 0.0,
            reason: format!("rating JSON parse failed: {raw}"),
        }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionReport {
    pub persona: String,
    pub goal: String,
    pub turns: Vec<InteractionTurn>,
    pub metrics: InteractionMetrics,
    pub rating_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionTurn {
    pub turn_index: usize,
    pub user: String,
    pub agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionMetrics {
    pub turns_to_success: Option<usize>,
    pub goal_met: bool,
    pub legibility_score: f64,
    pub score: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Rating {
    goal_met: bool,
    legibility_score: f64,
    score: f64,
    reason: String,
}

impl Default for Rating {
    fn default() -> Self {
        Self {
            goal_met: false,
            legibility_score: 0.0,
            score: 0.0,
            reason: "not rated".into(),
        }
    }
}

async fn complete_text(
    provider: &dyn Provider,
    ctx: &Ctx,
    req: CompletionRequest,
) -> Result<String, HarnessError> {
    let resp = provider
        .complete(ctx, &req)
        .await
        .map_err(HarnessError::Provider)?;
    Ok(Message {
        role: Role::Assistant,
        content: resp.content,
    }
    .text())
}
