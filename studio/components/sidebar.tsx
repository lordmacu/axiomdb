'use client'
import Link from 'next/link'
import { usePathname } from 'next/navigation'
import { useState, useEffect } from 'react'
import {
  Table2, Eye, Search, Activity, Code2, Settings, Zap,
  GitGraph, Braces, Clock, Keyboard,
} from 'lucide-react'
import { TABLES } from '@/lib/mock'
import { cn } from '@/lib/utils'

const RECENT_KEY = 'axiomstudio_recent'

function useLatency() {
  const [latency, setLatency] = useState<number | null>(null)

  useEffect(() => {
    function ping() {
      // Mock: simulate 1-8ms ping with 20-50ms async delay
      setTimeout(() => {
        setLatency(1 + Math.floor(Math.random() * 7))
      }, 20 + Math.random() * 30)
    }
    ping()
    const interval = setInterval(ping, 5000)
    return () => clearInterval(interval)
  }, [])

  return latency
}

function useRecentTables(pathname: string): string[] {
  const [recent, setRecent] = useState<string[]>([])

  // Load on mount
  useEffect(() => {
    try {
      setRecent(JSON.parse(localStorage.getItem(RECENT_KEY) ?? '[]'))
    } catch { /* ignore */ }
  }, [])

  // Track navigation
  useEffect(() => {
    if (!pathname.startsWith('/tables/')) return
    const name = pathname.split('/').pop()
    if (!name) return
    setRecent(prev => {
      const next = [name, ...prev.filter(n => n !== name)].slice(0, 5)
      localStorage.setItem(RECENT_KEY, JSON.stringify(next))
      return next
    })
  }, [pathname])

  return recent
}

export function Sidebar() {
  const pathname = usePathname()
  const [search, setSearch] = useState('')
  const latency = useLatency()
  const recent = useRecentTables(pathname)

  const tables = TABLES.filter(t => t.type === 'table' && t.name.includes(search))
  const views = TABLES.filter(t => t.type === 'view' && t.name.includes(search))

  // Latency color
  const latencyColor =
    latency === null ? 'text-text-secondary' :
    latency < 5      ? 'text-[#10b981]' :
    latency <= 20    ? 'text-[#d29922]' :
                       'text-[#f85149]'

  const dotColor =
    latency === null ? 'bg-text-secondary' :
    latency < 5      ? 'bg-[#10b981] animate-pulse' :
    latency <= 20    ? 'bg-[#d29922] animate-pulse' :
                       'bg-[#f85149] animate-pulse'

  return (
    <aside className="w-56 flex flex-col border-r border-border bg-surface shrink-0">
      {/* Logo */}
      <div className="px-4 py-3 border-b border-border">
        <div className="flex items-center gap-2">
          <div className="w-6 h-6 rounded bg-accent/20 flex items-center justify-center">
            <Zap className="w-3.5 h-3.5 text-accent" />
          </div>
          <span className="font-semibold text-sm text-text-primary tracking-tight">AxiomStudio</span>
        </div>
        {/* Latency badge */}
        <div className="mt-2 flex items-center gap-1.5 text-xs text-text-secondary">
          <div className={cn('w-1.5 h-1.5 rounded-full shrink-0', dotColor)} />
          <span className="font-mono">localhost:3306</span>
          <span className={cn('font-mono ml-auto shrink-0', latencyColor)}>
            {latency === null ? '…' : `${latency}ms`}
          </span>
        </div>
      </div>

      {/* Nav */}
      <nav className="px-2 py-2 space-y-0.5 border-b border-border">
        {[
          { href: '/',          icon: Activity,  label: 'Dashboard' },
          { href: '/query',     icon: Code2,     label: 'Query Editor' },
          { href: '/objects',   icon: Braces,    label: 'DB Objects' },
          { href: '/diagram',   icon: GitGraph,  label: 'ER Diagram' },
          { href: '/shortcuts', icon: Keyboard,  label: 'Shortcuts' },
        ].map(({ href, icon: Icon, label }) => (
          <Link key={href} href={href}
            className={cn(
              'flex items-center gap-2 px-2 py-1.5 rounded text-xs transition-colors',
              pathname === href
                ? 'bg-accent/10 text-accent'
                : 'text-text-secondary hover:text-text-primary hover:bg-elevated'
            )}>
            <Icon className="w-3.5 h-3.5" />
            {label}
          </Link>
        ))}
      </nav>

      {/* Search */}
      <div className="px-3 py-2 border-b border-border">
        <div className="flex items-center gap-2 px-2 py-1 rounded bg-elevated border border-border text-xs">
          <Search className="w-3 h-3 text-text-secondary shrink-0" />
          <input
            value={search}
            onChange={e => setSearch(e.target.value)}
            placeholder="Search tables..."
            className="bg-transparent outline-none text-text-secondary placeholder-text-secondary/50 w-full font-mono"
          />
        </div>
      </div>

      {/* Tables + Recent */}
      <div className="flex-1 overflow-y-auto py-2">
        <div className="px-3 mb-1">
          <span className="text-[10px] font-semibold tracking-widest text-text-secondary uppercase">
            Tables ({tables.length})
          </span>
        </div>
        {tables.map(t => (
          <Link key={t.name} href={`/tables/${t.name}`}
            className={cn(
              'flex items-center justify-between px-3 py-1.5 text-xs group transition-colors',
              pathname === `/tables/${t.name}`
                ? 'bg-accent/10 text-accent'
                : 'text-text-secondary hover:text-text-primary hover:bg-elevated'
            )}>
            <div className="flex items-center gap-2">
              <Table2 className="w-3 h-3 shrink-0" />
              <span className="font-mono">{t.name}</span>
            </div>
            <span className="text-[10px] opacity-60">{t.rows.toLocaleString()}</span>
          </Link>
        ))}

        {views.length > 0 && (
          <>
            <div className="px-3 mt-3 mb-1">
              <span className="text-[10px] font-semibold tracking-widest text-text-secondary uppercase">
                Views ({views.length})
              </span>
            </div>
            {views.map(t => (
              <Link key={t.name} href={`/tables/${t.name}`}
                className="flex items-center justify-between px-3 py-1.5 text-xs text-text-secondary hover:text-text-primary hover:bg-elevated transition-colors">
                <div className="flex items-center gap-2">
                  <Eye className="w-3 h-3 shrink-0" />
                  <span className="font-mono">{t.name}</span>
                </div>
                <span className="text-[10px] opacity-60">{t.rows.toLocaleString()}</span>
              </Link>
            ))}
          </>
        )}

        {/* Recent */}
        {recent.length > 0 && !search && (
          <>
            <div className="px-3 mt-3 mb-1">
              <span className="text-[10px] font-semibold tracking-widest text-text-secondary uppercase">
                Recent
              </span>
            </div>
            {recent.map(name => (
              <Link key={name} href={`/tables/${name}`}
                className={cn(
                  'flex items-center gap-2 px-3 py-1.5 text-xs transition-colors',
                  pathname === `/tables/${name}`
                    ? 'bg-accent/10 text-accent'
                    : 'text-text-secondary hover:text-text-primary hover:bg-elevated'
                )}>
                <Clock className="w-3 h-3 shrink-0 opacity-60" />
                <span className="font-mono">{name}</span>
              </Link>
            ))}
          </>
        )}
      </div>

      {/* Bottom */}
      <div className="border-t border-border px-3 py-2">
        <Link href="/settings" className="flex items-center gap-2 text-xs text-text-secondary hover:text-text-primary transition-colors">
          <Settings className="w-3.5 h-3.5" />
          Settings
        </Link>
      </div>
    </aside>
  )
}
