# Browser CDP Client & Gateway Browser Control Plane

> **Audience:** contributors working on the `browser_*` tools, gateway operators pointing agents at browsers
> **Source files:** `src/browser/mod.rs` (endpoint resolution, `CdpClient`, `BrowserSession`, managed launch), `src/browser/connect.rs` (local attach), `src/browser/guard.rs` (SSRF guards), `src/browser/camofox.rs` (REST backend), `src/browser/cloud.rs` (cloud browser providers), `src/gateway/mod.rs` (`/v1/browser/*`)
> **Related:** `docs/en/hermes-parity.md` (browser rows), `docs/design/multiplexing-gateway.md`
> **Hermes parity:** `tools/browser_tool.py`, `tools/browser_cdp_tool.py`, `tools/browser_camofox.py`, `hermes_cli/browser_connect.py`, `agent/browser_provider.py` + `agent/browser_registry.py` + `plugins/browser/*` (v2026.8.3)

## Overview

The 12 `browser_*` tools (`browser_navigate`, `browser_snapshot`,
`browser_click`, `browser_type`, `browser_scroll`, `browser_press`,
`browser_screenshot`, `browser_evaluate`, `browser_console`, `browser_dialog`,
`browser_raw_cdp`, `browser_page_info`) ride a Chrome DevTools Protocol (CDP)
WebSocket client implemented natively in Rust. Three backends exist behind
one tool surface:

1. **Local CDP** — an existing DevTools endpoint, or a gateway-managed
   headless Chrome/Chromium (`ULNCLAW_BROWSER_CDP=auto` launches and
   supervises it);
