# MCP Server Tools

MCP is the preferred seam for out-of-process tools. A configured MCP server is
mounted into the normal tool registry. The model sees a normal tool name:

```text
mcp:<server>:<tool>
```

The mounted tool still passes through Truss policy, budget, cancellation, durable
memoization, and signed provenance.

## Stdio server

`examples/contract-mcp.toml` mounts the tiny stdio server in
`examples/fake-mcp-server.py`:

```toml
[[mcp.servers]]
name = "demo"
command = "python3"
args = ["examples/fake-mcp-server.py"]
allow_tools = ["echo"]

[constraints]
allow_tools = ["read_file", "write_file", "list_dir", "bash", "mcp:demo:echo"]
```

Probe it without a model:

```bash
truss --config examples/harness.toml mcp list
truss --config examples/contract-mcp.toml mcp probe demo
```

## HTTP server

When Truss is built with `mcp-http`, the same config shape supports HTTP:

```toml
[[mcp.servers]]
name = "search"
transport = "http"
url = "https://mcp.example.com"
allow_tools = ["query"]

[mcp.servers.env_headers]
authorization = "SEARCH_MCP_AUTHORIZATION"
```

Use `headers` only for non-secret metadata. Put bearer tokens and API keys in
`env_headers`; Truss records header names, server name, tool name, input hash, and
result hash, but not secret header values.

## Durable behavior

Durable MCP calls write a signed `mcp_tool_call` provenance entry and an atomic
tool memo entry. A resumed run reuses the memo after verifying it against the
signed provenance hashes, so the MCP server is not invoked again.

Long-running MCP tools may return a pending content item:

```json
{ "type": "pending", "signal": "job-123", "reason": "remote job still running" }
```

Durable Truss runs suspend until that signal is delivered. The delivered signal
payload becomes the recorded tool result and is memoized for future resumes.

## Authoring checklist

- Keep server and tool names stable; they become policy names.
- Use `allow_tools` on the server and in contract constraints.
- Keep secrets in environment variables, not TOML.
- Return structured text that reviewers/checks can use as evidence.
- For long jobs, return pending instead of holding the process open forever.
