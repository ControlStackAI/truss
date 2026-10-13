use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use truss_core::ctx::Ctx;
use truss_core::error::ToolError;
use truss_core::policy::{NetworkPolicy, Policy};
use truss_core::store::{tool_call_key, Store};
use truss_core::tools::{
    WebFetchResponse, WebHttpTransport, WebJournal, WebSearchConfig, WebSearchProvider,
    WebSearchResult, WebTool,
};

#[tokio::test]
async fn web_fetch_records_signed_hashed_tool_output() {
    let store = Store::open_memory().unwrap();
    store
        .create_session("web-fetch", Some("mock"), "local", "local")
        .unwrap();
    let transport = Arc::new(FakeHttp::new(vec![WebFetchResponse {
        status: 200,
        final_url: "https://example.com/page".into(),
        content_type: Some("text/html; charset=utf-8".into()),
        body: b"<html><head><script>bad()</script></head><body>Hello <b>world</b></body></html>"
            .to_vec(),
    }]));
    let mut registry = truss_core::tools::ToolRegistry::new();
    registry.register(Arc::new(WebTool::fetch_with_transport(
        Some(WebJournal {
            store: store.clone(),
            session_id: "web-fetch".into(),
        }),
        transport.clone(),
        Arc::new(FakeSearch::default()),
    )));
    let ctx = Ctx::new().with_policy(policy_for_hosts(&["example.com"]));

    let out = registry
        .call_with_ledger(
            &ctx,
            "web_fetch",
            json!({ "url": "https://example.com/page" }),
            None,
        )
        .await
        .expect("web fetch");
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["status"], 200);
    assert_eq!(value["final_url"], "https://example.com/page");
    assert_eq!(value["content"], "Hello world");
    assert_eq!(
        value["raw_sha256"],
        sha256_bytes(
            b"<html><head><script>bad()</script></head><body>Hello <b>world</b></body></html>"
        )
    );
    assert_eq!(transport.calls(), 1);

    let entries = store.entries("web-fetch").unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.kind == "web_tool_call")
        .expect("web provenance entry");
    assert_eq!(entry.payload["tool"], "web_fetch");
    assert_eq!(entry.payload["side_effect"], true);
    assert_eq!(entry.payload["result_sha256"], sha256_str(&out));
    assert!(!entry.signature.is_empty());
    assert!(store.verify_chain("web-fetch").unwrap().is_ok());
}

#[tokio::test]
async fn web_fetch_captures_redirect_final_url() {
    let transport = Arc::new(FakeHttp::new(vec![WebFetchResponse {
        status: 200,
        final_url: "https://example.com/final".into(),
        content_type: Some("text/plain".into()),
        body: b"landed".to_vec(),
    }]));
    let mut registry = truss_core::tools::ToolRegistry::new();
    registry.register(Arc::new(WebTool::fetch_with_transport(
        None,
        transport,
        Arc::new(FakeSearch::default()),
    )));
    let ctx = Ctx::new().with_policy(policy_for_hosts(&["example.com"]));

    let out = registry
        .call(
            &ctx,
            "web_fetch",
            json!({ "url": "https://example.com/start" }),
        )
        .await
        .expect("web fetch");
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["url"], "https://example.com/start");
    assert_eq!(value["final_url"], "https://example.com/final");
}

#[tokio::test]
async fn web_search_unconfigured_error_is_clean_and_signed() {
    let env_var = "TRUSS_TEST_MISSING_WEB_SEARCH_KEY";
    std::env::remove_var(env_var);
    let store = Store::open_memory().unwrap();
    store
        .create_session("web-search-missing", Some("mock"), "local", "local")
        .unwrap();
    let mut registry = truss_core::tools::ToolRegistry::new();
    registry.register_web_tools(
        Some(WebJournal {
            store: store.clone(),
            session_id: "web-search-missing".into(),
        }),
        WebSearchConfig {
            endpoint: Some("https://search.example/api".into()),
            api_key_env: Some(env_var.into()),
        },
    );
    let ctx = Ctx::new().with_policy(policy_for_hosts(&["search.example"]));

    let err = registry
        .call_with_ledger(&ctx, "web_search", json!({ "query": "truss" }), None)
        .await
        .expect_err("missing key should be a clean tool error");
    assert!(err
        .to_string()
        .contains("web_search unconfigured: missing env var TRUSS_TEST_MISSING_WEB_SEARCH_KEY"));

    let entries = store.entries("web-search-missing").unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.kind == "web_tool_call")
        .expect("signed error provenance");
    assert_eq!(entry.payload["tool"], "web_search");
    assert_eq!(entry.payload["is_error"], true);
    assert!(!entry.signature.is_empty());
}

