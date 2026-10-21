# Upstream sync — hermes desktop → ulnclaw desktop

> **策略（中文摘要）**：ulnclaw desktop 是 hermes desktop（Nous Research
> hermes-agent `apps/desktop`，MIT）的**直接复制**。以后 hermes desktop 升级，
> 用 `scripts/sync-from-hermes.mjs` 一条命令把上游代码原样更新进来即可。
> **唯一有意差异是底层 gateway**：ulnclaw desktop 拉起并对接的是 ulnclaw
> 自己的 Rust gateway 二进制，而非 hermes 的 Python 后端。全部差异见下文清单
> （7 个文件、约 70 行），由同步脚本自动重放，无需手工维护。

**Upstream:** `NousResearch/hermes-agent` → `apps/desktop` + `apps/shared`
(current vendored revision: **v2026.8.3**)

## Vendoring map

| hermes checkout            | ulnclaw tree            |
| -------------------------- | ----------------------- |
| `apps/desktop/*`           | `desktop-electron/*`    |
| `apps/shared/*`            | `desktop-electron/shared/*` |

`shared/` is vendored **inside** `desktop-electron/` (hermes keeps it as a
sibling). Three config files carry path adjustments for that layout (below).

## Sync procedure

```bash
# 1. get the hermes checkout at the tag you want to sync to
git clone --depth 1 --branch v2026.9.0 https://github.com/NousResearch/hermes-agent /tmp/hermes-new

# 2. preview what changes (dry run)
node scripts/sync-from-hermes.mjs /tmp/hermes-new

# 3. write it
node scripts/sync-from-hermes.mjs /tmp/hermes-new --apply

# 4. rebuild + smoke-test
npm ci && npm run build
```

The script transforms every upstream file in two passes, then compares it
byte-for-byte against the tree:

1. **Mechanical brand rename** (contents *and* file names):
   `HERMES → ULNCLAW`, `useHermes → useUlnclaw` (hook boundary — see below),
   `Hermes → ulnclaw`, `hermes → ulnclaw`
   (applied in that order, so `HERMES_HOME` → `ULNCLAW_HOME`,
   `HermesSkin` → `ulnclawSkin`, `~/.hermes` → `~/.ulnclaw`).
   🔴 The `useHermes → useUlnclaw` rule runs before the blanket `Hermes → ulnclaw`
   and is anchored at an identifier boundary, because a React hook must be
   `use` + Uppercase: the blanket rule alone maps `useHermesConfig` →
   `useulnclawConfig`, violating `react-hooks/rules-of-hooks` (5 errors). It
   touches only hook names — every other `Hermes` still lowercases as before.
2. **Functional divergence patches** — the gateway/packaging differences
   cataloged below, re-applied automatically.

Exit code 0 with `identical: N` means the tree is in sync. Any file that
fails a patch aborts loudly (upstream moved the anchor — review and update
the patch catalog here and in the script).

Test files (`*.test.ts`, `e2e/`, playwright/vitest configs) are **excluded by
default** because the vendored tree does not yet run hermes' test harness in
CI; pass `--with-tests` to vendor them too.

## Functional divergence catalog (the ONLY intentional differences)

| File | Divergence | Why |
| ---- | ---------- | --- |
| `electron/backend-command.ts` | backend argv `serve` → `gateway` | ulnclaw's headless-gateway subcommand is `ulnclaw gateway` (hermes: `hermes serve`) |
| `electron/backend-ready.ts` | readiness regex also matches `listening on http://…:PORT` | the ulnclaw gateway prints a listening line; hermes' `*_BACKEND_READY` marker kept as a second arm |
| `electron/main.ts` | `bundledulnclawBinary()` + prefer `resources/binaries/ulnclaw(.exe)` over PATH | self-contained installers ship the statically linked gateway binary inside the app (fixes clean-Windows "gateway won't start") |
| `package.json` | name `ulnclaw-desktop`, productName `ulnclaw desktop`, author/description, `file:./shared` dep, `executableName`, extraResources `resources/binaries → binaries`, fork-owned `version` | branding + bundling the gateway binary into installers |
| `vite.config.ts`, `tsconfig.json` | `../shared` → `./shared`, `../../node_modules` → `./node_modules` | vendored layout: shared/ lives inside desktop-electron/ |
| `eslint.config.mjs` | `import shared from '../../eslint.config.shared.mjs'` → `'./eslint.config.shared.mjs'` | the shared config is vendored INTO desktop-electron/ (below); `../../` would resolve outside the repo — this was the sole reason `npm run lint` never worked here |
| `package.json` devDependencies | + `eslint`, `@eslint/js`, `typescript-eslint`, `eslint-plugin-perfectionist`, `eslint-plugin-react-hooks`, `eslint-plugin-unused-imports`, `globals` (pinned to upstream's root-package versions) | upstream installs these at the monorepo root; the standalone vendored package must carry them itself or ESLint cannot load the config |
| `scripts/assert-root-install.mjs` | repo-root depth `../../..` → `..` | desktop-electron/ sits at the repo root |

Everything else — the whole renderer (views, styles, i18n, interactions), the
Electron main-process logic, tray/menus/deep-links/window management — is the
renamed upstream verbatim. **Do not hand-edit vendored files**; change the
patch catalog instead, or send the change upstream to hermes.

## Fork-only files (never overwritten by sync)

- `resources/binaries/` — staged `ulnclaw` gateway binary (built by CI /
  `release-desktop.yml`) packaged via electron-builder `extraResources`
- `build/` — installer icon assets
- `scripts/sync-from-hermes.mjs`, `UPSTREAM-SYNC.md` — this machinery
- `eslint.config.shared.mjs` — vendored copy of the monorepo-root shared ESLint config
  (body byte-identical to upstream, only the fork header differs); re-vendor on sync, do not
  hand-edit the body
- `.gitignore`

## Gateway contract notes

The shell talks to the gateway exactly like hermes desktop talks to the
hermes gateway: HTTP/SSE + JSON-RPC WebSocket (`shared/src/json-rpc-gateway
.ts`, vendored verbatim), OpenAI-compatible surface, `/api/*` routes, desktop
bridge `/api/desktop/events` SSE, managed child spawn with
`ULNCLAW_DESKTOP=1`. If a hermes desktop upgrade starts using a gateway
surface the ulnclaw gateway lacks, implement that surface in the gateway
(`src/gateway/`) — **not** by patching the vendored shell.
