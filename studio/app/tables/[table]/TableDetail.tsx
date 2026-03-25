'use client'
import { useState, use, useRef, useEffect, useCallback } from 'react'
import { notFound } from 'next/navigation'
import Link from 'next/link'
import {
  Table2, ChevronUp, ChevronDown, ChevronsUpDown,
  Check, X, Trash2, Search, Columns3, Copy, Upload, FileDown,
} from 'lucide-react'
import { TABLES, SCHEMAS, INDEXES, generateUsers, generateOrders } from '@/lib/mock'
import { cn } from '@/lib/utils'
import { useToast } from '@/components/toast'
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
  active_users: ALL_USERS.filter(u => u.active).map(u => ({
    id: u.id, name: u.name, email: u.email, age: u.age, created_at: u.created_at,
  })),
}

type EditingCell = { rowIndex: number; colKey: string } | null

// ── Context menu types ────────────────────────────────────────────────────────

type ContextMenu = {
  x: number
  y: number
  value: unknown
  rowData: Record<string, unknown>
  colKey: string
}

// ── Toast ─────────────────────────────────────────────────────────────────────

function Toast({ message, onDone }: { message: string; onDone: () => void }) {
  useEffect(() => {
    const t = setTimeout(onDone, 2000)
    return () => clearTimeout(t)
  }, [onDone])
  return (
    <div className="fixed bottom-6 left-1/2 -translate-x-1/2 z-50 bg-accent text-white text-xs px-4 py-2 rounded-lg shadow-xl font-semibold animate-fade-in">
      {message}
    </div>
  )
}

// ── Cell value renderer ───────────────────────────────────────────────────────

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

// ── CSV parser ────────────────────────────────────────────────────────────────

function parseCsv(text: string): Record<string, unknown>[] {
  const lines = text.trim().split('\n')
  if (lines.length < 2) return []
  const headers = lines[0].split(',').map(h => h.trim().replace(/^"|"$/g, ''))
  return lines.slice(1).map(line => {
    const vals = line.split(',').map(v => v.trim().replace(/^"|"$/g, ''))
    return Object.fromEntries(headers.map((h, i) => [h, vals[i] ?? '']))
  })
}

// ── SQL exporter ──────────────────────────────────────────────────────────────

