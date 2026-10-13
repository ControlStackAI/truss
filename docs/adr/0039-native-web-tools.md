# ADR 0039: Native Web Search And Fetch Tools

## Status

Accepted

## Context

Phase 5 adds daily-driver native tools without moving work outside Truss's
verification and replay substrate. Web fetch and search are common model actions,
but delegating them through shell commands or unjournaled adapters loses the
signed evidence trail that verdicts depend on.

Network reads are also nondeterministic. A page can change, a search provider can
rerank results, credentials can expire, or a request can fail differently on
resume. The runtime needs an exact replay path for the observation that influenced
the model.

## Decision

Add two typed, opt-in native tools:

- `web_fetch` for HTTP(S) GET requests.
- `web_search` for provider-backed web search.

Both tools are registered only when `--web-tools` or `[tools] web = true` is set.
Policy remains separate: `policy.allow_tools` may still restrict the exact tool
names, and `policy.network` controls reachable hosts.

`web_fetch` uses the existing Rust async HTTP stack (`reqwest`) and returns status,
content type, final URL after redirects, readable content, and SHA-256 hashes.

`web_search` is behind a small `WebSearchProvider` trait. The first concrete
backend is Brave Search, configured by environment/config rather than hardcoded
secrets:

- `TRUSS_WEB_SEARCH_API_KEY` by default for the API key.
- `TRUSS_WEB_SEARCH_ENDPOINT` optionally overrides the endpoint, unless TOML
  provides `[tools.web_search].endpoint`.
- `[tools.web_search].api_key_env` can point at a different key env var.

If no key is configured, `web_search` returns a normal signed tool error instead
of panicking.

Durable runs record both tools as signed `web_tool_call` entries and write the
result into the tool memo table in the same transaction. On resume, Truss verifies
the memo against the signed ledger entry and replays it. Passing
`revalidate = true` bypasses the memo and records a new observation.

## Consequences

- Verdicts can cite web observations by signed ledger entry and result hash.
- Resume is deterministic for web observations unless the caller explicitly asks
  to revalidate.
- Tests can inject fake HTTP/search transports and never hit the live network.
- The core runtime now directly depends on `reqwest` because native web tools are
  compiled into the tool registry; runtime use remains default-off.
- The attestation is deliberately narrow: Truss proves what it retrieved, not
  whether the retrieved content or search result is true.

## Alternatives Considered

- **Only expose web via MCP.** MCP is useful, but web fetch/search are baseline
  parity tools and should inherit first-class typed schemas, policy checks,
  ledger entries, and replay semantics without requiring a separate server.
- **Use shell commands such as `curl`.** That would make network access opaque to
  the tool policy and would not give structured final URL, content type, hashes,
  or provider-specific search results.
- **Hardcode one search provider with no seam.** This would make tests brittle and
  lock search quality to a single backend. The trait keeps provider behavior
  swappable while preserving one concrete implementation.
