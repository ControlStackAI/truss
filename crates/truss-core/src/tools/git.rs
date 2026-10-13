//! Native git tools.
//!
//! These tools intentionally invoke `git` directly instead of going through the
//! shell tool. The model sees typed operations, Truss policy checks the target
//! repository, and durable runs can record signed provenance plus memoized
//! side-effect results.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::memory::LedgerRef;
use crate::provider::ToolDef;
use crate::store::{tool_call_key, Store};

use super::Tool;

#[derive(Clone)]
pub struct GitJournal {
    pub store: Store,
    pub session_id: String,
}

#[derive(Clone, Copy)]
enum GitToolKind {
    Status,
    Diff,
    Branch,
    Commit,
}

pub struct GitTool {
    kind: GitToolKind,
    journal: Option<GitJournal>,
}

struct GitCallRecord<'a> {
    produced_by: Option<LedgerRef>,
    memo_key: &'a str,
    input_hash: &'a str,
    result: &'a str,
    is_error: bool,
    side_effect: bool,
    error: Option<&'a str>,
}

impl GitTool {
    fn new(kind: GitToolKind, journal: Option<GitJournal>) -> Self {
        Self { kind, journal }
    }

    pub fn status(journal: Option<GitJournal>) -> Self {
        Self::new(GitToolKind::Status, journal)
    }

    pub fn diff(journal: Option<GitJournal>) -> Self {
        Self::new(GitToolKind::Diff, journal)
    }

    pub fn branch(journal: Option<GitJournal>) -> Self {
        Self::new(GitToolKind::Branch, journal)
    }

    pub fn commit(journal: Option<GitJournal>) -> Self {
        Self::new(GitToolKind::Commit, journal)
    }

    fn name(&self) -> &'static str {
        match self.kind {
            GitToolKind::Status => "git_status",
            GitToolKind::Diff => "git_diff",
            GitToolKind::Branch => "git_branch",
            GitToolKind::Commit => "git_commit",
        }
    }

    fn is_side_effect(&self, input: &Value) -> bool {
        match self.kind {
            GitToolKind::Commit => true,
            GitToolKind::Branch => {
                let action = input
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("current");
                matches!(action, "create" | "checkout" | "create_checkout")
            }
            GitToolKind::Status | GitToolKind::Diff => false,
        }
    }

    async fn call_inner(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let input_hash = sha256_json(&input);
        let memo_key = tool_call_key(self.name(), &input);
        let side_effect = self.is_side_effect(&input);

        if let Some(journal) = &self.journal {
            if let Some((content, is_error)) = journal
                .store
                .memo_get(&journal.session_id, &memo_key)
                .map_err(|e| ToolError::Execution(format!("read git memo: {e}")))?
            {
                self.verify_memo(journal, &memo_key, &input_hash, &content, is_error)?;
                if is_error {
                    return Err(ToolError::Execution(content));
                }
                return Ok(content);
            }
        }

        let result = match self.kind {
            GitToolKind::Status => git_status(ctx, &input).await,
            GitToolKind::Diff => git_diff(ctx, &input).await,
            GitToolKind::Branch => git_branch(ctx, &input).await,
            GitToolKind::Commit => git_commit(ctx, &input).await,
        };

        match result {
            Ok(out) => {
                self.record_provenance(
                    ctx,
                    GitCallRecord {
                        produced_by,
                        memo_key: &memo_key,
                        input_hash: &input_hash,
                        result: &out,
                        is_error: false,
                        side_effect,
                        error: None,
                    },
                )?;
                Ok(out)
            }
            Err(err) => {
                let err_text = err.to_string();
                self.record_provenance(
                    ctx,
                    GitCallRecord {
                        produced_by,
                        memo_key: &memo_key,
                        input_hash: &input_hash,
                        result: &err_text,
                        is_error: true,
                        side_effect,
                        error: Some(&err_text),
                    },
                )?;
                Err(err)
            }
        }
    }

    fn record_provenance(&self, ctx: &Ctx, record: GitCallRecord<'_>) -> Result<(), ToolError> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        let payload = json!({
            "tool": self.name(),
            "memo_key": record.memo_key,
            "input_sha256": record.input_hash,
            "result_sha256": sha256_str(record.result),
            "is_error": record.is_error,
            "side_effect": record.side_effect,
            "produced_by": record.produced_by,
            "error": record.error,
        });
        if record.side_effect {
            journal
                .store
                .append_with_memo(
                    &journal.session_id,
                    "git_tool_call",
                    &payload,
                    record.memo_key,
                    record.result,
                    record.is_error,
                )
                .map_err(|e| ToolError::Execution(format!("record git provenance: {e}")))?;
        } else {
            journal
                .store
                .append(&journal.session_id, "git_tool_call", &payload)
                .map_err(|e| ToolError::Execution(format!("record git provenance: {e}")))?;
        }
        Ok(())
    }

    fn verify_memo(
        &self,
        journal: &GitJournal,
        memo_key: &str,
        input_hash: &str,
        content: &str,
        is_error: bool,
    ) -> Result<(), ToolError> {
        let result_hash = sha256_str(content);
        let entries = journal
            .store
            .entries(&journal.session_id)
            .map_err(|e| ToolError::Execution(format!("read git provenance: {e}")))?;
        let verified = entries.iter().rev().any(|entry| {
            entry.kind == "git_tool_call"
                && entry.payload["tool"].as_str() == Some(self.name())
                && entry.payload["memo_key"].as_str() == Some(memo_key)
                && entry.payload["input_sha256"].as_str() == Some(input_hash)
                && entry.payload["result_sha256"].as_str() == Some(result_hash.as_str())
                && entry.payload["is_error"].as_bool() == Some(is_error)
                && entry.payload["side_effect"].as_bool() == Some(true)
        });
        if verified {
            Ok(())
        } else {
            Err(ToolError::Execution(
                "git memo missing matching signed provenance".into(),
            ))
        }
    }
}

