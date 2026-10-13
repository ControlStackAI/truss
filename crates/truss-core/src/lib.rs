//! truss — a provider-agnostic LLM agent harness.
//!
//! The whole architecture in one breath: a [`provider::Provider`] is "one call
//! in, one response out". The harness ([`agent::Agent`]) is a loop around that
//! call — it sends history, and when the model asks for a tool it runs the tool,
//! appends the result, and calls again until the model is done.
//!
//! What turns that core loop into something you can actually delegate to is the
//! [`ctx::Ctx`] threaded through every call: it carries cancellation, a deadline,
//! a shared [`budget`] meter, the calling [`ctx::Principal`], an enforced
//! [`policy::Policy`], and an [`events`] sink. Add a cross-cutting concern later
//! and it goes on the `Ctx` — the `complete`/`call` signatures never change again.
//!
//! Modules:
//! * [`provider`] / [`providers`] — the backend abstraction + Anthropic/OpenAI.
//! * [`agent`] — the loop (retries, budgets, parallel tools, events).
//! * [`ctx`] — the run context (the keystone).
//! * [`budget`], [`policy`], [`retry`], [`pricing`] — the enforcement primitives.
//! * [`events`], [`state`], [`observability`] — the "review proofs, not
//!   transcripts" spine: a typed event stream, a durable JSONL audit log, traces.
//! * [`config`] — declarative TOML setup.

pub mod agent;
pub mod anchor;
pub mod approval;
pub mod budget;
pub mod child_run;
pub mod compaction_eval;
pub mod context_gauge;
pub mod contract;
pub mod ctx;
pub mod delegation;
pub mod durable;
pub mod error;
pub mod eval;
pub mod events;
pub mod executor;
pub mod fan_out;
pub mod identity;
pub mod ledger;
pub mod mcp;
pub mod mcp_server;
pub mod memory;
pub mod message;
pub mod observability;
pub mod policy;
pub mod pricing;
pub mod projection;
pub mod provider;
pub mod replay;
pub mod retry;
pub mod sandbox;
pub mod state;
pub mod store;
pub mod tools;
