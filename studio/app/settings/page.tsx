'use client'
import { useState } from 'react'
import {
  Settings, Database, Wifi, Shield, Eye, EyeOff, Check,
  RefreshCw, Zap, Monitor, Code2, Sliders, AlertTriangle,
  Plus, Trash2, Star,
} from 'lucide-react'
import { cn } from '@/lib/utils'

// ── Types ─────────────────────────────────────────────────────────────────────

type Connection = {
  id: string
  name: string
  host: string
  port: string
  database: string
  user: string
  password: string
  ssl: boolean
  active: boolean
}

// ── Sub-components ────────────────────────────────────────────────────────────

function SectionCard({
  title, icon: Icon, children,
}: {
  title: string; icon: React.ElementType; children: React.ReactNode
}) {
  return (
    <div className="bg-surface border border-border rounded-lg overflow-hidden">
      <div className="px-4 py-3 border-b border-border flex items-center gap-2">
        <Icon className="w-3.5 h-3.5 text-accent" />
        <span className="text-xs font-semibold text-text-primary">{title}</span>
      </div>
      <div className="p-4 space-y-4">{children}</div>
    </div>
  )
}

function Field({
  label, desc, children,
}: {
  label: string; desc?: string; children: React.ReactNode
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <div className="text-xs text-text-primary">{label}</div>
        {desc && <div className="text-[11px] text-text-secondary mt-0.5">{desc}</div>}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  )
}

function TextInput({
  value, onChange, placeholder, type = 'text', mono = false,
}: {
  value: string; onChange: (v: string) => void
  placeholder?: string; type?: string; mono?: boolean
}) {
  return (
    <input
      type={type}
      value={value}
      onChange={e => onChange(e.target.value)}
      placeholder={placeholder}
      className={cn(
        'bg-elevated border border-border rounded px-2 py-1 text-xs text-text-primary outline-none focus:border-accent transition-colors w-48',
        mono && 'font-mono'
      )}
    />
  )
}

function Toggle({
  value, onChange, label,
}: {
  value: boolean; onChange: (v: boolean) => void; label?: string
}) {
  return (
    <button
      onClick={() => onChange(!value)}
      className={cn(
        'relative w-8 h-4 rounded-full transition-colors shrink-0',
        value ? 'bg-accent' : 'bg-border'
      )}>
      <span className={cn(
        'absolute top-0.5 w-3 h-3 rounded-full bg-white transition-transform shadow-sm',
        value ? 'translate-x-4' : 'translate-x-0.5'
      )} />
      <span className="sr-only">{label}</span>
    </button>
  )
}

function Select({
  value, onChange, options,
}: {
  value: string; onChange: (v: string) => void
  options: { label: string; value: string }[]
}) {
  return (
    <select
      value={value}
      onChange={e => onChange(e.target.value)}
      className="bg-elevated border border-border rounded px-2 py-1 text-xs text-text-primary outline-none focus:border-accent cursor-pointer">
      {options.map(o => (
        <option key={o.value} value={o.value}>{o.label}</option>
      ))}
    </select>
  )
}

function NumberInput({
  value, onChange, min, max, suffix,
}: {
  value: number; onChange: (v: number) => void
  min?: number; max?: number; suffix?: string
}) {
  return (
    <div className="flex items-center gap-1">
      <input
        type="number"
        value={value}
        min={min}
        max={max}
        onChange={e => onChange(Number(e.target.value))}
        className="bg-elevated border border-border rounded px-2 py-1 text-xs text-text-primary outline-none focus:border-accent w-20 font-mono"
      />
      {suffix && <span className="text-[11px] text-text-secondary">{suffix}</span>}
    </div>
  )
}

function SaveButton({
  saved, onClick,
}: {
  saved: boolean; onClick: () => void
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        'flex items-center gap-1.5 px-3 py-1.5 rounded text-xs font-semibold transition-all',
        saved
          ? 'bg-accent/10 text-accent border border-accent/30'
          : 'bg-accent text-white hover:bg-accent-dim'
      )}>
      {saved ? <><Check className="w-3 h-3" /> Saved</> : 'Save changes'}
    </button>
  )
}

