# Auth strategy — subscription vs API key (research capture, 2026-06-15)

Findings from investigating how to authenticate to model providers, especially via
a **subscription** (Claude Pro/Max, ChatGPT Plus/Pro) instead of pay-as-you-go API
keys. Informs ADR 0001 (plugins) and the auth abstraction.

## The three mechanisms

- **(a) Drive the vendor CLI/SDK as a subprocess.** Compliant; the official client
  owns OAuth login/refresh. Necessarily per-vendor. (OpenClaw's `claude-cli` and
  Codex app-server runtimes; our PR #10 prototype.)
- **(b) Replay the vendor's OAuth token against its HTTP endpoint** ("subscription
  like an API key"). Works, but **not provider-agnostic** and per-vendor fragile.
- **(c) API keys only.** Clean, multi-vendor, always-supported. The safe baseline.

**Bottom line:** only OAuth *login/refresh* generalizes. The inference call is
irreducibly per-vendor (different host, wire API, mandatory first-party headers).
There is no universal "swap base URL + bearer" shim.

## OpenAI / ChatGPT (Codex) — mechanism (b) is viable, tolerated
- Token: `~/.codex/auth.json` → `tokens.{access_token, refresh_token, id_token,
  account_id}`; `auth_mode`. Access-token JWT: `iss=https://auth.openai.com`,
  `aud=https://api.openai.com/v1`, claims namespaced `https://api.openai.com/auth`.
- Inference: **POST `https://chatgpt.com/backend-api/codex/responses`** (Responses
  API, SSE, `store:false`). Headers: `Authorization: Bearer <access_token>`,
  `ChatGPT-Account-ID: <account_id>`, `originator: codex_cli_rs`,
  `OpenAI-Beta: responses_websockets=2026-02-06`, `session-id`/`thread-id`.
- Refresh: POST `https://auth.openai.com/oauth/token`,
  `client_id=app_EMoamEEZ73f0CkXaXp7hrann`, `grant_type=refresh_token`.
- Status: reverse-engineered, **tolerated for personal use** (semi-endorsed by
  OpenAI). No replay block to date. Rate-limited to the ChatGPT plan window.

## Anthropic / Claude Pro/Max — mechanism (b) is BANNED
- Token: `~/.claude/.credentials.json` → `claudeAiOauth.{accessToken, refreshToken,
  expiresAt, scopes, subscriptionType, rateLimitTier}` (Keychain on macOS).
- Inference: `POST https://api.anthropic.com/v1/messages` with
  `Authorization: Bearer <accessToken>`, **`anthropic-beta:
  claude-code-20250219,oauth-2025-04-20`**, `anthropic-version: 2023-06-01`, **no**
  `x-api-key`, and a required first system block `"You are Claude Code,
  Anthropic's official CLI for Claude."`
- Refresh: `https://platform.claude.com/v1/oauth/token`,
  `client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e`, 8h expiry.
- Status: **prohibited.** ToS amended **2026-02-19** to ban Pro/Max OAuth
  credentials in any non-Claude-Code tool (incl. the Agent SDK); blocked
  server-side; accounts banned. OpenCode and Crush were forced to remove it.
  → For Anthropic subscriptions, use mechanism **(a)** (official CLI) or **(c)**.

## What others do
- **pi** (`earendil-works/pi`, the minimal harness we resemble): mechanism **(b)** —
  its own OAuth flow + token replay. Its tracker shows the Anthropic path breaking
  / being per-token billed — i.e. the approach now under pressure.
- **OpenClaw:** hybrid mechanism **(a)** — `claude-cli` runtime + Codex app-server
  runtime, plus a ChatGPT-responses token transport. All per-vendor.
- **OpenCode / Crush:** had **(b)**; Claude path removed at Anthropic's request.
- **aider:** **(c)** only.

## Product implications
Shipping a tool that replays *Anthropic* subscription tokens violates ToS and can
get users banned (since 2026-02-19). Safe matrix for a distributable product:
- **API keys** — always.
- **OpenAI subscription** — via token replay, with a personal-use caveat.
- **Anthropic subscription** — official CLI only, or omit.

## Design consequence
Auth is an `Auth` strategy behind the `Provider`, and every strategy is a **plugin**
(ADR 0001): `auth-apikey` (default), `auth-oauth` (generic login/refresh + tiny
per-vendor descriptor; use for OpenAI), `runtime-cli` (compliant Anthropic path).
Per-vendor, ToS-sensitive logic stays out of the core.

### Sources
`github.com/openai/codex` (codex-rs), `code.claude.com/docs/.../authentication`,
`github.com/earendil-works/pi`, `github.com/sst/opencode` (#417),
`charmbracelet/crush` (PR #1783), theregister.com 2026-02-20,
`simonw/llm-openai-via-codex`.

---

# Deep dive (2026-06-15, round 2): Codex vs pi vs OpenClaw, read from source

Three parallel investigations (reading `openai/codex` Rust source, `earendil-works/pi`
source, and the locally-installed `openclaw` `dist/` + docs) sharpened the picture
and surfaced one architectural insight the matrix above was missing.

## Corrections / sharper facts

- **OpenAI CAN fingerprint third-party clients — "no replay block to date" is too
  optimistic.** Codex issues #14215 (non-official OAuth client passes
  authorize/callback but gets **403 at `/oauth/token`**, while official codex-cli
  works on the identical account/network) and #2951/#14215 (third-party calls hit
  **429 quota**) show the auth/quota layer *can* distinguish clients. OpenAI hasn't
  slammed the door like Anthropic, but it has the same technical precursors Anthropic
  used right before it did. Treat OpenAI subscription replay as **"unclear and
  revocable," not "safe."** Defensible only as single-user, bring-your-own-subscription,
  personal-use — never multi-tenant/commercial.
- **Codex programmatically mints a real API key** from the login: after the PKCE
  code exchange it does an RFC 8693 token-exchange (`grant_type=…token-exchange`,
  `requested_token=openai-api-key`) and stores the result as `OPENAI_API_KEY` in
  `auth.json` (falls back to the ChatGPT access token if the account lacks the Codex
  entitlement). Source: `codex-rs/login/src/`.
- **pi confirms mechanism (b) in-process, with client impersonation.** pi runs its
  own PKCE OAuth (`packages/ai/src/utils/oauth/`), then in `providers/anthropic.ts`
  detects an OAuth token and injects **Claude Code identity** — `anthropic-beta:
  claude-code-20250219,oauth-2025-04-20` + rewrites system/tool messages to the
  "Claude Code" identity. Anthropic now meters this as per-token "extra usage"
  (pi issue #3670) — i.e. the replay path is actively under pressure, exactly as
  predicted. **This is the pattern we do NOT copy.**
- **OpenClaw uses two *different* shapes, not one.** `claude-cli` = a **CLI
  subprocess backend**: it spawns the real `claude` binary (`-p --output-format
  stream-json … --resume {sessionId}`), and **clears all Anthropic key/OAuth env
  vars** (`CLAUDE_CLI_CLEAR_ENV`) so the CLI falls back to its *own* stored
  subscription login — **zero token replay by OpenClaw.** Codex = an **embedded
  harness over a long-lived app-server stdio process** holding a stored Codex OAuth
  profile (avoids per-turn subprocess spawn latency). Both are per-vendor.

## The insight the matrix missed: vendor CLIs are *agents*, not endpoints

`claude` (Claude Code) and `codex` are not raw "messages in → one completion out"
endpoints. They are **full agents** that own their inner loop: they run *their own*
tools, *their own* context compaction, and *their own* session state. OpenClaw's
`claude-cli` backend therefore sets `ownsNativeCompaction: true` and
`nativeToolMode: "always-on"` — it **cedes the loop** to the CLI and can only
observe.

That collides head-on with Truss's whole thesis. Truss's value is that **we** own a
*durable, signed, independently-verified* loop. If we get subscription auth by
shelling out to Claude Code in print mode, Claude Code runs the tools and the loop —
our per-step journal, policy enforcement, and tool sandbox don't apply to the inner
work. You can't have both "the vendor CLI authenticates and drives" and "Truss owns
every step" at the same time.

## Resolution: two integration depths, one trust boundary

Don't force every provider into one mold. Truss supports a provider at one of two
depths, and **verification happens at the contract boundary either way**:

1. **Inference provider** (we own the loop). Raw model endpoint: API keys + local
   servers (Ollama/vLLM/etc.). Full Truss stack — per-step durable journal, policy,
   tool sandbox, reviewer panel. This is the registry we already built.
2. **Delegated agent** (the vendor owns the inner loop). Subscription via the vendor
   CLI/app-server, env-cleared so the *vendor's own client* authenticates — the
   ToS-clean path, no token replay by us. We **don't** journal the inner steps; we
   wrap at the **contract boundary**: record the contract, let the delegated agent
   execute, then run our *independent* checks + N-reviewer panel on the result and
   sign the verdict. We lose inner-step replay; we keep the thing that matters most —
   **verified delegation** — and we stay ToS-clean.

So subscription auth doesn't weaken the moat: a delegated-agent run is still a
*contract* that produces a *signed, independently-verified verdict*. It just trusts
the vendor for the inner loop instead of journaling it.

## Recommended Truss design (still: no code yet)

- **Keep the in-process registry for inference providers** (API key / local). Done.
- **Subscription auth = a `runtime-cli` / delegated-agent tier** that shells out to
  the vendor's own CLI/app-server with credentials cleared (OpenClaw's model), behind
  the **same `Provider`/registry seam** — i.e. ADR 0002's `ProviderConfig` grows an
  `auth`/`runtime` variant; the CLI never learns a new shape. Per-vendor, ToS-robust,
  no token storage/refresh/replay on our side.
- **Explicitly reject in-process subscription-OAuth replay (pi's approach).**
  Anthropic: banned. OpenAI: grey/revocable — at most a personal-use,
  bring-your-own, off-by-default flag, never the default and never multi-tenant.
- **Anything commercial/multi-tenant routes to API keys** — the documented, stable,
  sanctioned path.

### Round-2 sources
codex: `github.com/openai/codex` (`codex-rs/login/src/`), discussions/8338,
issues/14215, developers.openai.com/codex/auth, simonwillison.net/2026/Apr/25/romain-huet.
pi: `github.com/earendil-works/pi` (`packages/ai/src/utils/oauth/`,
`providers/anthropic.ts`, README), issue #3670.
openclaw: local `openclaw@2026.6.5` — `docs/concepts/agent-runtimes.md`,
`docs/plugins/cli-backend-plugins.md`, `dist/cli-backend-*.js`, `dist/cli-shared-*.js`.

---

# Working assumption & receipts (2026-06-15)

## Receipt: the billing surface is volatile (live example)
On 2026-06-15 Anthropic emailed ("**We're pausing the Agent SDK credit change**"):
*"The change today is on hold. Your SDK and `-p` usage keeps working as it did
before today, and we'll reach out with further updates."* They had a change
scheduled to land **that same day** that would have altered how the **Agent SDK**
and **`claude -p --model claude-opus-4-8`** (print mode) consume credits — i.e. metering Max/subscription
usage through those paths as paid usage — and walked it back at the last minute.
Note `claude -p --model claude-opus-4-8` is exactly the mechanism the **delegated-agent / `runtime-cli`**
tier rides on, so even the ToS-clean subprocess path depends on billing terms the
vendor can change unilaterally. This is the concrete receipt for "subscription
billing is unclear and **revocable**, not stable."

## Decision (Matt, 2026-06-15): proceed assuming 3rd-party subscription stays supported
**Working assumption:** subscription usage through 3rd-party apps **will be
supported for the near future**; we build for it now and **adjust when the time
comes**. We do *not* gate the roadmap on revocation fear.

What this changes and doesn't:
- **Does change:** subscription auth is an explicit, supported roadmap feature (not
  a someday-maybe). Both the delegated-agent subprocess path *and* — where a vendor
  tolerates it — in-process OAuth are on the table as plugin tiers.
- **Doesn't change the architecture:** still behind the registry/plugin seam
  (ADR 0002), still per-vendor, ToS-sensitive logic out of core. So if a vendor
  flips (like Anthropic nearly did today), we disable/swap **one plugin**, and the
  durable+verified core and API-key baseline are untouched. The plugin boundary *is*
  the "adjust when the time comes" mechanism.
- **Still true:** API keys remain the stable default; multi-tenant/commercial
  deployments route to API keys; subscription is opt-in. The assumption lets us
  *build* the subscription tiers, not *depend* on them.
