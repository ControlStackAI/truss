# Serving Truss Over MCP

Run Truss as an MCP server over stdio:

```bash
truss --db harness.db --key harness-identity.key serve-mcp \
  --transport stdio \
  --principal-tenant local \
  --principal-user alice
```

An MCP client can call:

```json
{
  "name": "truss_run_contract",
  "arguments": {
    "contract_path": "examples/contract.toml",
    "session_id": "example-mcp-contract"
  }
}
```

The result includes the contract verdict, the signed ledger verdict bundle, the
session head hash, and a bundle reference:

```json
{
  "handle": "example-mcp-contract",
  "session_id": "example-mcp-contract",
  "status": "completed",
  "head_hash": "…",
  "bundle_ref": {
    "session_id": "example-mcp-contract",
    "read_tool": "truss_ledger_export"
  }
}
```

For long-running work, pass `"async": true` and poll:

```json
{
  "name": "truss_contract_status",
  "arguments": {
    "handle": "example-mcp-contract"
  }
}
```

Ledger reads are read-only:

```json
{
  "name": "truss_ledger_export",
  "arguments": {
    "session_id": "example-mcp-contract"
  }
}
```

HTTP serving is not implemented in this slice. The server also does not expose
raw `bash`, filesystem, or internal write tools over MCP; callers delegate a
contract and read signed proof bundles.
