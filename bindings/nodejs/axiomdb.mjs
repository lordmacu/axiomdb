/**
 * AxiomDB Node.js binding — FFI wrapper over libaxiomdb_embedded.
 *
 * Usage:
 *   import { AxiomDB } from './axiomdb.mjs';
 *
 *   const db = new AxiomDB('./myapp.db');
 *   db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)");
 *   db.execute("INSERT INTO users VALUES (1, 'Alice')");
 *
 *   const rows = db.query("SELECT * FROM users");
 *   console.log(rows); // [{ id: 1, name: 'Alice' }]
 *
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
  return name; // fallback to system search
}

const lib = koffi.load(findLibrary());

// ── Type constants ──────────────────────────────────────────────────────────

const TYPE_NULL = 0;
const TYPE_INTEGER = 1;
const TYPE_REAL = 2;
const TYPE_TEXT = 3;
const TYPE_BLOB = 4;

// ── C function declarations ─────────────────────────────────────────────────

const axiomdb_open = lib.func('void* axiomdb_open(const char* path)');
const axiomdb_execute = lib.func('int64_t axiomdb_execute(void* db, const char* sql)');
const axiomdb_query = lib.func('void* axiomdb_query(void* db, const char* sql)');
const axiomdb_close = lib.func('void axiomdb_close(void* db)');

const axiomdb_rows_count = lib.func('int64_t axiomdb_rows_count(void* rows)');
const axiomdb_rows_columns = lib.func('int32_t axiomdb_rows_columns(void* rows)');
const axiomdb_rows_column_name = lib.func('const char* axiomdb_rows_column_name(void* rows, int32_t col)');
const axiomdb_rows_type = lib.func('int32_t axiomdb_rows_type(void* rows, int64_t row, int32_t col)');
const axiomdb_rows_get_int = lib.func('int64_t axiomdb_rows_get_int(void* rows, int64_t row, int32_t col)');
const axiomdb_rows_get_double = lib.func('double axiomdb_rows_get_double(void* rows, int64_t row, int32_t col)');
const axiomdb_rows_get_text = lib.func('const char* axiomdb_rows_get_text(void* rows, int64_t row, int32_t col)');
const axiomdb_rows_free = lib.func('void axiomdb_rows_free(void* rows)');
const axiomdb_last_error = lib.func('const char* axiomdb_last_error(void* db)');

// ── JavaScript API ──────────────────────────────────────────────────────────

export class AxiomDB {
  #ptr = null;

  /**
   * Open or create a database at the given file path.
   * @param {string} path — path to the database file
   */
  constructor(path) {
    this.#ptr = axiomdb_open(path);
    if (!this.#ptr) {
      throw new Error(`Failed to open database at '${path}'`);
    }
  }

  /**
   * Execute a SQL statement (INSERT, UPDATE, DELETE, DDL).
   * @param {string} sql
   * @returns {number} — rows affected
   */
  execute(sql) {
    this.#checkOpen();
    const result = axiomdb_execute(this.#ptr, sql);
    if (result < 0) {
      throw new Error(this.#lastError() || 'execute failed');
    }
    return Number(result);
  }

  /**
   * Execute a SELECT and return rows as array of objects.
   * @param {string} sql
   * @returns {Array<Object>} — rows with column names as keys
   */
  query(sql) {
    this.#checkOpen();
    const rowsPtr = axiomdb_query(this.#ptr, sql);
    if (!rowsPtr) {
      throw new Error(this.#lastError() || 'query failed');
    }

    try {
      const nRows = Number(axiomdb_rows_count(rowsPtr));
      const nCols = axiomdb_rows_columns(rowsPtr);

      // Column names
      const colNames = [];
      for (let c = 0; c < nCols; c++) {
        colNames.push(axiomdb_rows_column_name(rowsPtr, c) || `col_${c}`);
      }

      // Extract rows
      const result = [];
      for (let r = 0; r < nRows; r++) {
        const row = {};
        for (let c = 0; c < nCols; c++) {
          const typ = axiomdb_rows_type(rowsPtr, r, c);
          switch (typ) {
            case TYPE_NULL:
              row[colNames[c]] = null;
              break;
            case TYPE_INTEGER:
              row[colNames[c]] = Number(axiomdb_rows_get_int(rowsPtr, r, c));
              break;
            case TYPE_REAL:
              row[colNames[c]] = axiomdb_rows_get_double(rowsPtr, r, c);
              break;
            case TYPE_TEXT:
              row[colNames[c]] = axiomdb_rows_get_text(rowsPtr, r, c);
              break;
            case TYPE_BLOB:
              row[colNames[c]] = null; // TODO: blob support
              break;
            default:
              row[colNames[c]] = null;
          }
        }
        result.push(row);
      }
      return result;
    } finally {
      axiomdb_rows_free(rowsPtr);
    }
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

// ── Demo ────────────────────────────────────────────────────────────────────

import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

const tmp = mkdtempSync(join(tmpdir(), 'axiomdb-'));
const dbPath = join(tmp, 'demo.db');

try {
  console.log(`Opening database at ${dbPath}`);
  const db = new AxiomDB(dbPath);

  db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)");
  console.log("Created table 'users'");

  db.execute("INSERT INTO users VALUES (1, 'Alice', 30)");
  db.execute("INSERT INTO users VALUES (2, 'Bob', 25)");
  db.execute("INSERT INTO users VALUES (3, 'Charlie', 35)");
  console.log("Inserted 3 rows");

  let rows = db.query("SELECT * FROM users");
  console.log(`\nSELECT * FROM users (${rows.length} rows):`);
  rows.forEach(r => console.log(' ', r));

  rows = db.query("SELECT name, age FROM users WHERE age > 28");
  console.log(`\nWHERE age > 28 (${rows.length} rows):`);
  rows.forEach(r => console.log(' ', r));

  rows = db.query("SELECT COUNT(*) AS total FROM users");
  console.log(`\nCOUNT(*):`, rows[0]);

  const affected = db.execute("UPDATE users SET age = 31 WHERE id = 1");
  console.log(`\nUPDATE affected ${affected} row(s)`);

  db.execute("DELETE FROM users WHERE id = 3");
  console.log("DELETE affected 1 row(s)");

  rows = db.query("SELECT * FROM users");
  console.log(`\nFinal state (${rows.length} rows):`);
  rows.forEach(r => console.log(' ', r));

  db.close();
  console.log("\nDone! Database closed.");
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
