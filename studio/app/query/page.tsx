'use client'
import { useState, useRef, useEffect, useCallback } from 'react'
import dynamic from 'next/dynamic'
import type * as Monaco from 'monaco-editor'
import {
  Play, Zap, Download, AlertCircle, ChevronUp, ChevronDown,
  ChevronsUpDown, Check, X, Clock, Trash2, Plus,
  AlignLeft, LayoutPanelLeft, BookmarkPlus, DollarSign, BarChart2,
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

// ── Schema / completion constants ─────────────────────────────────────────────

const TABLE_NAMES = ['users', 'orders', 'products', 'categories', 'active_users']

const SCHEMA_COMPLETIONS: Record<string, { name: string; type: string }[]> = {
  users: [
    { name: 'id', type: 'INT' }, { name: 'name', type: 'TEXT' },
    { name: 'email', type: 'TEXT' }, { name: 'age', type: 'INT' },
    { name: 'active', type: 'BOOL' }, { name: 'created_at', type: 'TIMESTAMP' },
  ],
  orders: [
    { name: 'id', type: 'INT' }, { name: 'user_id', type: 'INT' },
    { name: 'amount', type: 'REAL' }, { name: 'status', type: 'TEXT' },
    { name: 'created_at', type: 'TIMESTAMP' },
  ],
  products: [
    { name: 'id', type: 'INT' }, { name: 'name', type: 'TEXT' },
    { name: 'price', type: 'REAL' }, { name: 'stock', type: 'INT' },
    { name: 'category_id', type: 'INT' },
  ],
  categories: [
    { name: 'id', type: 'INT' }, { name: 'name', type: 'TEXT' }, { name: 'slug', type: 'TEXT' },
  ],
  u: [
    { name: 'id', type: 'INT' }, { name: 'name', type: 'TEXT' }, { name: 'email', type: 'TEXT' },
    { name: 'age', type: 'INT' }, { name: 'active', type: 'BOOL' }, { name: 'created_at', type: 'TIMESTAMP' },
  ],
  o: [
    { name: 'id', type: 'INT' }, { name: 'user_id', type: 'INT' }, { name: 'amount', type: 'REAL' },
    { name: 'status', type: 'TEXT' }, { name: 'created_at', type: 'TIMESTAMP' },
  ],
}

// ── Monaco: register AxiomQL language + completion provider ───────────────────

function registerAxiomQL(monaco: typeof Monaco) {
  if (monaco.languages.getLanguages().some(l => l.id === 'axiomql')) return

  monaco.languages.register({ id: 'axiomql' })

  monaco.languages.setMonarchTokensProvider('axiomql', {
    tokenizer: {
      root: [
        [/--.*$/, 'comment'],
        [/\.(filter|pick|sort|join|group|take|skip|distinct|count|sum|avg|min|max|union|intersect|except|window|explain|export|insert|update|delete|upsert|returning|watch|subscribe)\b/, 'keyword'],
        [/\b(true|false|null)\b/, 'constant'],
        [/\b(from|where|let|proc|fn|on|transaction|create|drop|index|migration)\b/, 'type'],
        [/'[^']*'/, 'string'],
        [/"[^"]*"/, 'string'],
        [/\b\d+(\.\d+)?\b/, 'number'],
        [/[(),.:]/, 'delimiter'],
        [/→/, 'keyword'],
        [/\w+(?=\s*\()/, 'function'],
        [/\w+(?=\s*\.)/, 'variable'],
      ],
    },
  })

  monaco.editor.defineTheme('axiomql-dark', {
    base: 'vs-dark',
    inherit: true,
    rules: [
      { token: 'keyword', foreground: '10b981', fontStyle: 'bold' },
      { token: 'constant', foreground: 'f59e0b' },
      { token: 'type', foreground: '60a5fa' },
      { token: 'function', foreground: 'c084fc' },
      { token: 'variable', foreground: 'e2e8f0' },
      { token: 'string', foreground: '86efac' },
      { token: 'number', foreground: 'fb923c' },
      { token: 'comment', foreground: '6b7280', fontStyle: 'italic' },
    ],
    colors: {
      'editor.background': '#0d1117',
    },
  })
}

// Defined outside component so it is referentially stable
function handleEditorMount(
  _editor: Monaco.editor.IStandaloneCodeEditor,
  monaco: typeof Monaco,
  currentMode: 'sql' | 'axiomql',
) {
  registerAxiomQL(monaco)

  // Apply AxiomQL theme when in AxiomQL mode
  if (currentMode === 'axiomql') {
    monaco.editor.setTheme('axiomql-dark')
  }

  monaco.languages.registerCompletionItemProvider('sql', {
    triggerCharacters: [' ', '.', '\n'],
    provideCompletionItems: (model, position) => {
      const word = model.getWordUntilPosition(position)
      const range = {
        startLineNumber: position.lineNumber,
        endLineNumber: position.lineNumber,
        startColumn: word.startColumn,
        endColumn: word.endColumn,
      }
      const lineText = model.getLineContent(position.lineNumber)
      const textBeforeCursor = lineText.slice(0, position.column - 1)

      // After FROM / JOIN / INTO / UPDATE: suggest table names
      if (/\b(FROM|JOIN|INTO|UPDATE)\s+\w*$/i.test(textBeforeCursor)) {
        return {
          suggestions: TABLE_NAMES.map(t => ({
            label: t,
            kind: monaco.languages.CompletionItemKind.Class,
            insertText: t,
            range,
          })),
        }
      }

      // After "table." or "alias.": suggest columns
      const dotMatch = textBeforeCursor.match(/(\w+)\.\w*$/)
      if (dotMatch) {
        const cols = SCHEMA_COMPLETIONS[dotMatch[1].toLowerCase()] ?? []
        return {
          suggestions: cols.map(c => ({
            label: c.name,
            detail: c.type,
            kind: monaco.languages.CompletionItemKind.Field,
            insertText: c.name,
            range,
          })),
        }
      }

      // SQL keywords
      const keywords = [
        'SELECT', 'FROM', 'WHERE', 'JOIN', 'LEFT JOIN', 'GROUP BY', 'ORDER BY',
        'LIMIT', 'HAVING', 'INSERT INTO', 'UPDATE', 'DELETE FROM', 'CREATE TABLE',
        'DROP TABLE', 'AND', 'OR', 'NOT', 'NULL', 'TRUE', 'FALSE',
        'COUNT(*)', 'SUM(', 'AVG(', 'MIN(', 'MAX(', 'DISTINCT', 'AS',
      ]
      return {
        suggestions: keywords.map(k => ({
          label: k,
          kind: monaco.languages.CompletionItemKind.Keyword,
          insertText: k,
          range,
        })),
      }
    },
  })
}

// ── Format SQL ────────────────────────────────────────────────────────────────

function formatSql(sql: string): string {
  const keywords = [
    'SELECT', 'FROM', 'WHERE', 'JOIN', 'LEFT JOIN', 'RIGHT JOIN', 'INNER JOIN',
    'GROUP BY', 'ORDER BY', 'HAVING', 'LIMIT', 'OFFSET', 'INSERT INTO', 'VALUES',
    'UPDATE', 'SET', 'DELETE FROM', 'CREATE TABLE', 'DROP TABLE', 'AND', 'OR',
  ]

  let result = sql.replace(/\s+/g, ' ').trim()

  // Uppercase keywords (longest first to avoid partial matches)
  const sorted = [...keywords].sort((a, b) => b.length - a.length)
  sorted.forEach(kw => {
    result = result.replace(new RegExp(`\\b${kw}\\b`, 'gi'), kw)
  })

  // Line breaks before main clauses
  const breakBefore = [
    'FROM', 'WHERE', 'JOIN', 'LEFT JOIN', 'RIGHT JOIN', 'INNER JOIN',
    'GROUP BY', 'ORDER BY', 'HAVING', 'LIMIT', 'OFFSET',
  ]
  breakBefore.sort((a, b) => b.length - a.length).forEach(kw => {
    result = result.replace(new RegExp(`\\s+${kw}\\b`, 'g'), `\n${kw}`)
  })

  // AND / OR on indented new lines
  result = result.replace(/\s+(AND|OR)\s+/g, '\n  $1 ')

  return result
}

// ── Chart detection and component ─────────────────────────────────────────────

function detectChartable(
  data: Record<string, unknown>[],
): { labelCol: string; valueCol: string } | null {
  if (!data.length) return null
  const keys = Object.keys(data[0])
  const numericCols = keys.filter(k => typeof data[0][k] === 'number')
  const textCols = keys.filter(k => typeof data[0][k] === 'string')
  if (numericCols.length === 1 && textCols.length >= 1) {
    return { labelCol: textCols[0], valueCol: numericCols[0] }
  }
  if (numericCols.length >= 2) {
    return { labelCol: keys[0], valueCol: numericCols[0] }
  }
  return null
}

function BarChart({
  data,
  labelCol,
  valueCol,
}: {
  data: Record<string, unknown>[]
  labelCol: string
  valueCol: string
}) {
  const items = data.slice(0, 20)
  const max = Math.max(...items.map(d => Number(d[valueCol]) || 0))
  const barH = 20
  const gap = 6
  const labelW = 120
  const barAreaW = 200
  const valueW = 60
  const totalW = labelW + barAreaW + valueW
  const totalH = items.length * (barH + gap)

  return (
    <div className="flex-1 overflow-auto p-4">
      <svg width={totalW} height={totalH} className="font-mono">
        {items.map((row, i) => {
          const val = Number(row[valueCol]) || 0
          const barW = max > 0 ? (val / max) * barAreaW : 0
          const y = i * (barH + gap)
          return (
            <g key={i}>
              <text
                x={labelW - 8}
                y={y + barH * 0.7}
                textAnchor="end"
                fontSize="10"
                fill="#8b949e"
              >
                {String(row[labelCol]).slice(0, 16)}
              </text>
              <rect
                x={labelW}
                y={y}
                width={barW}
                height={barH}
                fill="#10b981"
                opacity="0.8"
                rx="2"
              />
              <text x={labelW + barW + 6} y={y + barH * 0.7} fontSize="10" fill="#e6edf3">
                {val.toLocaleString()}
              </text>
            </g>
          )
        })}
      </svg>
    </div>
  )
}

// ── Saved queries ─────────────────────────────────────────────────────────────

type QueryMode = 'sql' | 'axiomql'

type SavedQuery = {
  id: string
  name: string
  query: string
  mode: QueryMode
  savedAt: string
}

const SAVED_KEY = 'axiomstudio_saved'

function loadSaved(): SavedQuery[] {
  if (typeof window === 'undefined') return []
  try {
    return JSON.parse(localStorage.getItem(SAVED_KEY) ?? '[]') as SavedQuery[]
  } catch {
    return []
  }
}

function persistSaved(items: SavedQuery[]) {
  localStorage.setItem(SAVED_KEY, JSON.stringify(items))
}

function SavedPanel({
  currentQuery,
  currentMode,
  onLoad,
  onClose,
}: {
  currentQuery: string
  currentMode: QueryMode
  onLoad: (s: SavedQuery) => void
  onClose: () => void
}) {
  const [items, setItems] = useState<SavedQuery[]>(() => loadSaved())
  const [naming, setNaming] = useState(false)
  const [newName, setNewName] = useState('')
  const nameInputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (naming) nameInputRef.current?.focus()
  }, [naming])

  function save() {
    if (!newName.trim()) return
    const entry: SavedQuery = {
      id: crypto.randomUUID(),
      name: newName.trim(),
      query: currentQuery,
      mode: currentMode,
      savedAt: new Date().toLocaleString(),
    }
    const next = [entry, ...items]
    setItems(next)
    persistSaved(next)
    setNaming(false)
    setNewName('')
  }

  function remove(id: string) {
    const next = items.filter(i => i.id !== id)
    setItems(next)
    persistSaved(next)
  }

  return (
    <div className="absolute right-0 top-full mt-1 z-50 w-96 bg-surface border border-border rounded-lg shadow-xl overflow-hidden">
      <div className="flex items-center justify-between px-3 py-2 border-b border-border">
        <span className="text-xs font-semibold text-text-primary">Saved Queries</span>
        <button onClick={onClose} className="text-text-secondary hover:text-text-primary transition-colors">
          <X className="w-3.5 h-3.5" />
        </button>
      </div>

      <div className="px-3 py-2 border-b border-border/50">
        {naming ? (
          <div className="flex items-center gap-1.5">
            <input
              ref={nameInputRef}
              value={newName}
              onChange={e => setNewName(e.target.value)}
              onKeyDown={e => {
                if (e.key === 'Enter') save()
                if (e.key === 'Escape') { setNaming(false); setNewName('') }
              }}
              placeholder="Query name..."
              className="flex-1 text-xs bg-elevated border border-border rounded px-2 py-1 outline-none focus:border-accent text-text-primary placeholder:text-text-secondary"
            />
            <button
              onClick={save}
              className="text-xs px-2 py-1 rounded bg-accent text-white font-medium hover:bg-accent-dim transition-colors"
            >
              Save
            </button>
            <button
              onClick={() => { setNaming(false); setNewName('') }}
              className="text-xs px-2 py-1 rounded border border-border text-text-secondary hover:bg-elevated transition-colors"
            >
              Cancel
            </button>
          </div>
        ) : (
          <button
            onClick={() => setNaming(true)}
            className="w-full flex items-center gap-1.5 text-xs text-accent hover:text-accent-dim transition-colors py-0.5"
          >
            <Plus className="w-3 h-3" />
            Save current query
          </button>
        )}
      </div>

      <div className="max-h-72 overflow-y-auto">
        {items.length === 0 ? (
          <div className="px-4 py-6 text-center text-xs text-text-secondary">No saved queries</div>
        ) : (
          items.map(item => (
            <div
              key={item.id}
              className="flex items-start justify-between px-3 py-2.5 border-b border-border/50 hover:bg-elevated transition-colors group"
            >
              <button
                onClick={() => { onLoad(item); onClose() }}
                className="flex-1 text-left"
              >
                <div className="text-xs font-medium text-text-primary">{item.name}</div>
                <div className="flex items-center gap-2 mt-0.5">
                  <span className="text-[10px] text-text-secondary font-mono">{item.savedAt}</span>
                  <span className={`text-[9px] px-1 rounded font-semibold ${item.mode === 'sql' ? 'bg-blue-400/10 text-blue-400' : 'bg-accent/10 text-accent'}`}>
                    {item.mode.toUpperCase()}
                  </span>
                </div>
                <div className="font-mono text-[10px] text-text-secondary truncate mt-0.5">
                  {item.query.replace(/--[^\n]*/g, '').trim().slice(0, 50)}…
                </div>
              </button>
              <button
                onClick={() => remove(item.id)}
                className="opacity-0 group-hover:opacity-100 ml-2 text-text-secondary hover:text-error transition-all shrink-0"
              >
                <X className="w-3 h-3" />
              </button>
            </div>
          ))
        )}
      </div>
    </div>
  )
}

