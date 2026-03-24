'use client'
import { useState, useEffect, useRef, useCallback } from 'react'
import { useRouter } from 'next/navigation'
import { Search, Table2, Zap, Activity, Settings, Code2 } from 'lucide-react'
import { TABLES } from '@/lib/mock'
import { cn } from '@/lib/utils'

const HISTORY_KEY = 'axiomstudio_history'

type HistoryEntry = { id: string; query: string; duration: number; rows: number; timestamp: string }

type ResultItem = {
  id: string
  label: string
  sub?: string
  icon: React.ReactNode
  action: () => void
  group: 'Tables' | 'Actions' | 'Recent queries'
}

function loadRecentQueries(): HistoryEntry[] {
  if (typeof window === 'undefined') return []
  try {
    return JSON.parse(localStorage.getItem(HISTORY_KEY) ?? '[]') as HistoryEntry[]
  } catch { return [] }
}

export function CommandPalette() {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState(0)
  const router = useRouter()
  const inputRef = useRef<HTMLInputElement>(null)

  const close = useCallback(() => {
    setOpen(false)
    setQuery('')
    setSelected(0)
  }, [])

  // Open on ⌘K / Ctrl+K
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        e.preventDefault()
        setOpen(p => {
          if (p) { close(); return false }
          return true
        })
      }
      if (e.key === 'Escape') close()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [close])

  // Focus input when opened
  useEffect(() => {
    if (open) {
      setTimeout(() => inputRef.current?.focus(), 50)
    }
  }, [open])

  // Build results
  function buildItems(): ResultItem[] {
    const q = query.toLowerCase().trim()

    const tableItems: ResultItem[] = TABLES
      .filter(t => !q || t.name.includes(q))
      .map(t => ({
        id: `table-${t.name}`,
        label: t.name,
        sub: `${t.rows.toLocaleString()} rows · ${t.size}`,
        icon: <Table2 className="w-3.5 h-3.5 text-accent" />,
        action: () => { router.push(`/tables/${t.name}`); close() },
        group: 'Tables' as const,
      }))

    const actions: ResultItem[] = [
      {
        id: 'action-new-query',
        label: 'New query',
        sub: 'Open Query Editor',
        icon: <Code2 className="w-3.5 h-3.5 text-blue-400" />,
        action: () => { router.push('/query'); close() },
        group: 'Actions' as const,
      },
      {
        id: 'action-dashboard',
        label: 'Dashboard',
        sub: 'Go to Dashboard',
        icon: <Activity className="w-3.5 h-3.5 text-text-secondary" />,
        action: () => { router.push('/'); close() },
        group: 'Actions' as const,
      },
      {
        id: 'action-settings',
        label: 'Settings',
        sub: 'Open Settings',
        icon: <Settings className="w-3.5 h-3.5 text-text-secondary" />,
        action: () => { router.push('/settings'); close() },
        group: 'Actions' as const,
      },
    ].filter(a => !q || a.label.toLowerCase().includes(q))

    const histEntries = loadRecentQueries()
    const recentItems: ResultItem[] = histEntries
      .filter(e => !q || e.query.toLowerCase().includes(q))
      .slice(0, 5)
      .map(e => ({
        id: `hist-${e.id}`,
        label: e.query.slice(0, 60) + (e.query.length > 60 ? '…' : ''),
        sub: `${e.timestamp} · ${e.duration}ms · ${e.rows} rows`,
        icon: <Zap className="w-3.5 h-3.5 text-warning" />,
        action: () => { router.push('/query'); close() },
        group: 'Recent queries' as const,
      }))

    return [...tableItems, ...actions, ...recentItems]
  }

  const items = buildItems()

  // Group items
  const groups: { label: string; items: ResultItem[] }[] = []
  const groupOrder = ['Tables', 'Actions', 'Recent queries'] as const
  for (const g of groupOrder) {
    const gi = items.filter(i => i.group === g)
    if (gi.length > 0) groups.push({ label: g, items: gi })
  }

  // Flatten for keyboard navigation
  const flatItems = groups.flatMap(g => g.items)

  // Clamp selection
  const clampedSelected = Math.min(selected, Math.max(0, flatItems.length - 1))

  function onKeyDown(e: React.KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setSelected(p => Math.min(p + 1, flatItems.length - 1))
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setSelected(p => Math.max(p - 1, 0))
    } else if (e.key === 'Enter') {
      flatItems[clampedSelected]?.action()
    }
  }

  if (!open) return null

  return (
    <div
      className="fixed inset-0 z-[9999] flex items-start justify-center pt-24"
      onClick={e => { if (e.target === e.currentTarget) close() }}>
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/50" />

      <div className="relative w-full max-w-lg bg-surface border border-border rounded-xl shadow-2xl overflow-hidden">
        {/* Search input */}
        <div className="flex items-center gap-3 px-4 py-3 border-b border-border">
          <Search className="w-4 h-4 text-text-secondary shrink-0" />
          <input
            ref={inputRef}
            value={query}
            onChange={e => { setQuery(e.target.value); setSelected(0) }}
            onKeyDown={onKeyDown}
            placeholder="Search tables, actions, queries..."
            className="flex-1 bg-transparent outline-none text-sm text-text-primary placeholder-text-secondary/50 font-mono"
          />
          <kbd className="text-[10px] px-1.5 py-0.5 rounded bg-elevated border border-border text-text-secondary font-mono">
            esc
          </kbd>
        </div>

        {/* Results */}
        <div className="max-h-80 overflow-y-auto py-1">
          {flatItems.length === 0 ? (
            <div className="px-4 py-6 text-center text-xs text-text-secondary">
              No results for &quot;{query}&quot;
            </div>
          ) : (
            groups.map(group => {
              let flatOffset = 0
              for (const g of groups) {
                if (g.label === group.label) break
                flatOffset += g.items.length
              }
              return (
                <div key={group.label}>
                  <div className="px-4 py-1.5 text-[10px] font-semibold tracking-wider text-text-secondary uppercase">
                    {group.label}
                  </div>
                  {group.items.map((item, idx) => {
                    const globalIdx = flatOffset + idx
                    return (
                      <button
                        key={item.id}
                        onClick={item.action}
                        onMouseEnter={() => setSelected(globalIdx)}
                        className={cn(
                          'w-full text-left flex items-center gap-3 px-4 py-2.5 transition-colors',
                          clampedSelected === globalIdx ? 'bg-elevated' : 'hover:bg-elevated/50'
                        )}>
                        <span className="shrink-0">{item.icon}</span>
                        <div className="flex-1 min-w-0">
                          <div className="text-xs text-text-primary truncate font-mono">{item.label}</div>
                          {item.sub && (
                            <div className="text-[10px] text-text-secondary truncate">{item.sub}</div>
                          )}
                        </div>
                        {clampedSelected === globalIdx && (
                          <kbd className="text-[10px] px-1.5 py-0.5 rounded bg-accent/20 border border-accent/30 text-accent font-mono shrink-0">
                            ↵
                          </kbd>
                        )}
                      </button>
                    )
                  })}
                </div>
              )
            })
          )}
        </div>

        {/* Footer hint */}
        <div className="border-t border-border px-4 py-2 flex items-center gap-4 text-[10px] text-text-secondary">
          <span><kbd className="font-mono">↑↓</kbd> navigate</span>
          <span><kbd className="font-mono">↵</kbd> select</span>
          <span><kbd className="font-mono">esc</kbd> close</span>
        </div>
      </div>
    </div>
  )
}
