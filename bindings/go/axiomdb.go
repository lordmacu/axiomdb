// Package axiomdb is a cgo binding for the AxiomDB embedded engine.
//
// Result materialization uses the packed-buffer path: one cgo call returns the
// whole result set as a single contiguous buffer, parsed once in Go. cgo has
// notable per-call overhead (goroutine stack switching), so the packed path —
// one call instead of ~2 per cell — is essential, exactly as it was for the
// Python ctypes and Node koffi bindings.
//
//	db, _ := axiomdb.Open("./myapp.db")
//	defer db.Close()
//	db.Execute("CREATE TABLE users (id INT, name TEXT)")
//	db.Execute("INSERT INTO users VALUES (1, 'Alice')")
//	rows, _ := db.QueryTuples("SELECT * FROM users") // [][]any{{int64(1), "Alice"}}
package axiomdb

/*
#cgo LDFLAGS: -L${SRCDIR}/../../target/release -laxiomdb_embedded -Wl,-rpath,${SRCDIR}/../../target/release
#include <stdlib.h>
#include <stdint.h>

typedef struct AxiomDb AxiomDb;
extern AxiomDb* axiomdb_open(const char* path);
extern long long axiomdb_execute(AxiomDb* db, const char* sql);
extern uint8_t* axiomdb_query_packed(AxiomDb* db, const char* sql, size_t* out_len);
extern void axiomdb_packed_free(uint8_t* ptr, size_t len);
extern void axiomdb_close(AxiomDb* db);
extern const char* axiomdb_last_error(AxiomDb* db);
*/
import "C"

import (
	"encoding/binary"
	"errors"
	"fmt"
	"math"
	"unsafe"
)

const packedMagic = 0x41584D31 // "AXM1"

// Packed cell tags (match the C FFI `axiomdb_query_packed` format).
const (
	tagNull = 0
	tagInt  = 1
	tagReal = 2
	tagText = 3
	tagBlob = 4
)

// DB is an in-process AxiomDB database handle. Not safe for concurrent use from
// multiple goroutines; open one DB per goroutine or serialize access.
type DB struct {
	ptr *C.AxiomDb
}

// Open opens or creates a database at path (":memory:" for ephemeral).
func Open(path string) (*DB, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	ptr := C.axiomdb_open(cpath)
	if ptr == nil {
		return nil, fmt.Errorf("axiomdb: failed to open database at %q", path)
	}
	return &DB{ptr: ptr}, nil
}

// Close closes the database. Safe to call multiple times.
func (db *DB) Close() {
	if db.ptr != nil {
		C.axiomdb_close(db.ptr)
		db.ptr = nil
	}
}

func (db *DB) lastError(fallback string) error {
	if db.ptr == nil {
		return errors.New("axiomdb: database is closed")
	}
	if msg := C.axiomdb_last_error(db.ptr); msg != nil {
		return fmt.Errorf("axiomdb: %s", C.GoString(msg))
	}
	return errors.New("axiomdb: " + fallback)
}

// Execute runs a DDL/DML statement and returns rows affected.
func (db *DB) Execute(sql string) (int64, error) {
	if db.ptr == nil {
		return 0, errors.New("axiomdb: database is closed")
	}
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	n := C.axiomdb_execute(db.ptr, csql)
	if n < 0 {
		return 0, db.lastError("execute failed")
	}
	return int64(n), nil
}

// queryPacked runs a SELECT and returns (columnNames, rows). One cgo call plus
// a single Go parse pass; no per-cell cgo crossings.
func (db *DB) queryPacked(sql string) ([]string, [][]any, error) {
	if db.ptr == nil {
		return nil, nil, errors.New("axiomdb: database is closed")
	}
	csql := C.CString(sql)
	defer C.free(unsafe.Pointer(csql))
	var outLen C.size_t
	ptr := C.axiomdb_query_packed(db.ptr, csql, &outLen)
	if ptr == nil {
		return nil, nil, db.lastError("query failed")
	}
	// One copy of the native buffer into an owned Go slice, then free + parse.
	buf := C.GoBytes(unsafe.Pointer(ptr), C.int(outLen))
	C.axiomdb_packed_free(ptr, outLen)
	return parsePacked(buf)
}

// QueryTuples runs a SELECT and returns rows as []any slices (positional).
// Column types map to: int64, float64, string, []byte, or nil (NULL).
func (db *DB) QueryTuples(sql string) ([][]any, error) {
	_, rows, err := db.queryPacked(sql)
	return rows, err
}

// QueryWithColumns runs a SELECT and returns column names plus rows.
func (db *DB) QueryWithColumns(sql string) ([]string, [][]any, error) {
	return db.queryPacked(sql)
}

// Query runs a SELECT and returns rows as maps (column name → value).
func (db *DB) Query(sql string) ([]map[string]any, error) {
	cols, rows, err := db.queryPacked(sql)
	if err != nil {
		return nil, err
	}
	out := make([]map[string]any, len(rows))
	for i, row := range rows {
		m := make(map[string]any, len(cols))
		for c, name := range cols {
			m[name] = row[c]
		}
		out[i] = m
	}
	return out, nil
}

// Begin / Commit / Rollback drive an explicit transaction.
func (db *DB) Begin() error    { _, err := db.Execute("BEGIN"); return err }
func (db *DB) Commit() error   { _, err := db.Execute("COMMIT"); return err }
func (db *DB) Rollback() error { _, err := db.Execute("ROLLBACK"); return err }

// parsePacked decodes a packed result buffer into (columns, rows). Go's i64 is
// native, so integers are exact (no BigInt workaround like JS/Python need).
func parsePacked(buf []byte) ([]string, [][]any, error) {
	if len(buf) < 16 {
		return nil, nil, errors.New("axiomdb: short packed buffer")
	}
	off := 0
	if binary.LittleEndian.Uint32(buf[off:]) != packedMagic {
		return nil, nil, errors.New("axiomdb: corrupt packed buffer")
	}
	off += 4
	nCols := int(binary.LittleEndian.Uint32(buf[off:]))
	off += 4
	nRows := int(binary.LittleEndian.Uint64(buf[off:]))
	off += 8

	cols := make([]string, nCols)
	for c := 0; c < nCols; c++ {
		l := int(binary.LittleEndian.Uint32(buf[off:]))
		off += 4
		cols[c] = string(buf[off : off+l])
		off += l
	}

	rows := make([][]any, nRows)
	for r := 0; r < nRows; r++ {
		row := make([]any, nCols)
		for c := 0; c < nCols; c++ {
			tag := buf[off]
			off++
			switch tag {
			case tagInt:
				row[c] = int64(binary.LittleEndian.Uint64(buf[off:]))
				off += 8
			case tagText:
				l := int(binary.LittleEndian.Uint32(buf[off:]))
				off += 4
				row[c] = string(buf[off : off+l])
				off += l
			case tagReal:
				row[c] = math.Float64frombits(binary.LittleEndian.Uint64(buf[off:]))
				off += 8
			case tagBlob:
				l := int(binary.LittleEndian.Uint32(buf[off:]))
				off += 4
				b := make([]byte, l)
				copy(b, buf[off:off+l])
				row[c] = b
				off += l
			default: // tagNull
				row[c] = nil
			}
		}
		rows[r] = row
	}
	return cols, rows, nil
}
