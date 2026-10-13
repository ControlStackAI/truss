//! Deliberate signed identity amendment tool.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::Tool;
use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::memory::{
    Amendment, AmendmentDecision, AmendmentTarget, IdentityPolicy, LedgerRef, Persona,
};
use crate::provider::ToolDef;

#[derive(Debug, Clone)]
pub struct BufferedIdentityAmendment {
    pub amendment: Amendment,
    pub decision: AmendmentDecision,
}

/// Buffers a signed `amend_identity` request for the next session boundary.
pub struct AmendIdentityTool {
    persona: Persona,
    policy: IdentityPolicy,
    buffer: Arc<Mutex<Vec<BufferedIdentityAmendment>>>,
}

impl AmendIdentityTool {
    pub fn new(
        persona: Persona,
        policy: IdentityPolicy,
        buffer: Arc<Mutex<Vec<BufferedIdentityAmendment>>>,
    ) -> Self {
        Self {
            persona,
            policy,
            buffer,
        }
    }

    pub fn buffer(&self) -> Arc<Mutex<Vec<BufferedIdentityAmendment>>> {
        self.buffer.clone()
    }
}

#[async_trait]
impl Tool for AmendIdentityTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "amend_identity".into(),
            description: "Deliberately request a signed amendment to L0 self identity. Core identity cannot be amended.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "enum": ["self_layer", "core"] },
                    "new_body": { "type": "string" },
                    "signer_pubkey_id": { "type": "string" },
                    "signature": { "type": "string" }
                },
                "required": ["target", "new_body", "signer_pubkey_id", "signature"]
            }),
        }
    }

    async fn call(&self, _ctx: &Ctx, input: Value) -> Result<String, ToolError> {
        self.call_with_ledger(_ctx, input, None).await
    }

    async fn call_with_ledger(
        &self,
        _ctx: &Ctx,
        input: Value,
        proposed_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let proposed_by = proposed_by.ok_or_else(|| {
            ToolError::Execution("amend_identity requires durable journal provenance".into())
        })?;
        let target = match input["target"].as_str().unwrap_or("self_layer") {
            "self" | "self_layer" => AmendmentTarget::SelfLayer,
            "core" => AmendmentTarget::Core,
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "target must be self_layer or core, got {other}"
                )))
            }
        };
        let amendment = Amendment {
            target,
            new_body: input["new_body"].as_str().unwrap_or_default().to_string(),
            proposed_by,
            signer_pubkey_id: input["signer_pubkey_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            signature: input["signature"].as_str().unwrap_or_default().to_string(),
        };
        let decision = self.persona.route_amendment(&amendment, self.policy, true);
        if let AmendmentDecision::Denied(reason) = decision {
            return Ok(format!("denied identity amendment: {reason}"));
        }
        self.buffer.lock().unwrap().push(BufferedIdentityAmendment {
            amendment,
            decision: decision.clone(),
        });
        Ok(format!("buffered identity amendment: {decision:?}"))
    }
}
