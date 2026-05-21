# frozen_string_literal: true
# Benchmark: AxiomDB (Fiddle packed) vs sqlite3 gem (native C ext).
#   ruby bindings/ruby/bench.rb
require_relative 'axiomdb'
require 'sqlite3'
require 'tmpdir'

N = 10_000
SCHEMA = 'CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, active INT, score INT, email TEXT)'

def ins(i)
  format("INSERT INTO t VALUES (%d,'user_%06d',%d,%d,%d,'u%d@b.local')",
         i, i, 18 + i % 62, i.even? ? 1 : 0, 100 + i % 1000, i)
end

def median(a)
  a.sort[a.length / 2]
end

def bench(iters = 11, warm = 3)
  ts = []
  (iters + warm).times do |k|
    t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    yield
    el = (Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0) * 1000.0
    ts << el if k >= warm
  end
  median(ts)
end

Dir.mktmpdir do |dir|
  db = AxiomDB.new(File.join(dir, 'a.db'))
  db.execute(SCHEMA)
  db.begin_txn
  N.times { |i| db.execute(ins(i)) }
  db.commit

  sq = SQLite3::Database.new(File.join(dir, 's.db'))
  sq.execute('PRAGMA journal_mode=WAL')
  sq.execute('PRAGMA synchronous=FULL')
  sq.execute(SCHEMA)
  sq.transaction { N.times { |i| sq.execute(ins(i)) } }

  ref = sq.execute('SELECT * FROM t')
  got = db.query_tuples('SELECT * FROM t')
  puts "correctness: #{got[0] == ref[0] ? 'MATCH' : 'MISMATCH'}"

  s = bench { sq.execute('SELECT * FROM t') }
  a = bench { db.query_tuples('SELECT * FROM t') }
  puts "\n=== Ruby read perf (10K x 6, median of 11) ==="
  puts format('  sqlite3 gem (native C ext): %6.2f ms   1.00x', s)
  puts format('  AxiomDB query_tuples:       %6.2f ms   %.2fx  %s', a, a / s, a < s ? '(FASTER)' : '')
  db.close
  sq.close
end