#[async_trait]
impl Tool for GitTool {
    fn def(&self) -> ToolDef {
        match self.kind {
            GitToolKind::Status => ToolDef {
                name: self.name().into(),
                description: "Return structured git status for a repository.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository path. Defaults to current directory." },
                        "include_untracked": { "type": "boolean", "description": "Include untracked files. Defaults true." }
                    }
                }),
            },
            GitToolKind::Diff => ToolDef {
                name: self.name().into(),
                description: "Return a git diff patch with a SHA-256 hash.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository path. Defaults to current directory." },
                        "staged": { "type": "boolean", "description": "Diff staged changes instead of worktree changes." },
                        "base": { "type": "string", "description": "Optional base revision or range left side." },
                        "head": { "type": "string", "description": "Optional head revision or range right side." },
                        "paths": { "type": "array", "items": { "type": "string" } }
                    }
                }),
            },
            GitToolKind::Branch => ToolDef {
                name: self.name().into(),
                description: "Inspect, create, or checkout git branches through a typed operation.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository path. Defaults to current directory." },
                        "action": { "type": "string", "enum": ["current", "list", "create", "checkout", "create_checkout"] },
                        "name": { "type": "string", "description": "Branch name for create/checkout actions." },
                        "start_point": { "type": "string", "description": "Optional start point for branch creation." }
                    }
                }),
            },
            GitToolKind::Commit => ToolDef {
                name: self.name().into(),
                description: "Create a git commit and return its structured result. In durable runs, the commit result is memoized so resume does not commit twice.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository path. Defaults to current directory." },
                        "message": { "type": "string", "description": "Commit message." },
                        "all": { "type": "boolean", "description": "Stage all tracked/untracked changes before committing." },
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "Specific paths to stage before committing." },
                        "allow_empty": { "type": "boolean", "description": "Allow an empty commit." }
                    },
                    "required": ["message"]
                }),
            },
        }
    }

    async fn call(&self, ctx: &Ctx, input: Value) -> Result<String, ToolError> {
        self.call_inner(ctx, input, None).await
    }

    async fn call_with_ledger(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        self.call_inner(ctx, input, produced_by).await
    }
}

async fn git_status(ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
    let repo = repo_path(ctx, input, false)?;
    let include_untracked = input
        .get("include_untracked")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let branch = run_git(&repo, &["branch", "--show-current"]).await?;
    let mut args = vec!["status", "--porcelain=v1", "--branch"];
    if !include_untracked {
        args.push("--untracked-files=no");
    }
    let porcelain = run_git(&repo, &args).await?;
    let files = porcelain
        .lines()
        .filter(|line| !line.starts_with("## "))
        .filter_map(parse_status_line)
        .collect::<Vec<_>>();
    render_json(json!({
        "repo": repo,
        "branch": branch.trim(),
        "clean": files.is_empty(),
        "files": files,
        "porcelain": porcelain,
        "porcelain_sha256": sha256_str(&porcelain),
    }))
}

