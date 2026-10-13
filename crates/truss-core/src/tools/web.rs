//! Native web fetch/search tools.
//!
//! Network reads are nondeterministic, so durable runs memoize both successful
//! and failed results. Replay returns the signed, hash-bound memo instead of
//! touching the network again unless the caller explicitly requests
//! revalidation.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::memory::LedgerRef;
use crate::provider::ToolDef;
use crate::store::{tool_call_key, Store};

use super::Tool;

const DEFAULT_FETCH_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_SEARCH_LIMIT: usize = 10;
const DEFAULT_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_SEARCH_ENDPOINT_ENV: &str = "TRUSS_WEB_SEARCH_ENDPOINT";
const DEFAULT_SEARCH_API_KEY_ENV: &str = "TRUSS_WEB_SEARCH_API_KEY";

#[derive(Clone)]
pub struct WebJournal {
    pub store: Store,
    pub session_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct WebSearchConfig {
    pub endpoint: Option<String>,
    pub api_key_env: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WebFetchResponse {
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[async_trait]
pub trait WebHttpTransport: Send + Sync {
    async fn get(&self, url: &str, max_bytes: usize) -> Result<WebFetchResponse, ToolError>;
}

#[derive(Clone, Debug)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<WebSearchResult>, ToolError>;
    fn provider_name(&self) -> &'static str;
    fn endpoint_url(&self) -> Option<&str> {
        None
    }
}

#[derive(Clone, Copy)]
enum WebToolKind {
    Fetch,
    Search,
}

pub struct WebTool {
    kind: WebToolKind,
    journal: Option<WebJournal>,
    fetch_transport: Arc<dyn WebHttpTransport>,
    search_provider: Arc<dyn WebSearchProvider>,
}

struct WebCallRecord<'a> {
    produced_by: Option<LedgerRef>,
    memo_key: &'a str,
    input_hash: &'a str,
    result: &'a str,
    is_error: bool,
    error: Option<&'a str>,
}

impl WebTool {
    fn new(
        kind: WebToolKind,
        journal: Option<WebJournal>,
        fetch_transport: Arc<dyn WebHttpTransport>,
        search_provider: Arc<dyn WebSearchProvider>,
    ) -> Self {
        Self {
            kind,
            journal,
            fetch_transport,
            search_provider,
        }
    }

    pub fn fetch(journal: Option<WebJournal>, search_config: WebSearchConfig) -> Self {
        Self::new(
            WebToolKind::Fetch,
            journal,
            Arc::new(ReqwestWebHttpTransport::default()),
            Arc::new(BraveWebSearchProvider::new(search_config)),
        )
    }

    pub fn search(journal: Option<WebJournal>, search_config: WebSearchConfig) -> Self {
        Self::new(
            WebToolKind::Search,
            journal,
            Arc::new(ReqwestWebHttpTransport::default()),
            Arc::new(BraveWebSearchProvider::new(search_config)),
        )
    }

    pub fn fetch_with_transport(
        journal: Option<WebJournal>,
        fetch_transport: Arc<dyn WebHttpTransport>,
        search_provider: Arc<dyn WebSearchProvider>,
    ) -> Self {
        Self::new(
            WebToolKind::Fetch,
            journal,
            fetch_transport,
            search_provider,
        )
    }

    pub fn search_with_provider(
        journal: Option<WebJournal>,
        fetch_transport: Arc<dyn WebHttpTransport>,
        search_provider: Arc<dyn WebSearchProvider>,
    ) -> Self {
        Self::new(
            WebToolKind::Search,
            journal,
            fetch_transport,
            search_provider,
        )
    }

    fn name(&self) -> &'static str {
        match self.kind {
            WebToolKind::Fetch => "web_fetch",
            WebToolKind::Search => "web_search",
        }
    }

