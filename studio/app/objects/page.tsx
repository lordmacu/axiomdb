'use client'
import { useState, useCallback } from 'react'
import dynamic from 'next/dynamic'
import type * as Monaco from 'monaco-editor'
import {
  Play, Plus, Trash2, ToggleLeft, ToggleRight,
  ChevronDown, ChevronRight, Code2, Zap, Search, X,
} from 'lucide-react'
import { PROCEDURES, FUNCTIONS, TRIGGERS, SEQUENCES, type Procedure, type Func, type Trigger, type Sequence } from '@/lib/mock'
import { cn } from '@/lib/utils'
import { useToast } from '@/components/toast'

const MonacoEditor = dynamic(() => import('@monaco-editor/react'), { ssr: false })

// ── AxiomQL language + theme registration (same as Query Editor) ───────────────

function registerAxiomQL(monaco: typeof Monaco) {
  if (monaco.languages.getLanguages().some((l: { id: string }) => l.id === 'axiomql')) return

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
      ],
    },
  })
  monaco.editor.defineTheme('axiomql-dark', {
    base: 'vs-dark',
    inherit: true,
    rules: [
      { token: 'keyword',  foreground: '10b981', fontStyle: 'bold' },
      { token: 'constant', foreground: 'f59e0b' },
      { token: 'type',     foreground: '60a5fa' },
      { token: 'function', foreground: 'c084fc' },
      { token: 'string',   foreground: '86efac' },
      { token: 'number',   foreground: 'fb923c' },
      { token: 'comment',  foreground: '6b7280', fontStyle: 'italic' },
    ],
    colors: { 'editor.background': '#0d1117' },
  })
}

type Tab = 'procedures' | 'functions' | 'triggers' | 'sequences'

// ── Highlight matching text ───────────────────────────────────────────────────

function HighlightMatch({ text, query }: { text: string; query: string }) {
  if (!query.trim()) return <>{text}</>
  const idx = text.toLowerCase().indexOf(query.toLowerCase())
  if (idx === -1) return <>{text}</>
  return (
    <>
      {text.slice(0, idx)}
      <span className="text-accent font-bold">{text.slice(idx, idx + query.length)}</span>
      {text.slice(idx + query.length)}
    </>
  )
}

// ── Search input for object lists ─────────────────────────────────────────────

function ObjectSearch({
  value,
  onChange,
  placeholder,
}: {
  value: string
  onChange: (v: string) => void
  placeholder: string
}) {
  return (
    <div className="px-2 py-1.5 border-b border-border shrink-0">
      <div className="flex items-center gap-1.5 px-2 py-1 rounded bg-elevated border border-border text-xs">
        <Search className="w-3 h-3 text-text-secondary shrink-0" />
        <input
          value={value}
          onChange={e => onChange(e.target.value)}
          placeholder={placeholder}
          className="bg-transparent outline-none text-text-secondary placeholder-text-secondary/50 w-full font-mono text-[11px]"
        />
        {value && (
          <button onClick={() => onChange('')} className="text-text-secondary hover:text-text-primary transition-colors shrink-0">
            <X className="w-3 h-3" />
          </button>
        )}
      </div>
    </div>
  )
}

// ── Shared editor panel ───────────────────────────────────────────────────────

