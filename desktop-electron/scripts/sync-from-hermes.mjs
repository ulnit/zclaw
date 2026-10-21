#!/usr/bin/env node
// Vendor-sync the hermes desktop source into this directory.
//
// The desktop shell carries MIT-licensed code derived from hermes desktop
// (Nous Research — attribution in NOTICE, divergence catalog in UPSTREAM-SYNC.md). This script
// re-applies that copy from a hermes checkout:
//
//   node scripts/sync-from-hermes.mjs /path/to/hermes-agent          # check
//   node scripts/sync-from-hermes.mjs /path/to/hermes-agent --apply  # write
//   node scripts/sync-from-hermes.mjs ... --with-tests               # incl. tests
//
// Pipeline: upstream file -> mechanical brand rename -> functional
// divergence patches (gateway wiring / packaging layout only) -> deterministic
// lint normalization (the rename breaks perfectionist/sort-* ordering; upstream
// keeps its tree sorted with an on-merge `npm run fix` bot) -> compare or write.
// See UPSTREAM-SYNC.md for the divergence catalog.
//
// Invariant: after a sync, `npm run lint` reports 0 errors — because the
// normalization step reuses ESLint's own fixer, the tree equals what
// `npm run lint:fix` would produce.

import fs from 'node:fs'
import path from 'node:path'

const SELF_DIR = path.resolve(import.meta.dirname, '..')

// EOL markers as plain text constants: writing /\r\n/ as a regex literal
// is fragile across editors/tools that normalize escapes, so split/join is used.
const CRLF_TEXT = '\r\n'
const LF_TEXT = '\n'


// ---------------------------------------------------------------------------
// Mechanical brand rename (applied to file contents AND file names).
// Order matters: uppercase first so "HERMES_HOME" -> "ULNCLAW_HOME".
// ---------------------------------------------------------------------------
const RENAME_RULES = [
  [/HERMES/g, 'ULNCLAW'],
  // 🔴 React hook names must stay `use` + Uppercase. The blanket `Hermes →
  // ulnclaw` rule below turns upstream's `useHermesConfig` into
  // `useulnclawConfig`, which violates react-hooks/rules-of-hooks (a hook must
  // start with `use` followed by a capital). This rule runs FIRST and only at an
  // identifier boundary, so `useHermesConfig` → `useUlnclawConfig` while every
  // other `Hermes` (types like `HermesSkin` → `ulnclawSkin`, prose, `HERMES_HOME`)
  // keeps the existing lowercase mapping. Without it, every re-sync re-introduces
  // the 5 rules-of-hooks errors this fork fixed by hand.
  [/(?<![A-Za-z0-9_$])useHermes/g, 'useUlnclaw'],
  [/Hermes/g, 'ulnclaw'],
  [/hermes/g, 'ulnclaw'],
]

function renameText(text) {
  let out = text
  for (const [pattern, replacement] of RENAME_RULES) {
    out = out.replace(pattern, replacement)
  }
  return out
}

// ---------------------------------------------------------------------------
// Deterministic lint normalization, applied AFTER the brand rename.
//
// 🔴 The rename is what breaks the ordering: `Hermes` maps to lowercase
// `ulnclaw`, which silently changes alphabetical position. Upstream's
// `hermesManagedNodePathEntries` legitimately sorted before
// `normalizeHermesHomeRoot` (h < n); after the rename it becomes
// `ulnclawManagedNodePathEntries` vs `normalizeulnclawHomeRoot` (u > n) and
// perfectionist/sort-* flags it. Upstream keeps its tree sorted with an
// on-merge `npm run fix` bot, so this fork's pipeline has to reproduce that
// step — otherwise every sync re-introduces ~125 lint errors AND reverts any
// local `npm run lint:fix`, making the tree and the sync expectation disagree
// forever.
//
// Reusing ESLint's own fixer (rather than a hand-rolled sorter) is what makes
// the invariant provable: sync output is byte-identical to `npm run lint:fix`
// output, so `npm run lint` reports 0 errors immediately after a sync.
//
// NOTE: only `perfectionist/sort-*` and `curly` were observed firing; ESLint's
// full fix set is applied anyway so the invariant stays "tree == lint:fix
// output" instead of "tree == lint:fix output minus whatever we forgot".
// ---------------------------------------------------------------------------
const SORTABLE_EXTENSIONS = new Set(['.ts', '.tsx'])
// 🔴 Must mirror the `lint` script in package.json (`eslint src/ electron/`).
// `shared/` is deliberately NOT here: neither this fork nor upstream lints it,
// so upstream's on-merge fix bot never sorted it either — normalizing it would
// (a) invent a divergence upstream does not have, and (b) fail outright, because
// shared/ ships its own `eslint.config.mjs` importing `../../eslint.config.shared.mjs`,
// which resolves outside this vendored tree (an unported monorepo-root file).
// If the lint script's targets ever change, change this list with it.
const LINT_TARGET_DIRS = ['src', 'electron']
let eslintFixerPromise = null

