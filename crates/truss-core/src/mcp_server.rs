//! MCP server support for exposing Truss as a contract-level verification backend.
//!
//! This is deliberately separate from [`crate::mcp`], which is the MCP *client*
//! tier for consuming external tools. The server tier exposes only contract and
//! signed-ledger operations. It never advertises raw shell or filesystem tools.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

use crate::ctx::Principal;

const PROTOCOL_VERSION: &str = "2024-11-05";

pub const TOOL_RUN_CONTRACT: &str = "truss_run_contract";
pub const TOOL_CONTRACT_STATUS: &str = "truss_contract_status";
pub const TOOL_LEDGER_VERDICT: &str = "truss_ledger_verdict";
pub const TOOL_LEDGER_EXPORT: &str = "truss_ledger_export";

#[derive(Debug, Clone, Deserialize)]
pub struct RunContractRequest {
    /// Path to a Truss contract TOML file, resolved by the serving process.
    pub contract_path: String,
    /// Optional durable session override. When omitted the contract id is used.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Return a handle immediately while the server runs the contract in-process.
    #[serde(default, rename = "async", alias = "async_run")]
    pub async_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContractStatusRequest {
    pub handle: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LedgerReadRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerBundleRef {
    pub session_id: String,
    pub read_tool: String,
}

impl LedgerBundleRef {
    pub fn export(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            read_tool: TOOL_LEDGER_EXPORT.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunContractResponse {
    pub handle: String,
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_verdict: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_ref: Option<LedgerBundleRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractStatusResponse {
    pub handle: String,
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_verdict: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_ref: Option<LedgerBundleRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct McpServerError {
    pub code: i64,
    pub message: String,
}

impl McpServerError {
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for McpServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for McpServerError {}

#[async_trait]
pub trait TrussMcpBackend: Send + Sync + 'static {
    async fn run_contract(
        &self,
        principal: &Principal,
        request: RunContractRequest,
    ) -> Result<RunContractResponse, McpServerError>;

    async fn contract_status(
        &self,
        principal: &Principal,
        request: ContractStatusRequest,
    ) -> Result<ContractStatusResponse, McpServerError>;

    async fn ledger_verdict(
        &self,
        principal: &Principal,
        request: LedgerReadRequest,
    ) -> Result<Value, McpServerError>;

    async fn ledger_export(
        &self,
        principal: &Principal,
        request: LedgerReadRequest,
    ) -> Result<Value, McpServerError>;
}

pub struct TrussMcpServer<B> {
    backend: Arc<B>,
    principal: Principal,
}

impl<B> TrussMcpServer<B>
where
    B: TrussMcpBackend,
{
    pub fn new(backend: Arc<B>, principal: Principal) -> Self {
        Self { backend, principal }
    }

    pub async fn serve_stdio<S>(&self, stream: S) -> Result<(), McpServerError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (read, mut write) = tokio::io::split(stream);
        let mut read = BufReader::new(read);
        loop {
            let value = match read_framed_json(&mut read).await {
                Ok(value) => value,
                Err(ReadFrameError::Eof) => return Ok(()),
                Err(ReadFrameError::Invalid(message)) => {
                    write_json(&mut write, &jsonrpc_error(None, -32700, message)).await?;
                    continue;
                }
            };
            let Some(response) = self.handle_message(value).await else {
                continue;
            };
            write_json(&mut write, &response).await?;
        }
    }

    async fn handle_message(&self, value: Value) -> Option<Value> {
        let id = value.get("id").cloned();
        let method = value.get("method").and_then(Value::as_str);
        match method {
            Some("notifications/initialized") => None,
            Some("initialize") => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "truss",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            })),
            Some("tools/list") => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_definitions() }
            })),
            Some("tools/call") => Some(
                self.handle_tool_call(id, value.get("params").cloned())
                    .await,
            ),
            Some(other) => Some(jsonrpc_error(
                id,
                -32601,
                format!("unsupported MCP method '{other}'"),
            )),
            None => Some(jsonrpc_error(id, -32600, "missing JSON-RPC method")),
        }
    }

    async fn handle_tool_call(&self, id: Option<Value>, params: Option<Value>) -> Value {
        let params = match params {
            Some(params) => params,
            None => return jsonrpc_error(id, -32602, "tools/call missing params"),
        };
        let name = match params.get("name").and_then(Value::as_str) {
            Some(name) => name,
            None => return jsonrpc_error(id, -32602, "tools/call missing tool name"),
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let result = match name {
            TOOL_RUN_CONTRACT => match parse_args(args) {
                Ok(request) => self
                    .backend
                    .run_contract(&self.principal, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            },
            TOOL_CONTRACT_STATUS => match parse_args(args) {
                Ok(request) => self
                    .backend
                    .contract_status(&self.principal, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            },
            TOOL_LEDGER_VERDICT => match parse_args(args) {
                Ok(request) => self.backend.ledger_verdict(&self.principal, request).await,
                Err(err) => Err(err),
            },
            TOOL_LEDGER_EXPORT => match parse_args(args) {
                Ok(request) => self.backend.ledger_export(&self.principal, request).await,
                Err(err) => Err(err),
            },
            other => Err(McpServerError::invalid_input(format!(
                "unknown Truss MCP tool '{other}'"
            ))),
        };
        match result {
            Ok(value) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": render_json(&value) }],
                    "isError": false
                }
            }),
            Err(err) => jsonrpc_error(id, err.code, err.message),
        }
    }
}

