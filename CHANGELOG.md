# Changelog

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
