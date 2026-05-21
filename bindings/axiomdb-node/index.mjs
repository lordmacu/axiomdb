/**
 * AxiomDB native Node.js binding (napi-rs addon loader).
 *
 * Exposes the native `Connection` plus packed-buffer helpers. See NOTES.md for
 * the performance comparison between the per-cell native path and the packed
 * path — the packed path wins on Node because N-API per-value object
 * construction is costlier than one buffer return + a JS parse loop.
 */
import { createRequire } from 'module';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

const require = createRequire(import.meta.url);
const native = require(join(dirname(fileURLToPath(import.meta.url)), 'axiomdb-node.node'));

const PACKED_MAGIC = 0x41584d31;

/** Parse a packed result buffer into { columns, rows } (rows as arrays). */
function parsePacked(buf) {
  let off = 0;
  if (buf.readUInt32LE(off) !== PACKED_MAGIC) throw new Error('corrupt packed buffer');
  off += 4;
  const nCols = buf.readUInt32LE(off); off += 4;
  const nRows = Number(buf.readBigUInt64LE(off)); off += 8;
  const columns = new Array(nCols);
  for (let c = 0; c < nCols; c++) {
    const len = buf.readUInt32LE(off); off += 4;
    columns[c] = buf.toString('utf8', off, off + len); off += len;
  }
  const rows = new Array(nRows);
  for (let r = 0; r < nRows; r++) {
    const row = new Array(nCols);
    for (let c = 0; c < nCols; c++) {
      const tag = buf[off++];
      // INT: read i64 as two i32s to avoid a per-cell BigInt allocation
      // (~33% faster parse). Exact for integers in the ±2^53 safe range; values
      // beyond it are approximated, same as Number(BigInt).
      if (tag === 1) { row[c] = buf.readUInt32LE(off) + buf.readInt32LE(off + 4) * 4294967296; off += 8; }
      else if (tag === 5) { const l = buf.readUInt32LE(off); off += 4; row[c] = buf.toString('latin1', off, off + l); off += l; } // ASCII fast path
      else if (tag === 3) { const l = buf.readUInt32LE(off); off += 4; row[c] = buf.toString('utf8', off, off + l); off += l; }
      else if (tag === 2) { row[c] = buf.readDoubleLE(off); off += 8; }
      else if (tag === 4) { const l = buf.readUInt32LE(off); off += 4; row[c] = Buffer.from(buf.subarray(off, off + l)); off += l; }
      else row[c] = null;
    }
    rows[r] = row;
  }
  return { columns, rows };
}

/**
 * Connection wrapping the native addon. `query`/`queryTuples` build JS objects
 * in Rust (native); `queryTuplesPacked`/`queryPacked` use the packed buffer +
 * JS parse (faster on Node — see NOTES.md).
 */
export class Connection {
  #c;
  constructor(path) { this.#c = new native.Connection(path); }
  // Pass `params` (an array) to bind `?` placeholders safely (no SQL injection).
  execute(sql, params) { return this.#c.execute(sql, params); }
  // Native object construction (slower on Node):
  query(sql, params) { return this.#c.query(sql, params); }
  queryTuples(sql, params) { return this.#c.queryTuples(sql, params); }
  // Packed buffer + JS parse (faster on Node):
  queryTuplesPacked(sql, params) { return parsePacked(this.#c.queryPacked(sql, params)).rows; }
  queryPacked(sql, params) {
    const { columns, rows } = parsePacked(this.#c.queryPacked(sql, params));
    return rows.map((r) => {
      const o = {};
      for (let i = 0; i < columns.length; i++) o[columns[i]] = r[i];
      return o;
    });
  }
  begin() { this.#c.begin(); }
  commit() { this.#c.commit(); }
  rollback() { this.#c.rollback(); }
  close() { this.#c.close(); }
}

export const NativeConnection = native.Connection;
export default native;
