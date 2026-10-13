# Truss Testing Strategy

> Status: active. Captured 2026-06-17. Principle: **our testing IS our
> marketing.** We sell "verified, auditable, can't-silently-rot." Every guarantee
> we sell needs a test category whose passing means *an adversary or edge case
> cannot break the guarantee* — not just that the happy path works. A buyer/
> acquirer doing diligence will open this suite; it must make the claims true.

## The trust ladder → test category mapping

| Guarantee we sell | Test category that defends it |
|---|---|
| `Verified` means a panel signed off | invariant/property test (unreachable any other way) |
| Ledger is tamper-evident | adversarial tamper suite (corrupt/forge/replay MUST fail verify) |
| Budget is bounded across a spawn tree | property test over random tree shapes |
| Memory can't silently rot | invariant: Unverified/Rejected/Stale never in trusted view |
| Everything has provenance | property test: every entry has a valid LedgerRef |
| Works on real models | gated live smoke + nightly eval corpus |

## Current base (good, keep)

- ~107 hermetic tests (fake provider), unit + per-feature integration.
- Gates every PR: `fmt --check`, `clippy --all-targets --all-features -D warnings`,
  `test`, `test --all-features`, `release` build. ALINA re-runs on merge.

## Priority additions

### P0 — Adversarial tamper suite (the diligence question)
Prove `--verify` CATCHES bad chains, not just passes good ones. Negative tests
that MUST fail verification: corrupt an entry hash, flip a signature byte,
reorder/replay an entry, forge a verdict, truncate the chain, swap a child anchor.
Same idea for the panel: a crafted tool output / result must not pass review as
correct. This is the single most important category for a provenance product.

### P1 — Property/invariant tests (proptest)
Randomized histories assert the moat invariants hold for ALL inputs:
- `Verified` ⇒ a signed panel verdict exists (no other path sets it)
- hash-chain validates after any write sequence
- child budget never exceeds parent ceiling, any tree shape
- Unverified/Rejected/Stale never surface in the trusted view
- every entry carries a resolvable provenance ref

### P2 — Live smoke + nightly eval corpus
- Gated live-smoke job (behind API keys, pre-release): close the known gap that
  `openai-codex` was never tested against the live backend.
- Nightly `truss eval` over a small contract corpus → track pass-rate + cost;
  regressions surface automatically. Turns SI-4 into a standing quality gate.

### P3 — Confidence amplifiers (periodic, not per-PR)
- **Mutation testing** (`cargo-mutants`): prove tests KILL bugs, not just pass.
  Highest signal that the suite is real.
- **Concurrency stress**: fan-out/spawn budget + cancellation races.

## Sequencing
P0 + P1 ship together (shared proptest harness; both back the sales claims) →
P2 (CI/nightly + live smoke) → P3 as periodic confidence runs.
