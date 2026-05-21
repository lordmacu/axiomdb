/**
 * AxiomDB Node.js binding — koffi FFI over libaxiomdb_embedded.
 *
 * Result materialization uses the packed-buffer path: one FFI call returns the
 * whole result as a single contiguous buffer, parsed once in JS. This avoids
 * the ~2 FFI crossings per cell of the naive accessor loop (which was ~9×
 * slower than better-sqlite3); the packed path is ~2× — a 4× improvement.
 *
 * For full parity with better-sqlite3 (a native C++ addon), see the native
 * N-API addon in `bindings/axiomdb-node` (built with napi-rs).
 *
 * Usage:
 *   import { AxiomDB } from './axiomdb.mjs';
 *   const db = new AxiomDB('./myapp.db');
 *   db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)");
 *   db.execute("INSERT INTO users VALUES (1, 'Alice')");
 *   db.query("SELECT * FROM users");        // [{ id: 1, name: 'Alice' }]
 *   db.queryTuples("SELECT * FROM users");  // [[1, 'Alice']]  (fastest)
 *   db.close();
 */

import koffi from 'koffi';
import { existsSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

// ── Library loading ─────────────────────────────────────────────────────────

const __dirname = dirname(fileURLToPath(import.meta.url));

function findLibrary() {
  const platform = process.platform;
  const ext = platform === 'darwin' ? 'dylib' : platform === 'win32' ? 'dll' : 'so';
  const name = `libaxiomdb_embedded.${ext}`;
  const paths = [
    resolve(__dirname, name),
    resolve(__dirname, '..', '..', 'target', 'release', name),
    resolve(__dirname, '..', '..', 'target', 'debug', name),
    `/usr/local/lib/${name}`,
  ];
  for (const p of paths) {
    if (existsSync(p)) return p;
  }
  return name;
}

const lib = koffi.load(findLibrary());

// ── Type constants (packed cell tags) ────────────────────────────────────────

const TYPE_NULL = 0;
const TYPE_INTEGER = 1;
const TYPE_REAL = 2;
const TYPE_TEXT = 3;
const TYPE_BLOB = 4;
const PACKED_MAGIC = 0x41584d31; // "AXM1"

// ── C function declarations ───────────────────────────────────────────────────

const axiomdb_open = lib.func('void* axiomdb_open(const char* path)');
const axiomdb_execute = lib.func('int64_t axiomdb_execute(void* db, const char* sql)');
const axiomdb_close = lib.func('void axiomdb_close(void* db)');
const axiomdb_last_error = lib.func('const char* axiomdb_last_error(void* db)');
// Packed result buffer: one FFI call returns the whole result set.
const axiomdb_query_packed = lib.func(
  'uint8_t* axiomdb_query_packed(void* db, const char* sql, _Out_ size_t* out_len)'
);
const axiomdb_packed_free = lib.func('void axiomdb_packed_free(uint8_t* ptr, size_t len)');
// Parameter binding: serialize ? params into a buffer; the engine prepares +
// binds them (real prepared-statement binding, not string escaping).
const axiomdb_execute_params = lib.func(
  'int64_t axiomdb_execute_params(void* db, const char* sql, const uint8_t* params, size_t params_len)'
);
const axiomdb_query_packed_params = lib.func(
  'uint8_t* axiomdb_query_packed_params(void* db, const char* sql, const uint8_t* params, size_t params_len, _Out_ size_t* out_len)'
);

// ── Packed buffer parser ──────────────────────────────────────────────────────

/**
 * Parses a packed result buffer into { columns, rows } where each row is an
 * array of values. Format (LE): magic, n_cols, n_rows, column names, then
 * per-cell tag + payload.
 */
function parsePacked(buf) {
  let off = 0;
  const magic = buf.readUInt32LE(off); off += 4;
  if (magic !== PACKED_MAGIC) {
    throw new Error(`corrupt packed buffer (magic=0x${magic.toString(16)})`);
  }
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
      if (tag === TYPE_INTEGER) {
        // Read i64 as two i32s to skip a per-cell BigInt alloc (~33% faster
        // parse). Exact within the ±2^53 safe-integer range; beyond it the
        // value is approximated, same as Number(BigInt).
        row[c] = buf.readUInt32LE(off) + buf.readInt32LE(off + 4) * 4294967296; off += 8;
      } else if (tag === TYPE_TEXT) {
        const len = buf.readUInt32LE(off); off += 4;
        row[c] = buf.toString('utf8', off, off + len); off += len;
      } else if (tag === TYPE_REAL) {
        row[c] = buf.readDoubleLE(off); off += 8;
      } else if (tag === TYPE_BLOB) {
        const len = buf.readUInt32LE(off); off += 4;
        row[c] = Buffer.from(buf.subarray(off, off + len)); off += len; // owned copy
      } else {
        row[c] = null; // TYPE_NULL
      }
    }
    rows[r] = row;
  }
  return { columns, rows };
}

/**
 * Serializes positional params into the AxiomDB param buffer: u32 count, then
 * per-param {u8 tag, payload}. Tags mirror the packed cell encoding:
 * 0=NULL, 1=INT i64, 2=REAL f64, 3=TEXT, 4=BLOB.
 */
