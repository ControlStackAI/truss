# ADR 0019: Provider-Aware Typed Tool Schemas

Status: Accepted

Date: 2026-06-18

## Context

Truss already exposes local filesystem and shell capabilities through generic
custom function schemas. That keeps execution local, policy-gated, and recorded
in the durable ledger, but it misses provider-specific schemas that frontier
models were trained to use for coding workflows.

Anthropic already had a precedent in Truss: the governed memory tool advertises
the native `memory_20250818` schema while still executing through Truss's local
memory substrate. The same principle applies to file and shell tools.

## Decision

Provider request serialization may map a generic internal tool definition onto a
provider-native, client-executed typed schema when the provider exposes one.
Execution stays inside the existing Truss tool registry and policy boundary.

For Anthropic:

- `read_file`, `write_file`, and `list_dir` are advertised as the
  `text_editor_20250728` tool named `str_replace_based_edit_tool`.
- `bash` is advertised as `bash_20250124` named `bash`.
- `memory` continues to use `memory_20250818`.

The Anthropic text-editor tool name is accepted as a provider alias at local
execution time. Its `view`, `create`, `str_replace`, and `insert` commands are
translated into local filesystem operations guarded by the same read/write
policy checks as the generic tools. Generic providers do not advertise this
alias.

OpenAI-compatible chat-completions providers and the current subscription Codex
provider continue to use generic function tools. OpenAI documents a Responses
API shell tool with local runtime mode, but Truss's OpenAI provider is currently
chat-completions based and the Codex subscription provider uses a
reverse-engineered Responses-compatible path whose shell item protocol is not
yet wired into the internal `Block` model. Mapping those providers should be a
separate Responses-provider change, not a partial schema-only fork.

## Consequences

Anthropic gets the file and shell schemas it was trained to use without moving
execution to Anthropic infrastructure or weakening ledger provenance.

The provider-neutral tool definitions and generic provider behavior remain
stable. The only provider-specific execution accommodation is a narrow alias
that converts Anthropic typed editor calls back into local tool operations.