// ── Mock results ──────────────────────────────────────────────────────────────

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

// ── SQL ↔ AxiomQL translator ──────────────────────────────────────────────────

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

// ── EXPLAIN plan ──────────────────────────────────────────────────────────────

type PlanNode = {
  type: string
  table?: string
  cost: number
  rows: number
  filter?: string
  children: PlanNode[]
}

function generateMockPlan(sql: string): PlanNode {
  const hasJoin = /JOIN/i.test(sql)
  const hasWhere = /WHERE/i.test(sql)
  const table = sql.match(/FROM\s+(\w+)/i)?.[1] ?? 'table'

  if (hasJoin) {
    return {
      type: 'Hash Join', cost: 285, rows: 1847, children: [
        { type: 'Seq Scan', table, cost: 45, rows: 10234, filter: hasWhere ? 'active = TRUE' : undefined, children: [] },
        {
          type: 'Hash', cost: 12, rows: 51847, children: [
            { type: 'Seq Scan', table: 'orders', cost: 12, rows: 51847, children: [] },
          ]
        },
      ]
    }
  }
  return {
    type: 'Seq Scan', table, cost: 45, rows: 10234,
    filter: hasWhere ? 'filter applied' : undefined,
    children: [],
  }
}

function PlanTree({ node, depth = 0 }: { node: PlanNode; depth?: number }) {
  const [open, setOpen] = useState(true)
  const isExpensive = node.cost > 200
  return (
    <div className="font-mono text-xs">
      <div
        className={cn(
          'flex items-center gap-2 py-1.5 rounded hover:bg-elevated cursor-pointer',
          depth === 0 && 'font-semibold',
        )}
        onClick={() => setOpen(o => !o)}
        style={{ paddingLeft: `${depth * 20 + 8}px` }}
      >
        {node.children.length > 0 && (
          <span className="text-text-secondary text-[10px] w-3 shrink-0">
            {open ? '▾' : '▸'}
          </span>
        )}
        {node.children.length === 0 && <span className="w-3 shrink-0" />}
        <span className={isExpensive ? 'text-[#d29922]' : 'text-[#10b981]'}>{node.type}</span>
        {node.table && (
          <span className="text-text-secondary">
            on <span className="text-text-primary">{node.table}</span>
          </span>
        )}
        <span className="ml-auto text-text-secondary text-[11px] pr-3">
          cost={node.cost}&nbsp;&nbsp;rows={node.rows.toLocaleString()}
        </span>
        {node.filter && (
          <span className="text-[#d29922] text-[10px] pr-2">filter: {node.filter}</span>
        )}
      </div>
      {open && node.children.map((child, i) => (
        <PlanTree key={i} node={child} depth={depth + 1} />
      ))}
    </div>
  )
}

