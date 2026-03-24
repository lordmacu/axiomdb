'use client'
import { useState, useRef, useEffect, useCallback } from 'react'
import dynamic from 'next/dynamic'
import {
  Play, Zap, Download, AlertCircle, ChevronUp, ChevronDown,
  ChevronsUpDown, Check, X, Clock, Trash2, Plus,
} from 'lucide-react'
import { generateUsers, generateOrders } from '@/lib/mock'
import { cn } from '@/lib/utils'
import {
  useReactTable,
  getCoreRowModel,
  getSortedRowModel,
  getPaginationRowModel,
  flexRender,
  type SortingState,
  type ColumnDef,
} from '@tanstack/react-table'

const MonacoEditor = dynamic(() => import('@monaco-editor/react'), { ssr: false })

const MOCK_RESULTS: Record<string, unknown[]> = {
  default: generateUsers(20),
  orders: generateOrders(20),
}

const DEFAULT_SQL = `-- AxiomDB Query Editor  (⌘↵ to run)

SELECT id, name, email, age, active, created_at
FROM users
WHERE active = TRUE
ORDER BY created_at DESC
LIMIT 50`

const DEFAULT_AXIOMQL = `-- AxiomDB Query Editor  (⌘↵ to run)

users
  .filter(active = true)
  .pick(id, name, email, age, active, created_at)
  .sort(created_at.desc)
  .take(50)`

const HISTORY_KEY = 'axiomstudio_history'
const MAX_HISTORY = 20

// ── SQL ↔ AxiomQL translator (mock/heuristic — real one arrives in Phase 36) ─

function sqlToAxiomql(sql: string): string {
  try {
    const s = sql.replace(/--[^\n]*/g, '').replace(/\s+/g, ' ').trim()
    const tableM = s.match(/FROM\s+(\w+)(?:\s+(?:AS\s+)?\w+)?/i)
    if (!tableM) return '-- Could not parse table name'
    const table = tableM[1]
    const lines: string[] = [table]
    const whereM = s.match(/WHERE\s+(.+?)(?:GROUP BY|ORDER BY|LIMIT|HAVING|$)/i)
    if (whereM) {
      const w = whereM[1].trim()
        .replace(/(\w+\.)(\w+)/g, '$2')
        .replace(/TRUE/gi, 'true').replace(/FALSE/gi, 'false')
        .replace(/AND/gi, ',').replace(/OR/gi, ' or ')
      lines.push(`  .filter(${w})`)
    }
    const groupM = s.match(/GROUP BY\s+(.+?)(?:ORDER BY|LIMIT|HAVING|$)/i)
    if (groupM) {
      const g = groupM[1].replace(/(\w+\.)(\w+)/g, '$2').trim()
      lines.push(`  .group(${g})`)
    }
    const orderM = s.match(/ORDER BY\s+(.+?)(?:LIMIT|$)/i)
    if (orderM) {
      const parts = orderM[1].split(',').map(p => {
        const [col, dir] = p.trim().split(/\s+/)
        const c = col.replace(/\w+\./, '')
        return dir?.toUpperCase() === 'DESC' ? `${c}.desc` : c
      })
      lines.push(`  .sort(${parts.join(', ')})`)
    }
    const limitM = s.match(/LIMIT\s+(\d+)/i)
    if (limitM) lines.push(`  .take(${limitM[1]})`)
    const selectM = s.match(/SELECT\s+([\s\S]+?)\s+FROM/i)
    if (selectM) {
      const cols = selectM[1].replace(/\s+/g, ' ').replace(/(\w+\.)(\w+)/g, '$2')
      if (!cols.trim().startsWith('*')) lines.push(`  .pick(${cols.trim()})`)
    }
    return lines.join('\n')
  } catch { return '-- Translation error' }
}

