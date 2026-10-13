//! Native GitHub tools backed by the `gh` CLI.
//!
//! `gh` remains the transport/auth adapter, but Truss exposes typed tools and
//! records signed hashes of GitHub content that can support a verdict.

use std::process::Stdio;

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::memory::LedgerRef;
use crate::policy::NetworkPolicy;
use crate::provider::ToolDef;
use crate::store::{tool_call_key, Store};

use super::Tool;

#[derive(Clone)]
pub struct GitHubJournal {
    pub store: Store,
    pub session_id: String,
}

#[derive(Clone, Copy)]
enum GitHubToolKind {
    PrView,
    PrCreate,
    PrDiff,
    PrChecks,
    IssueView,
    IssueList,
    CheckStatus,
}

pub struct GitHubTool {
    kind: GitHubToolKind,
    journal: Option<GitHubJournal>,
}

struct GitHubCallRecord<'a> {
    produced_by: Option<LedgerRef>,
    memo_key: &'a str,
    input_hash: &'a str,
    result: &'a str,
    is_error: bool,
    side_effect: bool,
    error: Option<&'a str>,
}

impl GitHubTool {
    fn new(kind: GitHubToolKind, journal: Option<GitHubJournal>) -> Self {
        Self { kind, journal }
    }

    pub fn pr_view(journal: Option<GitHubJournal>) -> Self {
        Self::new(GitHubToolKind::PrView, journal)
    }

    pub fn pr_create(journal: Option<GitHubJournal>) -> Self {
        Self::new(GitHubToolKind::PrCreate, journal)
    }

    pub fn pr_diff(journal: Option<GitHubJournal>) -> Self {
        Self::new(GitHubToolKind::PrDiff, journal)
    }

    pub fn pr_checks(journal: Option<GitHubJournal>) -> Self {
        Self::new(GitHubToolKind::PrChecks, journal)
    }

    pub fn issue_view(journal: Option<GitHubJournal>) -> Self {
        Self::new(GitHubToolKind::IssueView, journal)
    }

    pub fn issue_list(journal: Option<GitHubJournal>) -> Self {
        Self::new(GitHubToolKind::IssueList, journal)
    }

    pub fn check_status(journal: Option<GitHubJournal>) -> Self {
        Self::new(GitHubToolKind::CheckStatus, journal)
    }