function getEslintFixer() {
  if (!eslintFixerPromise) {
    // Lazy + cached: constructing ESLint parses the flat config once, and the
    // sync driver lints ~1000 files through this single instance.
    eslintFixerPromise = import('eslint').then(({ ESLint }) =>
      new ESLint({
        fix: true,
        cwd: SELF_DIR,
        // Respect the project's own ignores (`**/*.generated.ts`, `src/**/*.js`).
        warnIgnored: false
      })
    )
  }
  return eslintFixerPromise
}

/**
 * Run the project's own auto-fixable rules over already-renamed source text.
 * Returns the input unchanged when the file is not lintable (binary, ignored,
 * or no fixes applicable) so this step can never corrupt a vendored asset.
 */
async function normalizeLint(rel, text) {
  if (!SORTABLE_EXTENSIONS.has(path.extname(rel).toLowerCase())) {
    return text
  }

  // Only files the `lint` script actually covers get normalized (see
  // LINT_TARGET_DIRS). `rel` is target-relative with forward slashes.
  const topDir = rel.split('/')[0]
  if (!LINT_TARGET_DIRS.includes(topDir)) {
    return text
  }

  const eslint = await getEslintFixer()
  const targetPath = path.join(SELF_DIR, rel)
  let results

  try {
    results = await eslint.lintText(text, { filePath: targetPath })
  } catch (error) {
    // 🔴 Fail LOUD. Silently skipping the sort would reintroduce exactly the
    // drift this step exists to prevent, and the next `npm run lint` would
    // then look like a local mistake rather than a sync-pipeline gap.
    throw new Error(`${rel}: eslint normalization failed: ${error.message}`)
  }

  const result = results && results[0]
  // No result (ignored file) or no fixable findings: keep the text verbatim.
  if (!result || result.output === undefined) {
    return text
  }

  return result.output
}

