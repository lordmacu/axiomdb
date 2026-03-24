'use client'
import { useEffect } from 'react'
import { Keyboard } from 'lucide-react'

type ShortcutRow = {
  keys: string[]
  description: string
  comingSoon?: boolean
}

type Section = {
  title: string
  shortcuts: ShortcutRow[]
}

const SECTIONS: Section[] = [
  {
    title: 'Global',
    shortcuts: [
      { keys: ['⌘', 'K'],       description: 'Open command palette' },
      { keys: ['⌘', '/'],       description: 'Toggle keyboard shortcuts' },
    ],
  },
  {
    title: 'Query Editor',
    shortcuts: [
      { keys: ['⌘', '↵'],      description: 'Run query' },
      { keys: ['⌘', '⇧', 'F'], description: 'Format SQL' },
      { keys: ['⌘', '⇧', 'T'], description: 'New tab' },
      { keys: ['⌘', 'W'],      description: 'Close tab' },
      { keys: ['⌘', '⇧', 'H'], description: 'Toggle history' },
      { keys: ['⌘', '⇧', 'S'], description: 'Toggle saved queries' },
      { keys: ['⌘', '⇧', 'V'], description: 'Toggle variables panel' },
      { keys: ['⌘', '⇧', 'E'], description: 'Export CSV' },
      { keys: ['⌘', '⇧', 'L'], description: 'Split view toggle' },
    ],
  },
  {
    title: 'Table Detail',
    shortcuts: [
      { keys: ['⌘', 'F'],      description: 'Filter rows' },
      { keys: ['⌘', '⇧', 'C'], description: 'Copy DDL' },
      { keys: ['⌘', '⇧', 'X'], description: 'Export SQL' },
      { keys: ['↵'],           description: 'Start editing cell' },
      { keys: ['Escape'],      description: 'Cancel edit' },
      { keys: ['⌘', 'Z'],      description: 'Undo edit', comingSoon: true },
    ],
  },
]

function Key({ k }: { k: string }) {
  return (
    <kbd className="inline-flex items-center justify-center min-w-[22px] h-[22px] px-1.5 rounded
      bg-elevated border border-border text-[10px] font-semibold text-text-secondary font-mono
      shadow-[inset_0_-1px_0_0_#30363d]">
      {k}
    </kbd>
  )
}

export default function ShortcutsPage() {
  // ⌘/ opens this page (handled globally) — re-export close handler if needed
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key === '/') {
        e.preventDefault()
        window.history.back()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  return (
    <div className="flex flex-col h-full overflow-hidden">
      <div className="border-b border-border px-6 py-3 flex items-center gap-2 shrink-0">
        <Keyboard className="w-4 h-4 text-accent" />
        <h1 className="text-sm font-semibold text-text-primary">Keyboard Shortcuts</h1>
        <span className="text-xs text-text-secondary ml-1">Press ⌘/ to return</span>
      </div>

      <div className="flex-1 overflow-y-auto p-6">
        <div className="max-w-2xl space-y-6">
          {SECTIONS.map(section => (
            <div key={section.title} className="bg-surface border border-border rounded-lg overflow-hidden">
              <div className="px-4 py-2.5 border-b border-border bg-elevated/50">
                <span className="text-[10px] font-semibold tracking-widest text-text-secondary uppercase">
                  {section.title}
                </span>
              </div>
              <div className="divide-y divide-border/50">
                {section.shortcuts.map((s, i) => (
                  <div key={i} className="flex items-center justify-between px-4 py-2.5 hover:bg-elevated/40 transition-colors">
                    <span className={`text-xs ${s.comingSoon ? 'text-text-secondary/50' : 'text-text-primary'}`}>
                      {s.description}
                      {s.comingSoon && (
                        <span className="ml-2 text-[9px] px-1.5 py-0.5 rounded bg-border text-text-secondary font-semibold">
                          SOON
                        </span>
                      )}
                    </span>
                    <div className="flex items-center gap-1">
                      {s.keys.map((k, ki) => (
                        <Key key={ki} k={k} />
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  )
}