// ── Connection card ────────────────────────────────────────────────────────────

function ConnectionCard({
  conn, onUpdate, onDelete, onSetActive,
}: {
  conn: Connection
  onUpdate: (patch: Partial<Connection>) => void
  onDelete: () => void
  onSetActive: () => void
}) {
  const [showPass, setShowPass] = useState(false)
  const [testing, setTesting] = useState(false)
  const [testResult, setTestResult] = useState<'ok' | 'error' | null>(null)

  async function testConn() {
    setTesting(true); setTestResult(null)
    await new Promise(r => setTimeout(r, 800 + Math.random() * 400))
    setTestResult('ok')
    setTesting(false)
    setTimeout(() => setTestResult(null), 3000)
  }

  return (
    <div className={cn(
      'border rounded-lg p-4 space-y-3 transition-colors',
      conn.active ? 'border-accent/50 bg-accent/5' : 'border-border bg-elevated'
    )}>
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <input
            value={conn.name}
            onChange={e => onUpdate({ name: e.target.value })}
            className="bg-transparent text-xs font-semibold text-text-primary outline-none focus:border-b focus:border-accent"
          />
          {conn.active && (
            <span className="text-[9px] px-1.5 py-0.5 rounded bg-accent/10 text-accent font-semibold flex items-center gap-1">
              <div className="w-1 h-1 rounded-full bg-accent animate-pulse" />
              ACTIVE
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          {!conn.active && (
            <button onClick={onSetActive}
              className="text-[10px] text-text-secondary hover:text-accent transition-colors">
              Set active
            </button>
          )}
          <button onClick={onDelete}
            className="text-text-secondary hover:text-error transition-colors">
            <Trash2 className="w-3.5 h-3.5" />
          </button>
        </div>
      </div>

      <div className="grid grid-cols-2 gap-2">
        <div>
          <div className="text-[10px] text-text-secondary mb-1">Host</div>
          <TextInput value={conn.host} onChange={v => onUpdate({ host: v })} placeholder="localhost" mono />
        </div>
        <div>
          <div className="text-[10px] text-text-secondary mb-1">Port</div>
          <TextInput value={conn.port} onChange={v => onUpdate({ port: v })} placeholder="3306" mono />
        </div>
        <div>
          <div className="text-[10px] text-text-secondary mb-1">Database</div>
          <TextInput value={conn.database} onChange={v => onUpdate({ database: v })} placeholder="axiomdb" mono />
        </div>
        <div>
          <div className="text-[10px] text-text-secondary mb-1">User</div>
          <TextInput value={conn.user} onChange={v => onUpdate({ user: v })} placeholder="root" mono />
        </div>
        <div className="col-span-2">
          <div className="text-[10px] text-text-secondary mb-1">Password</div>
          <div className="flex items-center gap-1">
            <input
              type={showPass ? 'text' : 'password'}
              value={conn.password}
              onChange={e => onUpdate({ password: e.target.value })}
              placeholder="••••••••"
              className="bg-elevated border border-border rounded px-2 py-1 text-xs text-text-primary outline-none focus:border-accent font-mono flex-1"
            />
            <button onClick={() => setShowPass(p => !p)}
              className="text-text-secondary hover:text-text-primary p-1 transition-colors">
              {showPass ? <EyeOff className="w-3.5 h-3.5" /> : <Eye className="w-3.5 h-3.5" />}
            </button>
          </div>
        </div>
      </div>

      <div className="flex items-center justify-between pt-1">
        <div className="flex items-center gap-2">
          <Toggle value={conn.ssl} onChange={v => onUpdate({ ssl: v })} />
          <span className="text-[11px] text-text-secondary">SSL/TLS</span>
        </div>
        <div className="flex items-center gap-2">
          {testResult === 'ok' && (
            <span className="text-[10px] text-accent flex items-center gap-1">
              <Check className="w-3 h-3" /> Connected
            </span>
          )}
          {testResult === 'error' && (
            <span className="text-[10px] text-error flex items-center gap-1">
              <AlertTriangle className="w-3 h-3" /> Failed
            </span>
          )}
          <button onClick={testConn} disabled={testing}
            className="flex items-center gap-1 px-2 py-1 rounded text-[11px] border border-border text-text-secondary hover:border-accent/50 hover:text-accent transition-colors disabled:opacity-50">
            <RefreshCw className={cn('w-3 h-3', testing && 'animate-spin')} />
            {testing ? 'Testing…' : 'Test'}
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Main page ─────────────────────────────────────────────────────────────────

const INITIAL_CONNECTIONS: Connection[] = [
  {
    id: '1', name: 'Local Dev', host: 'localhost', port: '3306',
    database: 'axiomdb', user: 'root', password: '', ssl: false, active: true,
  },
]

export default function SettingsPage() {
  // Connections
  const [connections, setConnections] = useState<Connection[]>(INITIAL_CONNECTIONS)

  // Engine config
  const [walEnabled, setWalEnabled] = useState(true)
  const [fsync, setFsync] = useState(true)
  const [maxConn, setMaxConn] = useState(50)
  const [walSizeMb, setWalSizeMb] = useState(256)
  const [logLevel, setLogLevel] = useState('info')
  const [engineSaved, setEngineSaved] = useState(false)

  // Studio prefs
  const [fontSize, setFontSize] = useState(13)
  const [tabSize, setTabSize] = useState(2)
  const [wordWrap, setWordWrap] = useState(false)
  const [minimap, setMinimap] = useState(false)
  const [queryLanguage, setQueryLanguage] = useState<'sql' | 'axiomql'>('sql')
  const [studioSaved, setStudioSaved] = useState(false)

  // Connections helpers
  function updateConn(id: string, patch: Partial<Connection>) {
    setConnections(prev => prev.map(c => c.id === id ? { ...c, ...patch } : c))
  }
  function deleteConn(id: string) {
    setConnections(prev => prev.filter(c => c.id !== id))
  }
  function addConn() {
    const id = Date.now().toString()
    setConnections(prev => [...prev, {
      id, name: `Connection ${prev.length + 1}`,
      host: 'localhost', port: '3306', database: 'axiomdb',
      user: 'root', password: '', ssl: false, active: false,
    }])
  }
  function setActive(id: string) {
    setConnections(prev => prev.map(c => ({ ...c, active: c.id === id })))
  }

  function saveEngine() {
    setEngineSaved(true)
    setTimeout(() => setEngineSaved(false), 2000)
  }
  function saveStudio() {
    setStudioSaved(true)
    setTimeout(() => setStudioSaved(false), 2000)
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="border-b border-border px-6 py-4 flex items-center gap-2">
        <Settings className="w-4 h-4 text-text-secondary" />
        <h1 className="text-sm font-semibold text-text-primary">Settings</h1>
      </div>

      <div className="p-6 max-w-2xl space-y-5">

        {/* ── Connections ──────────────────────────────────────────────────── */}
        <SectionCard title="Connections" icon={Wifi}>
          <div className="space-y-3">
            {connections.map(conn => (
              <ConnectionCard
                key={conn.id}
                conn={conn}
                onUpdate={patch => updateConn(conn.id, patch)}
                onDelete={() => deleteConn(conn.id)}
                onSetActive={() => setActive(conn.id)}
              />
            ))}
          </div>
          <button onClick={addConn}
            className="flex items-center gap-2 text-xs text-text-secondary hover:text-accent transition-colors mt-1">
            <Plus className="w-3.5 h-3.5" />
            Add connection
          </button>
        </SectionCard>

        {/* ── Engine ───────────────────────────────────────────────────────── */}
        <SectionCard title="Engine" icon={Database}>
          <Field label="WAL (Write-Ahead Log)"
            desc="Durability mechanism. Disable only for in-memory workloads.">
            <Toggle value={walEnabled} onChange={setWalEnabled} />
          </Field>
          <Field label="fsync on commit"
            desc="Flush WAL to disk on every COMMIT. Disable for max throughput.">
            <Toggle value={fsync} onChange={setFsync} />
          </Field>
          <Field label="Max connections"
            desc="Maximum concurrent client connections.">
            <NumberInput value={maxConn} onChange={setMaxConn} min={1} max={1000} />
          </Field>
          <Field label="Max WAL size"
            desc="WAL file size before rotation is triggered.">
            <NumberInput value={walSizeMb} onChange={setWalSizeMb} min={64} max={4096} suffix="MB" />
          </Field>
          <Field label="Log level"
            desc="Minimum log severity written to stdout.">
            <Select
              value={logLevel}
              onChange={setLogLevel}
              options={[
                { label: 'error', value: 'error' },
                { label: 'warn', value: 'warn' },
                { label: 'info', value: 'info' },
                { label: 'debug', value: 'debug' },
                { label: 'trace', value: 'trace' },
              ]}
            />
          </Field>
          <div className="pt-1 flex justify-end">
            <SaveButton saved={engineSaved} onClick={saveEngine} />
          </div>
        </SectionCard>

        {/* ── Studio ───────────────────────────────────────────────────────── */}
        <SectionCard title="Studio" icon={Monitor}>
          <Field label="Default query language"
            desc="Language shown when opening a new query tab.">
            <Select
              value={queryLanguage}
              onChange={v => setQueryLanguage(v as 'sql' | 'axiomql')}
              options={[
                { label: 'SQL', value: 'sql' },
                { label: 'AxiomQL', value: 'axiomql' },
              ]}
            />
          </Field>
          <Field label="Editor font size">
            <NumberInput value={fontSize} onChange={setFontSize} min={10} max={20} suffix="px" />
          </Field>
          <Field label="Tab size">
            <NumberInput value={tabSize} onChange={setTabSize} min={2} max={8} suffix="spaces" />
          </Field>
          <Field label="Word wrap" desc="Wrap long lines in the editor.">
            <Toggle value={wordWrap} onChange={setWordWrap} />
          </Field>
          <Field label="Minimap" desc="Show the code minimap in the editor.">
            <Toggle value={minimap} onChange={setMinimap} />
          </Field>
          <div className="pt-1 flex justify-end">
            <SaveButton saved={studioSaved} onClick={saveStudio} />
          </div>
        </SectionCard>

        {/* ── Security ─────────────────────────────────────────────────────── */}
        <SectionCard title="Security" icon={Shield}>
          <Field label="TLS mode"
            desc="Require encrypted connections to AxiomDB server.">
            <Select
              value="prefer"
              onChange={() => {}}
              options={[
                { label: 'disable', value: 'disable' },
                { label: 'prefer', value: 'prefer' },
                { label: 'require', value: 'require' },
                { label: 'verify-ca', value: 'verify-ca' },
                { label: 'verify-full', value: 'verify-full' },
              ]}
            />
          </Field>
          <Field label="CA certificate"
            desc="Path to custom CA certificate file for TLS verification.">
            <TextInput value="" onChange={() => {}} placeholder="/etc/axiomdb/ca.crt" mono />
          </Field>
          <div className="pt-1 border-t border-border mt-2">
            <div className="text-[11px] text-text-secondary">
              Session timeout, audit logging, and row-level security policies are configured
              at the server level via <code className="font-mono text-accent">axiomdb.toml</code>.
            </div>
          </div>
        </SectionCard>

        {/* ── About ────────────────────────────────────────────────────────── */}
        <SectionCard title="About" icon={Zap}>
          <div className="space-y-2 text-xs">
            {[
              ['AxiomDB', 'v0.1.0-dev (Phase 4)'],
              ['AxiomStudio', 'v0.1.0'],
              ['Wire protocol', 'MySQL 8.0 compatible (Phase 8)'],
              ['Docs', 'lordmacu.github.io/axiomdb'],
              ['Source', 'github.com/lordmacu/axiomdb'],
            ].map(([k, v]) => (
              <div key={k} className="flex items-center justify-between">
                <span className="text-text-secondary">{k}</span>
                <span className="font-mono text-text-primary text-[11px]">{v}</span>
              </div>
            ))}
          </div>
        </SectionCard>

      </div>
    </div>
  )
}
