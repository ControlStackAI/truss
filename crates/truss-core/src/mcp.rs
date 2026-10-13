//! MCP client support for mounting external tools into Truss.
//!
//! MCP tools are still Truss tools once mounted: the model sees a normal
//! [`crate::provider::ToolDef`], calls route through [`crate::ctx::Ctx`] and
//! [`crate::policy::Policy`], and durable runs can record a signed
//! `mcp_tool_call` provenance entry alongside the replayable `tool_result`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::memory::LedgerRef;
use crate::provider::ToolDef;
use crate::store::{tool_call_key, Store};
use crate::tools::{Tool, ToolRegistry};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: McpTransport,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Static HTTP headers. Use only for non-secret metadata.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// HTTP headers populated from environment variables. Values are never
    /// recorded in the signed ledger.
    #[serde(default)]
    pub env_headers: BTreeMap<String, String>,
    /// Optional remote MCP tool names to mount from this server.
    #[serde(default)]
    pub allow_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

#[derive(Clone)]
pub struct McpJournal {
    pub store: Store,
    pub session_id: String,
}

#[derive(Clone)]
pub struct McpMountedTool {
    server: McpServerConfig,
    remote_name: String,
    def: ToolDef,
    journal: Option<McpJournal>,
}

enum McpClient {
    Stdio(StdioMcpClient),
    #[cfg(feature = "mcp-http")]
    Http(HttpMcpClient),
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcResponse {
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolsList {
    #[serde(default)]
    tools: Vec<McpTool>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    input_schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ToolCallResult {
    #[serde(default)]
    content: Vec<McpContent>,
    #[serde(rename = "isError", default)]
    is_error: bool,
}

struct McpCallRecord<'a> {
    produced_by: Option<LedgerRef>,
    memo_key: &'a str,
    input_hash: &'a str,
    result: &'a str,
    is_error: bool,
    error: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum McpContent {
    Text {
        text: String,
    },
    Pending {
        signal: String,
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(other)]
    Other,
}

struct PendingSignal {
    signal: String,
    reason: String,
}

pub async fn mount_mcp_servers(
    registry: &mut ToolRegistry,
    ctx: &Ctx,
    servers: &[McpServerConfig],
    journal: Option<McpJournal>,
) -> Result<(), ToolError> {
    for server in servers {
        mount_one_server(registry, ctx, server, journal.clone()).await?;
    }
    Ok(())
}

async fn mount_one_server(
    registry: &mut ToolRegistry,
    ctx: &Ctx,
    server: &McpServerConfig,
    journal: Option<McpJournal>,
) -> Result<(), ToolError> {
    validate_server_name(&server.name)?;
    let mut client = McpClient::connect(ctx, server.clone()).await?;
    client.initialize(ctx).await?;
    let tools = client.list_tools(ctx).await?;
    let allowed = server.allow_tools.as_ref().map(|tools| {
        tools
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>()
    });
    for tool in tools {
        if allowed
            .as_ref()
            .map(|set| set.contains(&tool.name))
            .unwrap_or(true)
        {
            registry.register(Arc::new(McpMountedTool::new(
                server.clone(),
                tool,
                journal.clone(),
            )?));
        }
    }
    Ok(())
}

impl McpMountedTool {
    fn new(
        server: McpServerConfig,
        remote_tool: McpTool,
        journal: Option<McpJournal>,
    ) -> Result<Self, ToolError> {
        validate_tool_name(&remote_tool.name)?;
        let mounted_name = mounted_tool_name(&server.name, &remote_tool.name);
        let description = remote_tool.description.unwrap_or_else(|| {
            format!(
                "MCP tool '{}' from server '{}'",
                remote_tool.name, server.name
            )
        });
        let input_schema = remote_tool
            .input_schema
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        Ok(Self {
            server,
            remote_name: remote_tool.name,
            def: ToolDef {
                name: mounted_name,
                description,
                input_schema,
            },
            journal,
        })
    }
}

#[async_trait]
impl Tool for McpMountedTool {
    fn def(&self) -> ToolDef {
        self.def.clone()
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

impl McpMountedTool {
    async fn call_inner(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let input_hash = sha256_json(&input);
        let memo_key = tool_call_key(&self.def.name, &input);
        if let Some(journal) = &self.journal {
            if let Some((content, is_error)) = journal
                .store
                .memo_get(&journal.session_id, &memo_key)
                .map_err(|e| ToolError::Execution(format!("read MCP memo: {e}")))?
            {
                self.verify_memo(journal, &memo_key, &input_hash, &content, is_error)?;
                if is_error {
                    return Err(ToolError::Execution(content));
                }
                return Ok(content);
            }
        }

        let mut client = McpClient::connect(ctx, self.server.clone()).await?;
        client.initialize(ctx).await?;
        let result = client
            .call_tool(ctx, &self.remote_name, input.clone())
            .await;
        match result {
            Ok(call_result) => {
                if let Some(pending) = pending_signal(&call_result) {
                    let pending_body =
                        serde_json::to_string(&call_result).unwrap_or_else(|_| String::new());
                    let pending_ref = self.record_pending_signal(
                        ctx,
                        produced_by,
                        &memo_key,
                        &input_hash,
                        &pending_body,
                        &pending,
                    )?;
                    return Err(ToolError::Suspend {
                        signal_name: pending.signal,
                        reason: pending.reason,
                        resume_payload: json!({
                            "kind": "mcp_pending",
                            "server": self.server.name,
                            "transport": self.server.transport,
                            "remote_tool": self.remote_name,
                            "mounted_tool": self.def.name,
                            "memo_key": memo_key,
                            "input_sha256": input_hash,
                            "pending_sha256": sha256_str(&pending_body),
                            "pending_ref": pending_ref,
                        }),
                    });
                }
                let text = render_call_result(&call_result);
                self.record_provenance(
                    ctx,
                    McpCallRecord {
                        produced_by,
                        memo_key: &memo_key,
                        input_hash: &input_hash,
                        result: &text,
                        is_error: call_result.is_error,
                        error: None,
                    },
                )?;
                if call_result.is_error {
                    Err(ToolError::Execution(text))
                } else {
                    Ok(text)
                }
            }
            Err(err) => {
                let err_text = err.to_string();
                self.record_provenance(
                    ctx,
                    McpCallRecord {
                        produced_by,
                        memo_key: &memo_key,
                        input_hash: &input_hash,
                        result: &err_text,
                        is_error: true,
                        error: Some(&err_text),
                    },
                )?;
                Err(err)
            }
        }
    }

    fn record_provenance(&self, ctx: &Ctx, record: McpCallRecord<'_>) -> Result<(), ToolError> {
        if let Some(journal) = &self.journal {
            ctx.check().map_err(|_| ToolError::Cancelled)?;
            journal
                .store
                .append_with_memo(
                    &journal.session_id,
                    "mcp_tool_call",
                    &json!({
                        "server": self.server.name,
                        "transport": self.server.transport,
                        "remote_tool": self.remote_name,
                        "mounted_tool": self.def.name,
                        "memo_key": record.memo_key,
                        "input_sha256": record.input_hash,
                        "result_sha256": sha256_str(record.result),
                        "is_error": record.is_error,
                        "produced_by": record.produced_by,
                        "error": record.error,
                    }),
                    record.memo_key,
                    record.result,
                    record.is_error,
                )
                .map_err(|e| ToolError::Execution(format!("record mcp provenance: {e}")))?;
        }
        Ok(())
    }

    fn record_pending_signal(
        &self,
        ctx: &Ctx,
        produced_by: Option<LedgerRef>,
        memo_key: &str,
        input_hash: &str,
        pending_body: &str,
        pending: &PendingSignal,
    ) -> Result<Option<LedgerRef>, ToolError> {
        let Some(journal) = &self.journal else {
            return Ok(None);
        };
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        let entry = journal
            .store
            .append(
                &journal.session_id,
                "mcp_tool_pending",
                &json!({
                    "server": self.server.name,
                    "transport": self.server.transport,
                    "remote_tool": self.remote_name,
                    "mounted_tool": self.def.name,
                    "memo_key": memo_key,
                    "input_sha256": input_hash,
                    "pending_sha256": sha256_str(pending_body),
                    "signal_name": pending.signal,
                    "reason": pending.reason,
                    "produced_by": produced_by,
                }),
            )
            .map_err(|e| ToolError::Execution(format!("record mcp pending provenance: {e}")))?;
        Ok(Some(LedgerRef {
            session: journal.session_id.clone(),
            seq: entry.seq,
        }))
    }

    fn verify_memo(
        &self,
        journal: &McpJournal,
        memo_key: &str,
        input_hash: &str,
        result: &str,
        is_error: bool,
    ) -> Result<(), ToolError> {
        let result_hash = sha256_str(result);
        let entries = journal
            .store
            .entries(&journal.session_id)
            .map_err(|e| ToolError::Execution(format!("read MCP provenance: {e}")))?;
        let verified = entries.iter().rev().any(|entry| {
            entry.kind == "mcp_tool_call"
                && entry.payload["server"].as_str() == Some(self.server.name.as_str())
                && entry.payload["transport"]
                    == serde_json::to_value(self.server.transport).unwrap_or(Value::Null)
                && entry.payload["remote_tool"].as_str() == Some(self.remote_name.as_str())
                && entry.payload["mounted_tool"].as_str() == Some(self.def.name.as_str())
                && entry.payload["memo_key"].as_str() == Some(memo_key)
                && entry.payload["input_sha256"].as_str() == Some(input_hash)
                && entry.payload["result_sha256"].as_str() == Some(result_hash.as_str())
                && entry.payload["is_error"].as_bool() == Some(is_error)
        });
        if verified {
            Ok(())
        } else {
            Err(ToolError::Execution(
                "MCP memo missing matching signed provenance".into(),
            ))
        }
    }
}

pub fn record_pending_resume(
    store: &Store,
    session_id: &str,
    metadata: &Value,
    result: &str,
    is_error: bool,
) -> Result<(), ToolError> {
    if metadata["kind"].as_str() != Some("mcp_pending") {
        return Ok(());
    }
    let server = metadata["server"]
        .as_str()
        .ok_or_else(|| ToolError::Execution("MCP pending metadata missing server".into()))?;
    let remote_tool = metadata["remote_tool"]
        .as_str()
        .ok_or_else(|| ToolError::Execution("MCP pending metadata missing remote_tool".into()))?;
    let mounted_tool = metadata["mounted_tool"]
        .as_str()
        .ok_or_else(|| ToolError::Execution("MCP pending metadata missing mounted_tool".into()))?;
    let memo_key = metadata["memo_key"]
        .as_str()
        .ok_or_else(|| ToolError::Execution("MCP pending metadata missing memo_key".into()))?;
    let input_hash = metadata["input_sha256"]
        .as_str()
        .ok_or_else(|| ToolError::Execution("MCP pending metadata missing input_sha256".into()))?;
    store
        .append_with_memo(
            session_id,
            "mcp_tool_call",
            &json!({
                "server": server,
                "transport": metadata["transport"].clone(),
                "remote_tool": remote_tool,
                "mounted_tool": mounted_tool,
                "memo_key": memo_key,
                "input_sha256": input_hash,
                "result_sha256": sha256_str(result),
                "is_error": is_error,
                "produced_by": metadata["pending_ref"].clone(),
                "pending_sha256": metadata["pending_sha256"].clone(),
                "completed_from_signal": true,
                "error": if is_error { Some(result) } else { None },
            }),
            memo_key,
            result,
            is_error,
        )
        .map_err(|e| ToolError::Execution(format!("record mcp pending resume: {e}")))?;
    Ok(())
}

impl McpClient {
    async fn connect(ctx: &Ctx, server: McpServerConfig) -> Result<Self, ToolError> {
        match server.transport {
            McpTransport::Stdio => StdioMcpClient::connect(server).await.map(Self::Stdio),
            McpTransport::Http => connect_http_client(ctx, server).await,
        }
    }

    async fn initialize(&mut self, ctx: &Ctx) -> Result<(), ToolError> {
        self.request(
            ctx,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "truss",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
        .await?;
        self.notify(ctx, "notifications/initialized", json!({}))
            .await?;
        Ok(())
    }

    async fn list_tools(&mut self, ctx: &Ctx) -> Result<Vec<McpTool>, ToolError> {
        let value = self.request(ctx, "tools/list", json!({})).await?;
        let list: ToolsList = serde_json::from_value(value)
            .map_err(|e| ToolError::Execution(format!("decode MCP tools/list: {e}")))?;
        Ok(list.tools)
    }

    async fn call_tool(
        &mut self,
        ctx: &Ctx,
        name: &str,
        arguments: Value,
    ) -> Result<ToolCallResult, ToolError> {
        let value = self
            .request(
                ctx,
                "tools/call",
                json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|e| ToolError::Execution(format!("decode MCP tools/call: {e}")))
    }

    async fn request(
        &mut self,
        ctx: &Ctx,
        method: &str,
        params: Value,
    ) -> Result<Value, ToolError> {
        let id = self.next_id();
        let response = self
            .request_json(
                ctx,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }),
            )
            .await?;
        if response.id != Some(id) {
            return Err(ToolError::Execution(format!(
                "MCP response id mismatch: expected {id}, got {:?}",
                response.id
            )));
        }
        if let Some(err) = response.error {
            return Err(ToolError::Execution(format!(
                "MCP error {}: {}",
                err.code, err.message
            )));
        }
        response
            .result
            .ok_or_else(|| ToolError::Execution("MCP response missing result".into()))
    }

    async fn notify(&mut self, ctx: &Ctx, method: &str, params: Value) -> Result<(), ToolError> {
        self.notify_json(
            ctx,
            json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
        )
        .await
    }

    fn next_id(&mut self) -> u64 {
        match self {
            McpClient::Stdio(client) => client.next_id(),
            #[cfg(feature = "mcp-http")]
            McpClient::Http(client) => client.next_id(),
        }
    }

    async fn request_json(
        &mut self,
        ctx: &Ctx,
        value: Value,
    ) -> Result<JsonRpcResponse, ToolError> {
        match self {
            McpClient::Stdio(client) => client.request_json(ctx, value).await,
            #[cfg(feature = "mcp-http")]
            McpClient::Http(client) => client.request_json(ctx, value).await,
        }
    }

    async fn notify_json(&mut self, ctx: &Ctx, value: Value) -> Result<(), ToolError> {
        match self {
            McpClient::Stdio(client) => client.notify_json(ctx, value).await,
            #[cfg(feature = "mcp-http")]
            McpClient::Http(client) => client.notify_json(ctx, value).await,
        }
    }
}

async fn connect_http_client(ctx: &Ctx, server: McpServerConfig) -> Result<McpClient, ToolError> {
    #[cfg(feature = "mcp-http")]
    {
        HttpMcpClient::connect(ctx, server)
            .await
            .map(McpClient::Http)
    }
    #[cfg(not(feature = "mcp-http"))]
    {
        let _ = ctx;
        let _ = server;
        Err(ToolError::Execution(
            "MCP HTTP transport requires the 'mcp-http' feature".into(),
        ))
    }
}

struct StdioMcpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl StdioMcpClient {
    async fn connect(server: McpServerConfig) -> Result<Self, ToolError> {
        let command_name = server.command.as_deref().ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "MCP stdio server '{}' requires command",
                server.name
            ))
        })?;
        let mut command = Command::new(command_name);
        command.args(&server.args);
        if let Some(cwd) = &server.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &server.env {
            command.env(key, value);
        }
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::null());
        command.kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            ToolError::Execution(format!("spawn MCP server '{}': {e}", server.name))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Execution("MCP server stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("MCP server stdout unavailable".into()))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn request_json(
        &mut self,
        ctx: &Ctx,
        value: Value,
    ) -> Result<JsonRpcResponse, ToolError> {
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        self.write_message(&value).await?;
        self.read_response(ctx).await
    }

