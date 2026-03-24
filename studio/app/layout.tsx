import type { Metadata } from 'next'
import { GeistSans } from 'geist/font/sans'
import { GeistMono } from 'geist/font/mono'
import './globals.css'
import { Sidebar } from '@/components/sidebar'
import { ClientProviders } from '@/components/client-providers'

export const metadata: Metadata = {
  title: 'AxiomStudio',
  description: 'Database admin for AxiomDB',
}

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={`${GeistSans.variable} ${GeistMono.variable}`}>
      <body className="bg-bg text-text-primary antialiased flex h-screen overflow-hidden">
        <Sidebar />
        <main className="flex-1 flex flex-col overflow-hidden">
          {children}
        </main>
        <ClientProviders />
      </body>
    </html>
  )
}
