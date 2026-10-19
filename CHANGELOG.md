# Changelog

## Unreleased

Changes landed after v0.6.5. No version bump yet: these are engine-only changes,
and releasing them as `0.6.6` would invalidate `mobile-ffi/Cargo.lock` and imply a
mobile app rebuild — that is a release decision, not bookkeeping.

### Retry 529 as overloaded (P3-2, `1a45671`)

`retriable_status` in both `provider/openai.rs` and `provider/anthropic.rs` was
`408 | 429 | 500 | 502 | 503 | 504` — missing **529**. Upstream
`agent/error_classifier.py` classifies `overloaded = 503/529 → backoff`, so an
overloaded upstream returned a hard error instead of being retried. Added `| 529`
to both (the only two retry lists).

### Assistant reasoning is now persisted (P3-1, `f528e3f` + mobile-ffi `bd04e8d`)

Reasoning was received and then discarded: `ProviderResponse` and stream deltas
carry it (`openai.rs` already merges `reasoning_content`/`reasoning`/
`reasoning_details`), but `Message` had no field and the `messages` table no
column, so every turn's chain of thought vanished on store.

- `Message.reasoning: Option<String>` (`skip_serializing_if = Option::is_none`, so
  existing messages serialize byte-identically).
- `messages.reasoning TEXT`: new DBs via `CREATE TABLE`, existing DBs via an
  idempotent `ensure_reasoning_column` (`PRAGMA table_info` → `ALTER TABLE`),
  mirroring the `display_metadata` precedent. **Already-installed users'
  `state.db` upgrades losslessly.**
- Read back by both paths (`load_messages` and `load_messages_with_timestamps`,
  which feeds export/recap). `load_message_rows` returns the lightweight
  `MessageRow` and correctly needs none.

### Per-provider reasoning echo policy (P3-1b, `669afc5`)

Makes the stored reasoning usable for replay. New module `provider::reasoning_echo`
transcribes hermes' `_REASONING_ECHO_RULES` / `_REASONING_CARRIER_ROUTES` into
three tiers resolved from `(provider name, model, endpoint)`:

