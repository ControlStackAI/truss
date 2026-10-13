//! A shell tool that is genuinely interruptible.
//!
//! This is the layer-2 interrupt demonstration: when the context is cancelled (or
//! its deadline passes), the spawned child process is **killed**, not just
//! abandoned. Most harnesses leak the child; we don't.

use async_trait::async_trait;
use serde_json::json;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::provider::ToolDef;
use crate::sandbox::SandboxCommand;

use super::Tool;

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "bash".into(),
            description: "Run a shell command via `sh -c` and return combined stdout/stderr. \
                          Subject to policy: forbidden commands are rejected."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'command'".into()))?;

        if let Some(bad) = ctx.policy.command_violation(command) {
            return Err(ToolError::PermissionDenied(format!(
                "command contains forbidden substring: {bad:?}"
            )));
        }

        let output = ctx.sandbox.run(SandboxCommand::shell(command), ctx).await?;
        Ok(output.combined_text())
    }
}
