//! Filesystem tools: read, write, list. All enforce [`crate::policy::Policy`]
//! scope before touching disk.

use std::path::Path;

use async_trait::async_trait;
use serde_json::json;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::provider::ToolDef;

use super::Tool;

fn arg_str(input: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{key}'")))
}

/// Read a UTF-8 text file.
pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description:
                "Read a UTF-8 text file from the local filesystem and return its contents.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        let path = arg_str(&input, "path")?;
        if !ctx.policy.can_read(Path::new(&path)) {
            return Err(ToolError::PermissionDenied(format!(
                "read outside scope: {path}"
            )));
        }
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))
    }
}

/// Write a UTF-8 text file, creating parent directories within scope.
pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write_file".into(),
            description: "Write (overwrite) a UTF-8 text file on the local filesystem.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to write" },
                    "content": { "type": "string", "description": "File contents" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        let path = arg_str(&input, "path")?;
        let content = arg_str(&input, "content")?;
        let p = Path::new(&path);
        if !ctx.policy.can_write(p) {
            return Err(ToolError::PermissionDenied(format!(
                "write outside scope: {path}"
            )));
        }
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ToolError::Execution(e.to_string()))?;
            }
        }
        tokio::fs::write(&path, content.as_bytes())
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }
}

/// List a directory's entries.
pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_dir".into(),
            description: "List the entries of a directory.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        let path = arg_str(&input, "path")?;
        if !ctx.policy.can_read(Path::new(&path)) {
            return Err(ToolError::PermissionDenied(format!(
                "list outside scope: {path}"
            )));
        }
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            let suffix = match entry.file_type().await {
                Ok(ft) if ft.is_dir() => "/",
                _ => "",
            };
            out.push(format!("{name}{suffix}"));
        }
        out.sort();
        Ok(out.join("\n"))
    }
}
