'use client'
import { useState, useRef, useCallback, useEffect } from 'react'
import Link from 'next/link'
import { SCHEMAS, TABLES } from '@/lib/mock'
import { cn } from '@/lib/utils'
import { ZoomIn, ZoomOut, Maximize2, LayoutGrid, ExternalLink } from 'lucide-react'

// ── Layout constants ───────────────────────────────────────────────────────────

const TABLE_W   = 210
const HEADER_H  = 36
const ROW_H     = 22
const PADDING   = 8
const MIN_ZOOM  = 0.3
const MAX_ZOOM  = 2.0

// ── Types ─────────────────────────────────────────────────────────────────────

type Pos = { x: number; y: number }

type TableBox = {
  name: string
  pos: Pos
  type: 'table' | 'view'
}

type Relation = {
  fromTable: string; fromCol: string
  toTable:   string; toCol:   string
}

// ── Collect FK relations ───────────────────────────────────────────────────────

function buildRelations(): Relation[] {
  const rels: Relation[] = []
  for (const [tbl, cols] of Object.entries(SCHEMAS)) {
    for (const col of cols) {
      if (!col.fk) continue
      const [toTable, toCol] = col.fk.split('.')
      if (toTable && toCol) {
        rels.push({ fromTable: tbl, fromCol: col.name, toTable, toCol })
      }
    }
  }
  return rels
}

// ── Compute table height ───────────────────────────────────────────────────────

function tableH(name: string): number {
  const cols = SCHEMAS[name] ?? []
  return HEADER_H + cols.length * ROW_H + PADDING
}

// ── Auto-layout: arrange in a loose grid ──────────────────────────────────────

function autoLayout(): Record<string, Pos> {
  const tables = TABLES.filter(t => t.type === 'table')
  const cols = 2
  const gapX = 80
  const gapY = 60
  const positions: Record<string, Pos> = {}

  tables.forEach((t, i) => {
    const col = i % cols
    const row = Math.floor(i / cols)
    const maxH = Math.max(...tables.slice(row * cols, row * cols + cols).map(tt => tableH(tt.name)))
    const prevRowH = Array.from({ length: row }, (_, r) => {
      const maxInRow = Math.max(...tables.slice(r * cols, r * cols + cols).map(tt => tableH(tt.name)))
      return maxInRow + gapY
    }).reduce((a, b) => a + b, 0)

    positions[t.name] = {
      x: col * (TABLE_W + gapX) + 40,
      y: prevRowH + 40,
    }
  })

  return positions
}

// ── Connection point helpers ───────────────────────────────────────────────────

function getColY(tableName: string, colName: string, pos: Pos): number {
  const cols = SCHEMAS[tableName] ?? []
  const idx  = cols.findIndex(c => c.name === colName)
  if (idx === -1) return pos.y + HEADER_H + ROW_H / 2
  return pos.y + HEADER_H + idx * ROW_H + ROW_H / 2
}

function connectionPath(
  fromPos: Pos, fromTable: string, fromCol: string,
  toPos:   Pos, toTable:   string, toCol:   string,
): string {
  const fY   = getColY(fromTable, fromCol, fromPos)
  const tY   = getColY(toTable,   toCol,   toPos)
  const fX   = fromPos.x + TABLE_W   // right edge of from-table
  const tX   = toPos.x               // left edge of to-table

  // Control point offset
  const dx = Math.abs(tX - fX)
  const cp = Math.max(40, dx * 0.4)

  return `M ${fX} ${fY} C ${fX + cp} ${fY}, ${tX - cp} ${tY}, ${tX} ${tY}`
}

// ── Table SVG component ───────────────────────────────────────────────────────