async fn git_diff(ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
    let repo = repo_path(ctx, input, false)?;
    let staged = input
        .get("staged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut owned = Vec::<String>::new();
    owned.push("diff".into());
    if staged {
        owned.push("--cached".into());
    }
    match (
        input.get("base").and_then(Value::as_str),
        input.get("head").and_then(Value::as_str),
    ) {
        (Some(base), Some(head)) => owned.push(format!("{base}..{head}")),
        (Some(base), None) => owned.push(base.to_string()),
        (None, Some(head)) => owned.push(head.to_string()),
        (None, None) => {}
    }
    if let Some(paths) = string_array(input, "paths")? {
        owned.push("--".into());
        owned.extend(paths);
    }
    let args = owned.iter().map(String::as_str).collect::<Vec<_>>();
    let patch = run_git(&repo, &args).await?;
    render_json(json!({
        "repo": repo,
        "staged": staged,
        "patch_sha256": sha256_str(&patch),
        "patch": patch,
    }))
}

async fn git_branch(ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
    let action = input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("current");
    let side_effect = matches!(action, "create" | "checkout" | "create_checkout");
    let repo = repo_path(ctx, input, side_effect)?;
    match action {
        "current" => {
            let branch = run_git(&repo, &["branch", "--show-current"]).await?;
            render_json(json!({ "repo": repo, "current": branch.trim() }))
        }
        "list" => {
            let out = run_git(&repo, &["branch", "--format=%(refname:short)"]).await?;
            let branches = out.lines().map(str::to_string).collect::<Vec<_>>();
            render_json(json!({ "repo": repo, "branches": branches }))
        }
        "create" => {
            let name = required_str(input, "name")?;
            let mut args = vec!["branch", name.as_str()];
            if let Some(start) = input.get("start_point").and_then(Value::as_str) {
                args.push(start);
            }
            run_git(&repo, &args).await?;
            let sha = run_git(&repo, &["rev-parse", name.as_str()]).await?;
            render_json(json!({ "repo": repo, "action": action, "name": name, "sha": sha.trim() }))
        }
        "checkout" => {
            let name = required_str(input, "name")?;
            run_git(&repo, &["checkout", name.as_str()]).await?;
            let sha = run_git(&repo, &["rev-parse", "HEAD"]).await?;
            render_json(
                json!({ "repo": repo, "action": action, "current": name, "sha": sha.trim() }),
            )
        }
        "create_checkout" => {
            let name = required_str(input, "name")?;
            let mut args = vec!["checkout", "-b", name.as_str()];
            if let Some(start) = input.get("start_point").and_then(Value::as_str) {
                args.push(start);
            }
            run_git(&repo, &args).await?;
            let sha = run_git(&repo, &["rev-parse", "HEAD"]).await?;
            render_json(
                json!({ "repo": repo, "action": action, "current": name, "sha": sha.trim() }),
            )
        }
        other => Err(ToolError::InvalidInput(format!(
            "unsupported branch action '{other}'"
        ))),
    }
}

async fn git_commit(ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
    let repo = repo_path(ctx, input, true)?;
    let message = required_str(input, "message")?;
    if input.get("all").and_then(Value::as_bool).unwrap_or(false) {
        run_git(&repo, &["add", "-A"]).await?;
    }
    if let Some(paths) = string_array(input, "paths")? {
        let mut owned = vec!["add".to_string(), "--".to_string()];
        owned.extend(paths);
        let args = owned.iter().map(String::as_str).collect::<Vec<_>>();
        run_git(&repo, &args).await?;
    }
    let before = run_git(&repo, &["rev-parse", "HEAD"]).await.ok();
    let mut args = vec!["commit", "-m", message.as_str()];
    if input
        .get("allow_empty")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        args.push("--allow-empty");
    }
    let commit_output = run_git(&repo, &args).await?;
    let sha = run_git(&repo, &["rev-parse", "HEAD"]).await?;
    let short = run_git(&repo, &["rev-parse", "--short", "HEAD"]).await?;
    render_json(json!({
        "repo": repo,
        "sha": sha.trim(),
        "short_sha": short.trim(),
        "message": message,
        "previous_head": before.map(|s| s.trim().to_string()),
        "commit_output_sha256": sha256_str(&commit_output),
        "commit_output": commit_output,
    }))
}

fn parse_status_line(line: &str) -> Option<Value> {
    if line.len() < 4 {
        return None;
    }
    let index = line.get(0..1).unwrap_or(" ");
    let worktree = line.get(1..2).unwrap_or(" ");
    let path = line.get(3..).unwrap_or("").to_string();
    Some(json!({
        "index": index,
        "worktree": worktree,
        "path": path,
    }))
}

fn repo_path(ctx: &Ctx, input: &Value, write: bool) -> Result<PathBuf, ToolError> {
    let raw = input.get("repo").and_then(Value::as_str).unwrap_or(".");
    let path = absolutize(Path::new(raw))?;
    let allowed = if write {
        ctx.policy.can_write(&path)
    } else {
        ctx.policy.can_read(&path)
    };
    if !allowed {
        let op = if write { "write" } else { "read" };
        return Err(ToolError::PermissionDenied(format!(
            "git {op} outside scope: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn absolutize(path: &Path) -> Result<PathBuf, ToolError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| ToolError::Execution(e.to_string()))?
            .join(path)
    };
    joined
        .canonicalize()
        .map_err(|e| ToolError::Execution(format!("resolve {}: {e}", joined.display())))
}

async fn run_git(repo: &Path, args: &[&str]) -> Result<String, ToolError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| ToolError::Execution(format!("run git: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(ToolError::Execution(format!(
            "git {} failed with {}:\n{}{}",
            args.join(" "),
            output.status,
            stdout,
            stderr
        )))
    }
}

fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{key}'")))
}

fn string_array(input: &Value, key: &str) -> Result<Option<Vec<String>>, ToolError> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| ToolError::InvalidInput(format!("'{key}' must be an array")))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ToolError::InvalidInput(format!("'{key}' entries must be strings")))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn render_json(value: Value) -> Result<String, ToolError> {
    serde_json::to_string_pretty(&value).map_err(|e| ToolError::Execution(e.to_string()))
}

fn sha256_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_bytes(&bytes)
}

fn sha256_str(value: &str) -> String {
    sha256_bytes(value.as_bytes())
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(value);
    format!("{:x}", h.finalize())
}
