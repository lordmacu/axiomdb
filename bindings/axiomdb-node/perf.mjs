import { Connection } from './index.mjs';
import Database from 'better-sqlite3';
import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
const N=10000;
const SCHEMA='CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, active INT, score INT, email TEXT)';
const ins=i=>`INSERT INTO t VALUES (${i},'user_${String(i).padStart(6,'0')}',${18+i%62},${i%2===0?1:0},${100+i%1000},'u${i}@b.local')`;
const median=a=>{a.sort((x,y)=>x-y);return a[a.length>>1];};
const bench=(fn,it=11,w=3)=>{const ts=[];for(let k=0;k<it+w;k++){const t=performance.now();fn();ts.push(performance.now()-t);}return median(ts.slice(w));};
const dir=mkdtempSync(join(tmpdir(),'axn-'));
try{
  const db=new Connection(join(dir,'a.db'));
  db.execute(SCHEMA);db.begin();for(let i=0;i<N;i++)db.execute(ins(i));db.commit();
  const sq=new Database(join(dir,'s.db'));sq.pragma('journal_mode=WAL');sq.pragma('synchronous=FULL');sq.exec(SCHEMA);
  const tx=sq.transaction(()=>{for(let i=0;i<N;i++)sq.exec(ins(i));});tx();
  const stmt=sq.prepare('SELECT * FROM t');
  const ref=stmt.raw().all();
  const got=db.queryTuplesPacked('SELECT * FROM t');
  console.log('correctness packed:', JSON.stringify(got[0])===JSON.stringify(ref[0])?'MATCH':'MISMATCH');
  const s=bench(()=>stmt.raw().all());
  const tn=bench(()=>db.queryTuples('SELECT * FROM t'));
  const tp=bench(()=>db.queryTuplesPacked('SELECT * FROM t'));
  console.log('\n=== Node: native per-value vs native packed vs better-sqlite3 (10K x 6, median 11) ===');
  console.log(`  better-sqlite3 .raw().all():       ${s.toFixed(2)} ms   1.00x`);
  console.log(`  napi native queryTuples:           ${tn.toFixed(2)} ms   ${(tn/s).toFixed(2)}x`);
  console.log(`  napi packed queryTuplesPacked:     ${tp.toFixed(2)} ms   ${(tp/s).toFixed(2)}x  ${tp<s?'(FASTER)':''}`);
  db.close();sq.close();
}finally{rmSync(dir,{recursive:true,force:true});}