    fn name(&self) -> &'static str {
        match self.kind {
            GitHubToolKind::PrView => "github_pr_view",
            GitHubToolKind::PrCreate => "github_pr_create",
            GitHubToolKind::PrDiff => "github_pr_diff",
            GitHubToolKind::PrChecks => "github_pr_checks",
            GitHubToolKind::IssueView => "github_issue_view",
            GitHubToolKind::IssueList => "github_issue_list",
            GitHubToolKind::CheckStatus => "github_check_status",
        }
    }

    fn is_side_effect(&self) -> bool {
        matches!(self.kind, GitHubToolKind::PrCreate)
    }

    async fn call_inner(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        ensure_github_network(ctx)?;
        let input_hash = sha256_json(&input);
        let memo_key = tool_call_key(self.name(), &input);
        let side_effect = self.is_side_effect();

        if let Some(journal) = &self.journal {
            if let Some((content, is_error)) = journal
                .store
                .memo_get(&journal.session_id, &memo_key)
                .map_err(|e| ToolError::Execution(format!("read GitHub memo: {e}")))?
            {
                self.verify_memo(journal, &memo_key, &input_hash, &content, is_error)?;
                if is_error {
                    return Err(ToolError::Execution(content));
                }
                return Ok(content);
            }
        }

        let result = match self.kind {
            GitHubToolKind::PrView => github_pr_view(&input).await,
            GitHubToolKind::PrCreate => github_pr_create(&input).await,
            GitHubToolKind::PrDiff => github_pr_diff(&input).await,
            GitHubToolKind::PrChecks => github_pr_checks(&input).await,
            GitHubToolKind::IssueView => github_issue_view(&input).await,
            GitHubToolKind::IssueList => github_issue_list(&input).await,
            GitHubToolKind::CheckStatus => github_check_status(&input).await,
        };

        match result {
            Ok(out) => {
                self.record_provenance(
                    ctx,
                    GitHubCallRecord {
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
                    GitHubCallRecord {
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

    fn record_provenance(&self, ctx: &Ctx, record: GitHubCallRecord<'_>) -> Result<(), ToolError> {
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
                    "github_tool_call",
                    &payload,
                    record.memo_key,
                    record.result,
                    record.is_error,
                )
                .map_err(|e| ToolError::Execution(format!("record GitHub provenance: {e}")))?;
        } else {
            journal
                .store
                .append(&journal.session_id, "github_tool_call", &payload)
                .map_err(|e| ToolError::Execution(format!("record GitHub provenance: {e}")))?;
        }
        Ok(())
    }

    fn verify_memo(
        &self,
        journal: &GitHubJournal,
        memo_key: &str,
        input_hash: &str,
        content: &str,
        is_error: bool,
    ) -> Result<(), ToolError> {
        let result_hash = sha256_str(content);
        let entries = journal
            .store
            .entries(&journal.session_id)
            .map_err(|e| ToolError::Execution(format!("read GitHub provenance: {e}")))?;
        let verified = entries.iter().rev().any(|entry| {
            entry.kind == "github_tool_call"
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
                "GitHub memo missing matching signed provenance".into(),
            ))
        }
    }
}

#[async_trait]
impl Tool for GitHubTool {
    fn def(&self) -> ToolDef {
        match self.kind {
            GitHubToolKind::PrView => ToolDef {
                name: self.name().into(),
                description: "View a GitHub pull request as structured JSON.".into(),
                input_schema: pr_number_schema(),
            },
            GitHubToolKind::PrCreate => ToolDef {
                name: self.name().into(),
                description: "Create a GitHub pull request. In durable runs, the result is memoized so resume does not create a duplicate PR.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository in owner/name form." },
                        "title": { "type": "string" },
                        "body": { "type": "string" },
                        "base": { "type": "string" },
                        "head": { "type": "string" },
                        "draft": { "type": "boolean", "description": "Create as draft. Defaults false." }
                    },
                    "required": ["title", "body"]
                }),
            },
            GitHubToolKind::PrDiff => ToolDef {
                name: self.name().into(),
                description: "Fetch a GitHub pull request diff and return it with a SHA-256 hash.".into(),
                input_schema: pr_number_schema(),
            },
            GitHubToolKind::PrChecks => ToolDef {
                name: self.name().into(),
                description: "Fetch GitHub pull request checks as structured JSON.".into(),
                input_schema: pr_number_schema(),
            },
            GitHubToolKind::IssueView => ToolDef {
                name: self.name().into(),
                description: "View a GitHub issue as structured JSON.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository in owner/name form." },
                        "issue": { "type": "integer" }
                    },
                    "required": ["issue"]
                }),
            },
            GitHubToolKind::IssueList => ToolDef {
                name: self.name().into(),
                description: "List GitHub issues as structured JSON.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository in owner/name form." },
                        "state": { "type": "string", "enum": ["open", "closed", "all"] },
                        "limit": { "type": "integer" }
                    }
                }),
            },
            GitHubToolKind::CheckStatus => ToolDef {
                name: self.name().into(),
                description: "Fetch combined status and check runs for a GitHub ref.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string", "description": "Repository in owner/name form." },
                        "ref": { "type": "string", "description": "Commit SHA, branch, or tag." }
                    },
                    "required": ["repo", "ref"]
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

fn pr_number_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repo": { "type": "string", "description": "Repository in owner/name form." },
            "pr": { "type": "integer", "description": "Pull request number." }
        },
        "required": ["pr"]
    })
}

async fn github_pr_view(input: &Value) -> Result<String, ToolError> {
    let mut args = vec![
        "pr".to_string(),
        "view".to_string(),
        required_i64(input, "pr")?.to_string(),
        "--json".to_string(),
        "number,title,body,state,url,headRefName,baseRefName,author,mergeStateStatus,reviewDecision,commits,files".to_string(),
    ];
    push_repo(input, &mut args);
    run_gh_json(&args).await
}

async fn github_pr_create(input: &Value) -> Result<String, ToolError> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--title".to_string(),
        required_str(input, "title")?,
        "--body".to_string(),
        required_str(input, "body")?,
    ];
    if let Some(base) = input.get("base").and_then(Value::as_str) {
        args.push("--base".into());
        args.push(base.into());
    }
    if let Some(head) = input.get("head").and_then(Value::as_str) {
        args.push("--head".into());
        args.push(head.into());
    }
    if input.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        args.push("--draft".into());
    }
    push_repo(input, &mut args);
    let url = run_gh_text(&args).await?.trim().to_string();
    if url.is_empty() {
        return Err(ToolError::Execution(
            "gh pr create did not return a pull request URL".into(),
        ));
    }
    run_gh_json(&[
        "pr".into(),
        "view".into(),
        url,
        "--json".into(),
        "number,url,state,title,headRefName,baseRefName".into(),
    ])
    .await
}

