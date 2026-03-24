import type { NextConfig } from 'next'

const nextConfig: NextConfig = {
  output: 'export',
  basePath: '/axiomdb/studio',
  trailingSlash: true,
  images: { unoptimized: true },
}

export default nextConfig
