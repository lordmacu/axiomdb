import { Settings, Database, Wifi, Shield } from 'lucide-react'

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="bg-surface border border-border rounded-lg">
      <div className="px-4 py-3 border-b border-border">
        <span className="text-xs font-semibold text-text-primary">{title}</span>
      </div>
      <div className="p-4 space-y-3">{children}</div>
    </div>
  )
}

function Field({ label, value, desc }: { label: string; value: string; desc?: string }) {
  return (
    <div className="flex items-start justify-between">
      <div>
        <div className="text-xs text-text-primary">{label}</div>
        {desc && <div className="text-[11px] text-text-secondary mt-0.5">{desc}</div>}
      </div>
      <div className="text-xs font-mono text-text-secondary bg-elevated border border-border rounded px-2 py-1">
        {value}
      </div>
    </div>
  )
}

export default function SettingsPage() {
  return (
    <div className="flex-1 overflow-y-auto">
      <div className="border-b border-border px-6 py-4 flex items-center gap-2">
        <Settings className="w-4 h-4 text-text-secondary" />
        <h1 className="text-sm font-semibold text-text-primary">Settings</h1>
      </div>
      <div className="p-6 max-w-2xl space-y-4">
        <Section title="Connection">
          <Field label="Host" value="localhost" desc="Database server hostname" />
          <Field label="Port" value="3306" desc="TCP port" />
          <Field label="Database" value="axiomdb" />
          <Field label="User" value="admin" />
        </Section>
        <Section title="Engine">
          <Field label="Version" value="AxiomDB v0.1.0" />
          <Field label="Buffer pool" value="256 MB" desc="In-memory page cache size" />
          <Field label="WAL mode" value="ENABLED" />
          <Field label="Max connections" value="50" />
        </Section>
        <Section title="Studio">
          <Field label="Theme" value="Dark" />
          <Field label="Font size" value="13px" desc="Editor font size" />
          <Field label="Tab size" value="2" />
        </Section>
      </div>
    </div>
  )
}