async fn github_pr_diff(input: &Value) -> Result<String, ToolError> {
    let mut args = vec![
        "pr".to_string(),
        "diff".to_string(),
        required_i64(input, "pr")?.to_string(),
    ];
    push_repo(input, &mut args);
    let diff = run_gh_text(&args).await?;
    render_json(json!({
        "pr": required_i64(input, "pr")?,
        "repo": input.get("repo").and_then(Value::as_str),
        "diff_sha256": sha256_str(&diff),
        "diff": diff,
    }))
}

async fn github_pr_checks(input: &Value) -> Result<String, ToolError> {
    let mut args = vec![
        "pr".to_string(),
        "checks".to_string(),
        required_i64(input, "pr")?.to_string(),
        "--json".to_string(),
        "bucket,completedAt,description,event,link,name,startedAt,state,workflow".to_string(),
    ];
    push_repo(input, &mut args);
    run_gh_json(&args).await
}

async fn github_issue_view(input: &Value) -> Result<String, ToolError> {
    let mut args = vec![
        "issue".to_string(),
        "view".to_string(),
        required_i64(input, "issue")?.to_string(),
        "--json".to_string(),
        "number,title,body,state,url,author,labels,comments".to_string(),
    ];
    push_repo(input, &mut args);
    run_gh_json(&args).await
}

async fn github_issue_list(input: &Value) -> Result<String, ToolError> {
    let mut args = vec![
        "issue".to_string(),
        "list".to_string(),
        "--state".to_string(),
        input
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("open")
            .to_string(),
        "--limit".to_string(),
        input
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(30)
            .max(1)
            .to_string(),
        "--json".to_string(),
        "number,title,state,url,author,labels,updatedAt".to_string(),
    ];
    push_repo(input, &mut args);
    run_gh_json(&args).await
}

async fn github_check_status(input: &Value) -> Result<String, ToolError> {
    let repo = required_str(input, "repo")?;
    let reference = required_str(input, "ref")?;
    let status = run_gh_json(&[
        "api".into(),
        format!("repos/{repo}/commits/{reference}/status"),
    ])
    .await?;
    let checks = run_gh_json(&[
        "api".into(),
        format!("repos/{repo}/commits/{reference}/check-runs"),
    ])
    .await?;
    let value = json!({
        "repo": repo,
        "ref": reference,
        "combined_status": serde_json::from_str::<Value>(&status).unwrap_or(Value::String(status)),
        "check_runs": serde_json::from_str::<Value>(&checks).unwrap_or(Value::String(checks)),
    });
    render_json(value)
}

fn push_repo(input: &Value, args: &mut Vec<String>) {
    if let Some(repo) = input.get("repo").and_then(Value::as_str) {
        args.push("--repo".into());
        args.push(repo.into());
    }
}

async fn run_gh_json(args: &[String]) -> Result<String, ToolError> {
    let text = run_gh_text(args).await?;
    let parsed = serde_json::from_str::<Value>(&text)
        .map_err(|e| ToolError::Execution(format!("gh returned non-json output: {e}: {text}")))?;
    render_json(json!({
        "content_sha256": sha256_str(&text),
        "content": parsed,
    }))
}

async fn run_gh_text(args: &[String]) -> Result<String, ToolError> {
    let output = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| ToolError::Execution(format!("run gh: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(ToolError::Execution(format!(
            "gh {} failed with {}:\n{}{}",
            args.join(" "),
            output.status,
            stdout,
            stderr
        )))
    }
}

fn ensure_github_network(ctx: &Ctx) -> Result<(), ToolError> {
    let allowed = match &ctx.policy.network {
        NetworkPolicy::None => false,
        _ => ctx.policy.host_allowed("github.com") || ctx.policy.host_allowed("api.github.com"),
    };
    if allowed {
        Ok(())
    } else {
        Err(ToolError::PermissionDenied(
            "GitHub tools require network policy allowing github.com/api.github.com".into(),
        ))
    }
}

fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{key}'")))
}

fn required_i64(input: &Value, key: &str) -> Result<i64, ToolError> {
    input
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{key}'")))
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
