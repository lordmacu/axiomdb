import type { Metadata } from 'next'
import { GeistSans } from 'geist/font/sans'
import { GeistMono } from 'geist/font/mono'
import './globals.css'
import { Sidebar } from '@/components/sidebar'
import { ClientProviders } from '@/components/client-providers'
import { Suspense } from 'react'

export const metadata: Metadata = {
  title: 'AxiomStudio',
  description: 'Database admin for AxiomDB',
}

function PageLoader() {
  return (
    <div className="flex-1 flex items-center justify-center">
      <div className="flex items-center gap-2 text-xs text-text-secondary">
        <div className="w-3 h-3 rounded-full border-2 border-accent border-t-transparent animate-spin" />
        Loading…
      </div>
    </div>
  )
}

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={`${GeistSans.variable} ${GeistMono.variable}`}>
      <body className="bg-bg text-text-primary antialiased flex h-screen overflow-hidden">
        <ClientProviders>
          <Sidebar />
          <main className="flex-1 flex flex-col overflow-hidden">
            <Suspense fallback={<PageLoader />}>
              {children}
            </Suspense>
          </main>
        </ClientProviders>
      </body>
    </html>
  )
}
