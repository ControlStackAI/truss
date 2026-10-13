# Native Web Tools

Truss provides two default-off native web tools:

- `web_fetch` fetches an HTTP(S) URL and returns readable content plus status,
  final URL after redirects, content type, and SHA-256 hashes.
- `web_search` queries a configured search backend and returns ranked results
  with title, URL, snippet, and a hash of the returned result list.

These tools are for agentic browsing and research inside the verified runtime.
They are not truth or source-quality validators. Truss attests that the content
or search results were retrieved through the configured transport and binds the
bytes/results into the signed ledger. It does not attest that a page is accurate,
complete, current, unbiased, or authoritative.

## Enabling

The tools are off by default. Enable them with the CLI flag:

```bash
truss --tools --web-tools --config examples/harness.toml "research the current docs"
```

Or in TOML:

```toml
[tools]
web = true
```

Tool registration and policy are separate. If `policy.allow_tools` is set, allow
the exact tool names:

```toml
[policy]
network = "allowlist"
allowlist = ["example.com", "api.search.brave.com"]
allow_tools = ["read_file", "web_fetch", "web_search"]
```

`policy.network` still applies. `web_fetch` checks the target URL host.
`web_search` checks the configured search endpoint host.

## `web_fetch`

Input:

```json
{
  "url": "https://example.com/page",
  "max_bytes": 1048576,
  "revalidate": false
}
```

Output includes:

- `url`: requested URL.
- `final_url`: final URL after redirects.
- `status`: HTTP response status.
- `content_type`: response content type when present.
- `raw_sha256`: SHA-256 of the fetched response bytes retained by Truss.
- `content_sha256`: SHA-256 of the readable content returned to the model.
- `content`: text content. HTML is stripped with a small best-effort extractor.
- `attestation`: currently `attested-as-retrieved`.

`max_bytes` is clamped to a 1 MiB default/ceiling. This keeps tool output bounded;
it is not a completeness guarantee.

## `web_search`

The first concrete backend is Brave Search. It is configured with environment
variables by default:

- `TRUSS_WEB_SEARCH_API_KEY`: Brave Search API key.
- `TRUSS_WEB_SEARCH_ENDPOINT`: optional endpoint override. The compiled default
  is `https://api.search.brave.com/res/v1/web/search`.

The endpoint and API-key env var can also be overridden in config:

```toml
[tools.web_search]
endpoint = "https://api.search.brave.com/res/v1/web/search"
api_key_env = "TRUSS_WEB_SEARCH_API_KEY"
```

If the key env var is missing, `web_search` returns a clean signed tool error:

```text
web_search unconfigured: missing env var TRUSS_WEB_SEARCH_API_KEY
```

Search quality, ranking, freshness, regional behavior, and filtering are provider
properties. Truss hashes and signs what the provider returned; it does not verify
that the provider found every relevant page.

## Ledger And Replay

In durable and contract runs, both tools record signed `web_tool_call` entries
with:

- tool name,
- deterministic memo key,
- input SHA-256,
- result SHA-256,
- error flag,
- optional producing ledger reference.

Because network reads are nondeterministic, both successful and failed results are
also written to the tool memo table in the same transaction as the signed journal
entry. Resume/replay returns the recorded result and verifies that the memo still
matches a signed ledger entry. It does not re-fetch or re-query.

To request a fresh observation, pass `revalidate = true`. Revalidation bypasses
the existing memo, performs the network operation again, and records a new signed
`web_tool_call` entry plus replacement memo for the same logical input.

If a memo is changed without a matching signed ledger entry, replay fails with:

```text
web memo missing matching signed provenance
```

That failure protects verdicts from consuming an unsigned or tampered web result.
