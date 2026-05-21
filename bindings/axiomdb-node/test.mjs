import { Connection } from './index.mjs';
import Database from 'better-sqlite3';
import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
const dir = mkdtempSync(join(tmpdir(),'axnode-native-'));
let failed=0; const ck=(n,c)=>{console.log((c?'PASS ':'FAIL ')+n);if(!c)failed++;};
try{
  const sq=new Database(join(dir,'s.db'));
  sq.exec('CREATE TABLE t (id INT, name TEXT, score REAL, m INT)');
  for(const r of [[1,'alice',3.5,null],[2,'héllo',2.25,99],[3,'日本',9.0,null]]) sq.prepare('INSERT INTO t VALUES (?,?,?,?)').run(...r);
  const ref=sq.prepare('SELECT id,name,score,m FROM t ORDER BY id').raw().all(); sq.close();

  const db=new Connection(join(dir,'a.db'));
  db.execute('CREATE TABLE t (id INT, name TEXT, score REAL, m INT)');
  db.execute("INSERT INTO t VALUES (1,'alice',3.5,NULL)");
  db.execute("INSERT INTO t VALUES (2,'héllo',2.25,99)");
  db.execute("INSERT INTO t VALUES (3,'日本',9.0,NULL)");

  const Q='SELECT id,name,score,m FROM t ORDER BY id';
  ck('native queryTuples == sqlite', JSON.stringify(db.queryTuples(Q))===JSON.stringify(ref));
  ck('packed queryTuplesPacked == sqlite', JSON.stringify(db.queryTuplesPacked(Q))===JSON.stringify(ref));
  ck('native query objects', JSON.stringify(db.query('SELECT id,name FROM t ORDER BY id'))===JSON.stringify([{id:1,name:'alice'},{id:2,name:'héllo'},{id:3,name:'日本'}]));
  ck('packed queryPacked objects', JSON.stringify(db.queryPacked('SELECT id,name FROM t ORDER BY id'))===JSON.stringify([{id:1,name:'alice'},{id:2,name:'héllo'},{id:3,name:'日本'}]));

  // blob via packed
  db.execute('CREATE TABLE b (data BLOB)'); db.execute("INSERT INTO b VALUES (X'010203')");
  const blob=db.queryTuplesPacked('SELECT data FROM b')[0][0];
  ck('packed blob is Buffer', Buffer.isBuffer(blob)&&blob.length===3&&blob[0]===1);

  db.begin(); db.execute("INSERT INTO t VALUES (9,'x',1.0,NULL)"); db.rollback();
  ck('rollback', db.queryTuplesPacked('SELECT COUNT(*) FROM t')[0][0]===3);
  let threw=false; try{db.queryTuplesPacked('SELECT * FROM nope');}catch{threw=true;} ck('bad sql throws', threw);

  db.close();
  console.log(failed?`\n${failed} FAILED`:'\nALL PASS');
}finally{rmSync(dir,{recursive:true,force:true});}
process.exit(failed?1:0);
