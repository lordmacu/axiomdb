import { Activity, Database, Zap, Wifi, Clock, TrendingUp } from 'lucide-react'
import { METRICS, QUERY_LOG, SPARKLINE_DATA } from '@/lib/mock'
import { formatNumber } from '@/lib/utils'

function MetricCard({ icon: Icon, label, value, sub, trend }: {
  icon: React.ComponentType<{ className?: string }>
  label: string
  value: string
  sub?: string
  trend?: string
}) {
  return (
    <div className="bg-surface border border-border rounded-lg p-4 flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <span className="text-xs text-text-secondary">{label}</span>
        <div className="w-6 h-6 rounded flex items-center justify-center bg-accent/10">
          <Icon className="w-3.5 h-3.5 text-accent" />
        </div>
      </div>
      <div>
        <div className="text-2xl font-semibold text-text-primary tracking-tight">{value}</div>
        {sub && <div className="text-xs text-text-secondary mt-0.5">{sub}</div>}
      </div>
      {trend && <div className="text-xs text-accent">{trend}</div>}
    </div>
  )
}

function Sparkline({ data }: { data: number[] }) {
  const max = Math.max(...data)
  const min = Math.min(...data)
  const range = max - min || 1
  const h = 32
  const w = 80
  const points = data.map((v, i) => {
    const x = (i / (data.length - 1)) * w
    const y = h - ((v - min) / range) * h
    return `${x},${y}`
  }).join(' ')
  return (
    <svg width={w} height={h} className="opacity-60">
      <polyline fill="none" stroke="#10b981" strokeWidth="1.5"
        strokeLinejoin="round" strokeLinecap="round" points={points} />
    </svg>
  )
}

export default function Dashboard() {
  return (
    <div className="flex-1 overflow-y-auto">
      {/* Header */}
      <div className="border-b border-border px-6 py-4 flex items-center justify-between">
        <div>
          <h1 className="text-sm font-semibold text-text-primary">Dashboard</h1>
          <p className="text-xs text-text-secondary mt-0.5">AxiomDB v0.1 · localhost:3306</p>
        </div>
        <div className="flex items-center gap-2 text-xs text-accent">
          <div className="w-1.5 h-1.5 rounded-full bg-accent animate-pulse" />
          Connected
        </div>
      </div>

      <div className="p-6 space-y-6">
        {/* Metrics grid */}
        <div className="grid grid-cols-4 gap-3">
          <MetricCard icon={Zap} label="Queries / sec"
            value={formatNumber(METRICS.queriesPerSecond)}
            sub="avg 8.3ms" trend="↑ 12% from yesterday" />
          <MetricCard icon={Wifi} label="Connections"
            value={`${METRICS.activeConnections} / ${METRICS.maxConnections}`}
            sub="24% utilized" />
          <MetricCard icon={Database} label="Database size"
            value={METRICS.dbSize}
            sub={`WAL: ${METRICS.walSize}`} />
          <MetricCard icon={TrendingUp} label="Cache hit rate"
            value={`${METRICS.cacheHitRate}%`}
            sub="Buffer pool" trend="↑ 2.1% this hour" />
        </div>

        {/* Second row */}
        <div className="grid grid-cols-3 gap-3">
          <MetricCard icon={Clock} label="Uptime" value={METRICS.uptime} />
          <div className="bg-surface border border-border rounded-lg p-4">
            <div className="text-xs text-text-secondary mb-2">Query throughput</div>
            <Sparkline data={SPARKLINE_DATA.qps} />
          </div>
          <div className="bg-surface border border-border rounded-lg p-4">
            <div className="text-xs text-text-secondary mb-2">Active connections</div>
            <Sparkline data={SPARKLINE_DATA.connections} />
          </div>
        </div>

        {/* Recent queries */}
        <div className="bg-surface border border-border rounded-lg">
          <div className="px-4 py-3 border-b border-border flex items-center justify-between">
            <span className="text-xs font-semibold text-text-primary">Recent queries</span>
            <span className="text-xs text-text-secondary">last 5</span>
          </div>
          <table className="w-full">
            <thead>
              <tr className="border-b border-border">
                {['Query', 'Duration', 'Rows', 'Time', 'Status'].map(h => (
                  <th key={h} className="text-left px-4 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase">
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {QUERY_LOG.map((q, i) => (
                <tr key={i} className="border-b border-border/50 hover:bg-elevated transition-colors">
                  <td className="px-4 py-2.5 font-mono text-xs text-text-secondary max-w-xs truncate">
                    {q.query}
                  </td>
                  <td className="px-4 py-2.5 text-xs font-mono text-text-primary">
                    {q.duration}ms
                  </td>
                  <td className="px-4 py-2.5 text-xs font-mono text-text-secondary">
                    {q.rows.toLocaleString()}
                  </td>
                  <td className="px-4 py-2.5 text-xs font-mono text-text-secondary">
                    {q.timestamp}
                  </td>
                  <td className="px-4 py-2.5">
                    <span className={`text-[10px] px-1.5 py-0.5 rounded font-semibold ${
                      q.status === 'ok'
                        ? 'bg-accent/10 text-accent'
                        : 'bg-error/10 text-error'
                    }`}>
                      {q.status}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}
