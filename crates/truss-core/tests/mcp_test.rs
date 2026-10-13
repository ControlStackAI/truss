use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use truss_core::ctx::Ctx;
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::ToolError;
use truss_core::mcp::{McpJournal, McpServerConfig, McpTransport};
use truss_core::message::Block;
#[cfg(feature = "mcp-http")]
use truss_core::policy::{NetworkPolicy, Policy};
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::store::{tool_call_key, Store};
use truss_core::tools::ToolRegistry;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct MockProvider {
    calls: Arc<Mutex<usize>>,
    script: Vec<CompletionResponse>,
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, truss_core::error::ProviderError> {
        let mut calls = self.calls.lock().unwrap();
        let index = *calls;
        *calls += 1;
        self.script.get(index).cloned().ok_or_else(|| {
            truss_core::error::ProviderError::Config(format!("script exhausted at call {index}"))
        })
    }
}

fn assistant_tool_use(id: &str, name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn assistant_text(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(text.into())],
        stop_reason: StopReason::EndTurn,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

#[tokio::test]
async fn mounts_and_invokes_stdio_mcp_tool() {
    let server = fake_server("basic");
    let ctx = Ctx::new();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(&ctx, &[server], None)
        .await
        .expect("mount mcp server");

    let defs = registry.defs();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "mcp:fake:echo");
    assert_eq!(defs[0].input_schema["required"][0], "message");

    let out = registry
        .call(
            &ctx,
            "mcp:fake:echo",
            json!({ "message": "hello from mcp" }),
        )
        .await
        .expect("call mcp tool");
    assert_eq!(out, "echo: hello from mcp");
}

#[tokio::test]
async fn records_signed_mcp_tool_call_provenance() {
    let server = fake_server("journal");
    let store = Store::open_memory().unwrap();
    store
        .create_session("mcp-session", Some("mock"), "local", "local")
        .unwrap();

    let ctx = Ctx::new();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(
            &ctx,
            &[server],
            Some(McpJournal {
                store: store.clone(),
                session_id: "mcp-session".into(),
            }),
        )
        .await
        .expect("mount mcp server");

    let out = registry
        .call_with_ledger(&ctx, "mcp:fake:echo", json!({ "message": "hash me" }), None)
        .await
        .expect("call mcp tool");
    assert_eq!(out, "echo: hash me");

    let entries = store.entries("mcp-session").unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.kind == "mcp_tool_call")
        .expect("mcp provenance entry");
    assert_eq!(entry.payload["server"], "fake");
    assert_eq!(entry.payload["remote_tool"], "echo");
    assert_eq!(entry.payload["mounted_tool"], "mcp:fake:echo");
    assert_eq!(entry.payload["is_error"], false);
    assert!(entry.payload["input_sha256"].as_str().unwrap().len() == 64);
    assert!(entry.payload["result_sha256"].as_str().unwrap().len() == 64);
    assert!(entry.payload.get("message").is_none());
    assert!(store.verify_chain("mcp-session").unwrap().is_ok());
}

#[tokio::test]
async fn memoized_mcp_success_does_not_reinvoke_server() {
    let counter = counter_file("success");
    let mut server = fake_server("memo-success");
    server.env.insert(
        "TRUSS_MCP_COUNTER_FILE".into(),
        counter.to_string_lossy().to_string(),
    );
    let store = Store::open_memory().unwrap();
    store
        .create_session("mcp-memo-success", Some("mock"), "local", "local")
        .unwrap();

    let ctx = Ctx::new();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(
            &ctx,
            &[server],
            Some(McpJournal {
                store: store.clone(),
                session_id: "mcp-memo-success".into(),
            }),
        )
        .await
        .expect("mount mcp server");

    let first = registry
        .call_with_ledger(&ctx, "mcp:fake:echo", json!({ "message": "once" }), None)
        .await
        .expect("first mcp call");
    let second = registry
        .call_with_ledger(&ctx, "mcp:fake:echo", json!({ "message": "once" }), None)
        .await
        .expect("second mcp call");
    let memo_key = tool_call_key("mcp:fake:echo", &json!({ "message": "once" }));
    let memo = store
        .memo_get("mcp-memo-success", &memo_key)
        .expect("read memo")
        .expect("memo row");

    assert_eq!(first, "echo: once");
    assert_eq!(second, "echo: once");
    assert_eq!(memo, ("echo: once".to_string(), false));
    assert_eq!(counter_value(&counter), 1, "server should run once");
    assert_eq!(
        store
            .entries("mcp-memo-success")
            .unwrap()
            .iter()
            .filter(|entry| entry.kind == "mcp_tool_call")
            .count(),
        1,
        "memo replay should not write a duplicate MCP provenance entry"
    );
    assert!(store.verify_chain("mcp-memo-success").unwrap().is_ok());
}