2. **Remote CDP** — any `ws://`/`wss://` browser endpoint (cloud browsers,
   another machine's debug port);
3. **Camofox REST** — anti-detection browser via the Camofox-browser
   Node.js server (`CAMOFOX_URL`), which maps its REST API 1:1 onto the
   tool interface.
4. **Cloud browser providers** — Browserbase, Browser Use, and Firecrawl
   sessions created on demand (`src/browser/cloud.rs`), each handing back a
   CDP websocket URL that rides the same client as any remote endpoint.

Backend priority (hermes semantics): a **live override** set at runtime
wins, then a **configured CDP endpoint** (`ULNCLAW_BROWSER_CDP` env >
`[browser] cdp_url` config), then Camofox when `CAMOFOX_URL` is set, then
the **cloud provider** (explicit `[browser] cloud_provider` or the legacy
availability walk), else the managed local launch.

## Endpoint resolution

`ULNCLAW_BROWSER_CDP` (or the `[browser]` config equivalent) accepts:

| Form | Meaning |
|---|---|
| `ws://...` / `wss://...` | used directly as the browser-level WS endpoint |
| `http://host:port` | discovery via `/json/version` (browser WS URL) with `/json` fallback |
| `auto` | the supervisor finds a Chromium-family binary and launches it headless with a debug implementation, then attaches |

Resolution chain (hermes `_get_cdp_override` + cloud-mode precedence):

1. live override (REPL `/browser connect`, gateway `POST /v1/browser/connect`);
2. `ULNCLAW_BROWSER_CDP` env var;
3. `[browser] cdp_url` from config.toml;
4. `auto` (absent/blank/`auto`/`launch`/`managed`): cloud provider session
   if one resolves, otherwise the managed local launch.

Runtime state lives in a process-global override slot:

- `set_cdp_override(url)` — verify-then-commit a live endpoint;
- `clear_cdp_override()` — revert to env/config resolution;
- `endpoint_with_source()` — `(source, raw)` for status reporting, where
  source ∈ `override` / `env` / `config`.

## CdpClient — one socket, multiplexed

`CdpClient::connect(ws_url)` owns a single WebSocket and runs:

- a **request/response pump** — monotonically increasing ids, pending-map
  with per-call timeouts (`call(method, params)`);
- an **event fan-out** — CDP events (`Page.*`, `Target.*`, ...) broadcast to
  prefix subscriptions (`subscribe("Page.")`), which is how dialogs,
  navigation events, and target lifecycle reach the session layer;
- `notify` for fire-and-forget methods.

`BrowserSession::open(endpoint)` attaches at the browser-level endpoint and
either adopts an existing page target or creates one
(`Target.createTarget`), then enables the domains the tools need. Sessions
are shared across tool calls in a process-wide session slot
(`with_session`), so `browser_navigate` state survives into
`browser_snapshot` and friends.

### Tool semantics

- **`snapshot`** produces a Playwright-style accessibility listing with
  numeric element refs (`[3] button "Submit"`); refs are stable within one
  snapshot and resolve through `DOM.resolveNode` +
  `Runtime.callFunctionOn` for interactions.
- **`click` / `type_text`** resolve the ref, scroll it into view, and
  dispatch real input events (type focuses + sets value + fires input
  events).
- **`press`** maps key names to `Input.dispatchKeyEvent` pairs (incl.
  modifier combos); **`scroll`** uses `Input.dispatchMouseEvent` wheel.
- **`evaluate`** runs JS in the page with a timeout and returns the JSON
  result; **`screenshot`** returns base64 PNG via `Page.captureScreenshot`;
  **`handle_dialog`** answers `Page.javascriptDialogOpening` events.
- **`alive`** pings; dead sessions are torn down and re-established on next
  use.

## Managed launch (`auto` mode)

`find_browser_binary` probes a candidate list — Chrome, Chromium, Brave,
Edge — across macOS app bundles, PATH names, and fixed Linux/Windows install
paths (WSL included). `launch_managed_browser` starts the chosen binary
headless with `--remote-debugging-port`, waits for the debug endpoint, and
records the child for `stop_managed_browser` (called at gateway shutdown).
The managed-launch candidate list mirrors the local-attach list so `auto`
and `/browser connect` find the same browsers.

## Local attach layer (`connect.rs`)

parity with hermes `hermes_cli/browser_connect.py` — the REPL `/browser connect`
default flow (`connect_local_default`):

1. **candidate discovery** — the same browser groups as managed launch,
   including WSL `/mnt/c` install paths;
2. **dual-stack loopback probes** — `is_browser_debug_ready` checks
   `/json/version` → `/json`, and raw TCP connect for
   `ws://…/devtools/browser/…` URLs; `discover_local_cdp_url` tries IPv4
   then `[::1]` (a browser pushed to IPv6-only by an IPv4 squatter still
   attaches);
3. **port arbitration** — `local_port_in_use` distinguishes free-vs-squatted
   ports; `find_free_debug_port` requires bindability on *both* loopback
   stacks;
4. **diagnostics-rich visible launch** — `launch_chrome_debug` records a
   `LaunchAttempt` per candidate (ready / starting / exited / spawn-failed),
   tails stderr into `<home>/chrome-debug/launch-stderr.log`, recognizes the
   exit-0 single-instance absorption case (Chrome handed the URL to an
   already-running instance), and falls back to
   `manual_chrome_debug_command` (incl. the macOS `open -a` form).

On success the live override is set and the REPL injects the hermes system
note announcing the connected browser; `/browser disconnect` clears the
override and injects the revert note.

## Security layer (`guard.rs`)

Browser tools are a model boundary — pages can render secrets and SSRF
targets. Guards (all on the shared `url_safety` module):

1. **Sensitive-query block** — URLs embedding API keys/tokens in query
   parameters are refused unconditionally (exfiltration vector).
2. **Cloud-metadata floor** — IMDS/metadata endpoints are refused for every
   backend, unconditionally.
3. **Private-address guard** — active when the browser's network position is
   not a trusted local one (remote CDP endpoint, or containerized terminal)
   and `[security] allow_private_urls` is not set.
4. **Current-page guard** — when the page itself sits on a private address,
   content-touching actions and raw CDP methods are refused; a small
   allowlist (`Browser.getVersion`, `Target.*` attach/detach,
   `Page.navigate/reload/stopLoading`) stays available so the model can
   navigate away.
5. **JS URL-literal screening** — `evaluate`/console payloads are scanned
   for URL literals that would bypass the navigation guards.
6. **Forced redaction** — every browser-originated payload (snapshots,
   console/eval results, raw CDP results) passes through the secret
   redactor before reaching the model.

## Camofox backend (`camofox.rs`)

With `CAMOFOX_URL` set (e.g. `http://localhost:9377`), tool calls route to
the Camofox-browser REST API instead of a local CDP session — accessibility
snapshots with element refs, click/type/scroll by ref, screenshots, 80k-char
snapshot pagination. Notable behaviors: `CAMOFOX_API_KEY` bearer auth,
`CAMOFOX_USER_ID`/`CAMOFOX_SESSION_KEY` identity override + existing-tab
adoption (`CAMOFOX_ADOPT_EXISTING_TAB`), Docker loopback URL rewriting
(`CAMOFOX_REWRITE_LOOPBACK_URLS` + `CAMOFOX_LOOPBACK_HOST_ALIAS`), VNC URL
discovery from `/health`, SSRF private-page guard on reads, and managed
persistence via `CAMOFOX_MANAGED_PERSISTENCE` (stable UUIDv5 profile-scoped
userId). Console/raw-CDP/dialogs report unsupported on this backend.

## Gateway control plane (`/v1/browser/*`)

ulnclaw ops extension over hermes' REPL-only `/browser` UX — lets the
desktop GUI and dashboards steer the browser without a terminal:

| Endpoint | Behavior |
|---|---|
| `GET /v1/browser/status` | current backend (camofox block with availability + VNC URL when in Camofox mode), endpoint + source, `auto` mode flag, managed-running flag |
| `POST /v1/browser/connect` | body `{"url": ...}` — verify the endpoint, then `set_cdp_override`; `auto` re-resolves managed launch; 400 on verification failure |
| `POST /v1/browser/disconnect` | `clear_cdp_override` + stop managed browser if we launched it |

All three sit behind the same bearer auth as the rest of the gateway. In a
multiplexing deployment (`docs/design/multiplexing-gateway.md`) the browser
endpoint state is process-global — one physical browser shared by all
profiles, matching hermes' single-process semantics.

## Design constraints & future work

- **One session slot per process.** Tool calls serialize through
  `with_session`; concurrent turns sharing one browser interleave on the
  same page (hermes parity — hermes is single-session too).
- **Computer-use** (`src/computer_use.rs`) is a separate surface — desktop
  control via the cua-driver daemon, not CDP.

## Cloud browser providers (`cloud.rs`)

parity with hermes `agent/browser_provider.py` + `agent/browser_registry.py`
and the built-in provider plugins (`plugins/browser/{browserbase,
browser_use,firecrawl}`). Each backend implements the `CloudBrowserProvider`
trait — `is_available` (cheap credential check, no network), `create_session`
(returns `CloudSessionInfo`: session name, provider session id, CDP URL,
optional provider-authoritative `expires_at`, feature flags), `close_session`,
and best-effort `emergency_cleanup`.

| Provider | Credentials | Create | Close |
|---|---|---|---|
| `browserbase` | `BROWSERBASE_API_KEY` + `BROWSERBASE_PROJECT_ID` | `POST /v1/sessions` (keepAlive/proxies/advancedStealth/timeout knobs; 402 → retry without keepAlive, then without proxies) | `POST /v1/sessions/{id}` `REQUEST_RELEASE` |
| `browser-use` | `BROWSER_USE_API_KEY` *or* managed Nous gateway (`[browser] use_gateway`) | `POST /browsers` (`X-Browser-Use-API-Key`; managed mode adds `X-Idempotency-Key` + short `{timeout, proxyCountryCode}` payload; reads `cdpUrl`/`connectUrl` + `timeoutAt` expiry) | `PATCH /browsers/{id}` `{"action": "stop"}` |
| `firecrawl` | `FIRECRAWL_API_KEY` (`FIRECRAWL_API_URL`, `FIRECRAWL_BROWSER_TTL` knobs) | `POST /v2/browser` (`{ttl}`) | `DELETE /v2/browser/{id}` |

Selection mirrors hermes `browser_registry._resolve`:

1. `[browser] cloud_provider = "local"` disables cloud mode entirely;
2. an explicit provider name wins regardless of availability — the
   dispatcher surfaces a precise "missing credentials" error instead of
   silently switching backends;
3. otherwise the legacy preference walk (`browser-use` → `browserbase`)
   filtered by availability. Firecrawl is deliberately **not** in the walk:
   it shares its API key with web extract, so a fresh install with
   `FIRECRAWL_API_KEY` must not be silently routed to a paid cloud browser.

Session lifecycle: `with_session` lazily creates the active provider's
session on first tool use and caches it process-wide; a provider-authoritative
expiry (`expires_at`, Browser Use `timeoutAt`) retires the cached session
(best-effort background close) and creates a fresh one instead of
reconnecting to a dead endpoint indefinitely. `main` calls
`shutdown_cloud_sessions()` on exit — the hermes atexit cleanup — so paid
backends never leak orphaned sessions after a clean shutdown.
