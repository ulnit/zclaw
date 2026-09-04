# ulnclaw desktop 🦞

**The desktop app for [ulnclaw](../README.md)** — a faithful, vendored port of
the hermes-agent Electron desktop (v2026.8.3, MIT), backed by the ulnclaw Rust
gateway instead of the hermes Python backend. Same views, same styles, same
interaction logic as hermes desktop: the shell is upstream code renamed, and
the only intentional differences are ~70 lines of backend plumbing across 7
files (see [UPSTREAM-SYNC.md](UPSTREAM-SYNC.md)). Available for **Windows,
macOS, and Linux**.

<table>
<tr><td><b>Sixteen views</b></td><td>Chat, sessions, jobs, usage, models, skills, kanban, projects, runs, webhooks, plugins, pairing, profiles, config, doctor, settings — plus a Ctrl/Cmd+K command palette, dashboard themes/fonts and a language picker.</td></tr>
<tr><td><b>Chat with the full agent</b></td><td>Live turn streaming over a JSON-RPC WebSocket plus HTTP/SSE, streaming tool output, approvals and clarify prompts rendered natively.</td></tr>
<tr><td><b>Terminal &amp; files</b></td><td>xterm.js terminal panes, a right-hand file tree with a git review pane.</td></tr>
<tr><td><b>Self-contained</b></td><td>Installers bundle the statically linked <code>ulnclaw</code> gateway binary — no Rust/Python toolchain needed; first launch works keyless with onboarding.</td></tr>
<tr><td><b>Stays current</b></td><td>Silent whole-shell auto-update (v0.7.1+): app + bundled gateway replaced in one background restart.</td></tr>
</table>

## Install

Prebuilt installers ship on the releases page for every `v*` tag, built by the
`release-desktop` workflow:

- **Windows** — `ulnclaw-<ver>-win-x64.exe` (NSIS, per-user install, directory
  selectable). Fully self-contained: the statically linked gateway binary
  (static CRT) rides inside the bundle.
- **macOS** — `ulnclaw-<ver>-mac-arm64.dmg` (Apple Silicon) /
  `ulnclaw-<ver>-mac-x64.dmg` (Intel). Ad-hoc signed: on first launch use
  right-click › Open, or run `xattr -cr "/Applications/ulnclaw desktop.app"`
  if Gatekeeper reports it as damaged.
- **Linux** — `ulnclaw-<ver>-linux-x86_64.AppImage` (deb/rpm also build).
  Every release ships a `SHA256SUMS.txt`.

## Launching from the CLI

```bash
ulnclaw gui              # alias: ulnclaw desktop — spawn the packaged app detached
ulnclaw gui --dev        # run the unpackaged app from this directory (npm start)
ulnclaw gui --binary PATH    # explicit executable
ULNCLAW_DESKTOP_BINARY=PATH ulnclaw gui
```

Binary resolution order: `--binary` → `ULNCLAW_DESKTOP_BINARY` →
`desktop-electron/release/{linux,win,mac}-unpacked/…` (built by
`npm run pack`). If nothing is found, `ulnclaw gui` prints build instructions.
Unlike `hermes gui`, this command does **not** build the app on demand — build
once with `npm run dist` / `npm run pack`, or use the prebuilt installers.

## How it works

- The **Electron main process** spawns and supervises the bundled statically
  linked `ulnclaw` gateway (`ULNCLAW_DESKTOP=1`) on `127.0.0.1:8642`, probes
  `/health` with capped respawn, shows boot diagnostics on the offline banner,
  and owns the tray + native menus, `ulnclaw://` deep links, single-instance
  handoff and window-state persistence.
- The **renderer** (React 19 + Vite) talks to the gateway over HTTP/SSE plus a
  JSON-RPC WebSocket — the same contract hermes desktop uses against the
  hermes gateway (`shared/src/json-rpc-gateway.ts`, vendored verbatim).
- The desktop bridge tools (`close_terminal` / `read_terminal` / `focus_pane`
  / `open_preview` / `react_to_message`) reach the webview over the
  `/api/desktop/events` SSE bridge.
- First launch needs no API key: the gateway boots keyless and the
  onboarding / Models view walks you through adding a provider key
  (persisted to `config.toml` / the credentials pool); restart the gateway
  once a key is saved (tray › Restart Gateway).

Configuration lives in `~/.ulnclaw/config.toml` (Windows:
`%USERPROFILE%\.ulnclaw\config.toml`) — exactly as for the CLI. Boot logs land
in `~/.ulnclaw/logs/desktop.log`; gateway output in
`~/.ulnclaw/gateway.log`.

## Development

```bash
npm install
npm run dev            # Vite renderer (:5174) + Electron against it
npm run dev:fake-boot  # exercise the startup overlay with deterministic delays

npm run typecheck && npm run lint
npm run test:ui            # renderer unit tests
npm run test:desktop:platforms   # electron-side tests
```

Note: hermes' upstream test suite (462 unit + 25 e2e files) is not vendored by
default — see the sync machinery below.

### Building installers

```bash
npm run pack         # unpacked app under release/ (what `ulnclaw gui` resolves)
npm run dist         # installers for the current OS
npm run dist:mac     # DMG + zip
npm run dist:win     # NSIS + MSI
npm run dist:linux   # AppImage + deb + rpm
```

Release installers are produced by `.github/workflows/release-desktop.yml` on
every `v*` tag: it builds the core `ulnclaw` gateway first (statically
linked), stages it into `resources/binaries/`, then builds the installers so
the packaged app can spawn the gateway without any local toolchain.

## Vendoring & upstream sync

This tree is a **direct copy** of hermes-agent `apps/desktop` + `apps/shared`
(current vendored revision: v2026.8.3), transformed in two passes: a
mechanical brand rename (`HERMES→ULNCLAW`, `Hermes→ulnclaw`, `hermes→ulnclaw`
— contents and file names) and the 7-file functional divergence catalog
(backend argv `serve`→`gateway`, readiness regex, bundled gateway binary,
package.json branding, vendored-layout paths, repo-root depth).

**Do not hand-edit vendored files.** Change the patch catalog instead, or send
the change upstream to hermes. When hermes desktop moves, sync with:

```bash
node scripts/sync-from-hermes.mjs /path/to/hermes-checkout          # dry run
node scripts/sync-from-hermes.mjs /path/to/hermes-checkout --apply  # write
```

Full details in [UPSTREAM-SYNC.md](UPSTREAM-SYNC.md).

## License & credits

The shell is vendored from [hermes-agent](https://github.com/NousResearch/hermes-agent)
by [Nous Research](https://nousresearch.com) (MIT); the ulnclaw project is
MIT OR Apache-2.0.