- **must-echo** — DeepSeek, Kimi/moonshot hosts, Xiaomi MiMo, Nous Portal
  `stealth/missingno`. Every assistant turn carries `reasoning_content`, padded
  with `" "` when there is no text; `""` is rejected by DeepSeek V4 (#17341), and
  omission is a proven HTTP 400 there.
- **strict** — Mistral, Groq, Cerebras, SambaNova, Tencent Hunyuan. Their
  `additionalProperties: false` schemas 400/422 on any reasoning key (#45655,
  #70233), so they never get one.
- **content** — openrouter, kilocode, nous, fireworks, novita, minimax, tokenhub,
  ollama-cloud, lmstudio, upstage; replays only when non-blank text exists.

🔴 **Deliberate deviation from upstream:** *unknown* routes resolve to **off**, not
to `reasoning_content`. hermes defaults unknown routes to sending it and stays safe
only because it **learns** rejections at runtime
(`record_reasoning_field_rejection`); ulnclaw has no such learner, so unknown
routes stay silent — preserving today's working behaviour instead of risking a
400/422 on a provider that currently works. Porting the runtime learner would
allow relaxing this.

Applied to **assistant turns only**; the route is resolved once per request in both
the streaming and non-streaming paths.

**Fallback needs no reapply step.** hermes requires
`reapply_reasoning_echo_for_provider` after a fallback because it mutates a live
provider; ulnclaw's `build_fallback_provider` constructs a fresh provider per
entry and `name`/`model`/`endpoint` are immutable after construction, so the route
is derived at request time and follows the switch automatically.

**Anthropic is out of scope.** It replays reasoning as *signed* `thinking` blocks;
`signature` appears 0 times in `anthropic.rs` and `messages_to_anthropic` builds no
thinking blocks, so it replays nothing today and remains correct. hermes'
`agent/anthropic_thinking_policy.py` (signed replay + durable suppression) is a
separate, larger item.

## v0.6.5 — 2026-10-16

### Tool-naming alignment with hermes-agent v0.22.2 (P1 + P2a)

hermes-agent renamed several core tools in the v0.22.0 "Threshold Release"
(`todo` → `todo_list`, `cronjob` → `cronjob_manage`, `process` → `process_manage`)
and consolidated the preview tool into a `desktop_ui` toolset as
`desktop_preview`. ulnclaw now advertises the same canonical names.

- **Legacy aliases, not a breaking rename.** `LEGACY_TOOL_ALIASES` +
  `canonical_tool_name()` resolve the old names at every dispatch seam
  (`get`/`has`/`dispatch`), so old sessions and saved transcripts keep working.
  Aliases never enter the registry, so `definitions()`/`names()` advertise only
  the new names — the model never sees a duplicate tool, and no tokens are wasted.
  Mirrors hermes `model_tools._LEGACY_TOOL_ALIASES`.
- **`desktop_preview`** replaces `open_preview` (alias retained). Gains an
  optional `action` parameter (`enum=[open]`, default `open`); `close`/`read`
  are intentionally *not* stubbed — they need Electron bridge wiring that this
  build does not verify, so they are deferred rather than shipped hollow.
- **Model-visible text corrected:** the `terminal` tool description, the cron
  blueprint prompt seed, and `learn_prompt`'s tool list now name the new tools.
- **Deliberately unchanged** (verified by point, not blind-replaced): the kanban
  status literal `"todo"` and the usage-JSON field `"process"` share spellings
  with the renamed tools but are unrelated.
- **Scope note:** the remaining hermes-v0.22.2 tools (`set_theme`, `show_tip`,
  `gui_tour`, `setup_beat`, `manage_catalog`, `manage_connections`,
  `annotate_preview`, `read_window_below`, `drive_preview`, `desktop_project`,
  `browser_vault`, `browser_exec`) are desktop-gated in hermes
  (`platforms={"desktop"}` / `CLIENT_SURFACE_TOOLSETS`) and are **not** part of
  the mobile-ffi surface (which registers only the 7 phone-safe families), so
  they are out of scope for this release. The parity matrix body remains a
  v2026.8.3 audit; only the tool-naming layer tracks v0.22.2 here.
- **Tests:** `tools::tests` (alias mapping, idempotency, aliases-never-leak-to-schema,
  dispatch-via-alias, disabled-toolset-blocks-alias, error-echoes-original-name)
  plus the existing desktop suite. Verified on Linux (the Unix-only upstream test
  target does not compile on Windows).

## v0.6.4 — 2026-10-05

### Outbound mTLS (client certificates)

- **New: `ULNCLAW_MTLS_*` egress identity.** Deployments behind an
  mTLS-fronted gateway (a control plane that authorises/meters calls) can now
  present a client certificate on outbound HTTPS instead of staying
  unauthenticated. `ULNCLAW_MTLS_CERT` (+ `_KEY` when the key is not embedded),
  `ULNCLAW_MTLS_CA` for the gateway's private CA, `ULNCLAW_MTLS_TRUST_BUILTIN=0`
  for pin-only trust, `ULNCLAW_MTLS_HOSTS` to scope the identity, and
  `ULNCLAW_MTLS_MIN_TLS` (1.2 default / 1.3). Resolved from the process
  environment or `~/.ulnclaw/.env` on every client build.
- **Fail-closed.** A certificate without a key, an unreadable PEM, an
  unsupported min-TLS value, or an identity without `ULNCLAW_MTLS_HOSTS` is a
  hard error on the egress path — never a silent downgrade to anonymous TLS.
  (Presenting a client certificate to *any* host that asks for one is a
  credential leak, so the scope must be written down explicitly, `*` included.)
- **New CLI: `ulnclaw mtls status|probe|keygen|csr`.** `status` prints the
  resolved policy plus the leaf SHA-256 fingerprint (never key material);
  `probe <url>` contacts the endpoint with and without the identity (the
  anonymous arm also sends one `GET /`, because a TLS 1.3 server may complete
  the handshake and only verify the certificate afterwards) and reports
  `enforced` / `not-required` / `rejected` / `identity-missing` with a `reason` —
  evidence, not an assumption — and states whether the target is inside
  `ULNCLAW_MTLS_HOSTS` (`host_in_scope`), since the probe presents the identity
  on purpose while the running agent would not;
  `keygen`/`csr` bootstrap certificates via the system openssl.
- **Wired:** `src/provider/openai.rs` (OpenAI-compatible chat), `src/provider/anthropic.rs`,
  `src/stt.rs`, `src/tts.rs`; `outbound_tls::apply_env` / `client_for` for other
  reqwest call sites and `OutboundTls::rustls_config()` for raw-rustls paths.
- **Tests:** unit tests for scope matching, fail-closed validation, PEM loading
  and fingerprinting, plus `tests/outbound_mtls_test.rs` — a loopback rustls
  server that *requires* a client certificate: identity accepted, anonymous
  refused, probe verdicts checked in both directions.
- **Docs:** `docs/design/outbound-mtls.md` (design + operations),
  provider guides (en/zh), parity matrix note (this is a deliberate ulnclaw
  extension — the hermes parity baseline has no outbound client certificates),
  README feature row.

### Fixes found by the end-to-end run

- `--san` is now case-insensitive (`dns:` == `DNS:`), normalised to the openssl
  spelling before it reaches `openssl req`. The `--help` text and the design note
  both showed lowercase, so the documented example used to fail validation.
- Scope matching (`ULNCLAW_MTLS_HOSTS=*.example.com`) requires a label boundary:
  `evilinternal.example.com` is no longer matched by `*.example.com`, and the
  identity is therefore never presented to it (fail-closed).
- A rustls `CryptoProvider` is installed explicitly before building client/server
  configs (process-wide default plus the outbound-TLS entry point), so mTLS works
  in binaries that never initialise a provider themselves.
- The mTLS probe now distinguishes "handshake completed but the request was
  refused" (TLS 1.3 post-handshake certificate check, e.g. openssl's
  `CertificateRequired` alert) from "endpoint served us anonymously", which is
  the difference between `enforced` and `not-required`.


## v0.2.0 — 2026-08-10

Hermes v2026.8.3 parity milestone.

### Hermes feature parity

- **50+ built-in tools** — terminal/process, file read/write/patch/search,
  web search/extract, X search, video understanding, memory, todo, session
  search, clarify, skills, delegation (background semantics + persistent
  async registry), execute_code, cronjob, vision, image generation, video
  generation, desktop projects, Discord server tools, Feishu docs, Spotify,
  Yuanbao, cross-channel `send_message`, learning timeline, TTS, Home
  Assistant, kanban, tool search.
- **MCP client** — stdio + Streamable HTTP/SSE, OAuth 2.1 + PKCE, lazy
  servers, malware preflight, catalog one-click install, dashboard
  management with OAuth brokering.
- **Browser automation** — 12 `browser_*` tools over CDP, managed headless
  Chrome, Camofox, Browserbase/Browser Use/Firecrawl cloud sessions,
  hermes-grade SSRF guards, forced secret redaction.
- **Computer-use** — cua-driver backend, status/doctor, settings surface.
- **Secrets vaults** — command / Bitwarden Secrets Manager / 1Password
  sources with full hermes precedence semantics, TTL cache, AES-GCM
  encrypted Bitwarden cache, setup wizards, gateway sync action.
- **Skills sync** — device identity, pull/push/now, enable/disable, remote
  manifest, gateway pull/push actions.
- **Messaging** — 26-platform catalog with posture, loops with bounded
  retry, cross-channel messaging, Doctor env-value editor.
- **Plugins + shell hooks** — lifecycle hooks with consent gate, plugin
  install/enable/update, hook doctor.
- **OAuth** — device-flow login + account sync surfaces.
- **Egress firewall** — managed iron-proxy sandbox outbound proxy.
- **Monitoring** — gateway health/diagnostic events, OTLP export, redaction.

### Gateway

- OpenAI-compatible `/v1/chat/completions` + `/v1/responses` with SSE
  streaming, async `/v1/runs` with approval resolution, sessions API with
  per-session model lock, 100+ `/api` management endpoints.

### Desktop (Tauri 2)

- Chat UI with session management, settings dialogs, Doctor diagnostics
  (30+ panels), themes + fonts, quick entry, deep links, tray integration,
  native notifications, update checks, gateway log viewer, single-instance
  handoff, quit guard.

### Continuous delivery

- Tag pushes run GitHub Actions (`.github/workflows/release.yml`):
  Windows NSIS installer + macOS dmg (Apple Silicon and Intel) built
  with the core binary bundled inside, checksums published, assets
  attached to the release (P805).

### Verification

- 2399 lib tests passing; desktop `tsc --noEmit` + vite build green.

## v0.1.0

Initial development series (P1–P336): core agent loop, tool registry,
gateway foundations, CLI surfaces.