function CodePanel({ title, language, body, args, returns, onBodyChange, onSave }: {
  title: string
  language: string
  body: string
  args?: { name: string; type: string }[]
  returns?: string
  onBodyChange: (v: string) => void
  onSave?: () => void
}) {
  const [running, setRunning] = useState(false)
  const [result, setResult] = useState<string | null>(null)

  async function run() {
    setRunning(true)
    await new Promise(r => setTimeout(r, 400 + Math.random() * 300))
    setResult('Executed successfully. (mock — connects to real AxiomDB in Phase 8)')
    setRunning(false)
    setTimeout(() => setResult(null), 3000)
  }

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="border-b border-border px-4 py-3 flex items-center justify-between shrink-0">
        <div>
          <h2 className="text-sm font-semibold font-mono text-text-primary">{title}</h2>
          {args && args.length > 0 && (
            <div className="text-[11px] text-text-secondary mt-0.5 font-mono">
              ({args.map(a => `${a.name}: ${a.type}`).join(', ')})
              {returns && <span className="text-accent"> → {returns}</span>}
            </div>
          )}
        </div>
        <div className="flex items-center gap-2">
          <span className={cn(
            'text-[10px] px-2 py-0.5 rounded font-semibold',
            language === 'axiomql' ? 'bg-accent/10 text-accent' : 'bg-blue-400/10 text-blue-400'
          )}>
            {language.toUpperCase()}
          </span>
          {onSave && (
            <button onClick={onSave}
              className="flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-semibold border border-[#30363d] text-[#8b949e] hover:text-[#e6edf3] hover:border-[#10b981]/50 transition-all">
              Save
            </button>
          )}
          <button onClick={run} disabled={running}
            className={cn(
              'flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-semibold transition-all',
              running ? 'bg-accent/50 text-white/50 cursor-not-allowed' : 'bg-accent text-white hover:bg-accent-dim active:scale-95'
            )}>
            <Play className="w-3 h-3" />
            {running ? 'Running…' : 'Run'}
          </button>
        </div>
      </div>

      {/* Editor */}
      <div className="flex-1 min-h-0">
        <MonacoEditor
          height="100%"
          language={language === 'axiomql' ? 'axiomql' : 'sql'}
          value={body}
          onChange={v => onBodyChange(v ?? '')}
          theme={language === 'axiomql' ? 'axiomql-dark' : 'vs-dark'}
          beforeMount={monaco => registerAxiomQL(monaco)}
          onMount={(_editor, monaco) => registerAxiomQL(monaco)}
          options={{
            fontSize: 13,
            fontFamily: 'var(--font-geist-mono)',
            lineHeight: 1.7,
            minimap: { enabled: false },
            scrollBeyondLastLine: false,
            padding: { top: 16, bottom: 16 },
            renderLineHighlight: 'none',
          }}
        />
      </div>

      {/* Result toast */}
      {result && (
        <div className="border-t border-border px-4 py-2 bg-accent/5 shrink-0">
          <span className="text-xs text-accent">{result}</span>
        </div>
      )}
    </div>
  )
}

// ── Confirm modal ─────────────────────────────────────────────────────────────

