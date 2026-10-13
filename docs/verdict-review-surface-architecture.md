# Verdict-Review Surface / Interactive TUI (Phase 4)

*Status: spec. Created 2026-06-22 from `docs/BUILD-PLAN.md` Phase 4.*

Truss's moat is "review proofs, not transcripts." That moat is only realized if a
human can actually look at the proofs and act on them quickly. Phases 1–3 built the
signed evidence (durable journal, N-reviewer panel, verdict bundles, diff-aware
review, governed memory + verified playbooks) and the **read** surface for it
(`LedgerReader` + `truss ledger list|show|get|verdict|export|recall` with stable
`--json`). Phase 4 builds the **human-facing review and sign-off** surface on top of
that read API.

## North-star "done when"

Matt can approve or reject a real Truss task from the verdict view — seeing the
definition-of-done clauses, the independent evidence, the reviewer panel result, the
diff, and the ledger chain/head status — **without reading the full transcript**, and
his decision becomes part of the signed chain.

## Design decisions

1. **In-repo interactive TUI first, external front-ends via the JSON seam.** We build
   a `ratatui`/`crossterm` TUI inside `truss-cli` as the first concrete surface
   because it is self-contained, testable, and hits the "done when" directly. The
   external ClawTerm/Warp front end consumes the **same** stable `--json` read surface
   (already shipped) plus the same core write primitives — we are not building a
   bespoke protocol. The TUI is a *presentation/controller* over `LedgerReader` and
   `Store`; it never becomes the provenance substrate.

2. **Provenance discipline (the load-bearing rule).**
   *Provenance-capturing = core. Stream-consuming = plugin/CLI.*
   - The human decision (approve / reject / needs-changes) is provenance → it lives in
     **core** as a new signed journal entry (`human_review`) on the same hash chain,
     verified by `verify_chain`.
   - The screens, tabs, scroll position, note input, and layout are presentation → they
     live in the **CLI/TUI** and are never written to the ledger.

3. **Human sign-off attests to a head hash.** A `human_review` entry records the
   decision, an optional note, the reviewer's signing key id, and the **session head
   hash it is attesting to** at decision time. If later entries change the head, the
   sign-off is still cryptographically pinned to exactly the state the human reviewed —
   you can prove what was approved, and detect approve-then-mutate.

4. **No fake authority.** A `human_review` decision is recorded and signed, but it does
   **not** silently overwrite the machine `Verdict`. The verdict bundle shows both: the
   independent machine verdict (panel + checks) and the human sign-off. They are
   distinct trust signals; the UI presents them side by side. (Default-to-fail is
   unchanged: absence of a human approval is not an approval.)

5. **TUI state is not durable.** Underlying runs are durable (journal authoritative);
   the review window is not. Don't engineer UI state into the ledger.

## Milestones (one PR + one ADR section each)

### VR-1 — signed human-review primitive (core + non-interactive CLI). FOUNDATION.
- New journal entry kind `human_review { decision: Approved|Rejected|NeedsChanges,
  note: Option<String>, attested_head_hash: String, reviewer_key_id: String }`, signed
  and chained like every other entry; `verify_chain` covers it (incl. tamper tests).
- Non-interactive CLI first (scriptable, testable without a terminal):
  `truss review approve|reject|needs-changes <session> [--note ...] [--json]`.
- `LedgerReader` gains read access to human-review entries; verdict bundle + `truss
  ledger verdict` render the human sign-off alongside the machine verdict.
- Honest boundary: this is the trust primitive; no TUI yet. ADR 0034.

### VR-2 — read-only interactive TUI (`truss review` / `truss tui`).
- `ratatui` + `crossterm`, feature-gated (`tui`) so headless/CI builds stay lean.
- Screens, all backed by `LedgerReader`: (1) list of recent/active contracts with
  machine verdict + human-review status; (2) verdict bundle detail — DoD clauses,
  clause evidence, self-validation evidence, reviewer panel majority + per-reviewer
  verdicts, diff summary, budget; (3) ledger chain view with head hash + verify status;
  (4) transcript drill-down (not the default view).
- Pure consumer; no writes. Hermetic render tests over a fixture DB (ratatui
  `TestBackend`).

### VR-3 — actions in the TUI.
- Wire approve / reject / needs-changes keybindings (`a` / `r` / `n`) to the VR-1
  signed primitive.
- Add confirmation + note capture; the note is persisted only as the core
  `human_review.note` payload.
- Add suspended-run signal control (`s`) by routing through `Store::add_signal`, plus
  a resume command control (`x`) that points back to the existing durable runner
  (`truss --resume <session> --db <db>`). The TUI does not duplicate provider/tool
  setup or create a second resume engine.
- Everything routes through core (`Store`); no UI-side provenance.

### VR-4 — approval gates. SHIPPED.
- High-risk tool calls or spend over a configurable threshold **suspend** the durable
  run pending a human `human_review` decision (reuses the suspend/signal substrate).
- Configurable (Directive 1): gate policy global→agent→contract; default off.
- Honest no: don't gate everything; gate is opt-in and policy-driven.
- Release/block semantics (ADR 0037): approved `human_review` entries release the
  pending gate by queueing the existing approval-gate signal; rejected and
  needs-changes entries stay signed in the chain but keep the run suspended. A
  raw signal without the signed approval does not release the gate.

## Lens (per BUILD-PLAN discipline)

- **Verification / crypto:** human decision is a signed, chained, head-pinned entry
  (yes for the decision; the TUI rendering itself is not provenance).
- **Pause / resume:** underlying runs are durable; VR-3 can deliver pending signals and
  points to the existing resume command; VR-4 gates ride the existing suspend/signal
  substrate; UI window state is presentation, not durable. Approval gates are
  signed `suspended` ledger entries waiting on an approval signal that only an
  approved `human_review` decision can release.
- **Honest no:** the TUI does not replace the machine verdict, does not invent a
  protocol (reuses `--json`), and does not store layout/scroll in the ledger.

## Reuse map (what already exists — do not rebuild)

- `truss_core::ledger::LedgerReader` and its view structs (verdict bundle, evidence,
  panel result, verify report, export bundle).
- `truss ledger list|show|get|verdict|export|recall` + stable `--json`.
- `identity::Signer` (sign the human-review entry), `Store`/`verify_chain` (chain it),
  suspend/signal substrate (VR-3/VR-4), `docs/ux-presentation-principles.md`
  (proof-legibility principles to carry into the screens).
- `approval::ApprovalGatePolicy` mirrors the existing layered-config pattern:
  global defaults, per-agent overrides, then per-contract overrides.
