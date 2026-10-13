# ADR 0039: MCP Server Tier

Status: Accepted

## Context

Phase 7 adds the secondary MCP direction: Truss can be called by another
harness as a verification backend. This is distinct from the existing MCP client
tier, where Truss consumes external MCP servers as contract-scoped tools.

The design gate from `docs/BUILD-PLAN.md` applies:

- the server exposes the signed-verdict substrate, so returned results must point
  at the ledger head and the read-only verdict/export bundle;
- long-running delegated work must reconnect through durable sessions instead of
  re-executing work on resume;
- the MCP server must not become remote shell access. The unit of work is a
  contract.

## Decision

Truss adds `truss serve-mcp` over stdio. It speaks the same Content-Length framed
JSON-RPC style used by the MCP client transport and advertises only four tools:

- `truss_run_contract`
- `truss_contract_status`
- `truss_ledger_verdict`
- `truss_ledger_export`

`truss_run_contract` accepts a contract path and optional durable session id. In
synchronous mode it runs the contract through the existing delegation path and
returns:

- the contract verdict object;
- the signed ledger verdict bundle;
- the session head hash;
- a `bundle_ref` pointing callers to `truss_ledger_export`.

When called with `async = true`, the server returns a handle immediately and
runs the contract in the server process. `truss_contract_status` reads the
in-process job table when available and falls back to the durable store by
session id, so reconnects use recorded ledger state rather than rerunning the
contract. A future HTTP deployment can use the same handle shape with stronger
per-caller isolation.

Ledger tools are read-only wrappers around `LedgerReader::verdict` and
`LedgerReader::export`. They return signed store views only; there is no write
path through the MCP ledger surface.

Stdio mode carries a single configured `Principal` for the process. The CLI
exposes `--principal-tenant` and `--principal-user`, and the contract execution
context threads that principal into the durable run. HTTP mode is an honest stub
in this slice: auth, request-scoped principals, and network serving are reserved
for MCP-S3.

## Consequences

Other harnesses can delegate high-stakes work to Truss and receive a signed
verdict with a durable bundle reference, without being granted Truss's internal
tool surface.

The trust boundary is intentionally narrow. `bash`, `read_file`, `write_file`,
`list_dir`, and other raw internal tools are neither advertised nor callable over
the server. If a contract needs tools, they remain scoped inside Truss policy,
budget, sandbox, provenance, and durable replay.

The first server slice does not implement HTTP transport, HTTP auth, MCP
progress notifications, or raw resource/prompt surfaces. Those can be layered
later without changing the contract-level trust boundary.