function TableNode({
  name, pos, selected, onMouseDown, onSelect,
}: {
  name: string; pos: Pos; selected: boolean
  onMouseDown: (e: React.MouseEvent) => void
  onSelect: () => void
}) {
  const cols    = SCHEMAS[name] ?? []
  const h       = tableH(name)
  const tableInfo = TABLES.find(t => t.name === name)

  return (
    <g transform={`translate(${pos.x}, ${pos.y})`}
       style={{ cursor: 'grab' }}
       onMouseDown={onMouseDown}
       onClick={onSelect}>
      {/* Shadow */}
      <rect x="3" y="3" width={TABLE_W} height={h} rx="6"
        fill="rgba(0,0,0,0.4)" />

      {/* Body */}
      <rect x="0" y="0" width={TABLE_W} height={h} rx="6"
        fill="#161b22"
        stroke={selected ? '#10b981' : '#30363d'}
        strokeWidth={selected ? 2 : 1} />

      {/* Header */}
      <rect x="0" y="0" width={TABLE_W} height={HEADER_H} rx="6"
        fill={selected ? '#10b981' : '#21262d'} />
      <rect x="0" y={HEADER_H - 6} width={TABLE_W} height={6}
        fill={selected ? '#10b981' : '#21262d'} />

      {/* Table name */}
      <text x={TABLE_W / 2} y={HEADER_H / 2 + 5}
        textAnchor="middle" fontSize="12" fontWeight="600"
        fill={selected ? '#0d1117' : '#e6edf3'}
        fontFamily="var(--font-geist-sans)">
        {name}
      </text>

      {/* Row count badge */}
      {tableInfo && (
        <text x={TABLE_W - 8} y={HEADER_H / 2 + 5}
          textAnchor="end" fontSize="9" fill={selected ? '#0d1117' : '#8b949e'}
          fontFamily="var(--font-geist-mono)">
          {tableInfo.rows.toLocaleString()}
        </text>
      )}

      {/* Divider */}
      <line x1="0" y1={HEADER_H} x2={TABLE_W} y2={HEADER_H}
        stroke="#30363d" strokeWidth="1" />

      {/* Columns */}
      {cols.map((col, i) => {
        const y   = HEADER_H + i * ROW_H
        const mid = y + ROW_H / 2 + 4

        return (
          <g key={col.name}>
            {/* Row hover bg */}
            <rect x="1" y={y} width={TABLE_W - 2} height={ROW_H}
              fill="transparent"
              className="hover:fill-[#21262d]" />

            {/* PK icon */}
            {col.pk && (
              <text x="10" y={mid} fontSize="10" fill="#f59e0b">🔑</text>
            )}

            {/* FK icon */}
            {col.fk && !col.pk && (
              <text x="10" y={mid} fontSize="10" fill="#60a5fa">🔗</text>
            )}

            {/* Column name */}
            <text x={col.pk || col.fk ? 26 : 10} y={mid}
              fontSize="11" fontFamily="var(--font-geist-mono)"
              fill={col.pk ? '#f59e0b' : col.fk ? '#60a5fa' : '#e6edf3'}>
              {col.name}
            </text>

            {/* Column type */}
            <text x={TABLE_W - 8} y={mid}
              textAnchor="end" fontSize="10"
              fontFamily="var(--font-geist-mono)"
              fill="#8b949e">
              {col.type}
            </text>

            {/* Row separator */}
            {i < cols.length - 1 && (
              <line x1="8" y1={y + ROW_H} x2={TABLE_W - 8} y2={y + ROW_H}
                stroke="#30363d" strokeWidth="0.5" />
            )}
          </g>
        )
      })}
    </g>
  )
}

// ── Main page ─────────────────────────────────────────────────────────────────