function encodeParams(params) {
  const chunks = [];
  const header = Buffer.allocUnsafe(4);
  header.writeUInt32LE(params.length, 0);
  chunks.push(header);
  for (const p of params) {
    if (p === null || p === undefined) {
      chunks.push(Buffer.from([TYPE_NULL]));
    } else if (typeof p === 'number') {
      const b = Buffer.allocUnsafe(9);
      if (Number.isInteger(p)) {
        b[0] = TYPE_INTEGER;
        b.writeBigInt64LE(BigInt(p), 1);
      } else {
        b[0] = TYPE_REAL;
        b.writeDoubleLE(p, 1);
      }
      chunks.push(b);
    } else if (typeof p === 'bigint') {
      const b = Buffer.allocUnsafe(9);
      b[0] = TYPE_INTEGER;
      b.writeBigInt64LE(p, 1);
      chunks.push(b);
    } else if (typeof p === 'boolean') {
      const b = Buffer.allocUnsafe(9);
      b[0] = TYPE_INTEGER;
      b.writeBigInt64LE(p ? 1n : 0n, 1);
      chunks.push(b);
    } else if (typeof p === 'string') {
      const sb = Buffer.from(p, 'utf8');
      const hdr = Buffer.allocUnsafe(5);
      hdr[0] = TYPE_TEXT;
      hdr.writeUInt32LE(sb.length, 1);
      chunks.push(hdr, sb);
    } else if (Buffer.isBuffer(p) || p instanceof Uint8Array) {
      const bb = Buffer.isBuffer(p) ? p : Buffer.from(p);
      const hdr = Buffer.allocUnsafe(5);
      hdr[0] = TYPE_BLOB;
      hdr.writeUInt32LE(bb.length, 1);
      chunks.push(hdr, bb);
    } else {
      throw new Error(`unsupported param type: ${typeof p}`);
    }
  }
  return Buffer.concat(chunks);
}

// ── JavaScript API ──────────────────────────────────────────────────────────

export class AxiomDB {
  #ptr = null;

  /** Open or create a database at the given file path. */
  constructor(path) {
    this.#ptr = axiomdb_open(path);
    if (!this.#ptr) {
      throw new Error(`Failed to open database at '${path}'`);
    }
  }

  /**
   * Execute a DDL/DML statement. Returns rows affected.
   *
   * Pass `params` (an array) to bind `?` placeholders with real
   * prepared-statement binding — no string interpolation, no SQL injection:
   *
   *   db.execute('INSERT INTO t VALUES (?, ?)', [1, 'Alice']);
   *
   * Each param maps from: null, number, bigint, boolean, string, or
   * Buffer/Uint8Array (BLOB).
   */
  execute(sql, params) {
    this.#checkOpen();
    let result;
    if (!params || params.length === 0) {
      result = axiomdb_execute(this.#ptr, sql);
    } else {
      const enc = encodeParams(params);
      result = axiomdb_execute_params(this.#ptr, sql, enc, enc.length);
    }
    if (result < 0) {
      throw new Error(this.#lastError() || 'execute failed');
    }
    return Number(result);
  }

  /** Run a SELECT via the packed buffer (one FFI call). Internal. */
  #queryPacked(sql, params) {
    this.#checkOpen();
    const lenOut = [0n];
    let ptr;
    if (!params || params.length === 0) {
      ptr = axiomdb_query_packed(this.#ptr, sql, lenOut);
    } else {
      const enc = encodeParams(params);
      ptr = axiomdb_query_packed_params(this.#ptr, sql, enc, enc.length, lenOut);
    }
    if (!ptr) {
      throw new Error(this.#lastError() || 'query failed');
    }
    const len = Number(lenOut[0]);
    try {
      // `koffi.view` is a Buffer-compatible window into native memory (not an
      // owned copy). parsePacked must run BEFORE the buffer is freed; it copies
      // every value into owned JS objects (strings/numbers/blob Buffers), so the
      // returned result stays valid afterwards.
      const buf = Buffer.from(koffi.view(ptr, len));
      return parsePacked(buf);
    } finally {
      axiomdb_packed_free(ptr, len);
    }
  }

  /**
   * Execute a SELECT and return rows as an array of objects
   * (column name → value). Pass `params` to bind `?` placeholders (see execute).
   */
  query(sql, params) {
    const { columns, rows } = this.#queryPacked(sql, params);
    return rows.map((row) => {
      const obj = {};
      for (let c = 0; c < columns.length; c++) obj[columns[c]] = row[c];
      return obj;
    });
  }

  /**
   * Execute a SELECT and return rows as arrays (fastest path; matches
   * better-sqlite3's `.raw().all()` shape). Pass `params` to bind `?`
   * placeholders (see execute).
   */
  queryTuples(sql, params) {
    return this.#queryPacked(sql, params).rows;
  }

  /** Execute a SELECT and return { columns, rows } (rows as arrays). */
  queryWithColumns(sql, params) {
    return this.#queryPacked(sql, params);
  }

  /** Close the database. Safe to call multiple times. */
  close() {
    if (this.#ptr) {
      axiomdb_close(this.#ptr);
      this.#ptr = null;
    }
  }

  /** Return the last error message, or null. */
  lastError() {
    return this.#lastError();
  }

  #checkOpen() {
    if (!this.#ptr) throw new Error('Database is closed');
  }

  #lastError() {
    if (!this.#ptr) return null;
    return axiomdb_last_error(this.#ptr) || null;
  }
}
