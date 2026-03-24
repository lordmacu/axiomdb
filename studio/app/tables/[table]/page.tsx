'use client'
import { useState, use, useRef, useEffect } from 'react'
import { notFound } from 'next/navigation'
import { Table2, ChevronUp, ChevronDown, ChevronsUpDown, Check, X } from 'lucide-react'
import { TABLES, SCHEMAS, INDEXES, generateUsers, generateOrders } from '@/lib/mock'
import { cn } from '@/lib/utils'
import {
  useReactTable,
  getCoreRowModel,
  getPaginationRowModel,
  getSortedRowModel,
  flexRender,
  type ColumnDef,
  type SortingState,
} from '@tanstack/react-table'

type Tab = 'data' | 'schema' | 'indexes'

type ProductRow = { id: number; name: string; price: number; stock: number; category_id: number }
type CategoryRow = { id: number; name: string; slug: string }

const ALL_USERS = generateUsers(50)

const TABLE_DATA: Record<string, unknown[]> = {
  users: ALL_USERS,
  orders: generateOrders(100),
  products: Array.from({ length: 20 }, (_, i): ProductRow => ({
    id: i + 1,
    name: `Product ${i + 1}`,
    price: Math.round((9.99 + i * 5.5) * 100) / 100,
    stock: Math.floor(Math.random() * 500),
    category_id: 1 + (i % 5),
  })),
  categories: Array.from({ length: 20 }, (_, i): CategoryRow => ({
    id: i + 1,
    name: `Category ${i + 1}`,
    slug: `category-${i + 1}`,
  })),
  // Views
  active_users: ALL_USERS.filter(u => u.active).map(u => ({
    id: u.id, name: u.name, email: u.email, age: u.age, created_at: u.created_at,
  })),
}

type EditingCell = { rowIndex: number; colKey: string } | null

function CellValue({ value, colKey }: { value: unknown; colKey: string }) {
  if (typeof value === 'boolean') {
    return (
      <span className={`text-[10px] px-1.5 py-0.5 rounded font-semibold ${
        value ? 'bg-accent/10 text-accent' : 'bg-border text-text-secondary'
      }`}>{String(value)}</span>
    )
  }
  if (colKey === 'status') {
    const colors: Record<string, string> = {
      completed: 'bg-accent/10 text-accent',
      pending: 'bg-warning/10 text-warning',
      failed: 'bg-error/10 text-error',
      processing: 'bg-blue-400/10 text-blue-400',
    }
    return (
      <span className={`text-[10px] px-1.5 py-0.5 rounded font-semibold ${
        colors[String(value)] ?? 'text-text-secondary'
      }`}>{String(value)}</span>
    )
  }
  return <span className="font-mono text-xs">{String(value ?? '')}</span>
}

type SqlLog = { sql: string; axiomql: string; ts: string }