// ---------------------------------------------------------------------------
// Functional divergence patches — the ONLY intentional differences between
// hermes desktop and ulnclaw desktop (gateway wiring + vendored layout).
// Each entry: exact-string replacements applied after the brand rename.
// ---------------------------------------------------------------------------
const FUNCTIONAL_PATCHES = {
  // The gateway subcommand in ulnclaw is `gateway`, not hermes' `serve`.
  'electron/backend-command.ts': [
    {
      find: "return [...head, 'serve', '--host', '127.0.0.1', '--port', '0']",
      replace: "return [...head, 'gateway', '--host', '127.0.0.1', '--port', '0']",
    },
  ],
  // The ulnclaw gateway announces readiness with a "listening on http://…"
  // line; keep hermes' ULNCLAW_BACKEND_READY marker as a second arm.
  'electron/backend-ready.ts': [
    {
      find: 'const _READY_RE = /^ULNCLAW_(?:BACKEND|DASHBOARD)_READY port=(\\d+)/m',
      replace:
        'const _READY_RE = /listening on https?:\\/\\/[^:\\s]+:(\\d+)|^ULNCLAW_(?:BACKEND|DASHBOARD)_READY port=(\\d+)/m',
    },
    {
      find: 'resolve(parseInt(m[1], 10))',
      replace: 'resolve(parseInt(m[1] ?? m[2], 10))',
    },
  ],
  // Self-contained installers bundle the statically linked ulnclaw gateway
  // binary under resources/binaries/; prefer it over PATH so a clean machine
  // boots without a separate ulnclaw install.
  'electron/main.ts': [
    {
      find: 'function findOnPath(command) {',
      replace: `function bundledulnclawBinary(): string | null {
  if (!process.resourcesPath) return null
  const candidates = [
    path.join(process.resourcesPath, 'binaries', 'ulnclaw'),
    path.join(process.resourcesPath, 'binaries', 'ulnclaw.exe'),
  ]
  for (const candidate of candidates) {
    try {
      if (fs.existsSync(candidate)) return candidate
    } catch {
      // ignore
    }
  }
  return null
}

function findOnPath(command) {`,
    },
    {
      find: "      ulnclawCommand = findOnPath('ulnclaw')",
      replace: `      // Self-contained installers ship the gateway binary next to the app
      // (resources/binaries/). Prefer it over whatever is on PATH so a clean
      // machine without a ulnclaw install still boots straight into the shell.
      const bundled = bundledulnclawBinary()
      ulnclawCommand = bundled ?? findOnPath('ulnclaw')`,
    },
  ],
  // Vendored layout: shared/ lives INSIDE desktop-electron/ (hermes keeps it
  // as a sibling at ../shared), and node_modules is one level closer.
  'vite.config.ts': [
    { find: "'../shared/src/billing-types.ts'", replace: "'./shared/src/billing-types.ts'" },
    { find: "'../shared/src'", replace: "'./shared/src'" },
    { find: "'../../node_modules/react'", replace: "'./node_modules/react'" },
    { find: "'../../node_modules/react-dom'", replace: "'./node_modules/react-dom'" },
    {
      find: "'../../node_modules/react/jsx-dev-runtime.js'",
      replace: "'./node_modules/react/jsx-dev-runtime.js'",
    },
    {
      find: "'../../node_modules/react/jsx-runtime.js'",
      replace: "'./node_modules/react/jsx-runtime.js'",
    },
  ],
  'tsconfig.json': [
    { find: '"../shared/src/billing-types.ts"', replace: '"./shared/src/billing-types.ts"' },
    { find: '"../shared/src/index.ts"', replace: '"./shared/src/index.ts"' },
    { find: '"../shared/src"', replace: '"./shared/src"' },
  ],
  // desktop-electron/ sits at the repo root (hermes is nested two deeper).
  'scripts/assert-root-install.mjs': [
    {
      find: 'const root = resolve(import.meta.dirname, "..", "..", "..")',
      replace: 'const root = resolve(import.meta.dirname, "..")',
    },
  ],
}

// package.json gets line-level patches (structure-preserving, no JSON
// round-trip). "__VERSION__" is replaced with the fork's current version so
// upstream version bumps never overwrite ours.
const PACKAGE_JSON_PATCHES = ({ version }) => [
  { find: '"name": "ulnclaw",', replace: '"name": "ulnclaw-desktop",' },
  { find: '"productName": "ulnclaw",\n  "private"', replace: '"productName": "ulnclaw desktop",\n  "private"' },
  { find: /"version": "[^"]*",/, replace: `"version": "${version}",` },
  {
    find: '"description": "Native desktop shell for ulnclaw Agent.",',
    replace: '"description": "Native desktop shell for the ulnclaw gateway.",',
  },
  { find: '"author": "Nous Research",', replace: '"author": "ulnclaw",' },
  { find: '"@ulnclaw/shared": "file:../shared"', replace: '"@ulnclaw/shared": "file:./shared"' },
  { find: '"productName": "ulnclaw",', replace: '"productName": "ulnclaw desktop",' },
  { find: '"executableName": "ulnclaw",', replace: '"executableName": "ulnclaw-desktop",' },
  {
    find: `      {
        "from": "assets/icon.ico",
        "to": "icon.ico"
      }
    ],`,
    replace: `      {
        "from": "assets/icon.ico",
        "to": "icon.ico"
      },
      {
        "from": "resources/binaries",
        "to": "binaries"
      }
    ],`,
  },
]

