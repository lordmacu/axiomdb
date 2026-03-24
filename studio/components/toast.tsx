'use client'
import { createContext, useContext, useState, useCallback } from 'react'
import { Check, AlertTriangle, X, Info } from 'lucide-react'
import { cn } from '@/lib/utils'

type ToastType = 'success' | 'error' | 'warning' | 'info'
type Toast = { id: string; message: string; type: ToastType }
type ToastContextType = { show: (message: string, type?: ToastType) => void }

export const ToastContext = createContext<ToastContextType>({ show: () => {} })
export function useToast() { return useContext(ToastContext) }

export function ToastProvider({ children }: { children: React.ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([])

  const show = useCallback((message: string, type: ToastType = 'success') => {
    const id = crypto.randomUUID()
    setToasts(p => [...p, { id, message, type }])
    setTimeout(() => setToasts(p => p.filter(t => t.id !== id)), 3000)
  }, [])

  const icons = { success: Check, error: X, warning: AlertTriangle, info: Info }
  const colors = {
    success: 'border-[#10b981]/30 bg-[#10b981]/10 text-[#10b981]',
    error:   'border-[#f85149]/30 bg-[#f85149]/10 text-[#f85149]',
    warning: 'border-[#d29922]/30 bg-[#d29922]/10 text-[#d29922]',
    info:    'border-blue-400/30 bg-blue-400/10 text-blue-400',
  }

  return (
    <ToastContext.Provider value={{ show }}>
      {children}
      <div className="fixed bottom-4 right-4 z-50 flex flex-col gap-2 pointer-events-none">
        {toasts.map(t => {
          const Icon = icons[t.type]
          return (
            <div key={t.id}
              className={cn(
                'flex items-center gap-2 px-3 py-2 rounded border text-xs font-medium shadow-lg',
                'animate-in slide-in-from-right-4 duration-200',
                colors[t.type]
              )}>
              <Icon className="w-3.5 h-3.5 shrink-0" />
              {t.message}
            </div>
          )
        })}
      </div>
    </ToastContext.Provider>
  )
}