#[tokio::test]
async fn memoized_mcp_error_does_not_reinvoke_server() {
    let counter = counter_file("error");
    let mut server = fake_server("memo-error");
    server.env.insert(
        "TRUSS_MCP_COUNTER_FILE".into(),
        counter.to_string_lossy().to_string(),
    );
    let store = Store::open_memory().unwrap();
    store
        .create_session("mcp-memo-error", Some("mock"), "local", "local")
        .unwrap();

    let ctx = Ctx::new();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(
            &ctx,
            &[server],
            Some(McpJournal {
                store: store.clone(),
                session_id: "mcp-memo-error".into(),
            }),
        )
        .await
        .expect("mount mcp server");

    let first = registry
        .call_with_ledger(&ctx, "mcp:fake:echo", json!({ "message": "fail" }), None)
        .await
        .expect_err("first mcp call should fail");
    let memo_key = tool_call_key("mcp:fake:echo", &json!({ "message": "fail" }));
    let memo = store
        .memo_get("mcp-memo-error", &memo_key)
        .expect("read memo")
        .expect("memo row");
    let second = registry
        .call_with_ledger(&ctx, "mcp:fake:echo", json!({ "message": "fail" }), None)
        .await
        .expect_err("second mcp call should replay failure");

    assert!(first.to_string().contains("boom: fail"));
    assert!(second.to_string().contains("boom: fail"));
    assert_eq!(memo, ("boom: fail".to_string(), true));
    assert_eq!(counter_value(&counter), 1, "server should run once");
    assert_eq!(
        store
            .entries("mcp-memo-error")
            .unwrap()
            .iter()
            .filter(|entry| entry.kind == "mcp_tool_call")
            .count(),
        1,
        "memo replay should not write a duplicate MCP provenance entry"
    );
    assert!(store.verify_chain("mcp-memo-error").unwrap().is_ok());
}

#[tokio::test]
async fn mcp_pending_result_records_signed_suspend_provenance() {
    let server = fake_server("pending");
    let store = Store::open_memory().unwrap();
    store
        .create_session("mcp-pending", Some("mock"), "local", "local")
        .unwrap();

    let ctx = Ctx::new();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(
            &ctx,
            &[server],
            Some(McpJournal {
                store: store.clone(),
                session_id: "mcp-pending".into(),
            }),
        )
        .await
        .expect("mount mcp server");

    let err = registry
        .call_with_ledger(&ctx, "mcp:fake:echo", json!({ "message": "pending" }), None)
        .await
        .expect_err("pending result should suspend");
    match err {
        ToolError::Suspend {
            signal_name,
            reason,
            resume_payload,
        } => {
            assert_eq!(signal_name, "mcp-job-1");
            assert_eq!(reason, "async job running");
            assert_eq!(resume_payload["kind"], "mcp_pending");
            assert_eq!(resume_payload["mounted_tool"], "mcp:fake:echo");
            assert!(resume_payload["pending_ref"].is_object());
        }
        other => panic!("expected suspend error, got {other:?}"),
    }

    let entries = store.entries("mcp-pending").unwrap();
    let pending = entries
        .iter()
        .find(|entry| entry.kind == "mcp_tool_pending")
        .expect("pending provenance");
    assert_eq!(pending.payload["server"], "fake");
    assert_eq!(pending.payload["transport"], "stdio");
    assert_eq!(pending.payload["signal_name"], "mcp-job-1");
    assert!(store.verify_chain("mcp-pending").unwrap().is_ok());
}

