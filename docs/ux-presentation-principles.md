# UX & Visual Presentation Principles (carry-forward)

> Status: durable note, no UI yet. Captured 2026-06-17 (Matt): "we can't overlook
> visual presentation — how it looks to the user is sometimes as important as
> functionality. We don't have a UI yet but keep this in mind." This doc exists so
> the principle isn't lost before ClawTerm (the planned Warp-based front end) is built.

## The core idea

For a trust product, *presentation is not decoration — it's how trust becomes
felt.* A signed ledger nobody can read is worth little; the same proof rendered
legibly is the product's most persuasive moment. Our unique UX surface is **proof
legibility**: making verification, cost, and provenance immediately understandable.

## Principles to carry into ClawTerm (and any front end)

1. **Show proofs, not logs.** The hero view is the verdict + what the panel
   checked + the cost — not a raw transcript scroll. The ledger should be
   browsable and visually obviously tamper-evident (chain intact / broken).
2. **Make trust tiers visible.** Verified vs Unverified vs Stale should read at a
   glance (color/badge), never buried in text. The trust label is the product;
   surface it.
3. **Cost & budget always in view.** Spend, budget remaining, and per-step cost
   are first-class UI, tied to the local-first/cost-control story.
4. **Verification should feel tangible.** A `--verify` pass (and a tamper
   *catch*) should be a satisfying, legible visual event — this is the demo
   moment that beats any competitor's transcript.
5. **Spawn/fan-out trees are visual.** Sub-agent trees, which branch won, and why,
   should render as a structure, not prose.
6. **Legible failure.** When something fails, the message is actionable and
   visually distinct from success. Error legibility is a measurable UX metric
   (see testing-strategy + the sim-user harness).

## How this connects to testing

The sim-user harness and instrumented UX proxies (turns-to-success, time-to-first-
useful-output, error legibility, proof legibility) measure the *interaction* layer
now. Visual-presentation UX gets validated with a small-N human comparative pass
once ClawTerm is real. Keep these principles as acceptance criteria for that UI.
