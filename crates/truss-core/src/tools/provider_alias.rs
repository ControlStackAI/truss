//! Provider-native tool aliases that still execute through local Truss tools.
//!
//! These names are never advertised by generic providers. They are accepted only
//! so providers with trained typed-tool schemas can round-trip their native tool
//! calls back into the same local policy and execution boundary.

use std::path::Path;

use serde_json::{json, Value};

use crate::ctx::Ctx;
use crate::error::ToolError;

use super::ToolRegistry;

const ANTHROPIC_TEXT_EDITOR_TOOL_NAME: &str = "str_replace_based_edit_tool";

pub(super) async fn call_provider_alias(
    registry: &ToolRegistry,
    ctx: &Ctx,
    name: &str,
    input: Value,
) -> Option<Result<String, ToolError>> {
    match name {
        ANTHROPIC_TEXT_EDITOR_TOOL_NAME => Some(call_text_editor(registry, ctx, input).await),
        _ => None,
    }
}

async fn call_text_editor(
    registry: &ToolRegistry,
    ctx: &Ctx,
    input: Value,
) -> Result<String, ToolError> {
    let command = input_string(&input, "command")?;
    match command.as_str() {
        "view" => view(registry, ctx, &input).await,
        "create" => {
            let path = input_string(&input, "path")?;
            let file_text = input_string(&input, "file_text")?;
            registry
                .call_registered(
                    ctx,
                    "write_file",
                    json!({ "path": path, "content": file_text }),
                )
                .await
        }
        "str_replace" => str_replace(registry, ctx, &input).await,
        "insert" => insert(registry, ctx, &input).await,
        other => Err(ToolError::InvalidInput(format!(
            "unsupported text editor command: {other}"
        ))),
    }
}

async fn view(registry: &ToolRegistry, ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
    let path = input_string(input, "path")?;
    if !ctx.policy.can_read(Path::new(&path)) {
        return Err(ToolError::PermissionDenied(format!(
            "read outside scope: {path}"
        )));
    }
    if tokio::fs::metadata(&path)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return registry
            .call_registered(ctx, "list_dir", json!({ "path": path }))
            .await;
    }

    let content = registry
        .call_registered(ctx, "read_file", json!({ "path": path }))
        .await?;
    Ok(match input.get("view_range") {
        Some(range) => apply_view_range(&content, range)?,
        None => content,
    })
}

async fn str_replace(
    registry: &ToolRegistry,
    ctx: &Ctx,
    input: &Value,
) -> Result<String, ToolError> {
    let path = input_string(input, "path")?;
    let old_str = input_string(input, "old_str")?;
    let new_str = input_string(input, "new_str")?;
    if old_str.is_empty() {
        return Err(ToolError::InvalidInput("old_str must not be empty".into()));
    }

    let content = registry
        .call_registered(ctx, "read_file", json!({ "path": path }))
        .await?;
    let matches = content.matches(&old_str).count();
    if matches != 1 {
        return Err(ToolError::InvalidInput(format!(
            "old_str matched {matches} times; expected exactly once"
        )));
    }
    let updated = content.replacen(&old_str, &new_str, 1);
    registry
        .call_registered(
            ctx,
            "write_file",
            json!({ "path": path, "content": updated }),
        )
        .await
}

async fn insert(registry: &ToolRegistry, ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
    let path = input_string(input, "path")?;
    let insert_line = input
        .get("insert_line")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError::InvalidInput("missing 'insert_line'".into()))?;
    let insert_text = input_string(input, "insert_text")?;
    let content = registry
        .call_registered(ctx, "read_file", json!({ "path": path }))
        .await?;
    let offset = byte_offset_after_line(&content, insert_line)?;
    let mut updated = content;
    updated.insert_str(offset, &insert_text);
    registry
        .call_registered(
            ctx,
            "write_file",
            json!({ "path": path, "content": updated }),
        )
        .await
}

fn input_string(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{key}'")))
}

fn apply_view_range(content: &str, range: &Value) -> Result<String, ToolError> {
    let Some(values) = range.as_array().filter(|values| values.len() == 2) else {
        return Err(ToolError::InvalidInput(
            "view_range must be [start_line, end_line]".into(),
        ));
    };
    let start = values[0]
        .as_i64()
        .ok_or_else(|| ToolError::InvalidInput("view_range start must be an integer".into()))?;
    let end = values[1]
        .as_i64()
        .ok_or_else(|| ToolError::InvalidInput("view_range end must be an integer".into()))?;
    if start < 1 || (end != -1 && end < start) {
        return Err(ToolError::InvalidInput("invalid view_range".into()));
    }

    let lines: Vec<&str> = content.lines().collect();
    let start_idx = (start as usize).saturating_sub(1);
    let end_idx = if end == -1 {
        lines.len()
    } else {
        (end as usize).min(lines.len())
    };
    Ok(lines.get(start_idx..end_idx).unwrap_or_default().join("\n"))
}

fn byte_offset_after_line(content: &str, line: u64) -> Result<usize, ToolError> {
    if line == 0 {
        return Ok(0);
    }
    if content.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "insert_line {line} is past end of file"
        )));
    }

    let mut seen = 0_u64;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            seen += 1;
            if seen == line {
                return Ok(idx + 1);
            }
        }
    }
    if seen + 1 == line {
        return Ok(content.len());
    }
    Err(ToolError::InvalidInput(format!(
        "insert_line {line} is past end of file"
    )))
}
