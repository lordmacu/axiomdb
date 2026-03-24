'use client'
import { useEffect } from 'react'

export default function Error({ error, reset }: { error: Error; reset: () => void }) {
  useEffect(() => { console.error(error) }, [error])
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-3">
      <p className="text-xs text-error">Something went wrong</p>
      <button onClick={reset}
        className="text-xs text-text-secondary border border-border rounded px-3 py-1 hover:bg-elevated transition-colors">
        Try again
      </button>
    </div>
  )
}