function ConfirmModal({ title, message, onConfirm, onCancel }: {
  title: string; message: string; onConfirm: () => void; onCancel: () => void
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
            className="px-3 py-1.5 rounded text-xs font-semibold bg-[#f85149] text-white hover:bg-[#f85149]/80 transition-colors">
            Confirm
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Create object modal ────────────────────────────────────────────────────────

function CreateObjectModal({ title, onSave, onCancel, extraFields }: {
  title: string
  onSave: (name: string, language: string, body: string) => void
  onCancel: () => void
  extraFields?: React.ReactNode
}) {
  const [name, setName] = useState('')
  const [language, setLanguage] = useState<'sql' | 'axiomql'>('sql')
  const [body, setBody] = useState('')
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-[#161b22] border border-[#30363d] rounded-lg p-5 w-96 shadow-xl flex flex-col gap-3">
        <h3 className="text-sm font-semibold text-[#e6edf3]">{title}</h3>
        <div className="flex flex-col gap-1">
          <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">Name</label>
          <input value={name} onChange={e => setName(e.target.value)}
            placeholder="object_name"
            className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-xs font-mono text-[#e6edf3] outline-none focus:border-[#10b981]" />
        </div>
        <div className="flex flex-col gap-1">
          <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">Language</label>
          <div className="flex gap-1">
            {(['sql', 'axiomql'] as const).map(l => (
              <button key={l} onClick={() => setLanguage(l)}
                className={cn('px-3 py-1 rounded text-xs font-semibold transition-colors',
                  language === l ? 'bg-[#10b981] text-white' : 'border border-[#30363d] text-[#8b949e] hover:text-[#e6edf3]')}>
                {l.toUpperCase()}
              </button>
            ))}
          </div>
        </div>
        {extraFields}
        <div className="flex flex-col gap-1">
          <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">Body</label>
          <textarea value={body} onChange={e => setBody(e.target.value)}
            placeholder={language === 'sql' ? 'BEGIN\n  -- body\nEND' : '-- axiomql body'}
            rows={5}
            className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-xs font-mono text-[#e6edf3] outline-none focus:border-[#10b981] resize-none" />
        </div>
        <div className="flex justify-end gap-2">
          <button onClick={onCancel}
            className="px-3 py-1.5 rounded text-xs text-[#8b949e] border border-[#30363d] hover:bg-[#21262d] transition-colors">
            Cancel
          </button>
          <button onClick={() => name.trim() && onSave(name.trim(), language, body)}
            disabled={!name.trim()}
            className="px-3 py-1.5 rounded text-xs font-semibold bg-[#10b981] text-white hover:bg-[#10b981]/80 disabled:opacity-40 transition-colors">
            Create
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Procedures tab ────────────────────────────────────────────────────────────

function ProceduresTab() {
  const [procs, setProcs] = useState<Procedure[]>(PROCEDURES)
  const [selected, setSelected] = useState<string>(procs[0]?.name ?? '')
  const [search, setSearch] = useState('')
  const [showCreate, setShowCreate] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null)
  const { show: showToast } = useToast()

  const filtered = search.trim()
    ? procs.filter(p => p.name.toLowerCase().includes(search.toLowerCase()))
    : procs

  const proc = procs.find(p => p.name === selected)

  return (
    <div className="flex h-full">
      {showCreate && (
        <CreateObjectModal
          title="New Procedure"
          onSave={(name, language, body) => {
            setProcs(ps => [...ps, { name, language: language as 'sql' | 'axiomql', args: [], body, createdAt: new Date().toISOString(), updatedAt: new Date().toISOString() }])
            setSelected(name)
            setShowCreate(false)
            showToast('Procedure created')
          }}
          onCancel={() => setShowCreate(false)}
        />
      )}
      {confirmDelete && (
        <ConfirmModal
          title="Delete procedure"
          message={`Delete procedure "${confirmDelete}"? This cannot be undone.`}
          onConfirm={() => {
            setProcs(ps => ps.filter(p => p.name !== confirmDelete))
            if (selected === confirmDelete) setSelected(procs.find(p => p.name !== confirmDelete)?.name ?? '')
            setConfirmDelete(null)
            showToast('Procedure deleted')
          }}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      {/* List */}
      <div className="w-52 border-r border-border flex flex-col shrink-0">
        <div className="px-3 py-2 border-b border-border flex items-center justify-between shrink-0">
          <span className="text-[10px] font-semibold tracking-widest text-text-secondary uppercase">
            Procedures ({procs.length})
          </span>
          <button onClick={() => setShowCreate(true)} className="text-text-secondary hover:text-accent transition-colors">
            <Plus className="w-3.5 h-3.5" />
          </button>
        </div>
        <ObjectSearch value={search} onChange={setSearch} placeholder="Search procedures…" />
        {search && (
          <div className="px-3 py-1 shrink-0 text-[10px] text-text-secondary border-b border-border/50">
            {filtered.length} of {procs.length} procedures
          </div>
        )}
        <div className="flex-1 overflow-y-auto">
          {filtered.map(p => (
            <button key={p.name} onClick={() => setSelected(p.name)}
              className={cn(
                'w-full text-left px-3 py-2.5 border-b border-border/50 transition-colors group',
                selected === p.name ? 'bg-accent/10' : 'hover:bg-elevated'
              )}>
              <div className={cn('text-xs font-mono font-semibold truncate',
                selected === p.name ? 'text-accent' : 'text-text-primary')}>
                <HighlightMatch text={p.name} query={search} />
              </div>
              <div className="flex items-center gap-1 mt-0.5">
                <span className={cn('text-[9px] px-1 py-0.5 rounded font-semibold',
                  p.language === 'axiomql' ? 'bg-accent/10 text-accent' : 'bg-blue-400/10 text-blue-400')}>
                  {p.language}
                </span>
                <span className="text-[10px] text-text-secondary">{p.args.length} args</span>
                <button
                  onClick={e => { e.stopPropagation(); setConfirmDelete(p.name) }}
                  className="opacity-0 group-hover:opacity-60 hover:!opacity-100 text-[#f85149] transition-all mt-1 ml-auto self-start">
                  <Trash2 className="w-3 h-3" />
                </button>
              </div>
            </button>
          ))}
          {filtered.length === 0 && (
            <div className="px-3 py-4 text-[11px] text-text-secondary text-center">No matches</div>
          )}
        </div>
      </div>

      {/* Editor */}
      <div className="flex-1 min-w-0">
        {proc ? (
          <CodePanel
            title={proc.name}
            language={proc.language}
            body={proc.body}
            args={proc.args}
            onBodyChange={v => setProcs(ps => ps.map(p => p.name === selected ? { ...p, body: v } : p))}
            onSave={() => showToast('Procedure saved')}
          />
        ) : (
          <div className="flex items-center justify-center h-full text-xs text-text-secondary">
            Select a procedure or create a new one
          </div>
        )}
      </div>
    </div>
  )
}

// ── Functions tab ─────────────────────────────────────────────────────────────

function FunctionsTab() {
  const [fns, setFns] = useState<Func[]>(FUNCTIONS)
  const [selected, setSelected] = useState<string>(fns[0]?.name ?? '')
  const [search, setSearch] = useState('')
  const [showCreate, setShowCreate] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null)
  const [returnsValue, setReturnsValue] = useState('TEXT')
  const { show: showToast } = useToast()

  const filtered = search.trim()
    ? fns.filter(f => f.name.toLowerCase().includes(search.toLowerCase()))
    : fns

  const fn = fns.find(f => f.name === selected)

  return (
    <div className="flex h-full">
      {showCreate && (
        <CreateObjectModal
          title="New Function"
          onSave={(name, language, body) => {
            setFns(fs => [...fs, { name, language: language as 'sql' | 'axiomql', args: [], returns: returnsValue || 'VOID', body, createdAt: new Date().toISOString() }])
            setSelected(name)
            setShowCreate(false)
            setReturnsValue('TEXT')
            showToast('Function created')
          }}
          onCancel={() => { setShowCreate(false); setReturnsValue('TEXT') }}
          extraFields={
            <div className="flex flex-col gap-1">
              <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">Returns</label>
              <input value={returnsValue} onChange={e => setReturnsValue(e.target.value)}
                placeholder="TEXT"
                className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-xs font-mono text-[#e6edf3] outline-none focus:border-[#10b981]" />
            </div>
          }
        />
      )}
      {confirmDelete && (
        <ConfirmModal
          title="Delete function"
          message={`Delete function "${confirmDelete}"? This cannot be undone.`}
          onConfirm={() => {
            setFns(fs => fs.filter(f => f.name !== confirmDelete))
            if (selected === confirmDelete) setSelected(fns.find(f => f.name !== confirmDelete)?.name ?? '')
            setConfirmDelete(null)
            showToast('Function deleted')
          }}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      <div className="w-52 border-r border-border flex flex-col shrink-0">
        <div className="px-3 py-2 border-b border-border flex items-center justify-between shrink-0">
          <span className="text-[10px] font-semibold tracking-widest text-text-secondary uppercase">
            Functions ({fns.length})
          </span>
          <button onClick={() => setShowCreate(true)} className="text-text-secondary hover:text-accent transition-colors">
            <Plus className="w-3.5 h-3.5" />
          </button>
        </div>
        <ObjectSearch value={search} onChange={setSearch} placeholder="Search functions…" />
        {search && (
          <div className="px-3 py-1 shrink-0 text-[10px] text-text-secondary border-b border-border/50">
            {filtered.length} of {fns.length} functions
          </div>
        )}
        <div className="flex-1 overflow-y-auto">
          {filtered.map(f => (
            <button key={f.name} onClick={() => setSelected(f.name)}
              className={cn(
                'w-full text-left px-3 py-2.5 border-b border-border/50 transition-colors group',
                selected === f.name ? 'bg-accent/10' : 'hover:bg-elevated'
              )}>
              <div className={cn('text-xs font-mono font-semibold truncate',
                selected === f.name ? 'text-accent' : 'text-text-primary')}>
                <HighlightMatch text={f.name} query={search} />
              </div>
              <div className="flex items-center gap-1 mt-0.5">
                <span className={cn('text-[9px] px-1 py-0.5 rounded font-semibold',
                  f.language === 'axiomql' ? 'bg-accent/10 text-accent' : 'bg-blue-400/10 text-blue-400')}>
                  {f.language}
                </span>
                <span className="text-[10px] text-accent">→ {f.returns}</span>
                <button
                  onClick={e => { e.stopPropagation(); setConfirmDelete(f.name) }}
                  className="opacity-0 group-hover:opacity-60 hover:!opacity-100 text-[#f85149] transition-all mt-1 ml-auto self-start">
                  <Trash2 className="w-3 h-3" />
                </button>
              </div>
            </button>
          ))}
          {filtered.length === 0 && (
            <div className="px-3 py-4 text-[11px] text-text-secondary text-center">No matches</div>
          )}
        </div>
      </div>
      <div className="flex-1 min-w-0">
        {fn ? (
          <CodePanel
            title={fn.name}
            language={fn.language}
            body={fn.body}
            args={fn.args}
            returns={fn.returns}
            onBodyChange={v => setFns(fs => fs.map(f => f.name === selected ? { ...f, body: v } : f))}
            onSave={() => showToast('Function saved')}
          />
        ) : (
          <div className="flex items-center justify-center h-full text-xs text-text-secondary">
            Select a function
          </div>
        )}
      </div>
    </div>
  )
}

// ── Triggers tab ──────────────────────────────────────────────────────────────

function TriggersTab() {
  const [triggers, setTriggers] = useState<Trigger[]>(TRIGGERS)
  const [selected, setSelected] = useState<string>(triggers[0]?.name ?? '')
  const [search, setSearch] = useState('')
  const [showCreate, setShowCreate] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null)
  const [newTriggerTable, setNewTriggerTable] = useState('users')
  const [newTriggerEvent, setNewTriggerEvent] = useState<'INSERT' | 'UPDATE' | 'DELETE'>('INSERT')
  const [newTriggerTiming, setNewTriggerTiming] = useState<'BEFORE' | 'AFTER'>('BEFORE')
  const { show: showToast } = useToast()

  const filtered = search.trim()
    ? triggers.filter(t => t.name.toLowerCase().includes(search.toLowerCase()))
    : triggers

  const trig = triggers.find(t => t.name === selected)

  function toggleEnabled(name: string) {
    setTriggers(ts => ts.map(t => t.name === name ? { ...t, enabled: !t.enabled } : t))
  }

  const eventColor = { INSERT: 'bg-accent/10 text-accent', UPDATE: 'bg-warning/10 text-warning', DELETE: 'bg-error/10 text-error' }
  const timingColor = { BEFORE: 'text-blue-400', AFTER: 'text-purple-400' }

  return (
    <div className="flex h-full">
      {showCreate && (
        <CreateObjectModal
          title="New Trigger"
          onSave={(name, language, body) => {
            setTriggers(ts => [...ts, {
              name, language: language as 'sql' | 'axiomql', body,
              table: newTriggerTable || 'users',
              event: newTriggerEvent || 'INSERT',
              timing: newTriggerTiming || 'BEFORE',
              enabled: true,
              createdAt: new Date().toISOString(),
            }])
            setSelected(name)
            setShowCreate(false)
            showToast('Trigger created')
          }}
          onCancel={() => setShowCreate(false)}
          extraFields={
            <>
              <div className="flex flex-col gap-1">
                <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">Table</label>
                <select value={newTriggerTable} onChange={e => setNewTriggerTable(e.target.value)}
                  className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-xs font-mono text-[#e6edf3] outline-none focus:border-[#10b981]">
                  {['users', 'orders', 'products', 'categories'].map(t => (
                    <option key={t} value={t}>{t}</option>
                  ))}
                </select>
              </div>
              <div className="flex gap-2">
                <div className="flex flex-col gap-1 flex-1">
                  <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">Event</label>
                  <select value={newTriggerEvent} onChange={e => setNewTriggerEvent(e.target.value as 'INSERT' | 'UPDATE' | 'DELETE')}
                    className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-xs font-mono text-[#e6edf3] outline-none focus:border-[#10b981]">
                    {['INSERT', 'UPDATE', 'DELETE'].map(ev => (
                      <option key={ev} value={ev}>{ev}</option>
                    ))}
                  </select>
                </div>
                <div className="flex flex-col gap-1 flex-1">
                  <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">Timing</label>
                  <select value={newTriggerTiming} onChange={e => setNewTriggerTiming(e.target.value as 'BEFORE' | 'AFTER')}
                    className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-xs font-mono text-[#e6edf3] outline-none focus:border-[#10b981]">
                    {['BEFORE', 'AFTER'].map(t => (
                      <option key={t} value={t}>{t}</option>
                    ))}
                  </select>
                </div>
              </div>
            </>
          }
        />
      )}
      {confirmDelete && (
        <ConfirmModal
          title="Delete trigger"
          message={`Delete trigger "${confirmDelete}"? This cannot be undone.`}
          onConfirm={() => {
            setTriggers(ts => ts.filter(t => t.name !== confirmDelete))
            if (selected === confirmDelete) setSelected(triggers.find(t => t.name !== confirmDelete)?.name ?? '')
            setConfirmDelete(null)
            showToast('Trigger deleted')
          }}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      <div className="w-52 border-r border-border flex flex-col shrink-0">
        <div className="px-3 py-2 border-b border-border flex items-center justify-between shrink-0">
          <span className="text-[10px] font-semibold tracking-widest text-text-secondary uppercase">
            Triggers ({triggers.length})
          </span>
          <button onClick={() => setShowCreate(true)} className="text-text-secondary hover:text-accent transition-colors">
            <Plus className="w-3.5 h-3.5" />
          </button>
        </div>
        <ObjectSearch value={search} onChange={setSearch} placeholder="Search triggers…" />
        {search && (
          <div className="px-3 py-1 shrink-0 text-[10px] text-text-secondary border-b border-border/50">
            {filtered.length} of {triggers.length} triggers
          </div>
        )}
        <div className="flex-1 overflow-y-auto">
          {filtered.map(t => (
            <button key={t.name} onClick={() => setSelected(t.name)}
              className={cn(
                'w-full text-left px-3 py-2.5 border-b border-border/50 transition-colors group',
                selected === t.name ? 'bg-accent/10' : 'hover:bg-elevated',
                !t.enabled && 'opacity-50'
              )}>
              <div className={cn('text-xs font-mono font-semibold truncate',
                selected === t.name ? 'text-accent' : 'text-text-primary')}>
                <HighlightMatch text={t.name} query={search} />
              </div>
              <div className="flex items-center gap-1 mt-0.5 flex-wrap">
                <span className={cn('text-[9px] px-1 py-0.5 rounded font-semibold', eventColor[t.event])}>
                  {t.event}
                </span>
                <span className={cn('text-[9px] font-semibold', timingColor[t.timing])}>
                  {t.timing}
                </span>
                <span className="text-[10px] text-text-secondary font-mono">{t.table}</span>
                <button
                  onClick={e => { e.stopPropagation(); setConfirmDelete(t.name) }}
                  className="opacity-0 group-hover:opacity-60 hover:!opacity-100 text-[#f85149] transition-all mt-1 ml-auto self-start">
                  <Trash2 className="w-3 h-3" />
                </button>
              </div>
            </button>
          ))}
          {filtered.length === 0 && (
            <div className="px-3 py-4 text-[11px] text-text-secondary text-center">No matches</div>
          )}
        </div>
      </div>
      <div className="flex-1 min-w-0 flex flex-col">
        {trig ? (
          <>
            {/* Trigger meta bar */}
            <div className="border-b border-border px-4 py-2 flex items-center gap-3 shrink-0 bg-elevated">
              <span className="text-xs font-mono text-text-secondary">ON</span>
              <span className="text-xs font-mono font-semibold text-text-primary">{trig.table}</span>
              <span className={cn('text-xs font-semibold px-1.5 py-0.5 rounded', eventColor[trig.event])}>{trig.event}</span>
              <span className={cn('text-xs font-semibold', timingColor[trig.timing])}>{trig.timing}</span>
              <div className="ml-auto flex items-center gap-2">
                <span className="text-[11px] text-text-secondary">{trig.enabled ? 'Enabled' : 'Disabled'}</span>
                <button onClick={() => toggleEnabled(trig.name)} className="transition-colors">
                  {trig.enabled
                    ? <ToggleRight className="w-5 h-5 text-accent" />
                    : <ToggleLeft className="w-5 h-5 text-text-secondary" />
                  }
                </button>
              </div>
            </div>
            <div className="flex-1 min-h-0">
              <CodePanel
                title={trig.name}
                language={trig.language}
                body={trig.body}
                onBodyChange={v => setTriggers(ts => ts.map(t => t.name === selected ? { ...t, body: v } : t))}
                onSave={() => showToast('Trigger saved')}
              />
            </div>
          </>
        ) : (
          <div className="flex items-center justify-center h-full text-xs text-text-secondary">
            Select a trigger
          </div>
        )}
      </div>
    </div>
  )
}

// ── Sequences tab ─────────────────────────────────────────────────────────────

function SequencesTab() {
  const [seqs, setSeqs] = useState<Sequence[]>(SEQUENCES)
  const [showCreate, setShowCreate] = useState(false)
  const [confirmDrop, setConfirmDrop] = useState<string | null>(null)
  const [newSeq, setNewSeq] = useState({ name: '', start: '1', step: '1', min: '1', max: '', cycle: false })
  const { show: showToast } = useToast()

  return (
    <div className="overflow-auto p-4">
      {showCreate && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
          <div className="bg-[#161b22] border border-[#30363d] rounded-lg p-5 w-80 shadow-xl flex flex-col gap-3">
            <h3 className="text-sm font-semibold text-[#e6edf3]">New Sequence</h3>
            {[
              { key: 'name', label: 'Name', placeholder: 'seq_name' },
              { key: 'start', label: 'Start', placeholder: '1' },
              { key: 'step', label: 'Step', placeholder: '1' },
              { key: 'min', label: 'Min', placeholder: '1' },
              { key: 'max', label: 'Max (empty = ∞)', placeholder: '' },
            ].map(f => (
              <div key={f.key} className="flex flex-col gap-1">
                <label className="text-[10px] text-[#8b949e] uppercase tracking-wider">{f.label}</label>
                <input
                  value={newSeq[f.key as keyof typeof newSeq] as string}
                  onChange={e => setNewSeq(prev => ({ ...prev, [f.key]: e.target.value }))}
                  placeholder={f.placeholder}
                  className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-xs font-mono text-[#e6edf3] outline-none focus:border-[#10b981]" />
              </div>
            ))}
            <div className="flex items-center gap-2">
              <button onClick={() => setNewSeq(prev => ({ ...prev, cycle: !prev.cycle }))}
                className={cn('text-[10px] px-2 py-1 rounded font-semibold border transition-colors',
                  newSeq.cycle ? 'border-[#10b981] text-[#10b981] bg-[#10b981]/10' : 'border-[#30363d] text-[#8b949e]')}>
                {newSeq.cycle ? 'CYCLE' : 'NO CYCLE'}
              </button>
            </div>
            <div className="flex justify-end gap-2">
              <button onClick={() => { setShowCreate(false); setNewSeq({ name: '', start: '1', step: '1', min: '1', max: '', cycle: false }) }}
                className="px-3 py-1.5 rounded text-xs text-[#8b949e] border border-[#30363d] hover:bg-[#21262d] transition-colors">
                Cancel
              </button>
              <button
                disabled={!newSeq.name.trim()}
                onClick={() => {
                  if (!newSeq.name.trim()) return
                  setSeqs(ss => [...ss, {
                    name: newSeq.name.trim(),
                    current: Number(newSeq.start) || 1,
                    start: Number(newSeq.start) || 1,
                    step: Number(newSeq.step) || 1,
                    min: Number(newSeq.min) || 1,
                    max: newSeq.max ? Number(newSeq.max) : null,
                    cycle: newSeq.cycle,
                  }])
                  showToast('Sequence created')
                  setShowCreate(false)
                  setNewSeq({ name: '', start: '1', step: '1', min: '1', max: '', cycle: false })
                }}
                className="px-3 py-1.5 rounded text-xs font-semibold bg-[#10b981] text-white hover:bg-[#10b981]/80 disabled:opacity-40 transition-colors">
                Create
              </button>
            </div>
          </div>
        </div>
      )}
      {confirmDrop && (
        <ConfirmModal
          title="Drop sequence"
          message={`Drop sequence "${confirmDrop}"? This cannot be undone.`}
          onConfirm={() => {
            setSeqs(ss => ss.filter(s => s.name !== confirmDrop))
            setConfirmDrop(null)
            showToast('Sequence dropped')
          }}
          onCancel={() => setConfirmDrop(null)}
        />
      )}
      <div className="flex items-center justify-between mb-3">
        <span className="text-xs font-semibold text-text-secondary uppercase tracking-wider">
          Sequences ({seqs.length})
        </span>
        <button onClick={() => setShowCreate(true)} className="flex items-center gap-1 text-xs text-text-secondary hover:text-accent transition-colors">
          <Plus className="w-3.5 h-3.5" />
          New sequence
        </button>
      </div>
      <table className="w-full text-xs">
        <thead>
          <tr className="border-b border-border">
            {['Name', 'Current', 'Start', 'Step', 'Min', 'Max', 'Cycle', ''].map(h => (
              <th key={h} className="text-left px-3 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {seqs.map(seq => (
            <tr key={seq.name} className="border-b border-border/50 hover:bg-elevated transition-colors group">
              <td className="px-3 py-2.5 font-mono font-semibold text-text-primary">{seq.name}</td>
              <td className="px-3 py-2.5 font-mono text-accent">{seq.current.toLocaleString()}</td>
              <td className="px-3 py-2.5 font-mono text-text-secondary">{seq.start.toLocaleString()}</td>
              <td className="px-3 py-2.5 font-mono text-text-secondary">{seq.step}</td>
              <td className="px-3 py-2.5 font-mono text-text-secondary">{seq.min.toLocaleString()}</td>
              <td className="px-3 py-2.5 font-mono text-text-secondary">{seq.max?.toLocaleString() ?? '∞'}</td>
              <td className="px-3 py-2.5">
                <span className={cn('text-[10px] px-1.5 py-0.5 rounded font-semibold',
                  seq.cycle ? 'bg-accent/10 text-accent' : 'bg-border text-text-secondary')}>
                  {seq.cycle ? 'YES' : 'NO'}
                </span>
              </td>
              <td className="px-3 py-2.5">
                <div className="flex items-center gap-2 opacity-0 group-hover:opacity-100 transition-opacity">
                  <button
                    onClick={() => {
                      setSeqs(ss => ss.map(s => s.name === seq.name ? { ...s, current: s.current + s.step } : s))
                      showToast(`${seq.name}: next = ${seq.current + seq.step}`)
                    }}
                    className="text-xs text-text-secondary hover:text-accent transition-colors">Next val</button>
                  <button
                    onClick={() => setConfirmDrop(seq.name)}
                    className="text-xs text-text-secondary hover:text-error transition-colors">Drop</button>
                </div>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

// ── Main page ─────────────────────────────────────────────────────────────────

const TABS: { id: Tab; label: string; count: number }[] = [
  { id: 'procedures', label: 'Procedures', count: PROCEDURES.length },
  { id: 'functions',  label: 'Functions',  count: FUNCTIONS.length },
  { id: 'triggers',   label: 'Triggers',   count: TRIGGERS.length },
  { id: 'sequences',  label: 'Sequences',  count: SEQUENCES.length },
]

export default function ObjectsPage() {
  const [tab, setTab] = useState<Tab>('procedures')

  return (
    <div className="flex flex-col h-full overflow-hidden">
      <div className="border-b border-border px-6 py-3 flex items-center justify-between shrink-0">
        <div>
          <h1 className="text-sm font-semibold text-text-primary">Database Objects</h1>
          <p className="text-[11px] text-text-secondary mt-0.5">
            Procedures · Functions · Triggers · Sequences
          </p>
        </div>
        <div className="flex items-center gap-1 bg-elevated rounded p-0.5">
          {TABS.map(t => (
            <button key={t.id} onClick={() => setTab(t.id)}
              className={cn(
                'flex items-center gap-1.5 px-3 py-1 text-xs rounded font-medium transition-colors',
                tab === t.id ? 'bg-surface text-text-primary shadow-sm' : 'text-text-secondary hover:text-text-primary'
              )}>
              {t.label}
              <span className={cn('text-[9px] px-1 py-0.5 rounded',
                tab === t.id ? 'bg-accent/20 text-accent' : 'bg-border text-text-secondary')}>
                {t.count}
              </span>
            </button>
          ))}
        </div>
      </div>

      <div className="flex-1 overflow-hidden">
        {tab === 'procedures' && <ProceduresTab />}
        {tab === 'functions'  && <FunctionsTab />}
        {tab === 'triggers'   && <TriggersTab />}
        {tab === 'sequences'  && <SequencesTab />}
      </div>
    </div>
  )
}
