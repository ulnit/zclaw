'use strict'

/**
 * shell-updater.ts
 *
 * Silent whole-shell auto-update: the packaged desktop checks the GitHub
 * release channel (generic provider over release assets), downloads the new
 * installer in the background, and applies it when the user quits. The
 * update replaces the entire shell — Electron app plus the bundled gateway
 * binary — so one silent restart lands both halves atomically.
 *
 * Platform support follows electron-updater's transports:
 *   - Windows: NSIS installer (assisted first-install, silent `--updated`)
 *   - macOS:   zip target (dmg stays the manual-install surface)
 *   - Linux:   AppImage only; deb/rpm installs are skipped (no transport)
 *
 * The backend binary keeps its own in-place channel (`updater-process.ts` /
 * `ulnclaw update`); this module only owns the shell.
 */

import { autoUpdater } from 'electron-updater'

export interface ShellUpdaterDeps {
  isPackaged: boolean
  platform: NodeJS.Platform
  isAppImage: boolean
  log: (message: string) => void
  onDownloaded?: (version: string) => void
}

export const SHELL_UPDATE_INITIAL_DELAY_MS = 15_000
export const SHELL_UPDATE_INTERVAL_MS = 4 * 60 * 60_000

export function shellUpdateEligible(deps: Pick<ShellUpdaterDeps, 'isPackaged' | 'platform' | 'isAppImage'>): { eligible: boolean; reason: string } {
  if (!deps.isPackaged) {
    return { eligible: false, reason: 'dev mode (not packaged)' }
  }

  if (deps.platform === 'linux' && !deps.isAppImage) {
    return { eligible: false, reason: 'linux install is not an AppImage (deb/rpm have no auto-update transport)' }
  }

  return { eligible: true, reason: 'ok' }
}

export function startShellUpdater(deps: ShellUpdaterDeps): boolean {
  const gate = shellUpdateEligible(deps)

  if (!gate.eligible) {
    deps.log(`[shell-update] disabled: ${gate.reason}`)

    return false
  }

  autoUpdater.autoDownload = true
  autoUpdater.autoInstallOnAppQuit = true
  autoUpdater.allowDowngrade = false

  autoUpdater.on('checking-for-update', () => deps.log('[shell-update] checking for update'))
  autoUpdater.on('update-available', info => deps.log(`[shell-update] update available: ${info.version}`))
  autoUpdater.on('update-not-available', info => deps.log(`[shell-update] up to date (latest ${info.version})`))
  autoUpdater.on('download-progress', progress => {
    deps.log(`[shell-update] download ${Math.round(progress.percent)}% (${Math.round(progress.bytesPerSecond / 1024)} KiB/s)`)
  })
  autoUpdater.on('update-downloaded', info => {
    deps.log(`[shell-update] downloaded ${info.version}; installs on next quit`)
    deps.onDownloaded?.(info.version)
  })
  autoUpdater.on('error', error => {
    const message = error instanceof Error ? error.message : String(error)
    deps.log(`[shell-update] error: ${message}`)
  })

  const check = () => {
    autoUpdater.checkForUpdates().catch(error => {
      const message = error instanceof Error ? error.message : String(error)
      deps.log(`[shell-update] check failed: ${message}`)
    })
  }

  setTimeout(check, SHELL_UPDATE_INITIAL_DELAY_MS)
  setInterval(check, SHELL_UPDATE_INTERVAL_MS)
  deps.log('[shell-update] enabled (silent install on quit)')

  return true
}