export default function DiagramPage() {
  const initialPositions = autoLayout()
  const [positions, setPositions] = useState<Record<string, Pos>>(initialPositions)
  const [zoom, setZoom]           = useState(0.85)
  const [pan, setPan]             = useState<Pos>({ x: 0, y: 0 })
  const [selected, setSelected]   = useState<string | null>(null)
  const [dragging, setDragging]   = useState<string | null>(null)
  const [hoveredRel, setHoveredRel] = useState<number | null>(null)

  const svgRef     = useRef<SVGSVGElement>(null)
  const dragOffset = useRef<Pos>({ x: 0, y: 0 })
  const isPanning  = useRef(false)
  const panStart   = useRef<Pos>({ x: 0, y: 0 })
  const panOrigin  = useRef<Pos>({ x: 0, y: 0 })

  const relations = buildRelations()

  // ── Drag table ───────────────────────────────────────────────────────────────

  const onTableMouseDown = useCallback((name: string) => (e: React.MouseEvent) => {
    e.stopPropagation()
    const svg   = svgRef.current!
    const rect  = svg.getBoundingClientRect()
    const svgX  = (e.clientX - rect.left - pan.x) / zoom
    const svgY  = (e.clientY - rect.top  - pan.y) / zoom
    const pos   = positions[name]

    dragOffset.current = { x: svgX - pos.x, y: svgY - pos.y }
    setDragging(name)
    setSelected(name)
  }, [positions, pan, zoom])

  useEffect(() => {
    if (!dragging) return

    function onMouseMove(e: MouseEvent) {
      const svg  = svgRef.current!
      const rect = svg.getBoundingClientRect()
      const x    = (e.clientX - rect.left - pan.x) / zoom - dragOffset.current.x
      const y    = (e.clientY - rect.top  - pan.y) / zoom - dragOffset.current.y
      setPositions(prev => ({ ...prev, [dragging]: { x: Math.max(0, x), y: Math.max(0, y) } }))
    }

    function onMouseUp() { setDragging(null) }

    window.addEventListener('mousemove', onMouseMove)
    window.addEventListener('mouseup', onMouseUp)
    return () => {
      window.removeEventListener('mousemove', onMouseMove)
      window.removeEventListener('mouseup', onMouseUp)
    }
  }, [dragging, pan, zoom])

  // ── Pan canvas ───────────────────────────────────────────────────────────────

  function onSvgMouseDown(e: React.MouseEvent) {
    if (e.target !== svgRef.current && (e.target as Element).closest('g')) return
    isPanning.current  = true
    panStart.current   = { x: e.clientX, y: e.clientY }
    panOrigin.current  = { ...pan }
  }

  useEffect(() => {
    function onMouseMove(e: MouseEvent) {
      if (!isPanning.current) return
      setPan({
        x: panOrigin.current.x + (e.clientX - panStart.current.x),
        y: panOrigin.current.y + (e.clientY - panStart.current.y),
      })
    }
    function onMouseUp() { isPanning.current = false }
    window.addEventListener('mousemove', onMouseMove)
    window.addEventListener('mouseup', onMouseUp)
    return () => {
      window.removeEventListener('mousemove', onMouseMove)
      window.removeEventListener('mouseup', onMouseUp)
    }
  }, [])

  // ── Zoom ─────────────────────────────────────────────────────────────────────

  function onWheel(e: React.WheelEvent) {
    e.preventDefault()
    setZoom(z => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, z - e.deltaY * 0.001)))
  }

  function fitScreen() {
    setZoom(0.85); setPan({ x: 0, y: 0 })
  }

  function resetLayout() {
    setPositions(autoLayout()); setZoom(0.85); setPan({ x: 0, y: 0 })
  }

  // ── Render ────────────────────────────────────────────────────────────────────

  const tables = TABLES.filter(t => t.type === 'table' && SCHEMAS[t.name])

  return (
    <div className="flex flex-col h-full overflow-hidden">

      {/* Header */}
      <div className="border-b border-border px-6 py-3 flex items-center justify-between shrink-0">
        <div>
          <h1 className="text-sm font-semibold text-text-primary">ER Diagram</h1>
          <p className="text-[11px] text-text-secondary mt-0.5">
            {tables.length} tables · {relations.length} relationships · drag to move · scroll to zoom
          </p>
        </div>
        <div className="flex items-center gap-1">
          <button onClick={() => setZoom(z => Math.min(MAX_ZOOM, z + 0.1))}
            className="p-1.5 rounded text-text-secondary hover:text-text-primary hover:bg-elevated transition-colors">
            <ZoomIn className="w-4 h-4" />
          </button>
          <span className="text-xs text-text-secondary font-mono w-10 text-center">
            {Math.round(zoom * 100)}%
          </span>
          <button onClick={() => setZoom(z => Math.max(MIN_ZOOM, z - 0.1))}
            className="p-1.5 rounded text-text-secondary hover:text-text-primary hover:bg-elevated transition-colors">
            <ZoomOut className="w-4 h-4" />
          </button>
          <div className="w-px h-4 bg-border mx-1" />
          <button onClick={fitScreen}
            className="p-1.5 rounded text-text-secondary hover:text-text-primary hover:bg-elevated transition-colors"
            title="Fit screen">
            <Maximize2 className="w-4 h-4" />
          </button>
          <button onClick={resetLayout}
            className="p-1.5 rounded text-text-secondary hover:text-text-primary hover:bg-elevated transition-colors"
            title="Reset layout">
            <LayoutGrid className="w-4 h-4" />
          </button>
        </div>
      </div>

      {/* Legend */}
      <div className="border-b border-border px-6 py-1.5 flex items-center gap-4 shrink-0 bg-surface">
        {[
          { color: '#f59e0b', label: 'Primary Key' },
          { color: '#60a5fa', label: 'Foreign Key' },
          { color: '#8b949e', label: 'Column' },
          { color: '#10b981', label: 'Selected' },
        ].map(({ color, label }) => (
          <div key={label} className="flex items-center gap-1.5">
            <div className="w-2.5 h-2.5 rounded-sm" style={{ background: color }} />
            <span className="text-[10px] text-text-secondary">{label}</span>
          </div>
        ))}
        <div className="ml-auto flex items-center gap-1 text-[10px] text-text-secondary">
          <div className="w-6 h-px bg-accent/60 inline-block mr-1" />
          FK relationship
        </div>
      </div>

      {/* Canvas */}
      <div className="flex-1 overflow-hidden bg-bg relative" style={{ cursor: dragging ? 'grabbing' : 'default' }}>

        {/* Grid background */}
        <svg className="absolute inset-0 w-full h-full pointer-events-none" style={{ opacity: 0.4 }}>
          <defs>
            <pattern id="grid" width={20 * zoom} height={20 * zoom} patternUnits="userSpaceOnUse"
              x={pan.x % (20 * zoom)} y={pan.y % (20 * zoom)}>
              <path d={`M ${20 * zoom} 0 L 0 0 0 ${20 * zoom}`}
                fill="none" stroke="#21262d" strokeWidth="0.5" />
            </pattern>
          </defs>
          <rect width="100%" height="100%" fill="url(#grid)" />
        </svg>

        {/* Main SVG */}
        <svg
          ref={svgRef}
          className="w-full h-full"
          onMouseDown={onSvgMouseDown}
          onWheel={onWheel}
          style={{ cursor: isPanning.current ? 'grabbing' : 'grab' }}
          onClick={() => setSelected(null)}>

          <g transform={`translate(${pan.x}, ${pan.y}) scale(${zoom})`}>

            {/* FK connection lines */}
            {relations.map((rel, i) => {
              const fromPos = positions[rel.fromTable]
              const toPos   = positions[rel.toTable]
              if (!fromPos || !toPos) return null
              const isHovered = hoveredRel === i

              // Determine if from-table is to the right of to-table
              const reversed = fromPos.x > toPos.x + TABLE_W

              let path: string
              if (reversed) {
                // Connect from left edge of from-table to right edge of to-table
                const fY  = getColY(rel.fromTable, rel.fromCol, fromPos)
                const tY  = getColY(rel.toTable,   rel.toCol,   toPos)
                const fX  = fromPos.x
                const tX  = toPos.x + TABLE_W
                const dx  = Math.abs(fX - tX)
                const cp  = Math.max(40, dx * 0.4)
                path = `M ${fX} ${fY} C ${fX - cp} ${fY}, ${tX + cp} ${tY}, ${tX} ${tY}`
              } else {
                path = connectionPath(fromPos, rel.fromTable, rel.fromCol, toPos, rel.toTable, rel.toCol)
              }

              return (
                <g key={i}>
                  {/* Invisible wider hit area */}
                  <path d={path} fill="none" stroke="transparent" strokeWidth="10"
                    style={{ cursor: 'pointer' }}
                    onMouseEnter={() => setHoveredRel(i)}
                    onMouseLeave={() => setHoveredRel(null)} />

                  {/* Visible line */}
                  <path d={path} fill="none"
                    stroke={isHovered ? '#10b981' : '#60a5fa'}
                    strokeWidth={isHovered ? 2 : 1}
                    strokeDasharray={isHovered ? 'none' : '4 3'}
                    opacity={isHovered ? 1 : 0.5}
                    markerEnd={`url(#arrow-${isHovered ? 'green' : 'blue'})`} />

                  {/* Relationship label on hover */}
                  {isHovered && (() => {
                    const fY  = getColY(rel.fromTable, rel.fromCol, fromPos)
                    const tY  = getColY(rel.toTable,   rel.toCol,   toPos)
                    const fX  = reversed ? fromPos.x : fromPos.x + TABLE_W
                    const tX  = reversed ? toPos.x + TABLE_W : toPos.x
                    const midX = (fX + tX) / 2
                    const midY = (fY + tY) / 2
                    const label = `${rel.fromTable}.${rel.fromCol} → ${rel.toTable}.${rel.toCol}`
                    return (
                      <g>
                        <rect x={midX - label.length * 3} y={midY - 10}
                          width={label.length * 6} height={16} rx="3"
                          fill="#161b22" stroke="#30363d" />
                        <text x={midX} y={midY + 2}
                          textAnchor="middle" fontSize="9"
                          fontFamily="var(--font-geist-mono)"
                          fill="#e6edf3">{label}</text>
                      </g>
                    )
                  })()}
                </g>
              )
            })}

            {/* Arrow markers */}
            <defs>
              {['blue', 'green'].map(color => (
                <marker key={color} id={`arrow-${color}`}
                  markerWidth="8" markerHeight="8" refX="6" refY="3" orient="auto">
                  <path d="M 0 0 L 6 3 L 0 6 Z"
                    fill={color === 'green' ? '#10b981' : '#60a5fa'} />
                </marker>
              ))}
            </defs>

            {/* Tables */}
            {tables.map(t => (
              <TableNode
                key={t.name}
                name={t.name}
                pos={positions[t.name] ?? { x: 0, y: 0 }}
                selected={selected === t.name}
                onMouseDown={onTableMouseDown(t.name)}
                onSelect={() => setSelected(t.name)}
              />
            ))}

          </g>
        </svg>

        {/* Selected table detail panel */}
        {selected && SCHEMAS[selected] && (
          <div className="absolute bottom-4 right-4 w-56 bg-surface border border-border rounded-lg shadow-xl overflow-hidden">
            <div className="px-3 py-2 border-b border-border flex items-center justify-between bg-elevated">
              <span className="text-xs font-semibold font-mono text-text-primary">{selected}</span>
              <Link href={`/tables/${selected}`}
                className="text-text-secondary hover:text-accent transition-colors">
                <ExternalLink className="w-3.5 h-3.5" />
              </Link>
            </div>
            <div className="p-2 space-y-1 max-h-48 overflow-y-auto">
              {(SCHEMAS[selected] ?? []).map(col => (
                <div key={col.name} className="flex items-center justify-between text-[10px]">
                  <div className="flex items-center gap-1.5">
                    {col.pk && <span className="text-warning">🔑</span>}
                    {col.fk && !col.pk && <span className="text-blue-400">🔗</span>}
                    <span className={cn(
                      'font-mono',
                      col.pk ? 'text-warning' : col.fk ? 'text-blue-400' : 'text-text-primary'
                    )}>{col.name}</span>
                  </div>
                  <span className="text-text-secondary font-mono">{col.type}</span>
                </div>
              ))}
            </div>
            {relations.filter(r => r.fromTable === selected || r.toTable === selected).length > 0 && (
              <div className="border-t border-border px-3 py-2">
                <div className="text-[9px] text-text-secondary uppercase tracking-wider mb-1">Relations</div>
                {relations
                  .filter(r => r.fromTable === selected || r.toTable === selected)
                  .map((r, i) => (
                    <div key={i} className="text-[10px] text-text-secondary font-mono truncate">
                      {r.fromTable === selected
                        ? <><span className="text-blue-400">{r.fromCol}</span> → {r.toTable}.{r.toCol}</>
                        : <>{r.fromTable}.{r.fromCol} → <span className="text-accent">{r.toCol}</span></>
                      }
                    </div>
                  ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  )
}