// ---------------------------------------------------------------------------
// File set selection
// ---------------------------------------------------------------------------
const ALWAYS_EXCLUDE_DIRS = new Set(['node_modules', 'dist', 'release', '.git'])
const TEST_EXCLUDES = [
  /^e2e\//,
  /\.test\.[mc]?[jt]sx?$/,
  /\.spec\.[mc]?[jt]sx?$/,
  /^playwright\.config\.ts$/,
  /^vitest\.config\.ts$/,
  /^vitest\.setup\.ts$/,
  /^tsconfig\.e2e\.json$/,
]
const TEXT_EXTENSIONS = new Set([
  '.ts', '.tsx', '.mts', '.cts', '.js', '.mjs', '.cjs', '.jsx',
  '.json', '.css', '.html', '.md', '.yml', '.yaml', '.svg', '.txt', '.mjs',
])

function walk(root, rel = '') {
  const out = []
  const abs = path.join(root, rel)
  for (const entry of fs.readdirSync(abs, { withFileTypes: true })) {
    const relChild = rel ? `${rel}/${entry.name}` : entry.name
    if (entry.isDirectory()) {
      if (ALWAYS_EXCLUDE_DIRS.has(entry.name)) continue
      out.push(...walk(root, relChild))
    } else if (entry.isFile()) {
      out.push(relChild)
    }
  }
  return out
}

function isTestFile(rel) {
  return TEST_EXCLUDES.some(pattern => pattern.test(rel))
}

async function transform(rel, sourcePath) {
  const ext = path.extname(rel).toLowerCase()
  const raw = fs.readFileSync(sourcePath)
  if (!TEXT_EXTENSIONS.has(ext)) {
    return raw // binary asset: copy verbatim
  }
  let text = renameText(raw.toString('utf8'))
  if (rel === 'package.json') {
    const versionMatch = text.match(/"version": "([^"]*)"/)
    const forkPackagePath = path.join(SELF_DIR, 'package.json')
    let version = versionMatch ? versionMatch[1] : '0.0.0'
    if (fs.existsSync(forkPackagePath)) {
      const forkVersion = fs.readFileSync(forkPackagePath, 'utf8').match(/"version": "([^"]*)"/)
      if (forkVersion) version = forkVersion[1]
    }
    for (const patch of PACKAGE_JSON_PATCHES({ version })) {
      const before = text
      text = text.replace(patch.find, patch.replace)
      if (text === before) {
        throw new Error(`package.json patch did not apply: ${JSON.stringify(String(patch.find).slice(0, 60))}`)
      }
    }
    text = text.replace(/\n$/, '') // fork file carries no trailing newline
  } else if (FUNCTIONAL_PATCHES[rel]) {
    for (const patch of FUNCTIONAL_PATCHES[rel]) {
      const before = text
      text = text.replace(patch.find, patch.replace)
      if (text === before) {
        throw new Error(`${rel}: functional patch did not apply: ${JSON.stringify(patch.find.slice(0, 60))}`)
      }
    }
  }
  // 🔴 Deterministic re-sort AFTER rename + functional patches, so the
  // expectation on disk equals `npm run lint:fix` output byte-for-byte.
  text = await normalizeLint(rel, text)
  return Buffer.from(text, 'utf8')
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------
const args = process.argv.slice(2)
const apply = args.includes('--apply')
const withTests = args.includes('--with-tests')
const checkout = args.find(arg => !arg.startsWith('--'))

if (!checkout) {
  console.error('usage: node scripts/sync-from-hermes.mjs <hermes-checkout> [--apply] [--with-tests]')
  process.exit(2)
}

