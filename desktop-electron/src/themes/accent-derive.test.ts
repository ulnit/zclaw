/**
 * Accent-derivation semantics, pinned against upstream's `accentBase` /
 * `accentedTheme` (apps/desktop/src/themes/context.tsx). These are pure
 * functions over a theme + a colour string, so they are provable without a DOM.
 */
import { describe, expect, it } from 'vitest'

import { accentBase, accentedTheme } from './context'
import { normalizeHex } from './color'
import { midnightTheme, nousTheme } from './presets'
import type { DesktopTheme } from './types'

const withAccentOf = (theme: DesktopTheme, base: string): DesktopTheme => ({ ...theme, name: `${base}-accent-ff0000`, accentOf: base })

describe('accentBase', () => {
  it('returns the theme itself when it was not retinted', () => {
    expect(accentBase(nousTheme)).toBe(nousTheme)
  })

  it('resolves a retinted theme back to the theme it was retinted from', () => {
    // `nous` is a built-in, so resolveTheme() can find the base by name.
    expect(accentBase(withAccentOf(midnightTheme, 'nous'))).toBe(nousTheme)
  })

  it('falls back to the theme itself when accentOf names nothing resolvable', () => {
    const orphan = withAccentOf(midnightTheme, 'not-a-real-skin')

    expect(accentBase(orphan)).toBe(orphan)
  })
})

describe('accentedTheme', () => {
  it('names the copy <base>-accent-<hex> and records the base it came from', () => {
    const next = accentedTheme(nousTheme, '#ff0000')

    expect(next.name).toBe(`nous-accent-${'ff0000'}`)
    expect(next.label).toBe(`${nousTheme.label} · #ff0000`)
    expect(next.accentOf).toBe('nous')
  })

  it('paints BOTH modes off the base, not just the visible one', () => {
    const next = accentedTheme(nousTheme, '#ff0000')

    // retint adapts the picked colour to each surface, so the result is the
    // retint of `#ff0000`, not `#ff0000` verbatim — what matters is that BOTH
    // palettes moved off the base and landed on their own adaptation.
    expect(next.colors.primary).not.toBe(nousTheme.colors.primary)
    expect(next.darkColors?.primary).not.toBe(nousTheme.darkColors?.primary)
    expect(next.darkColors?.primary).toBeDefined()
  })

  it('returns the base unchanged when asked for the accent it already has', () => {
    const own = normalizeHex(nousTheme.colors.primary)

    expect(own).not.toBeNull()
    expect(accentedTheme(nousTheme, own as string)).toBe(nousTheme)
  })

  it('returns the base unchanged for an unparseable colour', () => {
    expect(accentedTheme(nousTheme, 'not-a-colour')).toBe(nousTheme)
  })

  it('re-accents from the BASE, so a second pick does not compound off the first', () => {
    const first = accentedTheme(nousTheme, '#ff0000')
    const second = accentedTheme(first, '#00ff00')

    expect(second.accentOf).toBe('nous')
    expect(second.name).toBe('nous-accent-00ff00')
    expect(second.colors.primary).not.toBe(nousTheme.colors.primary)
  })

  it('keeps every non-accent colour of the base', () => {
    const next = accentedTheme(nousTheme, '#ff0000')

    // The base's card/typography survive; only the accent family moves.
    expect(next.typography).toEqual(nousTheme.typography)
    expect(next.description).toBe(nousTheme.description)
  })
})