    async fn call_inner(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let revalidate = input
            .get("revalidate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let memo_input = memo_input(&input);
        let input_hash = sha256_json(&memo_input);
        let memo_key = tool_call_key(self.name(), &memo_input);

        if !revalidate {
            if let Some(journal) = &self.journal {
                if let Some((content, is_error)) = journal
                    .store
                    .memo_get(&journal.session_id, &memo_key)
                    .map_err(|e| ToolError::Execution(format!("read web memo: {e}")))?
                {
                    self.verify_memo(journal, &memo_key, &input_hash, &content, is_error)?;
                    if is_error {
                        return Err(ToolError::Execution(content));
                    }
                    return Ok(content);
                }
            }
        }

        let result = match self.kind {
            WebToolKind::Fetch => web_fetch(ctx, &input, self.fetch_transport.as_ref()).await,
            WebToolKind::Search => web_search(ctx, &input, self.search_provider.as_ref()).await,
        };
        match result {
            Ok(out) => {
                self.record_provenance(
                    ctx,
                    WebCallRecord {
                        produced_by,
                        memo_key: &memo_key,
                        input_hash: &input_hash,
                        result: &out,
                        is_error: false,
                        error: None,
                    },
                )?;
                Ok(out)
            }
            Err(err) => {
                let err_text = err.to_string();
                self.record_provenance(
                    ctx,
                    WebCallRecord {
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

    fn record_provenance(&self, ctx: &Ctx, record: WebCallRecord<'_>) -> Result<(), ToolError> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        journal
            .store
            .append_with_memo(
                &journal.session_id,
                "web_tool_call",
                &json!({
                    "tool": self.name(),
                    "memo_key": record.memo_key,
                    "input_sha256": record.input_hash,
                    "result_sha256": sha256_str(record.result),
                    "is_error": record.is_error,
                    "side_effect": true,
                    "produced_by": record.produced_by,
                    "error": record.error,
                }),
                record.memo_key,
                record.result,
                record.is_error,
            )
            .map_err(|e| ToolError::Execution(format!("record web provenance: {e}")))?;
        Ok(())
    }

    fn verify_memo(
        &self,
        journal: &WebJournal,
        memo_key: &str,
        input_hash: &str,
        content: &str,
        is_error: bool,
    ) -> Result<(), ToolError> {
        let result_hash = sha256_str(content);
        let entries = journal
            .store
            .entries(&journal.session_id)
            .map_err(|e| ToolError::Execution(format!("read web provenance: {e}")))?;
        let verified = entries.iter().rev().any(|entry| {
            entry.kind == "web_tool_call"
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
                "web memo missing matching signed provenance".into(),
            ))
        }
    }
}

#[async_trait]
impl Tool for WebTool {
    fn def(&self) -> ToolDef {
        match self.kind {
            WebToolKind::Fetch => ToolDef {
                name: self.name().into(),
                description: "Fetch a URL and return attested-as-retrieved readable content with status, final URL, content type, and SHA-256 hashes.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "HTTP or HTTPS URL to fetch." },
                        "max_bytes": { "type": "integer", "description": "Maximum response bytes to read. Defaults to 1048576." },
                        "revalidate": { "type": "boolean", "description": "Bypass any recorded memo and fetch again." }
                    },
                    "required": ["url"]
                }),
            },
            WebToolKind::Search => ToolDef {
                name: self.name().into(),
                description: "Run a configured web search backend and return ranked results with SHA-256 hashes. Requires TRUSS_WEB_SEARCH_API_KEY unless configured otherwise.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query." },
                        "limit": { "type": "integer", "description": "Maximum results. Defaults to 10." },
                        "revalidate": { "type": "boolean", "description": "Bypass any recorded memo and search again." }
                    },
                    "required": ["query"]
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

#[derive(Default)]
struct ReqwestWebHttpTransport {
    client: reqwest::Client,
}

#[async_trait]
impl WebHttpTransport for ReqwestWebHttpTransport {
    async fn get(&self, url: &str, max_bytes: usize) -> Result<WebFetchResponse, ToolError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("fetch {url}: {e}")))?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response
            .bytes()
            .await
            .map_err(|e| ToolError::Execution(format!("read {url}: {e}")))?;
        let mut body = body.to_vec();
        if body.len() > max_bytes {
            body.truncate(max_bytes);
        }
        Ok(WebFetchResponse {
            status,
            final_url,
            content_type,
            body,
        })
    }
}

struct BraveWebSearchProvider {
    endpoint: String,
    api_key_env: String,
    client: reqwest::Client,
}

impl BraveWebSearchProvider {
    fn new(config: WebSearchConfig) -> Self {
        Self {
            endpoint: config.endpoint.unwrap_or_else(|| {
                std::env::var(DEFAULT_SEARCH_ENDPOINT_ENV)
                    .unwrap_or_else(|_| DEFAULT_SEARCH_ENDPOINT.to_string())
            }),
            api_key_env: config
                .api_key_env
                .unwrap_or_else(|| DEFAULT_SEARCH_API_KEY_ENV.to_string()),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl WebSearchProvider for BraveWebSearchProvider {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<WebSearchResult>, ToolError> {
        let api_key = std::env::var(&self.api_key_env).map_err(|_| {
            ToolError::Execution(format!(
                "web_search unconfigured: missing env var {}",
                self.api_key_env
            ))
        })?;
        let response = self
            .client
            .get(&self.endpoint)
            .query(&[("q", query), ("count", &limit.to_string())])
            .header("X-Subscription-Token", api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("web_search request failed: {e}")))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| ToolError::Execution(format!("web_search read failed: {e}")))?;
        if !status.is_success() {
            return Err(ToolError::Execution(format!(
                "web_search returned {status}: {text}"
            )));
        }
        let value = serde_json::from_str::<Value>(&text)
            .map_err(|e| ToolError::Execution(format!("web_search decode failed: {e}")))?;
        let results = value
            .get("web")
            .and_then(|web| web.get("results"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .take(limit)
                    .map(|item| WebSearchResult {
                        title: item
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        url: item
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        snippet: item
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .filter(|result| !result.url.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(results)
    }

    fn provider_name(&self) -> &'static str {
        "brave"
    }

    fn endpoint_url(&self) -> Option<&str> {
        Some(&self.endpoint)
    }
}

async fn web_fetch(
    ctx: &Ctx,
    input: &Value,
    transport: &dyn WebHttpTransport,
) -> Result<String, ToolError> {
    let url = required_str(input, "url")?;
    let parsed = reqwest::Url::parse(&url)
        .map_err(|e| ToolError::InvalidInput(format!("invalid url: {e}")))?;
    ensure_http_scheme(&parsed)?;
    ensure_network_allowed(ctx, &parsed)?;
    let max_bytes = input
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, DEFAULT_FETCH_MAX_BYTES as u64) as usize)
        .unwrap_or(DEFAULT_FETCH_MAX_BYTES);
    let response = transport.get(&url, max_bytes).await?;
    let raw_sha256 = sha256_bytes(&response.body);
    let readable_content = readable_content(&response.body, response.content_type.as_deref());
    render_json(json!({
        "url": url,
        "final_url": response.final_url,
        "status": response.status,
        "content_type": response.content_type,
        "raw_sha256": raw_sha256,
        "content_sha256": sha256_str(&readable_content),
        "content": readable_content,
        "truncated_to_bytes": max_bytes,
        "attestation": "attested-as-retrieved",
    }))
}

async fn web_search(
    ctx: &Ctx,
    input: &Value,
    provider: &dyn WebSearchProvider,
) -> Result<String, ToolError> {
    let query = required_str(input, "query")?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, 20) as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT);
    if let Some(endpoint) = provider.endpoint_url() {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|e| ToolError::InvalidInput(format!("invalid search endpoint: {e}")))?;
        ensure_http_scheme(&endpoint)?;
        ensure_network_allowed(ctx, &endpoint)?;
    } else if matches!(&ctx.policy.network, crate::policy::NetworkPolicy::None) {
        return Err(ToolError::PermissionDenied(
            "web_search requires network policy allowing the configured search endpoint".into(),
        ));
    }
    let results = provider.search(&query, limit).await?;
    let rendered_results = results
        .into_iter()
        .enumerate()
        .map(|(rank, result)| {
            json!({
                "rank": rank + 1,
                "title": result.title,
                "url": result.url,
                "snippet": result.snippet,
            })
        })
        .collect::<Vec<_>>();
    let results_value = Value::Array(rendered_results);
    render_json(json!({
        "query": query,
        "provider": provider.provider_name(),
        "results_sha256": sha256_json(&results_value),
        "results": results_value,
        "attestation": "attested-as-retrieved",
    }))
}

fn ensure_http_scheme(url: &reqwest::Url) -> Result<(), ToolError> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(ToolError::InvalidInput(format!(
            "unsupported url scheme '{other}'"
        ))),
    }
}