    async fn notify_json(&mut self, ctx: &Ctx, value: Value) -> Result<(), ToolError> {
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        self.write_message(&value).await
    }

    async fn write_message(&mut self, value: &Value) -> Result<(), ToolError> {
        let body = serde_json::to_vec(value)
            .map_err(|e| ToolError::Execution(format!("encode MCP message: {e}")))?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| ToolError::Execution(format!("write MCP header: {e}")))?;
        self.stdin
            .write_all(&body)
            .await
            .map_err(|e| ToolError::Execution(format!("write MCP body: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| ToolError::Execution(format!("flush MCP message: {e}")))
    }

    async fn read_response(&mut self, ctx: &Ctx) -> Result<JsonRpcResponse, ToolError> {
        let token = ctx.cancel_token();
        if let Some(remaining) = ctx.remaining() {
            tokio::select! {
                _ = token.cancelled() => Err(ToolError::Cancelled),
                result = tokio::time::timeout(remaining, read_framed_json(&mut self.stdout)) => {
                    result
                        .map_err(|_| ToolError::Cancelled)?
                        .and_then(decode_response)
                }
            }
        } else {
            tokio::select! {
                _ = token.cancelled() => Err(ToolError::Cancelled),
                result = read_framed_json(&mut self.stdout) => result.and_then(decode_response),
            }
        }
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[cfg(feature = "mcp-http")]
struct HttpMcpClient {
    name: String,
    url: reqwest::Url,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    next_id: u64,
}

#[cfg(feature = "mcp-http")]
impl HttpMcpClient {
    async fn connect(ctx: &Ctx, server: McpServerConfig) -> Result<Self, ToolError> {
        let url = server.url.as_deref().ok_or_else(|| {
            ToolError::InvalidInput(format!("MCP HTTP server '{}' requires url", server.name))
        })?;
        let url = reqwest::Url::parse(url)
            .map_err(|e| ToolError::InvalidInput(format!("invalid MCP HTTP url: {e}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| ToolError::InvalidInput("MCP HTTP url missing host".into()))?;
        if !ctx.policy.host_allowed(host) {
            return Err(ToolError::PermissionDenied(format!(
                "network host not allowed: {host}"
            )));
        }
        let headers = resolve_http_headers(&server)?;
        Ok(Self {
            name: server.name,
            url,
            client: reqwest::Client::new(),
            headers,
            next_id: 1,
        })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn request_json(
        &mut self,
        ctx: &Ctx,
        value: Value,
    ) -> Result<JsonRpcResponse, ToolError> {
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        let request = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&value);
        let response = await_http(ctx, request.send()).await?;
        let status = response.status();
        if !status.is_success() {
            let body = await_http(ctx, response.text()).await.unwrap_or_default();
            return Err(ToolError::Execution(format!(
                "MCP HTTP server '{}' returned {}: {}",
                self.name, status, body
            )));
        }
        let value = await_http(ctx, response.json::<Value>()).await?;
        decode_response(value)
    }

    async fn notify_json(&mut self, ctx: &Ctx, value: Value) -> Result<(), ToolError> {
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        let request = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&value);
        let response = await_http(ctx, request.send()).await?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = await_http(ctx, response.text()).await.unwrap_or_default();
            Err(ToolError::Execution(format!(
                "MCP HTTP notification to '{}' returned {}: {}",
                self.name, status, body
            )))
        }
    }
}

#[cfg(feature = "mcp-http")]
fn resolve_http_headers(server: &McpServerConfig) -> Result<reqwest::header::HeaderMap, ToolError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (key, value) in &server.headers {
        let name = header_name(key)?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
            ToolError::InvalidInput(format!("invalid MCP HTTP header value for {key}: {e}"))
        })?;
        headers.insert(name, value);
    }
    for (key, env_var) in &server.env_headers {
        let value = std::env::var(env_var).map_err(|_| {
            ToolError::InvalidInput(format!(
                "missing env var {env_var} for MCP HTTP header {key}"
            ))
        })?;
        let name = header_name(key)?;
        let value = reqwest::header::HeaderValue::from_str(&value).map_err(|e| {
            ToolError::InvalidInput(format!(
                "invalid env-provided MCP HTTP header value for {key}: {e}"
            ))
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

#[cfg(feature = "mcp-http")]
fn header_name(key: &str) -> Result<reqwest::header::HeaderName, ToolError> {
    reqwest::header::HeaderName::from_bytes(key.as_bytes())
        .map_err(|e| ToolError::InvalidInput(format!("invalid MCP HTTP header name {key}: {e}")))
}

#[cfg(feature = "mcp-http")]
async fn await_http<T>(
    ctx: &Ctx,
    future: impl std::future::Future<Output = Result<T, reqwest::Error>>,
) -> Result<T, ToolError> {
    let token = ctx.cancel_token();
    if let Some(remaining) = ctx.remaining() {
        tokio::select! {
            _ = token.cancelled() => Err(ToolError::Cancelled),
            result = tokio::time::timeout(remaining, future) => {
                result
                    .map_err(|_| ToolError::Cancelled)?
                    .map_err(|e| ToolError::Execution(format!("MCP HTTP transport: {e}")))
            }
        }
    } else {
        tokio::select! {
            _ = token.cancelled() => Err(ToolError::Cancelled),
            result = future => result.map_err(|e| ToolError::Execution(format!("MCP HTTP transport: {e}"))),
        }
    }
}

async fn read_framed_json(stdout: &mut BufReader<ChildStdout>) -> Result<Value, ToolError> {
    let mut content_len = None;
    loop {
        let mut line = Vec::new();
        let n = stdout
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| ToolError::Execution(format!("read MCP header: {e}")))?;
        if n == 0 {
            return Err(ToolError::Execution("MCP server closed stdout".into()));
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            let len = value
                .trim()
                .parse::<usize>()
                .map_err(|e| ToolError::Execution(format!("invalid MCP Content-Length: {e}")))?;
            content_len = Some(len);
        }
    }
    let len =
        content_len.ok_or_else(|| ToolError::Execution("missing MCP Content-Length".into()))?;
    let mut body = vec![0u8; len];
    stdout
        .read_exact(&mut body)
        .await
        .map_err(|e| ToolError::Execution(format!("read MCP body: {e}")))?;
    serde_json::from_slice(&body).map_err(|e| ToolError::Execution(format!("decode MCP JSON: {e}")))
}

fn decode_response(value: Value) -> Result<JsonRpcResponse, ToolError> {
    serde_json::from_value(value)
        .map_err(|e| ToolError::Execution(format!("decode MCP response: {e}")))
}

fn render_call_result(result: &ToolCallResult) -> String {
    let texts: Vec<String> = result
        .content
        .iter()
        .filter_map(|content| match content {
            McpContent::Text { text } => Some(text.clone()),
            McpContent::Pending { .. } => None,
            McpContent::Other => None,
        })
        .collect();
    if texts.is_empty() {
        serde_json::to_string(result).unwrap_or_else(|_| String::new())
    } else {
        texts.join("\n")
    }
}

fn pending_signal(result: &ToolCallResult) -> Option<PendingSignal> {
    result.content.iter().find_map(|content| match content {
        McpContent::Pending { signal, reason } => Some(PendingSignal {
            signal: signal.clone(),
            reason: reason.clone().unwrap_or_else(|| "MCP tool pending".into()),
        }),
        _ => None,
    })
}

fn mounted_tool_name(server: &str, tool: &str) -> String {
    format!("mcp:{server}:{tool}")
}

fn validate_server_name(name: &str) -> Result<(), ToolError> {
    validate_namespace_part("server", name)
}

fn validate_tool_name(name: &str) -> Result<(), ToolError> {
    validate_namespace_part("tool", name)
}

fn validate_namespace_part(kind: &str, name: &str) -> Result<(), ToolError> {
    if name.is_empty()
        || name
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
    {
        return Err(ToolError::InvalidInput(format!(
            "MCP {kind} name {name:?} must use only ASCII letters, numbers, '_' or '-'"
        )));
    }
    Ok(())
}

fn sha256_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))
}

fn sha256_str(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