function axiomqlToSql(aql: string): string {
  try {
    const lines = aql.replace(/--[^\n]*/g, '').split('\n').map(l => l.trim()).filter(Boolean)
    const table = lines[0]?.match(/^(\w+)/)?.[1]
    if (!table) return '-- Could not parse table name'
    let select = '*', where = '', order = '', limit = '', group = ''
    for (const line of lines.slice(1)) {
      const m = line.match(/\.(\w+)\((.+)\)/)
      if (!m) continue
      const [, fn, args] = m
      if (fn === 'filter') where = args.replace(/,/g, ' AND').replace(/true/g, 'TRUE').replace(/false/g, 'FALSE')
      if (fn === 'pick') select = args
      if (fn === 'sort') {
        order = args.split(',').map(p => {
          const t = p.trim()
          return t.endsWith('.desc') ? `${t.replace('.desc', '')} DESC` : t
        }).join(', ')
      }
      if (fn === 'take') limit = args
      if (fn === 'group') group = args.split(',')[0]?.trim() ?? ''
    }
    let sql = `SELECT ${select}\nFROM ${table}`
    if (where) sql += `\nWHERE ${where}`
    if (group) sql += `\nGROUP BY ${group}`
    if (order) sql += `\nORDER BY ${order}`
    if (limit) sql += `\nLIMIT ${limit}`
    return sql + ';'
  } catch { return '-- Translation error' }
}

// ── CSV Export ────────────────────────────────────────────────────────────────