#[tokio::test]
async fn durable_run_suspends_on_mcp_pending_and_memoizes_signal_result() {
    let counter = counter_file("durable-pending");
    let mut server = fake_server("durable-pending");
    server.env.insert(
        "TRUSS_MCP_COUNTER_FILE".into(),
        counter.to_string_lossy().to_string(),
    );
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let sid = "mcp-durable-pending";

    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(
            &ctx,
            &[server],
            Some(McpJournal {
                store: store.clone(),
                session_id: sid.into(),
            }),
        )
        .await
        .expect("mount mcp server");
    let tools = Arc::new(registry);

    let provider_calls = Arc::new(Mutex::new(0));
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: provider_calls.clone(),
        script: vec![
            assistant_tool_use("c1", "mcp:fake:echo", json!({ "message": "pending" })),
            assistant_text("async complete"),
        ],
    });
    let cfg = DurableConfig {
        max_steps: 10,
        ..Default::default()
    };

    let out1 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        Some("start async mcp".into()),
    )
    .await
    .expect("first leg");
    match out1 {
        DurableOutcome::Suspended { await_desc, reason } => {
            assert_eq!(await_desc, "signal 'mcp-job-1'");
            assert_eq!(reason, "async job running");
        }
        other => panic!("expected suspend, got {other:?}"),
    }
    assert_eq!(counter_value(&counter), 1);
    assert_eq!(*provider_calls.lock().unwrap(), 1);

    let early = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        None,
    )
    .await
    .expect("early resume");
    assert!(matches!(early, DurableOutcome::StillWaiting { .. }));
    assert_eq!(*provider_calls.lock().unwrap(), 1);

    store
        .add_signal(sid, "mcp-job-1", Some("async: done"))
        .unwrap();
    let out2 = durable::run(provider, tools, &ctx, &store, sid, &cfg, None)
        .await
        .expect("resume after signal");
    match out2 {
        DurableOutcome::Completed { final_text } => assert_eq!(final_text, "async complete"),
        other => panic!("expected complete, got {other:?}"),
    }

    let memo_key = tool_call_key("mcp:fake:echo", &json!({ "message": "pending" }));
    let memo = store.memo_get(sid, &memo_key).unwrap().expect("memo row");
    assert_eq!(memo, ("async: done".to_string(), false));
    let kinds: Vec<String> = store
        .entries(sid)
        .unwrap()
        .into_iter()
        .map(|entry| entry.kind)
        .collect();
    assert!(kinds.contains(&"mcp_tool_pending".to_string()));
    assert!(kinds.contains(&"mcp_tool_call".to_string()));
    assert!(kinds.contains(&"signal_received".to_string()));
    assert_eq!(counter_value(&counter), 1, "resume must not re-invoke MCP");
    assert!(store.verify_chain(sid).unwrap().is_ok());
}

#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn mounts_invokes_and_memoizes_http_mcp_tool_with_env_auth() {
    let (url, hits) = spawn_http_mcp_server(Some(("x-truss-auth", "secret-token"))).await;
    std::env::set_var("TRUSS_MCP_TEST_TOKEN", "secret-token");
    let mut server = http_server("http-auth", url);
    server
        .env_headers
        .insert("x-truss-auth".into(), "TRUSS_MCP_TEST_TOKEN".into());

    let store = Store::open_memory().unwrap();
    store
        .create_session("mcp-http-session", Some("mock"), "local", "local")
        .unwrap();
    let ctx = Ctx::new().with_policy(Policy {
        network: NetworkPolicy::Open,
        ..Default::default()
    });
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(
            &ctx,
            &[server],
            Some(McpJournal {
                store: store.clone(),
                session_id: "mcp-http-session".into(),
            }),
        )
        .await
        .expect("mount http mcp server");

    let first = registry
        .call_with_ledger(&ctx, "mcp:http:echo", json!({ "message": "remote" }), None)
        .await
        .expect("first http mcp call");
    let second = registry
        .call_with_ledger(&ctx, "mcp:http:echo", json!({ "message": "remote" }), None)
        .await
        .expect("second http mcp call");

    assert_eq!(first, "http: remote");
    assert_eq!(second, "http: remote");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "HTTP server should run once"
    );
    let entry = store
        .entries("mcp-http-session")
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "mcp_tool_call")
        .expect("mcp provenance entry");
    assert_eq!(entry.payload["transport"], "http");
    assert_eq!(entry.payload["server"], "http");
    assert_eq!(entry.payload["remote_tool"], "echo");
    assert!(entry.payload.get("x-truss-auth").is_none());
    assert!(entry.payload.get("headers").is_none());
    assert!(store.verify_chain("mcp-http-session").unwrap().is_ok());
}

#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn http_mcp_transport_obeys_network_policy() {
    let (url, _hits) = spawn_http_mcp_server(None).await;
    let server = http_server("http-policy", url);
    let ctx = Ctx::new();
    let mut registry = ToolRegistry::new();
    let err = registry
        .register_mcp_servers(&ctx, &[server], None)
        .await
        .expect_err("default policy denies HTTP");
    assert!(err.to_string().contains("network host not allowed"));
}

fn fake_server(test_name: &str) -> McpServerConfig {
    let script = fake_server_script(test_name);
    McpServerConfig {
        name: "fake".into(),
        transport: McpTransport::Stdio,
        command: Some("python3".into()),
        args: vec![script.to_string_lossy().to_string()],
        url: None,
        cwd: None,
        env: Default::default(),
        headers: Default::default(),
        env_headers: Default::default(),
        allow_tools: None,
    }
}

#[cfg(feature = "mcp-http")]
fn http_server(_test_name: &str, url: String) -> McpServerConfig {
    McpServerConfig {
        name: "http".into(),
        transport: McpTransport::Http,
        command: None,
        args: Vec::new(),
        url: Some(url),
        cwd: None,
        env: Default::default(),
        headers: Default::default(),
        env_headers: Default::default(),
        allow_tools: None,
    }
}

