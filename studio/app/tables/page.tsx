import Link from 'next/link'
import { Table2, Eye, Clock, HardDrive } from 'lucide-react'
import { TABLES } from '@/lib/mock'

export default function TablesPage() {
  return (
    <div className="flex-1 overflow-y-auto">
      <div className="border-b border-border px-6 py-4">
        <h1 className="text-sm font-semibold text-text-primary">Tables</h1>
        <p className="text-xs text-text-secondary mt-0.5">{TABLES.length} objects</p>
      </div>
      <div className="p-6 grid grid-cols-3 gap-3">
        {TABLES.map(t => (
          <Link key={t.name} href={`/tables/${t.name}`}
            className="bg-surface border border-border rounded-lg p-4 hover:border-accent/50 hover:bg-elevated transition-all group">
            <div className="flex items-start justify-between mb-3">
              <div className="flex items-center gap-2">
                {t.type === 'view'
                  ? <Eye className="w-4 h-4 text-blue-400" />
                  : <Table2 className="w-4 h-4 text-accent" />
                }
                <span className="font-mono text-sm font-semibold text-text-primary group-hover:text-accent transition-colors">
                  {t.name}
                </span>
              </div>
              <span className="text-[10px] px-1.5 py-0.5 rounded bg-elevated text-text-secondary border border-border">
                {t.type}
              </span>
            </div>
            <div className="space-y-1.5">
              <div className="flex items-center gap-1.5 text-xs text-text-secondary">
                <span className="font-mono text-text-primary font-semibold">{t.rows.toLocaleString()}</span>
                <span>rows</span>
              </div>
              <div className="flex items-center gap-3 text-xs text-text-secondary">
                <div className="flex items-center gap-1">
                  <HardDrive className="w-3 h-3" />
                  {t.size}
                </div>
                <div className="flex items-center gap-1">
                  <Clock className="w-3 h-3" />
                  <span className="font-mono text-[10px]">{t.lastUpdated.split(' ')[1]}</span>
                </div>
              </div>
            </div>
          </Link>
        ))}
      </div>
    </div>
  )
}