fn ensure_network_allowed(ctx: &Ctx, url: &reqwest::Url) -> Result<(), ToolError> {
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::InvalidInput("url has no host".into()))?;
    if ctx.policy.host_allowed(host) {
        Ok(())
    } else {
        Err(ToolError::PermissionDenied(format!(
            "web tools require network policy allowing {host}"
        )))
    }
}

fn readable_content(body: &[u8], content_type: Option<&str>) -> String {
    let text = String::from_utf8_lossy(body).to_string();
    if content_type
        .map(|ct| ct.to_ascii_lowercase().contains("html"))
        .unwrap_or(false)
    {
        normalize_ws(&strip_html(&text))
    } else {
        text
    }
}

fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    let mut tag = String::new();
    let mut skipping: Option<&'static str> = None;
    for ch in input.chars() {
        if in_tag {
            if ch == '>' {
                let tag_name = tag
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if skipping.is_none() && (tag_name == "script" || tag_name == "style") {
                    skipping = if tag_name == "script" {
                        Some("script")
                    } else {
                        Some("style")
                    };
                } else if tag.trim_start().starts_with('/') && skipping == Some(tag_name.as_str()) {
                    skipping = None;
                }
                in_tag = false;
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
        } else if skipping.is_none() {
            out.push(ch);
        }
    }
    out
}

fn normalize_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing '{key}'")))
}

fn memo_input(input: &Value) -> Value {
    let mut value = input.clone();
    if let Value::Object(map) = &mut value {
        map.remove("revalidate");
    }
    value
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