#[tokio::test]
async fn web_fetch_replays_memo_and_revalidates_only_when_requested() {
    let store = Store::open_memory().unwrap();
    store
        .create_session("web-replay", Some("mock"), "local", "local")
        .unwrap();
    let transport = Arc::new(FakeHttp::new(vec![
        WebFetchResponse {
            status: 200,
            final_url: "https://example.com/page".into(),
            content_type: Some("text/plain".into()),
            body: b"first".to_vec(),
        },
        WebFetchResponse {
            status: 200,
            final_url: "https://example.com/page".into(),
            content_type: Some("text/plain".into()),
            body: b"second".to_vec(),
        },
    ]));
    let mut registry = truss_core::tools::ToolRegistry::new();
    registry.register(Arc::new(WebTool::fetch_with_transport(
        Some(WebJournal {
            store: store.clone(),
            session_id: "web-replay".into(),
        }),
        transport.clone(),
        Arc::new(FakeSearch::default()),
    )));
    let ctx = Ctx::new().with_policy(policy_for_hosts(&["example.com"]));
    let input = json!({ "url": "https://example.com/page" });

    let first = registry
        .call_with_ledger(&ctx, "web_fetch", input.clone(), None)
        .await
        .expect("first fetch");
    let replay = registry
        .call_with_ledger(&ctx, "web_fetch", input.clone(), None)
        .await
        .expect("memo replay");
    assert_eq!(first, replay);
    assert_eq!(transport.calls(), 1, "memo replay must not re-fetch");

    let revalidated = registry
        .call_with_ledger(
            &ctx,
            "web_fetch",
            json!({ "url": "https://example.com/page", "revalidate": true }),
            None,
        )
        .await
        .expect("explicit revalidation");
    assert_ne!(first, revalidated);
    assert_eq!(transport.calls(), 2);
    assert_eq!(
        store
            .entries("web-replay")
            .unwrap()
            .iter()
            .filter(|entry| entry.kind == "web_tool_call")
            .count(),
        2,
        "replay does not append duplicate provenance; revalidation records a new observation"
    );
}

#[tokio::test]
async fn web_fetch_rejects_tampered_memo_without_matching_signed_output() {
    let store = Store::open_memory().unwrap();
    store
        .create_session("web-tamper", Some("mock"), "local", "local")
        .unwrap();
    let transport = Arc::new(FakeHttp::new(vec![WebFetchResponse {
        status: 200,
        final_url: "https://example.com/page".into(),
        content_type: Some("text/plain".into()),
        body: b"original".to_vec(),
    }]));
    let mut registry = truss_core::tools::ToolRegistry::new();
    registry.register(Arc::new(WebTool::fetch_with_transport(
        Some(WebJournal {
            store: store.clone(),
            session_id: "web-tamper".into(),
        }),
        transport.clone(),
        Arc::new(FakeSearch::default()),
    )));
    let ctx = Ctx::new().with_policy(policy_for_hosts(&["example.com"]));
    let input = json!({ "url": "https://example.com/page" });
    registry
        .call_with_ledger(&ctx, "web_fetch", input.clone(), None)
        .await
        .expect("initial fetch");

    let memo_key = tool_call_key("web_fetch", &input);
    store
        .memo_put("web-tamper", &memo_key, "{\"content\":\"tampered\"}", false)
        .unwrap();
    let err = registry
        .call_with_ledger(&ctx, "web_fetch", input, None)
        .await
        .expect_err("tampered memo should not replay");
    assert!(err
        .to_string()
        .contains("web memo missing matching signed provenance"));
    assert_eq!(transport.calls(), 1, "tamper failure must not re-fetch");
}

#[tokio::test]
async fn web_search_returns_ranked_results_with_hash() {
    let search = Arc::new(FakeSearch::new(
        "https://search.example/api",
        vec![
            WebSearchResult {
                title: "Truss".into(),
                url: "https://example.com/truss".into(),
                snippet: "Agentic harness".into(),
            },
            WebSearchResult {
                title: "Docs".into(),
                url: "https://example.com/docs".into(),
                snippet: "Reference".into(),
            },
        ],
    ));
    let mut registry = truss_core::tools::ToolRegistry::new();
    registry.register(Arc::new(WebTool::search_with_provider(
        None,
        Arc::new(FakeHttp::new(Vec::new())),
        search.clone(),
    )));
    let ctx = Ctx::new().with_policy(policy_for_hosts(&["search.example"]));

    let out = registry
        .call(&ctx, "web_search", json!({ "query": "truss", "limit": 2 }))
        .await
        .expect("web search");
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["provider"], "fake");
    assert_eq!(value["results"][0]["rank"], 1);
    assert_eq!(value["results"][1]["rank"], 2);
    assert_eq!(value["results_sha256"], sha256_json(&value["results"]));
    assert_eq!(search.calls(), 1);
}

struct FakeHttp {
    responses: Mutex<VecDeque<WebFetchResponse>>,
    calls: AtomicUsize,
}

impl FakeHttp {
    fn new(responses: Vec<WebFetchResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl WebHttpTransport for FakeHttp {
    async fn get(&self, _url: &str, _max_bytes: usize) -> Result<WebFetchResponse, ToolError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ToolError::Execution("fake transport exhausted".into()))
    }
}

#[derive(Default)]
struct FakeSearch {
    endpoint: Option<String>,
    results: Vec<WebSearchResult>,
    calls: AtomicUsize,
}

impl FakeSearch {
    fn new(endpoint: &str, results: Vec<WebSearchResult>) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            results,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl WebSearchProvider for FakeSearch {
    async fn search(&self, _query: &str, limit: usize) -> Result<Vec<WebSearchResult>, ToolError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.results.iter().take(limit).cloned().collect())
    }

    fn provider_name(&self) -> &'static str {
        "fake"
    }

    fn endpoint_url(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }
}

fn policy_for_hosts(hosts: &[&str]) -> Policy {
    Policy {
        network: NetworkPolicy::Allowlist(hosts.iter().map(|host| (*host).to_string()).collect()),
        ..Default::default()
    }
}

fn sha256_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap();
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