// ── CSV Export ─────────────────────────────────────────────────────────────────

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

// ── Query variables ───────────────────────────────────────────────────────────

type QueryVar = { id: string; name: string; value: string }

function applyVariables(query: string, vars: QueryVar[]): string {
  let result = query
  for (const v of vars) {
    if (!v.name.trim()) continue
    const safeName = v.name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
    result = result.replace(new RegExp(`\\$${safeName}\\b`, 'g'), v.value)
  }
  return result
}

// ── DataTable ─────────────────────────────────────────────────────────────────

type EditCell = { rowIdx: number; col: string } | null

function DataTable({
  data: initial,
  tableName = 'result',
}: {
  data: Record<string, unknown>[]
  tableName?: string
}) {
  const [rows, setRows] = useState(initial)
  const [sorting, setSorting] = useState<SortingState>([])
  const [pageIndex, setPageIndex] = useState(0)
  const [editing, setEditing] = useState<EditCell>(null)
  const [editVal, setEditVal] = useState('')
  const [lastSql, setLastSql] = useState<{ sql: string; axiomql: string } | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<number | null>(null)
  const [resultsTab, setResultsTab] = useState<'table' | 'chart'>('table')
  const inputRef = useRef<HTMLInputElement>(null)
  const pageSize = 10
  const readOnly = new Set(['id', 'created_at'])

  const chartable = detectChartable(rows)

  useEffect(() => { if (editing) inputRef.current?.focus() }, [editing])

  function toggle(idx: number, col: string, cur: unknown) {
    const val = !cur
    setRows(p => p.map((r, i) => i === idx ? { ...r, [col]: val } : r))
    const row = rows[idx]
    setLastSql({
      sql: `UPDATE ${tableName} SET ${col} = ${val} WHERE id = ${row.id};`,
      axiomql: `${tableName}.filter(id = ${row.id}).update(${col}: ${val})`,
    })
  }

  function commit() {
    if (!editing) return
    const row = rows[editing.rowIdx]
    setRows(p => p.map((r, i) => i === editing.rowIdx ? { ...r, [editing.col]: editVal } : r))
    const v = typeof editVal === 'string' ? `'${editVal}'` : editVal
    setLastSql({
      sql: `UPDATE ${tableName} SET ${editing.col} = ${v} WHERE id = ${row.id};`,
      axiomql: `${tableName}.filter(id = ${row.id}).update(${editing.col}: ${v})`,
    })
    setEditing(null)
  }

  function deleteRow(idx: number) {
    const row = rows[idx]
    setRows(p => p.filter((_, i) => i !== idx))
    setLastSql({
      sql: `DELETE FROM ${tableName} WHERE id = ${row.id};`,
      axiomql: `${tableName}.filter(id = ${row.id}).delete()`,
    })
    setConfirmDelete(null)
  }

  const columns: ColumnDef<Record<string, unknown>>[] = rows.length > 0
    ? Object.keys(rows[0]).map(key => ({ accessorKey: key, header: key, enableSorting: true }))
    : []

  const table = useReactTable({
    data: rows,
    columns,
    state: { sorting, pagination: { pageIndex, pageSize } },
    onSortingChange: setSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
  })

  return (
    <div className="flex flex-col h-full">
      {/* Results tab switcher */}
      <div className="flex items-center gap-1 px-3 py-1.5 border-b border-border shrink-0">
        <button
          onClick={() => setResultsTab('table')}
          className={cn(
            'px-2.5 py-0.5 text-[11px] rounded font-medium transition-colors',
            resultsTab === 'table'
              ? 'bg-surface text-text-primary shadow-sm border border-border'
              : 'text-text-secondary hover:text-text-primary',
          )}
        >
          Table
        </button>
        {chartable && (
          <button
            onClick={() => setResultsTab('chart')}
            className={cn(
              'flex items-center gap-1 px-2.5 py-0.5 text-[11px] rounded font-medium transition-colors',
              resultsTab === 'chart'
                ? 'bg-surface text-text-primary shadow-sm border border-border'
                : 'text-text-secondary hover:text-text-primary',
            )}
          >
            <BarChart2 className="w-3 h-3" />
            Chart
          </button>
        )}
      </div>

      {resultsTab === 'chart' && chartable ? (
        <BarChart data={rows} labelCol={chartable.labelCol} valueCol={chartable.valueCol} />
      ) : (
        <>
          <div className="flex-1 overflow-auto">
            <table className="w-full text-xs">
              <thead className="sticky top-0 bg-surface z-10">
                {table.getHeaderGroups().map(hg => (
                  <tr key={hg.id} className="border-b border-border">
                    {hg.headers.map(h => {
                      const s = h.column.getIsSorted()
                      return (
                        <th
                          key={h.id}
                          onClick={h.column.getToggleSortingHandler()}
                          className="text-left px-3 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase cursor-pointer hover:text-text-primary select-none whitespace-nowrap group"
                        >
                          <div className="flex items-center gap-1">
                            {flexRender(h.column.columnDef.header, h.getContext())}
                            <span className="opacity-30 group-hover:opacity-80">
                              {s === 'asc'
                                ? <ChevronUp className="w-3 h-3 text-accent inline" />
                                : s === 'desc'
                                  ? <ChevronDown className="w-3 h-3 text-accent inline" />
                                  : <ChevronsUpDown className="w-3 h-3 inline" />}
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
                  <tr
                    key={row.id}
                    className="border-b border-border/50 hover:bg-elevated transition-colors group"
                  >
                    {/* Delete column */}
                    <td className="pl-1 pr-0 py-1.5 w-6">
                      {confirmDelete === row.index ? (
                        <div className="flex items-center gap-0.5">
                          <button
                            onClick={() => deleteRow(row.index)}
                            className="text-[9px] px-1 py-0.5 rounded bg-error/10 text-error hover:bg-error/20 transition-colors"
                          >
                            Del
                          </button>
                          <button
                            onClick={() => setConfirmDelete(null)}
                            className="text-[9px] px-1 py-0.5 rounded text-text-secondary hover:bg-elevated transition-colors"
                          >
                            ×
                          </button>
                        </div>
                      ) : (
                        <button
                          onClick={() => setConfirmDelete(row.index)}
                          className="opacity-0 group-hover:opacity-100 text-text-secondary hover:text-error transition-all"
                        >
                          <Trash2 className="w-3 h-3" />
                        </button>
                      )}
                    </td>
                    {row.getVisibleCells().map(cell => {
                      const col = cell.column.id
                      const isEditing = editing?.rowIdx === row.index && editing?.col === col
                      const canEdit = !readOnly.has(col)
                      const val = cell.getValue()
                      return (
                        <td
                          key={cell.id}
                          className={cn(
                            'whitespace-nowrap',
                            isEditing ? 'p-0' : 'px-3 py-1.5',
                            canEdit && 'cursor-pointer',
                          )}
                          onClick={() => {
                            if (isEditing || !canEdit) return
                            if (typeof val === 'boolean') { toggle(row.index, col, val); return }
                            setEditing({ rowIdx: row.index, col })
                            setEditVal(String(val ?? ''))
                          }}
                        >
                          {isEditing ? (
                            <div className="flex items-center">
                              <input
                                ref={inputRef}
                                value={editVal}
                                onChange={e => setEditVal(e.target.value)}
                                onKeyDown={e => {
                                  if (e.key === 'Enter') commit()
                                  if (e.key === 'Escape') setEditing(null)
                                }}
                                onBlur={commit}
                                className="w-full px-3 py-1.5 bg-elevated border-2 border-accent outline-none font-mono text-xs text-text-primary"
                              />
                              <button
                                onMouseDown={e => { e.preventDefault(); commit() }}
                                className="px-1.5 py-1.5 bg-accent/10 text-accent border-y-2 border-r-2 border-accent/30"
                              >
                                <Check className="w-3 h-3" />
                              </button>
                              <button
                                onMouseDown={e => { e.preventDefault(); setEditing(null) }}
                                className="px-1.5 py-1.5 bg-error/10 text-error border-y-2 border-r-2 border-error/30"
                              >
                                <X className="w-3 h-3" />
                              </button>
                            </div>
                          ) : (
                            <div className="flex items-center gap-1">
                              {typeof val === 'boolean' ? (
                                <span
                                  className={`text-[10px] px-1.5 py-0.5 rounded font-semibold ${val ? 'bg-accent/10 text-accent' : 'bg-border text-text-secondary'}`}
                                >
                                  {String(val)}
                                </span>
                              ) : (
                                <span className="font-mono text-xs text-text-secondary">
                                  {String(val ?? '')}
                                </span>
                              )}
                              {canEdit && (
                                <span className="opacity-0 group-hover:opacity-30 text-[9px]">✎</span>
                              )}
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
                <span className="text-[9px] text-text-secondary uppercase tracking-wider font-semibold">
                  Last operation
                </span>
                <button
                  onClick={() => setLastSql(null)}
                  className="text-[10px] text-text-secondary hover:text-text-primary"
                >
                  ✕
                </button>
              </div>
              <div className="grid grid-cols-2 gap-2">
                <code className="font-mono text-[11px] text-accent bg-surface px-2 py-1 rounded border border-border/50 truncate block">
                  {lastSql.sql}
                </code>
                <code className="font-mono text-[11px] text-blue-400 bg-surface px-2 py-1 rounded border border-border/50 truncate block">
                  {lastSql.axiomql}
                </code>
              </div>
            </div>
          )}

          <div className="border-t border-border px-3 py-2 flex items-center justify-between shrink-0">
            <span className="text-xs text-text-secondary">{rows.length.toLocaleString()} rows</span>
            <div className="flex items-center gap-2">
              <button
                onClick={() => setPageIndex(p => Math.max(0, p - 1))}
                disabled={pageIndex === 0}
                className="text-xs text-text-secondary hover:text-text-primary disabled:opacity-30 px-2 py-1 rounded hover:bg-elevated"
              >
                ← Prev
              </button>
              <span className="text-xs text-text-secondary">
                Page {pageIndex + 1} of {table.getPageCount()}
              </span>
              <button
                onClick={() => setPageIndex(p => Math.min(table.getPageCount() - 1, p + 1))}
                disabled={pageIndex >= table.getPageCount() - 1}
                className="text-xs text-text-secondary hover:text-text-primary disabled:opacity-30 px-2 py-1 rounded hover:bg-elevated"
              >
                Next →
              </button>
            </div>
          </div>
        </>
      )}
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
            <button
              onClick={clearAll}
              className="text-[10px] text-error/70 hover:text-error transition-colors"
            >
              Clear all
            </button>
          )}
          <button
            onClick={onClose}
            className="text-text-secondary hover:text-text-primary transition-colors"
          >
            <X className="w-3.5 h-3.5" />
          </button>
        </div>
      </div>
      <div className="max-h-80 overflow-y-auto">
        {entries.length === 0 ? (
          <div className="px-4 py-6 text-center text-xs text-text-secondary">No queries yet</div>
        ) : (
          entries.map(e => (
            <button
              key={e.id}
              onClick={() => { onLoad(e); onClose() }}
              className="w-full text-left px-3 py-2.5 border-b border-border/50 hover:bg-elevated transition-colors group"
            >
              <div className="font-mono text-[11px] text-text-primary truncate">
                {e.query.slice(0, 60)}{e.query.length > 60 ? '…' : ''}
              </div>
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

// ── Variables panel ───────────────────────────────────────────────────────────

function VariablesPanel({
  vars,
  onChange,
}: {
  vars: QueryVar[]
  onChange: (vars: QueryVar[]) => void
}) {
  function add() {
    onChange([...vars, { id: crypto.randomUUID(), name: '', value: '' }])
  }

  function remove(id: string) {
    onChange(vars.filter(v => v.id !== id))
  }

  function update(id: string, field: 'name' | 'value', val: string) {
    onChange(vars.map(v => v.id === id ? { ...v, [field]: val } : v))
  }

  return (
    <div className="border-b border-border px-4 py-2 flex items-center gap-2 flex-wrap shrink-0 bg-elevated/30">
      <span className="text-[10px] text-text-secondary uppercase tracking-wider font-semibold mr-1 shrink-0">
        Vars
      </span>
      {vars.map(v => (
        <div
          key={v.id}
          className="flex items-center gap-0 border border-border rounded overflow-hidden text-xs shrink-0"
        >
          <span className="px-1.5 py-0.5 bg-surface text-accent font-mono text-[10px] border-r border-border">$</span>
          <input
            value={v.name}
            onChange={e => update(v.id, 'name', e.target.value)}
            placeholder="name"
            className="w-16 px-1.5 py-0.5 bg-surface outline-none font-mono text-[11px] text-text-primary border-r border-border placeholder:text-text-secondary/50"
          />
          <input
            value={v.value}
            onChange={e => update(v.id, 'value', e.target.value)}
            placeholder="value"
            className="w-20 px-1.5 py-0.5 bg-surface outline-none font-mono text-[11px] text-accent placeholder:text-text-secondary/50"
          />
          <button
            onClick={() => remove(v.id)}
            className="px-1.5 py-0.5 bg-surface text-text-secondary hover:text-error transition-colors border-l border-border"
          >
            <X className="w-2.5 h-2.5" />
          </button>
        </div>
      ))}
      <button
        onClick={add}
        className="flex items-center gap-1 text-[10px] text-text-secondary hover:text-accent transition-colors px-1.5 py-0.5 rounded border border-border/50 border-dashed hover:border-accent/50"
      >
        <Plus className="w-3 h-3" />
        Add var
      </button>
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
  const [showSaved, setShowSaved] = useState(false)
  const [splitView, setSplitView] = useState(false)
  const [showVars, setShowVars] = useState(false)
  const [queryVars, setQueryVars] = useState<QueryVar[]>([])
  const [explainPlan, setExplainPlan] = useState<PlanNode | null>(null)
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

  // Stable ref so handleRun can close over latest values
  const activeTabRef = useRef(activeTab)
  const modeRef = useRef(mode)
  const queryVarsRef = useRef(queryVars)
  useEffect(() => { activeTabRef.current = activeTab }, [activeTab])
  useEffect(() => { modeRef.current = mode }, [mode])
  useEffect(() => { queryVarsRef.current = queryVars }, [queryVars])

  const handleRun = useCallback(async () => {
    setRunning(true)
    setError(null)
    await new Promise(r => setTimeout(r, 300 + Math.random() * 200))
    const d = Math.round(4 + Math.random() * 40)
    const results = MOCK_RESULTS.default as Record<string, unknown>[]
    updateActiveTab({ results, duration: d })
    setRunning(false)

    const rawQuery = modeRef.current === 'sql'
      ? activeTabRef.current.sqlValue
      : activeTabRef.current.axiomqlValue
    const query = applyVariables(rawQuery, queryVarsRef.current)
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
  }, [activeTabId])

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

  function loadSavedEntry(s: SavedQuery) {
    if (s.mode === 'sql') {
      updateActiveTab({ sqlValue: s.query, results: null, duration: null })
    } else {
      updateActiveTab({ axiomqlValue: s.query, results: null, duration: null })
    }
    setMode(s.mode)
  }

  function handleFormat() {
    if (mode === 'sql') {
      const formatted = formatSql(sqlValue)
      updateActiveTab({ sqlValue: formatted })
    }
  }

  // Stable mount handler that captures the current mode
  const mountedRef = useRef(false)
  const handleEditorMountCb = useCallback(
    (editor: Monaco.editor.IStandaloneCodeEditor, monaco: typeof Monaco) => {
      if (mountedRef.current) return
      mountedRef.current = true
      handleEditorMount(editor, monaco, modeRef.current)
    },
    [],
  )

  const results = activeTab.results
  const duration = activeTab.duration

  const editorLanguage = mode === 'sql' ? 'sql' : 'axiomql'
  const editorTheme = mode === 'sql' ? 'vs-dark' : 'axiomql-dark'

  // ── Shared editor + toolbar JSX ───────────────────────────────────────────

  const editorNode = (
    <div
      className={cn(
        'border-border shrink-0',
        splitView ? 'flex-1 border-r overflow-hidden h-full' : 'border-b',
      )}
      style={splitView ? undefined : { height: 280 }}
    >
      <MonacoEditor
        key={activeTabId + '-' + mode}
        height="100%"
        language={editorLanguage}
        value={currentValue}
        onChange={v => setCurrentValue(v ?? '')}
        theme={editorTheme}
        onMount={handleEditorMountCb}
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
  )

  const resultsNode = (
    <div className={cn('overflow-hidden flex flex-col', splitView ? 'flex-1 h-full' : 'flex-1')}>
      {explainPlan ? (
        <>
          <div className="flex items-center justify-between px-3 py-2 border-b border-border shrink-0 bg-elevated/40">
            <span className="text-xs font-semibold text-text-primary">EXPLAIN Plan</span>
            <button
              onClick={() => setExplainPlan(null)}
              className="text-xs text-text-secondary hover:text-accent transition-colors border border-border px-2 py-0.5 rounded">
              ← Back to results
            </button>
          </div>
          <div className="flex-1 overflow-auto p-3">
            <PlanTree node={explainPlan} />
          </div>
        </>
      ) : (
        <>
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
        </>
      )}
    </div>
  )

  return (
    <div className="flex flex-col h-full">
      {/* Tab bar */}
      <div className="border-b border-border flex items-center overflow-x-auto shrink-0 bg-surface">
        <div className="flex items-center min-w-0 flex-1 overflow-x-auto">
          {tabs.map(tab => (
            <div
              key={tab.id}
              className={cn(
                'flex items-center gap-1.5 px-3 py-2 text-xs font-medium border-r border-border shrink-0 cursor-pointer select-none transition-colors group',
                tab.id === activeTabId
                  ? 'bg-bg text-text-primary border-b-bg -mb-px relative z-10'
                  : 'text-text-secondary hover:text-text-primary hover:bg-elevated',
              )}
              onClick={() => setActiveTabId(tab.id)}
            >
              <span>{tab.title}</span>
              <button
                onClick={e => { e.stopPropagation(); closeTab(tab.id) }}
                className={cn(
                  'rounded hover:bg-border transition-colors p-0.5',
                  tab.id === activeTabId ? 'opacity-60 hover:opacity-100' : 'opacity-0 group-hover:opacity-60',
                )}
              >
                <X className="w-2.5 h-2.5" />
              </button>
            </div>
          ))}
        </div>
        <button
          onClick={addTab}
          className="px-3 py-2 text-text-secondary hover:text-text-primary hover:bg-elevated transition-colors shrink-0 border-l border-border"
        >
          <Plus className="w-3.5 h-3.5" />
        </button>
      </div>

      {/* Mode header */}
      <div className="border-b border-border px-6 py-3 flex items-center justify-between shrink-0">
        <div className="flex items-center gap-3">
          <h1 className="text-sm font-semibold text-text-primary">Query Editor</h1>
          {splitView && duration !== null && results && (
            <div className="flex items-center gap-1.5 text-xs text-text-secondary font-mono">
              <span className="text-accent">{duration}ms</span>
              <span>·</span>
              <span>{results.length.toLocaleString()} rows</span>
            </div>
          )}
        </div>
        <div className="flex items-center gap-2">
          {/* Split view toggle */}
          <button
            onClick={() => setSplitView(p => !p)}
            title="Split view"
            className={cn(
              'flex items-center gap-1.5 px-2 py-1 text-xs border rounded transition-colors font-medium',
              splitView
                ? 'border-accent text-accent bg-accent/10'
                : 'border-border text-text-secondary hover:border-accent/50 hover:text-accent',
            )}
          >
            <LayoutPanelLeft className="w-3.5 h-3.5" />
          </button>

          {/* Mode switcher */}
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
            className="flex items-center gap-1.5 px-2 py-1 text-xs text-text-secondary border border-border rounded hover:border-accent/50 hover:text-accent transition-colors font-medium"
          >
            {mode === 'sql' ? '→ AxiomQL' : '→ SQL'}
          </button>
          <div className="flex items-center gap-1 bg-elevated rounded p-0.5">
            {(['sql', 'axiomql'] as QueryMode[]).map(m => (
              <button
                key={m}
                onClick={() => setMode(m)}
                className={cn(
                  'px-3 py-1 text-xs rounded font-medium transition-colors',
                  mode === m
                    ? 'bg-surface text-text-primary shadow-sm'
                    : 'text-text-secondary hover:text-text-primary',
                )}
              >
                {m === 'sql' ? 'SQL' : 'AxiomQL'}
              </button>
            ))}
          </div>
        </div>
      </div>

      {/* Variables panel (conditional) */}
      {showVars && (
        <VariablesPanel vars={queryVars} onChange={setQueryVars} />
      )}

      {/* Split view layout or stacked layout */}
      {splitView ? (
        <div className="flex flex-col flex-1 overflow-hidden">
          {/* Toolbar in split mode (above the split) */}
          <div className="border-b border-border px-4 py-2 flex items-center gap-2 shrink-0">
            <button
              onClick={handleRun}
              disabled={running}
              className={cn(
                'flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-semibold transition-all',
                running
                  ? 'bg-accent/50 text-white/50 cursor-not-allowed'
                  : 'bg-accent text-white hover:bg-accent-dim active:scale-95',
              )}
            >
              <Play className="w-3 h-3" />
              {running ? 'Running...' : 'Run'}
              <span className="text-white/50 text-[10px] ml-1">⌘↵</span>
            </button>

            <button
              onClick={() => setExplainPlan(generateMockPlan(currentValue))}
              className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-blue-400 border border-blue-400/30 hover:bg-blue-400/10 transition-colors">
              <Zap className="w-3 h-3" />
              Explain
            </button>

            {mode === 'sql' && (
              <button
                onClick={handleFormat}
                className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-text-secondary border border-border hover:bg-elevated hover:text-text-primary transition-colors"
              >
                <AlignLeft className="w-3 h-3" />
                Format
              </button>
            )}

            <button
              onClick={() => results && exportCsv(results, `${activeTab.title.replace(/\s+/g, '_').toLowerCase()}.csv`)}
              disabled={!results}
              className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-text-secondary border border-border hover:bg-elevated hover:text-text-primary transition-colors disabled:opacity-40"
            >
              <Download className="w-3 h-3" />
              Export CSV
            </button>

            {/* Variables toggle */}
            <button
              onClick={() => setShowVars(p => !p)}
              className={cn(
                'flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs font-medium border transition-colors',
                showVars
                  ? 'border-accent text-accent bg-accent/10'
                  : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary',
              )}
            >
              <DollarSign className="w-3 h-3" />
            </button>

            {/* Saved queries */}
            <div className="relative">
              <button
                onClick={() => { setShowSaved(p => !p); setShowHistory(false) }}
                className={cn(
                  'flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs font-medium border transition-colors',
                  showSaved
                    ? 'border-accent text-accent bg-accent/10'
                    : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary',
                )}
              >
                <BookmarkPlus className="w-3 h-3" />
              </button>
              {showSaved && (
                <SavedPanel
                  currentQuery={currentValue}
                  currentMode={mode}
                  onLoad={loadSavedEntry}
                  onClose={() => setShowSaved(false)}
                />
              )}
            </div>

            {/* History toggle */}
            <div className="relative">
              <button
                onClick={() => { setShowHistory(p => !p); setShowSaved(false) }}
                className={cn(
                  'flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs font-medium border transition-colors',
                  showHistory
                    ? 'border-accent text-accent bg-accent/10'
                    : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary',
                )}
              >
                <Clock className="w-3 h-3" />
              </button>
              {showHistory && (
                <HistoryPanel
                  onLoad={loadHistoryEntry}
                  onClose={() => setShowHistory(false)}
                />
              )}
            </div>
          </div>

          {/* Side-by-side panels */}
          <div className="flex flex-1 overflow-hidden">
            {editorNode}
            {resultsNode}
          </div>
        </div>
      ) : (
        <>
          {/* Editor */}
          {editorNode}

          {/* Toolbar */}
          <div className="border-b border-border px-4 py-2 flex items-center gap-2 shrink-0">
            <button
              onClick={handleRun}
              disabled={running}
              className={cn(
                'flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-semibold transition-all',
                running
                  ? 'bg-accent/50 text-white/50 cursor-not-allowed'
                  : 'bg-accent text-white hover:bg-accent-dim active:scale-95',
              )}
            >
              <Play className="w-3 h-3" />
              {running ? 'Running...' : 'Run'}
              <span className="text-white/50 text-[10px] ml-1">⌘↵</span>
            </button>

            <button
              onClick={() => setExplainPlan(generateMockPlan(currentValue))}
              className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-blue-400 border border-blue-400/30 hover:bg-blue-400/10 transition-colors">
              <Zap className="w-3 h-3" />
              Explain
            </button>

            {mode === 'sql' && (
              <button
                onClick={handleFormat}
                className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-text-secondary border border-border hover:bg-elevated hover:text-text-primary transition-colors"
              >
                <AlignLeft className="w-3 h-3" />
                Format
              </button>
            )}

            <button
              onClick={() => results && exportCsv(results, `${activeTab.title.replace(/\s+/g, '_').toLowerCase()}.csv`)}
              disabled={!results}
              className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-medium text-text-secondary border border-border hover:bg-elevated hover:text-text-primary transition-colors disabled:opacity-40"
            >
              <Download className="w-3 h-3" />
              Export CSV
            </button>

            {/* Variables toggle */}
            <button
              onClick={() => setShowVars(p => !p)}
              className={cn(
                'flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs font-medium border transition-colors',
                showVars
                  ? 'border-accent text-accent bg-accent/10'
                  : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary',
              )}
            >
              <DollarSign className="w-3 h-3" />
            </button>

            {/* Saved queries */}
            <div className="relative">
              <button
                onClick={() => { setShowSaved(p => !p); setShowHistory(false) }}
                className={cn(
                  'flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs font-medium border transition-colors',
                  showSaved
                    ? 'border-accent text-accent bg-accent/10'
                    : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary',
                )}
              >
                <BookmarkPlus className="w-3 h-3" />
              </button>
              {showSaved && (
                <SavedPanel
                  currentQuery={currentValue}
                  currentMode={mode}
                  onLoad={loadSavedEntry}
                  onClose={() => setShowSaved(false)}
                />
              )}
            </div>

            {/* History toggle */}
            <div className="relative">
              <button
                onClick={() => { setShowHistory(p => !p); setShowSaved(false) }}
                className={cn(
                  'flex items-center gap-1.5 px-2.5 py-1.5 rounded text-xs font-medium border transition-colors',
                  showHistory
                    ? 'border-accent text-accent bg-accent/10'
                    : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary',
                )}
              >
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
          {resultsNode}
        </>
      )}
    </div>
  )
}
