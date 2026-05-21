import { AxiomDB } from './axiomdb.mjs';
import Database from 'better-sqlite3';
import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import assert from 'assert';

const dir = mkdtempSync(join(tmpdir(), 'axnode-test-'));
let failed = 0;
function check(name, cond) { if (cond) console.log(`PASS ${name}`); else { failed++; console.log(`FAIL ${name}`); } }

try {
  // sqlite reference
  const sq = new Database(join(dir, 's.db'));
  sq.exec('CREATE TABLE t (id INT, name TEXT, score REAL, m INT)');
  for (const r of [[1,'alice',3.5,null],[2,'héllo',2.25,99],[3,'日本',9.0,null]])
    sq.prepare('INSERT INTO t VALUES (?,?,?,?)').run(...r);
  const ref = sq.prepare('SELECT id,name,score,m FROM t ORDER BY id').raw().all();
  sq.close();

  const db = new AxiomDB(join(dir, 'a.db'));
  db.execute('CREATE TABLE t (id INT, name TEXT, score REAL, m INT)');
  db.execute("INSERT INTO t VALUES (1,'alice',3.5,NULL)");
  db.execute("INSERT INTO t VALUES (2,'héllo',2.25,99)");
  db.execute("INSERT INTO t VALUES (3,'日本',9.0,NULL)");

  const tuples = db.queryTuples('SELECT id,name,score,m FROM t ORDER BY id');
  check('queryTuples matches sqlite', JSON.stringify(tuples) === JSON.stringify(ref));

  const objs = db.query('SELECT id,name FROM t ORDER BY id');
  check('query (objects) shape', JSON.stringify(objs) === JSON.stringify([{id:1,name:'alice'},{id:2,name:'héllo'},{id:3,name:'日本'}]));

  const wc = db.queryWithColumns('SELECT id,name FROM t ORDER BY id');
  check('queryWithColumns columns', JSON.stringify(wc.columns) === JSON.stringify(['id','name']));
  check('queryWithColumns rows', JSON.stringify(wc.rows) === JSON.stringify([[1,'alice'],[2,'héllo'],[3,'日本']]));

  check('empty result', JSON.stringify(db.queryTuples('SELECT * FROM t WHERE id=999')) === '[]');

  // unicode + null preserved
  check('unicode preserved', tuples[2][1] === '日本');
  check('null preserved', tuples[0][3] === null);

  // blob
  db.execute('CREATE TABLE b (data BLOB)');
  db.execute("INSERT INTO b VALUES (X'010203')");
  const blob = db.queryTuples('SELECT data FROM b')[0][0];
  check('blob is Buffer', Buffer.isBuffer(blob) && blob.length === 3 && blob[0] === 1);

  // error handling
  let threw = false;
  try { db.queryTuples('SELECT * FROM nonexistent'); } catch { threw = true; }
  check('bad sql throws', threw);

  db.close();
  console.log(`\n${failed === 0 ? 'ALL PASS' : failed + ' FAILED'}`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