const desktopRoot = path.resolve(checkout, 'apps', 'desktop')
const sharedRoot = path.resolve(checkout, 'apps', 'shared')
if (!fs.existsSync(path.join(desktopRoot, 'package.json'))) {
  console.error(`error: ${desktopRoot} does not look like a hermes desktop checkout`)
  process.exit(2)
}

const jobs = [] // { rel (target-relative), sourcePath }
for (const rel of walk(desktopRoot)) {
  if (!withTests && isTestFile(rel)) continue
  jobs.push({ rel, sourcePath: path.join(desktopRoot, rel) })
}
for (const rel of walk(sharedRoot)) {
  if (!withTests && isTestFile(rel)) continue
  jobs.push({ rel: `shared/${rel}`, sourcePath: path.join(sharedRoot, rel) })
}

let identical = 0
const changed = []
const added = []
const errors = []

for (const job of jobs) {
  const targetPath = path.join(SELF_DIR, renameText(job.rel))
  let expected
  try {
    expected = await transform(job.rel, job.sourcePath)
  } catch (error) {
    errors.push(`${job.rel}: ${error.message}`)
    continue
  }
  if (!fs.existsSync(targetPath)) {
    added.push(job.rel)
    if (apply) {
      fs.mkdirSync(path.dirname(targetPath), { recursive: true })
      fs.writeFileSync(targetPath, expected)
    }
    continue
  }
  const current = fs.readFileSync(targetPath)
  // 🔴 EOL-agnostic compare. With `core.autocrlf=true` this worktree holds CRLF
  // while upstream (and this repo's git blob) is LF, so a byte compare flagged
  // 2297 of 2321 vendored files as "drifted" (`identical: 24`) — the report was
  // unusable for finding real divergence. Compare line-ending-normalized text.
  if (sameContent(current, expected)) {
    identical += 1
  } else {
    changed.push(job.rel)
    // Write with the target's existing line endings so `--apply` never rewrites
    // the whole tree's EOL (which would show up as a giant, meaningless diff).
    if (apply) fs.writeFileSync(targetPath, matchEol(expected, current))
  }
}

/** Line-ending-insensitive content equality (CRLF worktree vs LF upstream). */
function sameContent(a, b) {
  if (a.equals(b)) {
    return true
  }
  const norm = buf => buf.toString('utf8').split(CRLF_TEXT).join(LF_TEXT)
  return norm(a) === norm(b)
}

/**
 * Re-encode `buf` to the dominant line ending already present in `reference`,
 * so a write preserves the worktree's checkout style instead of flipping it.
 */
function matchEol(buf, reference) {
  const refText = reference.toString('utf8')
  const refCrlf = refText.split(CRLF_TEXT).length - 1
  if (refCrlf === 0) {
    return buf // reference is LF (or has no newlines): write upstream bytes as-is
  }
  const refLfOnly = refText.split(LF_TEXT).length - 1 - refCrlf
  if (refCrlf < refLfOnly) {
    return buf // reference is predominantly LF; don't convert
  }
  // Reference is predominantly CRLF: re-encode THE NEW CONTENT (buf, not the
  // reference) to CRLF. Normalize to LF first so an existing CRLF in buf cannot
  // become CR-CR-LF.
  const out = buf.toString('utf8').split(CRLF_TEXT).join(LF_TEXT).split(LF_TEXT).join(CRLF_TEXT)
  return Buffer.from(out, 'utf8')
}

console.log(`upstream files scanned : ${jobs.length}`)
console.log(`identical              : ${identical}`)
console.log(`${apply ? 'updated' : 'would update'}        : ${changed.length}`)
for (const rel of changed) console.log(`  ~ ${rel}`)
console.log(`${apply ? 'added' : 'would add'}            : ${added.length}`)
for (const rel of added) console.log(`  + ${rel}`)
if (errors.length) {
  console.log(`patch errors           : ${errors.length}`)
  for (const error of errors) console.log(`  ! ${error}`)
}
if (!apply && (changed.length || added.length)) {
  console.log('\ncheck mode: re-run with --apply to write these changes')
}
process.exit(errors.length ? 1 : 0)
