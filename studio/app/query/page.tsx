'use client'
import { useState, useRef, useEffect } from 'react'
import dynamic from 'next/dynamic'
import { Play, Zap, Download, AlertCircle, ChevronUp, ChevronDown, ChevronsUpDown, Check, X } from 'lucide-react'
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

const DEFAULT_SQL = `-- AxiomDB Query Editor
-- Press Cmd+Enter to run

SELECT
  u.id,
  u.name,
  u.email,
  u.age,
  u.active,
  u.created_at
FROM users u
WHERE u.active = TRUE
ORDER BY u.created_at DESC
LIMIT 50`

const DEFAULT_AXIOMQL = `users
  .filter(active)
  .sort(created_at.desc)
  .take(50)`

type QueryMode = 'sql' | 'axiomql'

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

export default function QueryPage() {
  const [mode, setMode] = useState<QueryMode>('sql')
  const [sqlValue, setSqlValue] = useState(DEFAULT_SQL)
  const [axiomqlValue, setAxiomqlValue] = useState(DEFAULT_AXIOMQL)
  const [results, setResults] = useState<Record<string, unknown>[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [duration, setDuration] = useState<number | null>(null)
  const [running, setRunning] = useState(false)

  const handleRun = async () => {
    setRunning(true)
    setError(null)
    await new Promise(r => setTimeout(r, 300 + Math.random() * 200))
    const d = Math.round(4 + Math.random() * 40)
    setDuration(d)
    setResults(MOCK_RESULTS.default as Record<string, unknown>[])
    setRunning(false)
  }

  const currentValue = mode === 'sql' ? sqlValue : axiomqlValue
  const setCurrentValue = mode === 'sql' ? setSqlValue : setAxiomqlValue

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="border-b border-border px-6 py-3 flex items-center justify-between">
        <h1 className="text-sm font-semibold text-text-primary">Query Editor</h1>
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

      {/* Editor */}
      <div className="border-b border-border" style={{ height: 280 }}>
        <MonacoEditor
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
      <div className="border-b border-border px-4 py-2 flex items-center gap-2">
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

        <button className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-text-secondary border border-border hover:bg-elevated hover:text-text-primary transition-colors">
          <Download className="w-3 h-3" />
          Export CSV
        </button>

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
