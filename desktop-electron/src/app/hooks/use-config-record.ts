import { useQuery } from '@tanstack/react-query'

import { getulnclawConfigRecord } from '@/ulnclaw'
import { queryClient, writeCache } from '@/lib/query-client'
import type { ulnclawConfigRecord } from '@/types/ulnclaw'

// One shared cache for the whole profile config record (`GET /api/config`).
// Every settings surface (MCP, model, config) reads and writes through this key
// so a save in one shows in the others, and revisiting a tab paints the cache
// instead of blanking on a fresh fetch.
//
// Distinct from session/hooks/use-ulnclaw-config.ts, which is side-effecting —
// it pushes personality/cwd/voice/… into the session stores for live chat.
export const ULNCLAW_CONFIG_KEY = ['ulnclaw-config-record'] as const

// staleTime 0 → serve cache instantly, background-revalidate on every mount.
export const useUlnclawConfigRecord = () =>
  useQuery({ queryKey: ULNCLAW_CONFIG_KEY, queryFn: getulnclawConfigRecord, staleTime: 0 })

export const setulnclawConfigCache = writeCache<ulnclawConfigRecord>(ULNCLAW_CONFIG_KEY)

export const invalidateulnclawConfig = () => queryClient.invalidateQueries({ queryKey: ULNCLAW_CONFIG_KEY })
