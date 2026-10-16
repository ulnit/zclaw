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
// divergence patches (gateway wiring / packaging layout only) -> compare
// or write. See UPSTREAM-SYNC.md for the divergence catalog.

import fs from 'node:fs'
import path from 'node:path'

const SELF_DIR = path.resolve(import.meta.dirname, '..')

// ---------------------------------------------------------------------------
// Mechanical brand rename (applied to file contents AND file names).
// Order matters: uppercase first so "HERMES_HOME" -> "ULNCLAW_HOME".
// ---------------------------------------------------------------------------
const RENAME_RULES = [
  [/HERMES/g, 'ULNCLAW'],
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

function transform(rel, sourcePath) {
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
    expected = transform(job.rel, job.sourcePath)
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
  if (current.equals(expected)) {
    identical += 1
  } else {
    changed.push(job.rel)
    if (apply) fs.writeFileSync(targetPath, expected)
  }
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