function DataTab({ tableName }: { tableName: string }) {
  const initial = (TABLE_DATA[tableName] ?? []) as Record<string, unknown>[]
  const [rows, setRows] = useState(initial)
  const [pageIndex, setPageIndex] = useState(0)
  const [sorting, setSorting] = useState<SortingState>([])
  const [editing, setEditing] = useState<EditingCell>(null)
  const [editValue, setEditValue] = useState('')
  const [lastSql, setLastSql] = useState<SqlLog | null>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  const pageSize = 15

  useEffect(() => { if (editing) inputRef.current?.focus() }, [editing])

  const nonEditableCols = new Set(['id', 'created_at', 'status'])

  function buildSql(rowId: unknown, col: string, val: unknown) {
    const v = typeof val === 'string' ? `'${val}'` : String(val)
    return `UPDATE ${tableName} SET ${col} = ${v} WHERE id = ${rowId};`
  }

  function buildAxiomql(rowId: unknown, col: string, val: unknown) {
    const v = typeof val === 'string' ? `'${val}'` : String(val)
    return `${tableName}.filter(id = ${rowId}).update(${col}: ${v})`
  }

  function logEdit(row: Record<string, unknown>, col: string, val: unknown) {
    const now = new Date().toTimeString().slice(0, 8)
    setLastSql({ sql: buildSql(row.id, col, val), axiomql: buildAxiomql(row.id, col, val), ts: now })
  }

  function toggleBool(rowIndex: number, colKey: string, current: unknown) {
    const newVal = !current
    const row = rows[rowIndex]
    setRows(prev => prev.map((r, i) => i === rowIndex ? { ...r, [colKey]: newVal } : r))
    logEdit(row, colKey, newVal)
  }

  function startEdit(rowIndex: number, colKey: string, currentValue: unknown) {
    if (nonEditableCols.has(colKey)) return
    if (typeof currentValue === 'boolean') { toggleBool(rowIndex, colKey, currentValue); return }
    setEditing({ rowIndex, colKey })
    setEditValue(String(currentValue ?? ''))
  }

  function commitEdit() {
    if (!editing) return
    const row = rows[editing.rowIndex]
    setRows(prev => prev.map((r, i) => i === editing.rowIndex ? { ...r, [editing.colKey]: editValue } : r))
    logEdit(row, editing.colKey, editValue)
    setEditing(null)
  }

  function cancelEdit() { setEditing(null) }

  const columns: ColumnDef<Record<string, unknown>>[] = rows.length > 0
    ? Object.keys(rows[0]).map(key => ({
        accessorKey: key,
        header: key,
        enableSorting: true,
      }))
    : []

  const table = useReactTable({
    data: rows,
    columns,
    state: { pagination: { pageIndex, pageSize }, sorting },
    onSortingChange: setSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    // track original row index for editing
  })

  // Map page rows back to their original indices
  const sortedData = table.getSortedRowModel().rows
  const pageRows = table.getRowModel().rows

  return (
    <div className="flex flex-col h-full overflow-hidden">
      <div className="flex-1 overflow-auto">
        <table className="w-full text-xs">
          <thead className="sticky top-0 bg-surface z-10">
            {table.getHeaderGroups().map(hg => (
              <tr key={hg.id} className="border-b border-border">
                {hg.headers.map(h => {
                  const sorted = h.column.getIsSorted()
                  return (
                    <th key={h.id}
                      onClick={h.column.getToggleSortingHandler()}
                      className="text-left px-3 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase whitespace-nowrap cursor-pointer hover:text-text-primary select-none transition-colors group">
                      <div className="flex items-center gap-1">
                        {flexRender(h.column.columnDef.header, h.getContext())}
                        <span className="opacity-40 group-hover:opacity-100 transition-opacity">
                          {sorted === 'asc'
                            ? <ChevronUp className="w-3 h-3 text-accent" />
                            : sorted === 'desc'
                              ? <ChevronDown className="w-3 h-3 text-accent" />
                              : <ChevronsUpDown className="w-3 h-3" />
                          }
                        </span>
                      </div>
                    </th>
                  )
                })}
              </tr>
            ))}
          </thead>
          <tbody>
            {pageRows.map(row => {
              const originalIndex = row.index
              return (
                <tr key={row.id} className="border-b border-border/50 hover:bg-elevated transition-colors group">
                  {row.getVisibleCells().map(cell => {
                    const colKey = cell.column.id
                    const isEditing = editing?.rowIndex === originalIndex && editing?.colKey === colKey
                    const isEditable = !nonEditableCols.has(colKey)
                    return (
                      <td key={cell.id}
                        className={cn(
                          'px-3 py-1.5 text-text-secondary whitespace-nowrap relative',
                          isEditable && 'cursor-pointer',
                          isEditing && 'p-0'
                        )}
                        onClick={() => !isEditing && startEdit(originalIndex, colKey, cell.getValue())}>
                        {isEditing ? (
                          <div className="flex items-center">
                            <input
                              ref={inputRef}
                              value={editValue}
                              onChange={e => setEditValue(e.target.value)}
                              onKeyDown={e => {
                                if (e.key === 'Enter') commitEdit()
                                if (e.key === 'Escape') cancelEdit()
                              }}
                              onBlur={commitEdit}
                              className="w-full px-3 py-1.5 bg-elevated border-2 border-accent outline-none font-mono text-xs text-text-primary"
                            />
                            <button onMouseDown={e => { e.preventDefault(); commitEdit() }}
                              className="px-1.5 py-1.5 bg-accent/10 hover:bg-accent/20 text-accent border-y-2 border-r-2 border-accent/30 transition-colors">
                              <Check className="w-3 h-3" />
                            </button>
                            <button onMouseDown={e => { e.preventDefault(); cancelEdit() }}
                              className="px-1.5 py-1.5 bg-error/10 hover:bg-error/20 text-error border-y-2 border-r-2 border-error/30 transition-colors">
                              <X className="w-3 h-3" />
                            </button>
                          </div>
                        ) : (
                          <div className={cn(
                            'flex items-center gap-1 min-h-[20px]',
                            isEditable && 'group-hover:text-text-primary'
                          )}>
                            <CellValue value={cell.getValue()} colKey={colKey} />
                            {isEditable && (
                              <span className="opacity-0 group-hover:opacity-30 transition-opacity text-[9px] text-text-secondary">
                                ✎
                              </span>
                            )}
                          </div>
                        )}
                      </td>
                    )
                  })}
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>
      {/* SQL Preview footer */}
      {lastSql && (
        <div className="border-t border-border/50 bg-elevated px-3 py-2 shrink-0">
          <div className="flex items-center justify-between mb-1">
            <span className="text-[10px] text-text-secondary uppercase tracking-wider font-semibold">Last operation — {lastSql.ts}</span>
            <button onClick={() => setLastSql(null)} className="text-[10px] text-text-secondary hover:text-text-primary transition-colors">✕</button>
          </div>
          <div className="grid grid-cols-2 gap-2">
            <div>
              <div className="text-[9px] text-text-secondary mb-0.5 uppercase tracking-wider">SQL</div>
              <code className="block font-mono text-[11px] text-accent bg-surface px-2 py-1 rounded border border-border/50 truncate">
                {lastSql.sql}
              </code>
            </div>
            <div>
              <div className="text-[9px] text-text-secondary mb-0.5 uppercase tracking-wider">AxiomQL</div>
              <code className="block font-mono text-[11px] text-blue-400 bg-surface px-2 py-1 rounded border border-border/50 truncate">
                {lastSql.axiomql}
              </code>
            </div>
          </div>
        </div>
      )}

      <div className="border-t border-border px-3 py-2 flex items-center justify-between shrink-0">
        <div className="flex items-center gap-3">
          <span className="text-xs text-text-secondary font-mono">{rows.length.toLocaleString()} rows</span>
          {sorting.length > 0 && (
            <span className="text-[10px] text-accent">
              sorted by {sorting[0].id} {sorting[0].desc ? '↓' : '↑'}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          <button onClick={() => setPageIndex(p => Math.max(0, p - 1))} disabled={pageIndex === 0}
            className="text-xs text-text-secondary hover:text-text-primary disabled:opacity-30 px-2 py-1 rounded hover:bg-elevated transition-colors">
            ← Prev
          </button>
          <span className="text-xs text-text-secondary">
            Page {pageIndex + 1} / {table.getPageCount()}
          </span>
          <button onClick={() => setPageIndex(p => Math.min(table.getPageCount() - 1, p + 1))} disabled={pageIndex >= table.getPageCount() - 1}
            className="text-xs text-text-secondary hover:text-text-primary disabled:opacity-30 px-2 py-1 rounded hover:bg-elevated transition-colors">
            Next →
          </button>
        </div>
      </div>
    </div>
  )
}

const AXIOMDB_TYPES = [
  'INT', 'BIGINT', 'REAL', 'DECIMAL', 'BOOL',
  'TEXT', 'VARCHAR', 'BYTEA',
  'DATE', 'TIME', 'TIMESTAMP',
  'UUID', 'JSON', 'JSONB', 'VECTOR',
]

function SchemaTab({ tableName }: { tableName: string }) {
  const initial = SCHEMAS[tableName] ?? []
  const [cols, setCols] = useState(initial.map(c => ({ ...c })))
  const [editingType, setEditingType] = useState<string | null>(null)
  const [editingFk, setEditingFk] = useState<string | null>(null)
  const [fkValue, setFkValue] = useState('')

  function setType(colName: string, type: string) {
    setCols(prev => prev.map(c => c.name === colName ? { ...c, type } : c))
    setEditingType(null)
  }

  function toggleNullable(colName: string) {
    setCols(prev => prev.map(c => c.name === colName && !c.pk ? { ...c, nullable: !c.nullable } : c))
  }

  function startFkEdit(colName: string, current: string | null) {
    setEditingFk(colName)
    setFkValue(current ?? '')
  }

  function saveFk(colName: string) {
    setCols(prev => prev.map(c => c.name === colName ? { ...c, fk: fkValue.trim() || null } : c))
    setEditingFk(null)
  }

  function removeFk(colName: string) {
    setCols(prev => prev.map(c => c.name === colName ? { ...c, fk: null } : c))
  }

  return (
    <div className="overflow-auto">
      <table className="w-full text-xs">
        <thead>
          <tr className="border-b border-border">
            {['Column', 'Type', 'Nullable', 'Default', 'Constraints'].map(h => (
              <th key={h} className="text-left px-4 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {cols.map(c => (
            <tr key={c.name} className="border-b border-border/50 hover:bg-elevated transition-colors group">
              <td className="px-4 py-2.5">
                <div className="flex items-center gap-2">
                  <span className="font-mono font-semibold text-text-primary">{c.name}</span>
                  {c.pk && <span className="text-[9px] px-1 py-0.5 rounded bg-warning/10 text-warning font-semibold">PK</span>}
                </div>
              </td>

              {/* Type — click to open dropdown */}
              <td className="px-4 py-2 relative">
                {editingType === c.name ? (
                  <div className="flex items-center gap-1">
                    <select
                      autoFocus
                      value={c.type}
                      onChange={e => setType(c.name, e.target.value)}
                      onBlur={() => setEditingType(null)}
                      className="bg-elevated border border-accent rounded px-2 py-0.5 text-xs font-mono text-accent outline-none cursor-pointer">
                      {AXIOMDB_TYPES.map(t => (
                        <option key={t} value={t}>{t}</option>
                      ))}
                    </select>
                  </div>
                ) : (
                  <button
                    onClick={() => setEditingType(c.name)}
                    className="font-mono text-accent text-[11px] hover:bg-accent/10 px-1.5 py-0.5 rounded transition-colors flex items-center gap-1">
                    {c.type}
                    <span className="opacity-0 group-hover:opacity-40 text-[9px]">▾</span>
                  </button>
                )}
              </td>

              {/* Nullable — click to toggle */}
              <td className="px-4 py-2.5">
                <button
                  onClick={() => toggleNullable(c.name)}
                  disabled={c.pk}
                  className={cn(
                    'text-[10px] px-1.5 py-0.5 rounded font-semibold transition-colors',
                    c.pk ? 'opacity-30 cursor-default' : 'cursor-pointer hover:opacity-80',
                    c.nullable ? 'bg-border text-text-secondary' : 'bg-accent/10 text-accent'
                  )}>
                  {c.nullable ? 'YES' : 'NO'}
                </button>
              </td>

              <td className="px-4 py-2.5 font-mono text-text-secondary text-[11px]">{c.default ?? '—'}</td>
              {/* FK — click to edit, × to remove */}
              <td className="px-4 py-2">
                {editingFk === c.name ? (
                  <div className="flex items-center gap-1">
                    <input
                      autoFocus
                      value={fkValue}
                      onChange={e => setFkValue(e.target.value)}
                      placeholder="table.column"
                      onKeyDown={e => { if (e.key === 'Enter') saveFk(c.name); if (e.key === 'Escape') setEditingFk(null) }}
                      onBlur={() => saveFk(c.name)}
                      className="bg-elevated border border-accent rounded px-2 py-0.5 font-mono text-[11px] text-blue-400 outline-none w-32"
                    />
                  </div>
                ) : c.fk ? (
                  <div className="flex items-center gap-1 group/fk">
                    <button onClick={() => startFkEdit(c.name, c.fk)}
                      className="text-[10px] px-1.5 py-0.5 rounded bg-blue-400/10 text-blue-400 font-mono hover:bg-blue-400/20 transition-colors">
                      → {c.fk}
                    </button>
                    <button onClick={() => removeFk(c.name)}
                      className="opacity-0 group-hover/fk:opacity-100 text-error/60 hover:text-error text-[10px] transition-all">×</button>
                  </div>
                ) : (
                  <button onClick={() => startFkEdit(c.name, null)}
                    className="opacity-0 group-hover:opacity-100 text-[10px] text-text-secondary hover:text-blue-400 transition-all">
                    + FK
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function IndexesTab({ tableName }: { tableName: string }) {
  const initial = INDEXES[tableName] ?? []
  const [idxs, setIdxs] = useState(initial.map(i => ({ ...i })))
  const [editing, setEditing] = useState<string | null>(null)
  const [editName, setEditName] = useState('')
  const [editCols, setEditCols] = useState('')
  const [editUnique, setEditUnique] = useState(false)
  const [adding, setAdding] = useState(false)
  const [newName, setNewName] = useState('')
  const [newCols, setNewCols] = useState('')
  const [newUnique, setNewUnique] = useState(false)

  function startEdit(idx: typeof idxs[0]) {
    setEditing(idx.name)
    setEditName(idx.name)
    setEditCols(idx.columns.join(', '))
    setEditUnique(idx.unique)
  }

  function saveEdit(originalName: string) {
    setIdxs(prev => prev.map(i => i.name === originalName
      ? { ...i, name: editName, columns: editCols.split(',').map(s => s.trim()).filter(Boolean), unique: editUnique }
      : i
    ))
    setEditing(null)
  }

  function deleteIdx(name: string) {
    setIdxs(prev => prev.filter(i => i.name !== name))
  }

  function addIdx() {
    if (!newName || !newCols) return
    setIdxs(prev => [...prev, {
      name: newName,
      columns: newCols.split(',').map(s => s.trim()).filter(Boolean),
      unique: newUnique,
      type: 'B-Tree',
    }])
    setNewName(''); setNewCols(''); setNewUnique(false); setAdding(false)
  }

  return (
    <div className="flex flex-col gap-0">
      <div className="overflow-auto">
        <table className="w-full text-xs">
          <thead>
            <tr className="border-b border-border">
              {['Name', 'Columns', 'Type', 'Unique', ''].map((h, i) => (
                <th key={i} className="text-left px-4 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase">
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {idxs.map(idx => (
              <tr key={idx.name} className="border-b border-border/50 hover:bg-elevated transition-colors group">
                {editing === idx.name ? (
                  <>
                    <td className="px-2 py-1.5">
                      <input value={editName} onChange={e => setEditName(e.target.value)}
                        className="w-full bg-elevated border border-accent rounded px-2 py-1 font-mono text-xs text-text-primary outline-none" />
                    </td>
                    <td className="px-2 py-1.5">
                      <input value={editCols} onChange={e => setEditCols(e.target.value)}
                        placeholder="col1, col2"
                        className="w-full bg-elevated border border-border rounded px-2 py-1 font-mono text-xs text-text-secondary outline-none" />
                    </td>
                    <td className="px-4 py-1.5 text-text-secondary text-[11px]">{idx.type}</td>
                    <td className="px-4 py-1.5">
                      <button onClick={() => setEditUnique(!editUnique)}
                        className={cn('text-[10px] px-1.5 py-0.5 rounded font-semibold transition-colors',
                          editUnique ? 'bg-accent/10 text-accent' : 'bg-border text-text-secondary')}>
                        {editUnique ? 'UNIQUE' : 'NO'}
                      </button>
                    </td>
                    <td className="px-2 py-1.5 flex items-center gap-1">
                      <button onClick={() => saveEdit(idx.name)}
                        className="px-2 py-1 rounded bg-accent/10 text-accent text-xs hover:bg-accent/20 transition-colors">Save</button>
                      <button onClick={() => setEditing(null)}
                        className="px-2 py-1 rounded text-text-secondary text-xs hover:bg-elevated transition-colors">Cancel</button>
                    </td>
                  </>
                ) : (
                  <>
                    <td className="px-4 py-2.5 font-mono text-text-primary text-[11px]">{idx.name}</td>
                    <td className="px-4 py-2.5 font-mono text-accent text-[11px]">{idx.columns.join(', ')}</td>
                    <td className="px-4 py-2.5 text-text-secondary">{idx.type}</td>
                    <td className="px-4 py-2.5">
                      {idx.unique && <span className="text-[10px] px-1.5 py-0.5 rounded bg-accent/10 text-accent font-semibold">UNIQUE</span>}
                    </td>
                    <td className="px-4 py-2.5">
                      <div className="flex items-center gap-2 opacity-0 group-hover:opacity-100 transition-opacity">
                        <button onClick={() => startEdit(idx)}
                          className="text-[10px] text-text-secondary hover:text-text-primary transition-colors">Edit</button>
                        <button onClick={() => deleteIdx(idx.name)}
                          className="text-[10px] text-error/60 hover:text-error transition-colors">Delete</button>
                      </div>
                    </td>
                  </>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {/* Add new index */}
      {adding ? (
        <div className="border-t border-border px-4 py-3 flex items-center gap-2 bg-elevated">
          <input value={newName} onChange={e => setNewName(e.target.value)}
            placeholder="index_name"
            className="bg-surface border border-border rounded px-2 py-1 font-mono text-xs text-text-primary outline-none focus:border-accent w-40" />
          <input value={newCols} onChange={e => setNewCols(e.target.value)}
            placeholder="col1, col2"
            className="bg-surface border border-border rounded px-2 py-1 font-mono text-xs text-text-secondary outline-none focus:border-accent flex-1" />
          <button onClick={() => setNewUnique(!newUnique)}
            className={cn('text-[10px] px-2 py-1 rounded font-semibold border transition-colors',
              newUnique ? 'border-accent text-accent bg-accent/10' : 'border-border text-text-secondary')}>
            {newUnique ? 'UNIQUE' : 'NOT UNIQUE'}
          </button>
          <button onClick={addIdx}
            className="px-3 py-1 rounded bg-accent text-white text-xs font-semibold hover:bg-accent-dim transition-colors">
            Add
          </button>
          <button onClick={() => setAdding(false)}
            className="px-2 py-1 rounded text-text-secondary text-xs hover:bg-surface transition-colors">
            Cancel
          </button>
        </div>
      ) : (
        <div className="border-t border-border px-4 py-2">
          <button onClick={() => setAdding(true)}
            className="text-xs text-text-secondary hover:text-accent transition-colors flex items-center gap-1">
            <span className="text-lg leading-none">+</span> Add index
          </button>
        </div>
      )}
    </div>
  )
}

export default function TableDetailPage({ params }: { params: Promise<{ table: string }> }) {
  const { table: tableName } = use(params)
  const tableInfo = TABLES.find(t => t.name === tableName)
  if (!tableInfo) notFound()

  const [tab, setTab] = useState<Tab>('data')
  const TABS: { id: Tab; label: string }[] = [
    { id: 'data', label: 'Data' },
    { id: 'schema', label: 'Schema' },
    { id: 'indexes', label: 'Indexes' },
  ]

  return (
    <div className="flex flex-col h-full overflow-hidden">
      <div className="border-b border-border px-6 py-3 flex items-center justify-between shrink-0">
        <div className="flex items-center gap-2">
          <Table2 className="w-4 h-4 text-accent" />
          <h1 className="text-sm font-semibold font-mono text-text-primary">{tableName}</h1>
          <span className="text-xs text-text-secondary">
            · {tableInfo.rows.toLocaleString()} rows · {tableInfo.size}
          </span>
        </div>
        <div className="flex gap-1 bg-elevated rounded p-0.5">
          {TABS.map(t => (
            <button key={t.id} onClick={() => setTab(t.id)}
              className={cn(
                'px-3 py-1 text-xs rounded font-medium transition-colors',
                tab === t.id
                  ? 'bg-surface text-text-primary shadow-sm'
                  : 'text-text-secondary hover:text-text-primary'
              )}>
              {t.label}
            </button>
          ))}
        </div>
      </div>
      <div className="flex-1 overflow-hidden">
        {tab === 'data' && <DataTab tableName={tableName} />}
        {tab === 'schema' && <SchemaTab tableName={tableName} />}
        {tab === 'indexes' && <IndexesTab tableName={tableName} />}
      </div>
    </div>
  )
}
