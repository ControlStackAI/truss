# Contract With MCP Tool

`examples/contract-mcp.toml` shows the stdio MCP shape: a contract-scoped MCP
server is mounted into the normal Truss tool registry as `mcp:<server>:<tool>`.
That mounted tool still has to pass the contract's `allow_tools` policy.

Durable and contract runs re-mount MCP tools with store-backed provenance, so
each call records a signed `mcp_tool_call` ledger entry with argument and result
hashes. The normal `tool_result` remains replayable for the model, and repeated
same-session MCP calls replay from the memo instead of re-invoking the server.

HTTP MCP servers use the same contract/config block with `transport = "http"` and
`url = "..."` when Truss is built with `--features mcp-http`. Use `env_headers`
for secret header values; URLs and headers are not recorded in the signed MCP
provenance entry.

Long-running MCP tools may return a Truss pending content item with a signal
name. Durable runs suspend until that signal arrives, then treat the delivered
payload as the tool result and memoize it, so resume does not reconnect to the
MCP server.

```toml
{{#include ../../examples/contract-mcp.toml}}
```
