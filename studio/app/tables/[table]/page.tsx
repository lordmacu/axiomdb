import TableDetail from './TableDetail'

export function generateStaticParams() {
  return [
    { table: 'users' },
    { table: 'orders' },
    { table: 'products' },
    { table: 'categories' },
    { table: 'active_users' },
  ]
}

export default function Page({ params }: { params: Promise<{ table: string }> }) {
  return <TableDetail params={params} />
}
