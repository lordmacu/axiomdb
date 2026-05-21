import koffi from 'koffi';
import { resolve, dirname, join } from 'path';
import { fileURLToPath } from 'url';
import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import Database from 'better-sqlite3';

const __dirname = dirname(fileURLToPath(import.meta.url));
const lib = koffi.load(resolve(__dirname, '..', '..', 'target', 'release', 'libaxiomdb_embedded.dylib'));

const axiomdb_open = lib.func('void* axiomdb_open(const char*)');
const axiomdb_execute = lib.func('int64_t axiomdb_execute(void*, const char*)');
const axiomdb_query = lib.func('void* axiomdb_query(void*, const char*)');
const axiomdb_close = lib.func('void axiomdb_close(void*)');
const rows_count = lib.func('int64_t axiomdb_rows_count(void*)');
const rows_columns = lib.func('int32_t axiomdb_rows_columns(void*)');
const rows_type = lib.func('int32_t axiomdb_rows_type(void*, int64_t, int32_t)');
const rows_get_int = lib.func('int64_t axiomdb_rows_get_int(void*, int64_t, int32_t)');
const rows_get_double = lib.func('double axiomdb_rows_get_double(void*, int64_t, int32_t)');
const rows_get_text = lib.func('const char* axiomdb_rows_get_text(void*, int64_t, int32_t)');
const rows_free = lib.func('void axiomdb_rows_free(void*)');
// packed
const query_packed = lib.func('uint8_t* axiomdb_query_packed(void*, const char*, _Out_ size_t*)');
const packed_free = lib.func('void axiomdb_packed_free(uint8_t*, size_t)');

const N = 10000;
const SCHEMA = 'CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, active INT, score INT, email TEXT)';
const ins = i => `INSERT INTO t VALUES (${i},'user_${String(i).padStart(6,'0')}',${18+i%62},${i%2===0?1:0},${100+i%1000},'u${i}@b.local')`;

function median(a){a.sort((x,y)=>x-y);return a[a.length>>1];}
function bench(fn,it=9,w=2){const ts=[];for(let k=0;k<it+w;k++){const t=fn();if(k>=w)ts.push(t);}return median(ts);}

// per-cell (current binding approach)
function axiomPerCell(db){
  const t0=performance.now();
  const rp=axiomdb_query(db,'SELECT * FROM t');
  const nr=Number(rows_count(rp)), nc=rows_columns(rp);
  const out=[];
  for(let r=0;r<nr;r++){const row=new Array(nc);for(let c=0;c<nc;c++){const ty=rows_type(rp,r,c);if(ty===1)row[c]=Number(rows_get_int(rp,r,c));else if(ty===3)row[c]=rows_get_text(rp,r,c);else if(ty===2)row[c]=rows_get_double(rp,r,c);else row[c]=null;}out.push(row);}
  rows_free(rp);
  return performance.now()-t0;
}
// packed buffer (one FFI call)
function axiomPacked(db){
  const t0=performance.now();
  const lenP=[0n];
  const ptr=query_packed(db,'SELECT * FROM t',lenP);
  const len=Number(lenP[0]);
  const buf=Buffer.from(koffi.view(ptr,len));
  let off=4; // skip magic
  const nc=buf.readUInt32LE(off);off+=4;
  const nr=Number(buf.readBigUInt64LE(off));off+=8;
  for(let c=0;c<nc;c++){const l=buf.readUInt32LE(off);off+=4+l;}
  const out=[];
  for(let r=0;r<nr;r++){const row=new Array(nc);for(let c=0;c<nc;c++){const tag=buf[off++];if(tag===1){row[c]=buf.readUInt32LE(off)+buf.readInt32LE(off+4)*4294967296;off+=8;}else if(tag===3){const l=buf.readUInt32LE(off);off+=4;row[c]=buf.toString('utf8',off,off+l);off+=l;}else if(tag===2){row[c]=buf.readDoubleLE(off);off+=8;}else if(tag===4){const l=buf.readUInt32LE(off);off+=4;row[c]=buf.subarray(off,off+l);off+=l;}else row[c]=null;}out.push(row);}
  packed_free(ptr,len);
  return performance.now()-t0;
}

function setupAxiom(dir){const db=axiomdb_open(join(dir,'a'));axiomdb_execute(db,SCHEMA);axiomdb_execute(db,'BEGIN');for(let i=0;i<N;i++)axiomdb_execute(db,ins(i));axiomdb_execute(db,'COMMIT');return db;}
function setupSqlite(dir){const db=new Database(join(dir,'s.db'));db.pragma('journal_mode=WAL');db.pragma('synchronous=FULL');db.exec(SCHEMA);const tx=db.transaction(()=>{for(let i=0;i<N;i++)db.exec(ins(i));});tx();return db;}

const dir=mkdtempSync(join(tmpdir(),'axnode-'));
try{
  const adb=setupAxiom(dir);
  const sdb=setupSqlite(dir);
  const stmt=sdb.prepare('SELECT * FROM t');
  // correctness
  const ref=stmt.raw().all();
  const pk=axiomPacked(adb), _=ref; // warm
  // correctness check (packed tuples vs sqlite raw)
  const lenP=[0n];const ptr=query_packed(adb,'SELECT * FROM t',lenP);const len=Number(lenP[0]);const buf=Buffer.from(koffi.view(ptr,len));
  let off=4;const nc=buf.readUInt32LE(off);off+=4;const nr=Number(buf.readBigUInt64LE(off));off+=8;for(let c=0;c<nc;c++){const l=buf.readUInt32LE(off);off+=4+l;}
  const first=[];for(let c=0;c<nc;c++){const tag=buf[off++];if(tag===1){first.push(buf.readUInt32LE(off)+buf.readInt32LE(off+4)*4294967296);off+=8;}else if(tag===3){const l=buf.readUInt32LE(off);off+=4;first.push(buf.toString('utf8',off,off+l));off+=l;}else if(tag===2){first.push(buf.readDoubleLE(off));off+=8;}else first.push(null);}
  packed_free(ptr,len);
  console.log('correctness row0 sqlite:', ref[0]);
  console.log('correctness row0 packed:', first, '->', JSON.stringify(first)===JSON.stringify(ref[0])?'MATCH':'MISMATCH');

  console.log('\n=== Node read perf (10K x 6, median of 9) ===');
  const sq=bench(()=>{const t=performance.now();const r=stmt.raw().all();return performance.now()-t;});
  const pc=bench(()=>axiomPerCell(adb));
  const pkk=bench(()=>axiomPacked(adb));
  console.log(`  better-sqlite3 .raw().all(): ${sq.toFixed(2)} ms   1.00x`);
  console.log(`  AxiomDB per-cell (koffi):    ${pc.toFixed(2)} ms   ${(pc/sq).toFixed(2)}x`);
  console.log(`  AxiomDB packed buffer:       ${pkk.toFixed(2)} ms   ${(pkk/sq).toFixed(2)}x`);
  axiomdb_close(adb);sdb.close();
}finally{rmSync(dir,{recursive:true,force:true});}
