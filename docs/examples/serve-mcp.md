# Serving Truss Over MCP

Run Truss as an MCP server over stdio:

```bash
truss --db harness.db --key harness-identity.key serve-mcp \
  --transport stdio \
  --principal-tenant local \
  --principal-user alice
```

An MCP client can call `truss_run_contract` with a contract TOML path:

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
session head hash, and a `bundle_ref` pointing at `truss_ledger_export`.

For long-running work, pass `"async": true` and poll
`truss_contract_status`. Ledger reads are read-only through
`truss_ledger_verdict` and `truss_ledger_export`.

HTTP serving is not implemented in this slice. The server also does not expose
raw `bash`, filesystem, or internal write tools over MCP; callers delegate a
contract and read signed proof bundles.