fn fake_server_script(test_name: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("truss-mcp-test-{test_name}-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fake_mcp.py");
    std::fs::write(&path, FAKE_MCP).unwrap();
    path
}

fn counter_file(test_name: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("truss-mcp-counter-{test_name}-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("count.txt");
    std::fs::write(&path, "0").unwrap();
    path
}

fn counter_value(path: &PathBuf) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(feature = "mcp-http")]
async fn spawn_http_mcp_server(
    expected_header: Option<(&'static str, &'static str)>,
) -> (String, std::sync::Arc<AtomicU64>) {
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicU64::new(0));
    let server_hits = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let hits = server_hits.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0_u8; 1024];
                let header_end = loop {
                    let n = stream.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = find_header_end(&buf) {
                        break pos;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]);
                if let Some((name, value)) = expected_header {
                    let needle = format!("{}: {}", name.to_ascii_lowercase(), value);
                    if !headers.to_ascii_lowercase().contains(&needle) {
                        write_http_json(
                            &mut stream,
                            401,
                            &json!({"error": "missing expected auth header"}),
                        )
                        .await;
                        return;
                    }
                }
                let len = content_length(&headers);
                let body_start = header_end + 4;
                while buf.len() < body_start + len {
                    let n = stream.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = &buf[body_start..body_start + len];
                let msg: serde_json::Value = serde_json::from_slice(body).unwrap();
                let response = http_mcp_response(&msg, &hits);
                write_http_json(&mut stream, 200, &response).await;
            });
        }
    });
    (format!("http://{addr}/mcp"), hits)
}

#[cfg(feature = "mcp-http")]
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

#[cfg(feature = "mcp-http")]
fn content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

#[cfg(feature = "mcp-http")]
fn http_mcp_response(msg: &serde_json::Value, hits: &AtomicU64) -> serde_json::Value {
    let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
    match msg.get("method").and_then(|method| method.as_str()) {
        Some("initialize") => json!({"jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "http", "version": "1.0.0"}
        }}),
        Some("tools/list") => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{
            "name": "echo",
            "description": "Echo a message over HTTP",
            "inputSchema": {
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"]
            }
        }]}}),
        Some("tools/call") => {
            hits.fetch_add(1, Ordering::SeqCst);
            let args = msg
                .get("params")
                .and_then(|params| params.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let text = format!(
                "http: {}",
                args.get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
            );
            json!({"jsonrpc": "2.0", "id": id, "result": {
                "content": [{"type": "text", "text": text}],
                "isError": false
            }})
        }
        _ => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
    }
}

#[cfg(feature = "mcp-http")]
async fn write_http_json(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &serde_json::Value,
) {
    use tokio::io::AsyncWriteExt;

    let body = serde_json::to_vec(body).unwrap();
    let reason = if status == 200 { "OK" } else { "ERROR" };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.flush().await;
}

const FAKE_MCP: &str = r#"
import json
import os
import sys

def read_msg():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise SystemExit(0)
        line = line.decode("utf-8").strip()
        if line == "":
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    body = sys.stdin.buffer.read(length)
    return json.loads(body.decode("utf-8"))

def write_msg(msg):
    body = json.dumps(msg).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        write_msg({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "1.0.0"},
        }})
    elif method == "tools/list":
        write_msg({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{
            "name": "echo",
            "description": "Echo a message",
            "inputSchema": {
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"],
            },
        }]}})
    elif method == "tools/call":
        counter_file = os.environ.get("TRUSS_MCP_COUNTER_FILE")
        if counter_file:
            try:
                with open(counter_file, "r", encoding="utf-8") as f:
                    count = int(f.read().strip() or "0")
            except FileNotFoundError:
                count = 0
            with open(counter_file, "w", encoding="utf-8") as f:
                f.write(str(count + 1))
        args = msg.get("params", {}).get("arguments", {})
        if args.get("message") == "fail":
            text = "boom: fail"
            is_error = True
        elif args.get("message") == "pending":
            write_msg({"jsonrpc": "2.0", "id": msg["id"], "result": {
                "content": [{"type": "pending", "signal": "mcp-job-1", "reason": "async job running"}],
                "isError": False,
            }})
            continue
        else:
            text = "echo: " + str(args.get("message", ""))
            is_error = False
        write_msg({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "content": [{"type": "text", "text": text}],
            "isError": is_error,
        }})
    else:
        write_msg({"jsonrpc": "2.0", "id": msg["id"], "error": {
            "code": -32601,
            "message": "unknown method",
        }})
"#;
