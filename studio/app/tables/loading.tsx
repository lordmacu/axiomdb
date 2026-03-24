export default function Loading() {
  return (
    <div className="flex-1 flex items-center justify-center">
      <div className="flex items-center gap-2 text-xs text-text-secondary">
        <div className="w-3 h-3 rounded-full border-2 border-accent border-t-transparent animate-spin" />
        Loading…
      </div>
    </div>
  )
}
