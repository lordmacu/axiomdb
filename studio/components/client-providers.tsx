'use client'
import { CommandPalette } from './command-palette'
import { ToastProvider } from './toast'

export function ClientProviders({ children }: { children?: React.ReactNode }) {
  return (
    <ToastProvider>
      {children}
      <CommandPalette />
    </ToastProvider>
  )
}
