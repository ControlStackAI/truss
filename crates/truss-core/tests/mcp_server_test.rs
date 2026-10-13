use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

use truss_core::ctx::Principal;
use truss_core::mcp_server::{
    ContractStatusRequest, ContractStatusResponse, LedgerBundleRef, LedgerReadRequest,
    McpServerError, RunContractRequest, RunContractResponse, TrussMcpBackend, TrussMcpServer,
    TOOL_CONTRACT_STATUS, TOOL_LEDGER_EXPORT, TOOL_LEDGER_VERDICT, TOOL_RUN_CONTRACT,
};

#[derive(Default)]
struct FakeBackend {
    sessions: Mutex<HashMap<String, Value>>,
}

#[async_trait]
impl TrussMcpBackend for FakeBackend {
    async fn run_contract(
        &self,
        principal: &Principal,
        request: RunContractRequest,
    ) -> Result<RunContractResponse, McpServerError> {
        let session_id = request
            .session_id
            .unwrap_or_else(|| "fake-contract-session".to_string());
        let signed_verdict = json!({
            "contract": { "id": session_id, "title": "Fake contract" },
            "overall": "PASS",
            "verification": { "status": "verified" },
            "head_hash": "abc123",
            "principal": { "tenant": principal.tenant, "user": principal.user },
            "contract_path": request.contract_path,
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), signed_verdict.clone());
        Ok(RunContractResponse {
            handle: session_id.clone(),
            session_id: session_id.clone(),
            status: "completed".to_string(),
            verdict: Some(json!({ "overall": "PASS" })),
            signed_verdict: Some(signed_verdict),
            head_hash: Some("abc123".to_string()),
            bundle_ref: Some(LedgerBundleRef::export(session_id)),
        })
    }

    async fn contract_status(
        &self,
        _principal: &Principal,
        request: ContractStatusRequest,
    ) -> Result<ContractStatusResponse, McpServerError> {
        let signed_verdict = self.sessions.lock().unwrap().get(&request.handle).cloned();
        Ok(ContractStatusResponse {
            handle: request.handle.clone(),
            session_id: request.handle.clone(),
            status: if signed_verdict.is_some() {
                "completed"
            } else {
                "unknown"
            }
            .to_string(),
            signed_verdict,
            head_hash: Some("abc123".to_string()),
            bundle_ref: Some(LedgerBundleRef::export(request.handle)),
            error: None,
        })
    }

    async fn ledger_verdict(
        &self,
        _principal: &Principal,
        request: LedgerReadRequest,
    ) -> Result<Value, McpServerError> {
        self.sessions
            .lock()
            .unwrap()
            .get(&request.session_id)
            .cloned()
            .ok_or_else(|| McpServerError::invalid_input("session not found"))
    }

    async fn ledger_export(
        &self,
        _principal: &Principal,
        request: LedgerReadRequest,
    ) -> Result<Value, McpServerError> {
        let verdict = self.ledger_verdict(_principal, request).await?;
        Ok(json!({
            "command": "export",
            "verdict": verdict,
            "offline_verification": { "status": "verified" },
        }))
    }
}

#[tokio::test]
async fn stdio_server_runs_contract_and_returns_signed_verdict_head() {
    let mut client = spawn_fake_server();
    initialize(&mut client).await;

    let response = call_tool(
        &mut client,
        2,
        TOOL_RUN_CONTRACT,
        json!({
            "contract_path": "fake-contract.toml",
            "session_id": "mcp-fake-run",
        }),
    )
    .await;
    let result = tool_text_json(&response);
    assert_eq!(result["status"], "completed");
    assert_eq!(result["session_id"], "mcp-fake-run");
    assert_eq!(result["head_hash"], "abc123");
    assert_eq!(result["signed_verdict"]["overall"], "PASS");
    assert_eq!(result["bundle_ref"]["read_tool"], TOOL_LEDGER_EXPORT);
}

#[tokio::test]
async fn ledger_export_reads_recorded_bundle() {
    let mut client = spawn_fake_server();
    initialize(&mut client).await;
    call_tool(
        &mut client,
        2,
        TOOL_RUN_CONTRACT,
        json!({
            "contract_path": "fake-contract.toml",
            "session_id": "ledger-export-run",
        }),
    )
    .await;

    let response = call_tool(
        &mut client,
        3,
        TOOL_LEDGER_EXPORT,
        json!({ "session_id": "ledger-export-run" }),
    )
    .await;
    let result = tool_text_json(&response);
    assert_eq!(result["command"], "export");
    assert_eq!(result["verdict"]["contract"]["id"], "ledger-export-run");
    assert_eq!(result["offline_verification"]["status"], "verified");
}

#[tokio::test]
async fn raw_shell_and_fs_tools_are_not_advertised_or_callable() {
    let mut client = spawn_fake_server();
    initialize(&mut client).await;
    let list = request(&mut client, jsonrpc(2, "tools/list", json!({}))).await;
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            TOOL_RUN_CONTRACT,
            TOOL_CONTRACT_STATUS,
            TOOL_LEDGER_VERDICT,
            TOOL_LEDGER_EXPORT,
        ]
    );
    for raw_tool in ["bash", "read_file", "write_file", "list_dir"] {
        assert!(!names.iter().any(|name| name == raw_tool));
        let response = call_tool(&mut client, 10, raw_tool, json!({})).await;
        assert_eq!(response["error"]["code"], -32602);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown Truss MCP tool"),
            "{response}"
        );
    }
}

struct TestClient {
    read: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

fn spawn_fake_server() -> TestClient {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let backend = Arc::new(FakeBackend::default());
    let principal = Principal {
        tenant: "test-tenant".to_string(),
        user: "test-user".to_string(),
    };
    tokio::spawn(async move {
        TrussMcpServer::new(backend, principal)
            .serve_stdio(server)
            .await
            .unwrap();
    });
    let (read, write) = tokio::io::split(client);
    TestClient {
        read: BufReader::new(read),
        write,
    }
}

async fn initialize(client: &mut TestClient) {
    let response = request(
        client,
        jsonrpc(
            1,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "truss-test", "version": "0" },
            }),
        ),
    )
    .await;
    assert_eq!(response["result"]["serverInfo"]["name"], "truss");
    notify(client, "notifications/initialized", json!({})).await;
}

async fn call_tool(client: &mut TestClient, id: u64, name: &str, arguments: Value) -> Value {
    request(
        client,
        jsonrpc(
            id,
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        ),
    )
    .await
}

async fn request(client: &mut TestClient, value: Value) -> Value {
    write_message(&mut client.write, &value).await;
    read_message(&mut client.read).await
}

async fn notify(client: &mut TestClient, method: &str, params: Value) {
    write_message(
        &mut client.write,
        &json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }),
    )
    .await;
}

fn jsonrpc(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn tool_text_json(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

async fn write_message<W>(write: &mut W, value: &Value)
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    write.write_all(header.as_bytes()).await.unwrap();
    write.write_all(&body).await.unwrap();
    write.flush().await.unwrap();
}

async fn read_message<R>(read: &mut R) -> Value
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    let mut content_len = None;
    loop {
        let mut line = Vec::new();
        let n = read.read_until(b'\n', &mut line).await.unwrap();
        assert_ne!(n, 0, "server closed stdout");
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_len = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0u8; content_len.unwrap()];
    read.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}