function exportCsv(data: Record<string, unknown>[], filename = 'results.csv') {
  if (!data.length) return
  const headers = Object.keys(data[0])
  const csv = [
    headers.join(','),
    ...data.map(row =>
      headers.map(h => JSON.stringify(row[h] ?? '')).join(',')
    ),
  ].join('\n')
  const blob = new Blob([csv], { type: 'text/csv' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

// ── History helpers ───────────────────────────────────────────────────────────

type HistoryEntry = {
  id: string
  query: string
  duration: number
  rows: number
  timestamp: string
}

function loadHistory(): HistoryEntry[] {
  if (typeof window === 'undefined') return []
  try {
    return JSON.parse(localStorage.getItem(HISTORY_KEY) ?? '[]') as HistoryEntry[]
  } catch { return [] }
}

function saveHistory(entries: HistoryEntry[]) {
  localStorage.setItem(HISTORY_KEY, JSON.stringify(entries.slice(0, MAX_HISTORY)))
}

// ── Tab types ─────────────────────────────────────────────────────────────────

type QueryMode = 'sql' | 'axiomql'

type QueryTab = {
  id: string
  title: string
  sqlValue: string
  axiomqlValue: string
  results: Record<string, unknown>[] | null
  duration: number | null
}

function makeTab(idx: number): QueryTab {
  return {
    id: crypto.randomUUID(),
    title: `Query ${idx}`,
    sqlValue: idx === 1 ? DEFAULT_SQL : `-- Query ${idx}\nSELECT * FROM users LIMIT 10`,
    axiomqlValue: idx === 1 ? DEFAULT_AXIOMQL : `-- Query ${idx}\nusers\n  .take(10)`,
    results: null,
    duration: null,
  }
}

// ── DataTable ─────────────────────────────────────────────────────────────────

type EditCell = { rowIdx: number; col: string } | null

function DataTable({ data: initial, tableName = 'result' }: { data: Record<string, unknown>[]; tableName?: string }) {
  const [rows, setRows] = useState(initial)
  const [sorting, setSorting] = useState<SortingState>([])
  const [pageIndex, setPageIndex] = useState(0)
  const [editing, setEditing] = useState<EditCell>(null)
  const [editVal, setEditVal] = useState('')
  const [lastSql, setLastSql] = useState<{ sql: string; axiomql: string } | null>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  const pageSize = 10
  const readOnly = new Set(['id', 'created_at'])

  useEffect(() => { if (editing) inputRef.current?.focus() }, [editing])

  function toggle(idx: number, col: string, cur: unknown) {
    const val = !cur
    setRows(p => p.map((r, i) => i === idx ? { ...r, [col]: val } : r))
    const row = rows[idx]
    setLastSql({ sql: `UPDATE ${tableName} SET ${col} = ${val} WHERE id = ${row.id};`, axiomql: `${tableName}.filter(id = ${row.id}).update(${col}: ${val})` })
  }

  function commit() {
    if (!editing) return
    const row = rows[editing.rowIdx]
    setRows(p => p.map((r, i) => i === editing.rowIdx ? { ...r, [editing.col]: editVal } : r))
    const v = typeof editVal === 'string' ? `'${editVal}'` : editVal
    setLastSql({ sql: `UPDATE ${tableName} SET ${editing.col} = ${v} WHERE id = ${row.id};`, axiomql: `${tableName}.filter(id = ${row.id}).update(${editing.col}: ${v})` })
    setEditing(null)
  }

  const columns: ColumnDef<Record<string, unknown>>[] = rows.length > 0
    ? Object.keys(rows[0]).map(key => ({ accessorKey: key, header: key, enableSorting: true }))
    : []

  const table = useReactTable({
    data: rows, columns,
    state: { sorting, pagination: { pageIndex, pageSize } },
    onSortingChange: setSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
  })

  return (
    <div className="flex flex-col h-full">
      <div className="flex-1 overflow-auto">
        <table className="w-full text-xs">
          <thead className="sticky top-0 bg-surface z-10">
            {table.getHeaderGroups().map(hg => (
              <tr key={hg.id} className="border-b border-border">
                {hg.headers.map(h => {
                  const s = h.column.getIsSorted()
                  return (
                    <th key={h.id} onClick={h.column.getToggleSortingHandler()}
                      className="text-left px-3 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase cursor-pointer hover:text-text-primary select-none whitespace-nowrap group">
                      <div className="flex items-center gap-1">
                        {flexRender(h.column.columnDef.header, h.getContext())}
                        <span className="opacity-30 group-hover:opacity-80">
                          {s === 'asc' ? <ChevronUp className="w-3 h-3 text-accent inline" /> : s === 'desc' ? <ChevronDown className="w-3 h-3 text-accent inline" /> : <ChevronsUpDown className="w-3 h-3 inline" />}
                        </span>
                      </div>
                    </th>
                  )
                })}
              </tr>
            ))}
          </thead>
          <tbody>
            {table.getRowModel().rows.map(row => (
              <tr key={row.id} className="border-b border-border/50 hover:bg-elevated transition-colors group">
                {row.getVisibleCells().map(cell => {
                  const col = cell.column.id
                  const isEditing = editing?.rowIdx === row.index && editing?.col === col
                  const canEdit = !readOnly.has(col)
                  const val = cell.getValue()
                  return (
                    <td key={cell.id}
                      className={cn('whitespace-nowrap', isEditing ? 'p-0' : 'px-3 py-1.5', canEdit && 'cursor-pointer')}
                      onClick={() => {
                        if (isEditing || !canEdit) return
                        if (typeof val === 'boolean') { toggle(row.index, col, val); return }
                        setEditing({ rowIdx: row.index, col }); setEditVal(String(val ?? ''))
                      }}>
                      {isEditing ? (
                        <div className="flex items-center">
                          <input ref={inputRef} value={editVal} onChange={e => setEditVal(e.target.value)}
                            onKeyDown={e => { if (e.key === 'Enter') commit(); if (e.key === 'Escape') setEditing(null) }}
                            onBlur={commit}
                            className="w-full px-3 py-1.5 bg-elevated border-2 border-accent outline-none font-mono text-xs text-text-primary" />
                          <button onMouseDown={e => { e.preventDefault(); commit() }} className="px-1.5 py-1.5 bg-accent/10 text-accent border-y-2 border-r-2 border-accent/30"><Check className="w-3 h-3" /></button>
                          <button onMouseDown={e => { e.preventDefault(); setEditing(null) }} className="px-1.5 py-1.5 bg-error/10 text-error border-y-2 border-r-2 border-error/30"><X className="w-3 h-3" /></button>
                        </div>
                      ) : (
                        <div className="flex items-center gap-1">
                          {typeof val === 'boolean'
                            ? <span className={`text-[10px] px-1.5 py-0.5 rounded font-semibold ${val ? 'bg-accent/10 text-accent' : 'bg-border text-text-secondary'}`}>{String(val)}</span>
                            : <span className="font-mono text-xs text-text-secondary">{String(val ?? '')}</span>
                          }
                          {canEdit && <span className="opacity-0 group-hover:opacity-30 text-[9px]">✎</span>}
                        </div>
                      )}
                    </td>
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {lastSql && (
        <div className="border-t border-border/50 bg-elevated px-3 py-2 shrink-0">
          <div className="flex items-center justify-between mb-1">
            <span className="text-[9px] text-text-secondary uppercase tracking-wider font-semibold">Last operation</span>
            <button onClick={() => setLastSql(null)} className="text-[10px] text-text-secondary hover:text-text-primary">✕</button>
          </div>
          <div className="grid grid-cols-2 gap-2">
            <code className="font-mono text-[11px] text-accent bg-surface px-2 py-1 rounded border border-border/50 truncate block">{lastSql.sql}</code>
            <code className="font-mono text-[11px] text-blue-400 bg-surface px-2 py-1 rounded border border-border/50 truncate block">{lastSql.axiomql}</code>
          </div>
        </div>
      )}

      <div className="border-t border-border px-3 py-2 flex items-center justify-between shrink-0">
        <span className="text-xs text-text-secondary">{rows.length.toLocaleString()} rows</span>
        <div className="flex items-center gap-2">
          <button onClick={() => setPageIndex(p => Math.max(0, p - 1))} disabled={pageIndex === 0}
            className="text-xs text-text-secondary hover:text-text-primary disabled:opacity-30 px-2 py-1 rounded hover:bg-elevated">← Prev</button>
          <span className="text-xs text-text-secondary">Page {pageIndex + 1} of {table.getPageCount()}</span>
          <button onClick={() => setPageIndex(p => Math.min(table.getPageCount() - 1, p + 1))} disabled={pageIndex >= table.getPageCount() - 1}
            className="text-xs text-text-secondary hover:text-text-primary disabled:opacity-30 px-2 py-1 rounded hover:bg-elevated">Next →</button>
        </div>
      </div>
    </div>
  )
}

// ── History panel ─────────────────────────────────────────────────────────────

function HistoryPanel({
  onLoad,
  onClose,
}: {
  onLoad: (entry: HistoryEntry) => void
  onClose: () => void
}) {
  const [entries, setEntries] = useState<HistoryEntry[]>(() => loadHistory())

  function clearAll() {
    localStorage.removeItem(HISTORY_KEY)
    setEntries([])
  }

  return (
    <div className="absolute right-0 top-full mt-1 z-50 w-96 bg-surface border border-border rounded-lg shadow-xl overflow-hidden">
      <div className="flex items-center justify-between px-3 py-2 border-b border-border">
        <span className="text-xs font-semibold text-text-primary">Query History</span>
        <div className="flex items-center gap-2">
          {entries.length > 0 && (
            <button onClick={clearAll}
              className="text-[10px] text-error/70 hover:text-error transition-colors">
              Clear all
            </button>
          )}
          <button onClick={onClose} className="text-text-secondary hover:text-text-primary transition-colors">
            <X className="w-3.5 h-3.5" />
          </button>
        </div>
      </div>
      <div className="max-h-80 overflow-y-auto">
        {entries.length === 0 ? (
          <div className="px-4 py-6 text-center text-xs text-text-secondary">No queries yet</div>
        ) : (
          entries.map(e => (
            <button key={e.id} onClick={() => { onLoad(e); onClose() }}
              className="w-full text-left px-3 py-2.5 border-b border-border/50 hover:bg-elevated transition-colors group">
              <div className="font-mono text-[11px] text-text-primary truncate">{e.query.slice(0, 60)}{e.query.length > 60 ? '…' : ''}</div>
              <div className="flex items-center gap-3 mt-1">
                <span className="text-[10px] text-text-secondary font-mono">{e.timestamp}</span>
                <span className="text-[10px] text-accent font-mono">{e.duration}ms</span>
                <span className="text-[10px] text-text-secondary font-mono">{e.rows} rows</span>
              </div>
            </button>
          ))
        )}
      </div>
    </div>
  )
}

// ── Main page ─────────────────────────────────────────────────────────────────

export default function QueryPage() {
  const [tabs, setTabs] = useState<QueryTab[]>([makeTab(1)])
  const [activeTabId, setActiveTabId] = useState<string>(() => tabs[0].id)
  const [mode, setMode] = useState<QueryMode>('sql')
  const [error, setError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)
  const [showHistory, setShowHistory] = useState(false)
  const tabCounterRef = useRef(2)

  const activeTab = tabs.find(t => t.id === activeTabId) ?? tabs[0]

  function updateActiveTab(patch: Partial<QueryTab>) {
    setTabs(prev => prev.map(t => t.id === activeTabId ? { ...t, ...patch } : t))
  }

  const sqlValue = activeTab.sqlValue
  const axiomqlValue = activeTab.axiomqlValue
  const currentValue = mode === 'sql' ? sqlValue : axiomqlValue

  function setCurrentValue(v: string) {
    if (mode === 'sql') updateActiveTab({ sqlValue: v })
    else updateActiveTab({ axiomqlValue: v })
  }

  const handleRun = useCallback(async () => {
    setRunning(true)
    setError(null)
    await new Promise(r => setTimeout(r, 300 + Math.random() * 200))
    const d = Math.round(4 + Math.random() * 40)
    const results = MOCK_RESULTS.default as Record<string, unknown>[]
    updateActiveTab({ results, duration: d })
    setRunning(false)

    // Save to history
    const query = mode === 'sql' ? activeTab.sqlValue : activeTab.axiomqlValue
    const entry: HistoryEntry = {
      id: crypto.randomUUID(),
      query: query.replace(/--[^\n]*/g, '').trim(),
      duration: d,
      rows: results.length,
      timestamp: new Date().toLocaleTimeString(),
    }
    const prev = loadHistory()
    saveHistory([entry, ...prev])
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTabId, mode, activeTab.sqlValue, activeTab.axiomqlValue])

  // ⌘+Enter / Ctrl+Enter shortcut
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
        e.preventDefault()
        if (!running) handleRun()
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [running, handleRun])

  function addTab() {
    const tab = makeTab(tabCounterRef.current++)
    setTabs(prev => [...prev, tab])
    setActiveTabId(tab.id)
  }

  function closeTab(id: string) {
    if (tabs.length === 1) return
    setTabs(prev => {
      const next = prev.filter(t => t.id !== id)
      if (activeTabId === id) setActiveTabId(next[next.length - 1].id)
      return next
    })
  }

  function loadHistoryEntry(entry: HistoryEntry) {
    updateActiveTab({ sqlValue: entry.query, results: null, duration: null })
    setMode('sql')
  }

  const results = activeTab.results
  const duration = activeTab.duration

  return (
    <div className="flex flex-col h-full">
      {/* Tab bar */}
      <div className="border-b border-border flex items-center overflow-x-auto shrink-0 bg-surface">
        <div className="flex items-center min-w-0 flex-1 overflow-x-auto">
          {tabs.map(tab => (
            <div key={tab.id}
              className={cn(
                'flex items-center gap-1.5 px-3 py-2 text-xs font-medium border-r border-border shrink-0 cursor-pointer select-none transition-colors group',
                tab.id === activeTabId
                  ? 'bg-bg text-text-primary border-b-bg -mb-px relative z-10'
                  : 'text-text-secondary hover:text-text-primary hover:bg-elevated'
              )}
              onClick={() => setActiveTabId(tab.id)}>
              <span>{tab.title}</span>
              <button
                onClick={e => { e.stopPropagation(); closeTab(tab.id) }}
                className={cn(
                  'rounded hover:bg-border transition-colors p-0.5',
                  tab.id === activeTabId ? 'opacity-60 hover:opacity-100' : 'opacity-0 group-hover:opacity-60'
                )}>
                <X className="w-2.5 h-2.5" />
              </button>
            </div>
          ))}
        </div>
        <button onClick={addTab}
          className="px-3 py-2 text-text-secondary hover:text-text-primary hover:bg-elevated transition-colors shrink-0 border-l border-border">
          <Plus className="w-3.5 h-3.5" />
        </button>
      </div>

      {/* Mode header */}
      <div className="border-b border-border px-6 py-3 flex items-center justify-between shrink-0">
        <h1 className="text-sm font-semibold text-text-primary">Query Editor</h1>
        <div className="flex items-center gap-2">
          <button
            onClick={() => {
              if (mode === 'sql') {
                updateActiveTab({ axiomqlValue: sqlToAxiomql(sqlValue) })
                setMode('axiomql')
              } else {
                updateActiveTab({ sqlValue: axiomqlToSql(axiomqlValue) })
                setMode('sql')
              }
            }}
            className="flex items-center gap-1.5 px-2 py-1 text-xs text-text-secondary border border-border rounded hover:border-accent/50 hover:text-accent transition-colors font-medium">
            {mode === 'sql' ? '→ AxiomQL' : '→ SQL'}
          </button>
          <div className="flex items-center gap-1 bg-elevated rounded p-0.5">
            {(['sql', 'axiomql'] as QueryMode[]).map(m => (
              <button key={m} onClick={() => setMode(m)}
                className={cn(
                  'px-3 py-1 text-xs rounded font-medium transition-colors',
                  mode === m ? 'bg-surface text-text-primary shadow-sm' : 'text-text-secondary hover:text-text-primary'
                )}>
                {m === 'sql' ? 'SQL' : 'AxiomQL'}
              </button>
            ))}
          </div>
        </div>
      </div>

      {/* Editor */}
      <div className="border-b border-border shrink-0" style={{ height: 280 }}>
        <MonacoEditor
          key={activeTabId + '-' + mode}
          height="100%"
          language={mode === 'sql' ? 'sql' : 'plaintext'}
          value={currentValue}
          onChange={v => setCurrentValue(v ?? '')}
          theme="vs-dark"
          options={{
            fontSize: 13,
            fontFamily: 'var(--font-geist-mono)',
            lineHeight: 1.6,
            minimap: { enabled: false },
            scrollBeyondLastLine: false,
            padding: { top: 16, bottom: 16 },
            renderLineHighlight: 'none',
            overviewRulerLanes: 0,
            hideCursorInOverviewRuler: true,
            scrollbar: { vertical: 'hidden', horizontal: 'hidden' },
            suggest: { showKeywords: true },
          }}
        />
      </div>

      {/* Toolbar */}
      <div className="border-b border-border px-4 py-2 flex items-center gap-2 shrink-0">
        <button onClick={handleRun} disabled={running}
          className={cn(
            'flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-semibold transition-all',
            running
              ? 'bg-accent/50 text-white/50 cursor-not-allowed'
              : 'bg-accent text-white hover:bg-accent-dim active:scale-95'
          )}>
          <Play className="w-3 h-3" />
          {running ? 'Running...' : 'Run'}
          <span className="text-white/50 text-[10px] ml-1">⌘↵</span>
        </button>

        <button className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-blue-400 border border-blue-400/30 hover:bg-blue-400/10 transition-colors">
          <Zap className="w-3 h-3" />
          Explain
        </button>

        <button
          onClick={() => results && exportCsv(results, `${activeTab.title.replace(/\s+/g, '_').toLowerCase()}.csv`)}
          disabled={!results}
          className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-text-secondary border border-border hover:bg-elevated hover:text-text-primary transition-colors disabled:opacity-40">
          <Download className="w-3 h-3" />
          Export CSV
        </button>

        {/* History toggle */}
        <div className="relative">
          <button
            onClick={() => setShowHistory(p => !p)}
            className={cn(
              'flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs font-medium border transition-colors',
              showHistory
                ? 'border-accent text-accent bg-accent/10'
                : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary'
            )}>
            <Clock className="w-3 h-3" />
          </button>
          {showHistory && (
            <HistoryPanel
              onLoad={loadHistoryEntry}
              onClose={() => setShowHistory(false)}
            />
          )}
        </div>

        {duration !== null && results && (
          <div className="ml-auto flex items-center gap-1.5 text-xs text-text-secondary font-mono">
            <span className="text-accent">{duration}ms</span>
            <span>·</span>
            <span>{results.length.toLocaleString()} rows</span>
          </div>
        )}
      </div>

      {/* Results */}
      <div className="flex-1 overflow-hidden">
        {error && (
          <div className="flex items-start gap-2 m-4 p-3 rounded bg-error/10 border border-error/30 text-xs text-error">
            <AlertCircle className="w-4 h-4 shrink-0 mt-0.5" />
            {error}
          </div>
        )}
        {results && !error && <DataTable data={results} />}
        {!results && !error && (
          <div className="flex items-center justify-center h-full text-text-secondary text-xs">
            Run a query to see results
          </div>
        )}
      </div>
    </div>
  )
}
