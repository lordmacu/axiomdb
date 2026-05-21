# frozen_string_literal: true

# Correctness tests for the AxiomDB Ruby binding, cross-checked vs the sqlite3
# gem (an independent native engine).
#
#   ruby bindings/ruby/test.rb

require_relative 'axiomdb'
require 'sqlite3'
require 'tmpdir'

failed = 0
def check(name, cond)
  puts((cond ? 'PASS ' : 'FAIL ') + name)
  cond
end

Dir.mktmpdir do |dir|
  # sqlite reference
  sq = SQLite3::Database.new(File.join(dir, 's.db'))
  sq.execute('CREATE TABLE t (id INT, name TEXT, score REAL, m INT)')
  [[1, 'alice', 3.5, nil], [2, 'héllo', 2.25, 99], [3, '日本', 9.0, nil]].each do |r|
    sq.execute('INSERT INTO t VALUES (?,?,?,?)', r)
  end
  ref = sq.execute('SELECT id,name,score,m FROM t ORDER BY id')
  sq.close

  db = AxiomDB.new(File.join(dir, 'a.db'))
  db.execute('CREATE TABLE t (id INT, name TEXT, score REAL, m INT)')
  db.execute("INSERT INTO t VALUES (1,'alice',3.5,NULL)")
  db.execute("INSERT INTO t VALUES (2,'héllo',2.25,99)")
  db.execute("INSERT INTO t VALUES (3,'日本',9.0,NULL)")

  tuples = db.query_tuples('SELECT id,name,score,m FROM t ORDER BY id')
  failed += 1 unless check('query_tuples matches sqlite', tuples == ref)

  objs = db.query('SELECT id,name FROM t ORDER BY id')
  expected = [{ 'id' => 1, 'name' => 'alice' }, { 'id' => 2, 'name' => 'héllo' }, { 'id' => 3, 'name' => '日本' }]
  failed += 1 unless check('query hashes', objs == expected)

  cols, rows = db.query_with_columns('SELECT id,name FROM t ORDER BY id')
  failed += 1 unless check('query_with_columns', cols == %w[id name] && rows[0] == [1, 'alice'])

  failed += 1 unless check('unicode preserved', tuples[2][1] == '日本')
  failed += 1 unless check('null preserved', tuples[0][3].nil?)
  failed += 1 unless check('empty result', db.query_tuples('SELECT * FROM t WHERE id=999') == [])

  # blob
  db.execute('CREATE TABLE b (data BLOB)')
  db.execute("INSERT INTO b VALUES (X'010203')")
  blob = db.query_tuples('SELECT data FROM b')[0][0]
  failed += 1 unless check('blob bytes', blob.bytes == [1, 2, 3])

  # transactions
  db.begin_txn
  db.execute("INSERT INTO t VALUES (4,'x',1.0,NULL)")
  db.rollback
  failed += 1 unless check('rollback', db.query_tuples('SELECT COUNT(*) FROM t')[0][0] == 3)

  # error handling
  threw = begin
    db.query_tuples('SELECT * FROM nope')
    false
  rescue AxiomDBError
    true
  end
  failed += 1 unless check('bad sql raises', threw)

  db.close
end

puts(failed.zero? ? "\nALL PASS" : "\n#{failed} FAILED")
exit(failed.zero? ? 0 : 1)
