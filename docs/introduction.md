# Truss Documentation

Truss is a Rust-native, provider-agnostic agentic harness for durable, verifiable
runs. The docs site collects the architecture docs, configuration surface,
worked examples, and decision records into one searchable book.

Start with:

- [System Architecture](architecture.md) for the contract-to-verdict path, runtime,
  provider boundary, and ledger model.
- [Configuration Surface](configuration.md) for the supported TOML and CLI knobs.
- [Example Overview](examples/index.md) for runnable config and contract files.
- [Decision Records](adr/0001-plugin-architecture.md) for accepted design choices.

The generated site is built from the Markdown already in this repository. Design
docs and ADRs remain the source of truth; example pages include the live files
from `examples/` so code snippets do not drift.