function exportSql(tableName: string, rows: Record<string, unknown>[]) {
  if (!rows.length) return
  const cols = Object.keys(rows[0])
  const header = `-- Export: ${tableName} (${rows.length} rows)\n-- Generated: ${new Date().toLocaleString()} by AxiomStudio\n\n`
  const values = rows.map(row =>
    '  (' + cols.map(c => {
      const v = row[c]
      if (v === null || v === undefined) return 'NULL'
      if (typeof v === 'boolean') return v ? 'TRUE' : 'FALSE'
      if (typeof v === 'number') return String(v)
      return `'${String(v).replace(/'/g, "''")}'`
    }).join(', ') + ')'
  ).join(',\n')
  const sql = header + `INSERT INTO ${tableName} (${cols.join(', ')}) VALUES\n${values}\n;\n`
  const blob = new Blob([sql], { type: 'text/sql' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = `${tableName}.sql`
  a.click()
  URL.revokeObjectURL(url)
}

// ── Confirm modal ─────────────────────────────────────────────────────────────

function ConfirmModal({ title, message, onConfirm, onCancel, danger = false }: {
  title: string
  message: string
  onConfirm: () => void
  onCancel: () => void
  danger?: boolean
}) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-[#161b22] border border-[#30363d] rounded-lg p-5 w-80 shadow-xl">
        <h3 className="text-sm font-semibold text-[#e6edf3] mb-2">{title}</h3>
        <p className="text-xs text-[#8b949e] mb-4">{message}</p>
        <div className="flex justify-end gap-2">
          <button onClick={onCancel}
            className="px-3 py-1.5 rounded text-xs text-[#8b949e] border border-[#30363d] hover:bg-[#21262d] transition-colors">
            Cancel
          </button>
          <button onClick={onConfirm}
            className={cn(
              'px-3 py-1.5 rounded text-xs font-semibold transition-colors',
              danger
                ? 'bg-[#f85149] text-white hover:bg-[#f85149]/80'
                : 'bg-[#10b981] text-white hover:bg-[#10b981]/80'
            )}>
            Confirm
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Import modal ──────────────────────────────────────────────────────────────

function ImportModal({ previewRows, onConfirm, onCancel, error }: {
  previewRows: Record<string, unknown>[] | null
  onConfirm: () => void
  onCancel: () => void
  error: string | null
}) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-[#161b22] border border-[#30363d] rounded-lg p-5 w-96 shadow-xl">
        <h3 className="text-sm font-semibold text-[#e6edf3] mb-3">Import data</h3>
        {error ? (
          <div className="mb-4 px-3 py-2 rounded bg-[#f85149]/10 border border-[#f85149]/30 text-xs text-[#f85149]">
            {error}
          </div>
        ) : previewRows ? (
          <p className="text-xs text-[#8b949e] mb-4">
            Ready to import <span className="text-[#e6edf3] font-semibold">{previewRows.length} rows</span>.
            This will append rows to the current table.
          </p>
        ) : null}
        <div className="flex justify-end gap-2">
          <button onClick={onCancel}
            className="px-3 py-1.5 rounded text-xs text-[#8b949e] border border-[#30363d] hover:bg-[#21262d] transition-colors">
            Cancel
          </button>
          {!error && previewRows && (
            <button onClick={onConfirm}
              className="px-3 py-1.5 rounded text-xs font-semibold bg-[#10b981] text-white hover:bg-[#10b981]/80 transition-colors">
              Import {previewRows.length} rows
            </button>
          )}
        </div>
      </div>
    </div>
  )
}


// ── DataTab ───────────────────────────────────────────────────────────────────

function DataTab({ tableName }: { tableName: string }) {
  const initial = (TABLE_DATA[tableName] ?? []) as Record<string, unknown>[]
  const [rows, setRows] = useState(initial)
  const [pageIndex, setPageIndex] = useState(0)
  const [sorting, setSorting] = useState<SortingState>([])
  const [editing, setEditing] = useState<EditingCell>(null)
  const [editValue, setEditValue] = useState('')
  const [lastSql, setLastSql] = useState<SqlLog | null>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const { show: showToast } = useToast()

  // Quick filter
  const [filterText, setFilterText] = useState('')

  // Column visibility
  const allColKeys = rows.length > 0 ? Object.keys(rows[0]) : []
  const [visibleCols, setVisibleCols] = useState<Set<string>>(() => new Set(allColKeys))
  const [showColMenu, setShowColMenu] = useState(false)
  const colMenuRef = useRef<HTMLDivElement>(null)

  // Add row
  const [addingRow, setAddingRow] = useState(false)
  const [newRowValues, setNewRowValues] = useState<Record<string, string>>({})

  // Delete confirmation (unused inline — modal used instead)
  const [deletingRowIdx] = useState<number | null>(null)

  // Import state
  const [importPreview, setImportPreview] = useState<Record<string, unknown>[] | null>(null)
  const [importError, setImportError] = useState<string | null>(null)
  const [showImportModal, setShowImportModal] = useState(false)

  // Delete confirm modal
  const [confirmDeleteIdx, setConfirmDeleteIdx] = useState<number | null>(null)

  // Context menu
  const [contextMenu, setContextMenu] = useState<ContextMenu | null>(null)

  // Filter applied on rows (before pagination)
  const filteredRows = filterText.trim() === ''
    ? rows
    : rows.filter(row =>
        Object.values(row).some(v =>
          String(v ?? '').toLowerCase().includes(filterText.toLowerCase())
        )
      )

  const pageSize = 15
  const nonEditableCols = new Set(['id', 'created_at', 'status'])

  useEffect(() => { if (editing) inputRef.current?.focus() }, [editing])

  // Close col visibility menu on outside click
  useEffect(() => {
    function handle(e: MouseEvent) {
      if (colMenuRef.current && !colMenuRef.current.contains(e.target as Node)) {
        setShowColMenu(false)
      }
    }
    if (showColMenu) document.addEventListener('mousedown', handle)
    return () => document.removeEventListener('mousedown', handle)
  }, [showColMenu])

  // Close context menu on outside click / scroll
  useEffect(() => {
    if (!contextMenu) return
    function close() { setContextMenu(null) }
    document.addEventListener('mousedown', close)
    document.addEventListener('scroll', close, true)
    return () => {
      document.removeEventListener('mousedown', close)
      document.removeEventListener('scroll', close, true)
    }
  }, [contextMenu])

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

  // Delete row
  function deleteRow(rowIndex: number) {
    const row = rows[rowIndex]
    setRows(prev => prev.filter((_, i) => i !== rowIndex))
    const now = new Date().toTimeString().slice(0, 8)
    setLastSql({
      sql: `DELETE FROM ${tableName} WHERE id = ${row.id};`,
      axiomql: `${tableName}.filter(id = ${row.id}).delete()`,
      ts: now,
    })
  }

  // Clone row
  function cloneRow(rowIndex: number) {
    const source = rows[rowIndex] as Record<string, unknown>
    const maxId = rows.reduce((m, r) => Math.max(m, Number((r as Record<string, unknown>).id ?? 0)), 0)
    const clone: Record<string, unknown> = { ...source, id: maxId + 1 }
    setRows(prev => {
      const next = [...prev]
      next.splice(rowIndex + 1, 0, clone)
      return next
    })
    const now = new Date().toTimeString().slice(0, 8)
    const cols = allColKeys.filter(k => k !== 'id').join(', ')
    const vals = allColKeys.filter(k => k !== 'id').map(k => `'${source[k] ?? ''}'`).join(', ')
    setLastSql({
      sql: `INSERT INTO ${tableName} (${cols}) VALUES (${vals});`,
      axiomql: `${tableName}.insert({${allColKeys.filter(k => k !== 'id').map(k => `${k}: '${source[k] ?? ''}'`).join(', ')}})`,
      ts: now,
    })
    showToast('Row cloned')
  }

  // Add row
  function commitAddRow() {
    const maxId = rows.reduce((m, r) => Math.max(m, Number(r.id ?? 0)), 0)
    const newRow: Record<string, unknown> = { id: maxId + 1 }
    for (const key of allColKeys) {
      if (key === 'id') continue
      newRow[key] = newRowValues[key] ?? ''
    }
    setRows(prev => [...prev, newRow])
    setAddingRow(false)
    setNewRowValues({})
    const now = new Date().toTimeString().slice(0, 8)
    const cols = allColKeys.filter(k => k !== 'id').join(', ')
    const vals = allColKeys.filter(k => k !== 'id').map(k => `'${newRowValues[k] ?? ''}'`).join(', ')
    setLastSql({
      sql: `INSERT INTO ${tableName} (${cols}) VALUES (${vals});`,
      axiomql: `${tableName}.insert({${allColKeys.filter(k => k !== 'id').map(k => `${k}: '${newRowValues[k] ?? ''}'`).join(', ')}})`,
      ts: now,
    })
  }

  // Context menu actions
  function copyValue(value: unknown) {
    navigator.clipboard.writeText(String(value ?? '')).catch(() => null)
    setContextMenu(null)
  }

  function copyRowAsJson(row: Record<string, unknown>) {
    navigator.clipboard.writeText(JSON.stringify(row, null, 2)).catch(() => null)
    setContextMenu(null)
  }

  function filterByValue(value: unknown) {
    setFilterText(String(value ?? ''))
    setContextMenu(null)
  }

  // Import handler
  function handleFileChange(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0]
    if (!file) return
    // Reset input so same file can be re-selected
    e.target.value = ''
    const reader = new FileReader()
    reader.onload = ev => {
      const text = ev.target?.result as string
      try {
        if (file.name.endsWith('.json')) {
          const parsed = JSON.parse(text)
          if (!Array.isArray(parsed)) {
            setImportError('JSON must be an array of objects.')
            setImportPreview(null)
          } else {
            setImportPreview(parsed as Record<string, unknown>[])
            setImportError(null)
          }
        } else {
          const parsed = parseCsv(text)
          if (!parsed.length) {
            setImportError('CSV is empty or has no data rows.')
            setImportPreview(null)
          } else {
            setImportPreview(parsed)
            setImportError(null)
          }
        }
      } catch {
        setImportError('Failed to parse file. Check the format.')
        setImportPreview(null)
      }
      setShowImportModal(true)
    }
    reader.readAsText(file)
  }

  function confirmImport() {
    if (!importPreview) return
    const maxId = rows.reduce((m, r) => Math.max(m, Number(r.id ?? 0)), 0)
    const withIds = importPreview.map((r, i) => ({ id: maxId + i + 1, ...r }))
    setRows(prev => [...prev, ...withIds])
    showToast(`${importPreview.length} rows imported`)
    setShowImportModal(false)
    setImportPreview(null)
  }

  function cancelImport() {
    setShowImportModal(false)
    setImportPreview(null)
    setImportError(null)
  }

  // Table columns — only visible ones
  const displayKeys = allColKeys.filter(k => visibleCols.has(k))

  const columns: ColumnDef<Record<string, unknown>>[] = filteredRows.length > 0
    ? displayKeys.map(key => ({
        accessorKey: key,
        header: key,
        enableSorting: true,
      }))
    : []

  const table = useReactTable({
    data: filteredRows,
    columns,
    state: { pagination: { pageIndex, pageSize }, sorting },
    onSortingChange: setSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
  })

  const pageRows = table.getRowModel().rows

  return (
    <div className="flex flex-col h-full overflow-hidden">
      {/* Hidden file input for import */}
      <input
        ref={fileInputRef}
        type="file"
        accept=".csv,.json"
        onChange={handleFileChange}
        className="hidden"
      />

      {/* Import modal */}
      {showImportModal && (
        <ImportModal
          previewRows={importPreview}
          onConfirm={confirmImport}
          onCancel={cancelImport}
          error={importError}
        />
      )}

      {/* Delete confirm modal */}
      {confirmDeleteIdx !== null && (
        <ConfirmModal
          title="Delete row"
          message={`Delete row with id = ${rows[confirmDeleteIdx]?.id ?? confirmDeleteIdx}? This cannot be undone.`}
          danger
          onConfirm={() => { deleteRow(confirmDeleteIdx); setConfirmDeleteIdx(null) }}
          onCancel={() => setConfirmDeleteIdx(null)}
        />
      )}

      {/* Toolbar above table */}
      <div className="flex items-center gap-2 px-3 py-2 border-b border-border shrink-0 bg-elevated/40">
        {/* Filter input */}
        <div className="flex items-center gap-1.5 flex-1 max-w-64 px-2 py-1 rounded bg-elevated border border-border text-xs">
          <Search className="w-3 h-3 text-text-secondary shrink-0" />
          <input
            value={filterText}
            onChange={e => { setFilterText(e.target.value); setPageIndex(0) }}
            placeholder="Filter rows..."
            className="bg-transparent outline-none text-text-secondary placeholder-text-secondary/50 w-full font-mono text-xs"
          />
          {filterText && (
            <button onClick={() => { setFilterText(''); setPageIndex(0) }}
              className="text-text-secondary hover:text-text-primary transition-colors shrink-0">
              <X className="w-3 h-3" />
            </button>
          )}
        </div>

        {filterText && (
          <span className="text-[10px] text-text-secondary font-mono">
            {filteredRows.length} of {rows.length} rows
          </span>
        )}

        {/* Import + Export SQL */}
        <button
          onClick={() => fileInputRef.current?.click()}
          className="flex items-center gap-1.5 px-2.5 py-1 rounded text-xs font-medium border border-border text-text-secondary hover:bg-elevated hover:text-text-primary transition-colors ml-auto">
          <Upload className="w-3 h-3" />
          Import
        </button>
        <button
          onClick={() => exportSql(tableName, rows)}
          disabled={!rows.length}
          className="flex items-center gap-1.5 px-2.5 py-1 rounded text-xs font-medium border border-border text-text-secondary hover:bg-elevated hover:text-text-primary transition-colors disabled:opacity-40">
          <FileDown className="w-3 h-3" />
          Export SQL
        </button>

        {/* Column visibility */}
        <div className="relative" ref={colMenuRef}>
          <button
            onClick={() => setShowColMenu(p => !p)}
            className={cn(
              'flex items-center gap-1.5 px-2.5 py-1 rounded text-xs font-medium border transition-colors',
              showColMenu
                ? 'border-accent text-accent bg-accent/10'
                : 'border-border text-text-secondary hover:bg-elevated hover:text-text-primary'
            )}>
            <Columns3 className="w-3 h-3" />
            Columns
          </button>
          {showColMenu && (
            <div className="absolute right-0 top-full mt-1 z-50 bg-surface border border-border rounded-lg shadow-xl p-2 min-w-36">
              <div className="flex items-center justify-between mb-2 px-1">
                <span className="text-[10px] text-text-secondary uppercase tracking-wider font-semibold">Visibility</span>
                <button
                  onClick={() => setVisibleCols(new Set(allColKeys))}
                  className="text-[10px] text-accent hover:text-accent/70 transition-colors">
                  Reset
                </button>
              </div>
              {allColKeys.map(key => (
                <label key={key}
                  className="flex items-center gap-2 px-1 py-1 rounded hover:bg-elevated cursor-pointer transition-colors">
                  <input
                    type="checkbox"
                    checked={visibleCols.has(key)}
                    onChange={e => {
                      setVisibleCols(prev => {
                        const next = new Set(prev)
                        if (e.target.checked) next.add(key)
                        else next.delete(key)
                        return next
                      })
                    }}
                    className="accent-[#10b981] w-3 h-3"
                  />
                  <span className="font-mono text-xs text-text-primary">{key}</span>
                </label>
              ))}
            </div>
          )}
        </div>
      </div>

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
                {/* Actions column header */}
                <th className="w-16 px-2 py-2" />
              </tr>
            ))}
          </thead>
          <tbody>
            {pageRows.map(row => {
              const originalIndex = row.index
              const fullRow = filteredRows[originalIndex]
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
                        onClick={() => !isEditing && startEdit(originalIndex, colKey, cell.getValue())}
                        onContextMenu={e => {
                          e.preventDefault()
                          setContextMenu({
                            x: e.clientX,
                            y: e.clientY,
                            value: cell.getValue(),
                            rowData: fullRow,
                            colKey,
                          })
                        }}>
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
                  {/* Row action buttons: clone + delete */}
                  <td className="px-2 py-1.5 w-16">
                    <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
                      <button
                        title="Clone row"
                        onClick={() => cloneRow(originalIndex)}
                        className="p-0.5 rounded text-text-secondary hover:text-accent hover:bg-accent/10 transition-colors">
                        <Copy className="w-3 h-3" />
                      </button>
                      <button
                        title="Delete row"
                        onClick={() => setConfirmDeleteIdx(originalIndex)}
                        className="p-0.5 rounded text-text-secondary hover:text-error hover:bg-error/10 transition-colors">
                        <Trash2 className="w-3 h-3" />
                      </button>
                    </div>
                  </td>
                </tr>
              )
            })}

            {/* Add row inline form */}
            {addingRow && (
              <tr className="border-b border-accent/30 bg-elevated/50">
                {displayKeys.map(key => (
                  <td key={key} className="px-2 py-1.5">
                    {key === 'id' ? (
                      <span className="font-mono text-xs text-text-secondary italic">auto</span>
                    ) : (
                      <input
                        value={newRowValues[key] ?? ''}
                        onChange={e => setNewRowValues(prev => ({ ...prev, [key]: e.target.value }))}
                        placeholder={key}
                        className="w-full bg-surface border border-border rounded px-2 py-0.5 font-mono text-xs text-text-primary outline-none focus:border-accent transition-colors"
                      />
                    )}
                  </td>
                ))}
                <td className="px-2 py-1.5">
                  <div className="flex items-center gap-1">
                    <button onClick={commitAddRow}
                      className="text-[10px] px-2 py-0.5 rounded bg-accent text-white font-semibold hover:bg-accent/80 transition-colors">
                      Save
                    </button>
                    <button onClick={() => { setAddingRow(false); setNewRowValues({}) }}
                      className="text-[10px] px-2 py-0.5 rounded text-text-secondary hover:bg-elevated transition-colors">
                      Cancel
                    </button>
                  </div>
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {/* Add row button */}
      {!addingRow && (
        <div className="border-t border-border/50 px-3 py-1.5 shrink-0 bg-elevated/30">
          <button
            onClick={() => { setAddingRow(true); setNewRowValues({}) }}
            className="flex items-center gap-1 text-xs text-text-secondary hover:text-accent transition-colors font-medium">
            <span className="text-base leading-none">+</span> Add row
          </button>
        </div>
      )}

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
          <span className="text-xs text-text-secondary font-mono">
            {filterText
              ? `${filteredRows.length} of ${rows.length} rows`
              : `${rows.length.toLocaleString()} rows`}
          </span>
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
            Page {pageIndex + 1} / {table.getPageCount() || 1}
          </span>
          <button onClick={() => setPageIndex(p => Math.min(table.getPageCount() - 1, p + 1))} disabled={pageIndex >= table.getPageCount() - 1}
            className="text-xs text-text-secondary hover:text-text-primary disabled:opacity-30 px-2 py-1 rounded hover:bg-elevated transition-colors">
            Next →
          </button>
        </div>
      </div>

      {/* Context menu */}
      {contextMenu && (
        <div
          style={{ position: 'fixed', top: contextMenu.y, left: contextMenu.x, zIndex: 9999 }}
          className="bg-surface border border-border rounded-lg shadow-xl py-1 min-w-40 text-xs"
          onMouseDown={e => e.stopPropagation()}>
          <button
            onClick={() => copyValue(contextMenu.value)}
            className="w-full text-left px-3 py-1.5 hover:bg-elevated transition-colors flex items-center gap-2 text-text-primary">
            <Copy className="w-3 h-3 text-text-secondary" />
            Copy value
          </button>
          <button
            onClick={() => copyRowAsJson(contextMenu.rowData)}
            className="w-full text-left px-3 py-1.5 hover:bg-elevated transition-colors flex items-center gap-2 text-text-primary">
            <Copy className="w-3 h-3 text-text-secondary" />
            Copy row as JSON
          </button>
          <div className="border-t border-border/50 my-1" />
          <button
            onClick={() => filterByValue(contextMenu.value)}
            className="w-full text-left px-3 py-1.5 hover:bg-elevated transition-colors flex items-center gap-2 text-text-primary">
            <Search className="w-3 h-3 text-text-secondary" />
            Filter by this value
          </button>
        </div>
      )}
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
  const [confirmDropIdx, setConfirmDropIdx] = useState<string | null>(null)

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
      {/* Confirm drop modal */}
      {confirmDropIdx !== null && (
        <ConfirmModal
          title="Drop index"
          message={`Drop index "${confirmDropIdx}"? This cannot be undone.`}
          danger
          onConfirm={() => { deleteIdx(confirmDropIdx); setConfirmDropIdx(null) }}
          onCancel={() => setConfirmDropIdx(null)}
        />
      )}
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
                        <button onClick={() => setConfirmDropIdx(idx.name)}
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

// ── Generate DDL ──────────────────────────────────────────────────────────────

function generateDDL(tableName: string): string {
  const schema = SCHEMAS[tableName]
  if (!schema) return `-- No schema found for ${tableName}`
  const cols = schema.map(c => {
    const parts = [`  ${c.name} ${c.type}`]
    if (!c.nullable) parts.push('NOT NULL')
    if (c.pk) parts.push('PRIMARY KEY')
    if (c.default) parts.push(`DEFAULT ${c.default}`)
    if (c.fk) parts.push(`REFERENCES ${c.fk}`)
    return parts.join(' ')
  })
  return `CREATE TABLE ${tableName} (\n${cols.join(',\n')}\n);`
}

// ── Table detail page ─────────────────────────────────────────────────────────

export default function TableDetailPage({ params }: { params: Promise<{ table: string }> }) {
  const { table: tableName } = use(params)
  const tableInfo = TABLES.find(t => t.name === tableName)
  if (!tableInfo) notFound()

  const [tab, setTab] = useState<Tab>('data')
  const { show: showToast } = useToast()
  const TABS: { id: Tab; label: string }[] = [
    { id: 'data', label: 'Data' },
    { id: 'schema', label: 'Schema' },
    { id: 'indexes', label: 'Indexes' },
  ]

  const handleCopyDDL = useCallback(() => {
    const ddl = generateDDL(tableName)
    navigator.clipboard.writeText(ddl).then(() => {
      showToast('DDL copied to clipboard')
    }).catch(() => null)
  }, [tableName, showToast])

  return (
    <div className="flex flex-col h-full overflow-hidden">
      <div className="border-b border-border px-6 py-3 flex items-center justify-between shrink-0">
        <div className="flex flex-col gap-1">
          {/* Breadcrumbs */}
          <div className="flex items-center gap-1.5 text-xs">
            <Link href="/tables" className="text-text-secondary hover:text-accent transition-colors">Tables</Link>
            <span className="text-border">/</span>
            <span className="text-text-primary font-semibold font-mono">{tableName}</span>
            <span className="text-text-secondary">· {tableInfo.rows.toLocaleString()} rows</span>
          </div>
          {/* Actions row */}
          <div className="flex items-center gap-2">
            <Table2 className="w-3.5 h-3.5 text-accent" />
            <span className="text-xs text-text-secondary">{tableInfo.size}</span>
            <button
              onClick={handleCopyDDL}
              className="flex items-center gap-1 ml-1 px-2 py-0.5 rounded text-[10px] font-medium border border-border text-text-secondary hover:border-accent/50 hover:text-accent transition-colors">
              <Copy className="w-2.5 h-2.5" />
              Copy DDL
            </button>
          </div>
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