fn parse_args<T>(value: Value) -> Result<T, McpServerError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(value)
        .map_err(|e| McpServerError::invalid_input(format!("invalid tool arguments: {e}")))
}

fn to_value<T: Serialize>(value: T) -> Result<Value, McpServerError> {
    serde_json::to_value(value)
        .map_err(|e| McpServerError::internal(format!("encode tool result: {e}")))
}

fn render_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}

fn jsonrpc_error(id: Option<Value>, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool_def(
            TOOL_RUN_CONTRACT,
            "Run a Truss contract and return the signed verdict head plus a ledger bundle reference.",
            json!({
                "type": "object",
                "properties": {
                    "contract_path": { "type": "string" },
                    "session_id": { "type": "string" },
                    "async": { "type": "boolean", "description": "Return a handle immediately and poll with truss_contract_status." }
                },
                "required": ["contract_path"]
            }),
        ),
        tool_def(
            TOOL_CONTRACT_STATUS,
            "Read status for a delegated Truss contract handle.",
            json!({
                "type": "object",
                "properties": { "handle": { "type": "string" } },
                "required": ["handle"]
            }),
        ),
        tool_def(
            TOOL_LEDGER_VERDICT,
            "Read the signed verdict bundle for a Truss ledger session.",
            ledger_read_schema(),
        ),
        tool_def(
            TOOL_LEDGER_EXPORT,
            "Export a portable read-only signed ledger bundle for offline verification.",
            ledger_read_schema(),
        ),
    ]
}

fn tool_def(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

fn ledger_read_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "session_id": { "type": "string" } },
        "required": ["session_id"]
    })
}

enum ReadFrameError {
    Eof,
    Invalid(String),
}

async fn read_framed_json<R>(read: &mut R) -> Result<Value, ReadFrameError>
where
    R: AsyncBufRead + Unpin,
{
    let mut headers = BTreeMap::new();
    loop {
        let mut line = Vec::new();
        let n = read
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| ReadFrameError::Invalid(format!("read MCP header: {e}")))?;
        if n == 0 {
            return Err(ReadFrameError::Eof);
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            headers.insert(key.to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let len = headers
        .get("content-length")
        .ok_or_else(|| ReadFrameError::Invalid("missing MCP Content-Length".into()))?
        .parse::<usize>()
        .map_err(|e| ReadFrameError::Invalid(format!("invalid MCP Content-Length: {e}")))?;
    let mut body = vec![0u8; len];
    read.read_exact(&mut body)
        .await
        .map_err(|e| ReadFrameError::Invalid(format!("read MCP body: {e}")))?;
    serde_json::from_slice(&body)
        .map_err(|e| ReadFrameError::Invalid(format!("decode MCP JSON: {e}")))
}

async fn write_json<W>(write: &mut W, value: &Value) -> Result<(), McpServerError>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value)
        .map_err(|e| McpServerError::internal(format!("encode MCP message: {e}")))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    write
        .write_all(header.as_bytes())
        .await
        .map_err(|e| McpServerError::internal(format!("write MCP header: {e}")))?;
    write
        .write_all(&body)
        .await
        .map_err(|e| McpServerError::internal(format!("write MCP body: {e}")))?;
    write
        .flush()
        .await
        .map_err(|e| McpServerError::internal(format!("flush MCP message: {e}")))
}
