// Benchmark: AxiomDB (cgo packed) vs mattn/go-sqlite3 (cgo), row materialization.
//
//	cd bindings/go && go run ./bench
package main

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"time"

	"axiomdb"

	_ "github.com/mattn/go-sqlite3"
)

const (
	nRows = 10000
	iters = 11
	warm  = 3
)

const schema = "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, active INT, score INT, email TEXT)"

func ins(i int) string {
	return fmt.Sprintf("INSERT INTO t VALUES (%d,'user_%06d',%d,%d,%d,'u%d@b.local')",
		i, i, 18+i%62, boolToInt(i%2 == 0), 100+i%1000, i)
}
func boolToInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

func median(xs []float64) float64 {
	sort.Float64s(xs)
	return xs[len(xs)/2]
}

func bench(fn func()) float64 {
	ts := make([]float64, 0, iters)
	for k := 0; k < iters+warm; k++ {
		t0 := time.Now()
		fn()
		el := float64(time.Since(t0).Microseconds()) / 1000.0
		if k >= warm {
			ts = append(ts, el)
		}
	}
	return median(ts)
}

func main() {
	dir, _ := os.MkdirTemp("", "axgo-bench-")
	defer os.RemoveAll(dir)

	// AxiomDB setup
	adb, err := axiomdb.Open(filepath.Join(dir, "a.db"))
	if err != nil {
		panic(err)
	}
	defer adb.Close()
	adb.Execute(schema)
	adb.Begin()
	for i := 0; i < nRows; i++ {
		adb.Execute(ins(i))
	}
	adb.Commit()

	// SQLite setup (mattn/go-sqlite3)
	sdb, _ := sql.Open("sqlite3", filepath.Join(dir, "s.db"))
	defer sdb.Close()
	sdb.Exec("PRAGMA journal_mode=WAL")
	sdb.Exec("PRAGMA synchronous=FULL")
	sdb.Exec(schema)
	tx, _ := sdb.Begin()
	for i := 0; i < nRows; i++ {
		tx.Exec(ins(i))
	}
	tx.Commit()

	// SQLite materialization: scan every column of every row.
	sqliteScan := func() {
		rows, _ := sdb.Query("SELECT * FROM t")
		var id, age, active, score int64
		var name, email string
		n := 0
		for rows.Next() {
			rows.Scan(&id, &name, &age, &active, &score, &email)
			n++
		}
		rows.Close()
		if n != nRows {
			panic("sqlite row count")
		}
	}

	axiomQuery := func() {
		r, err := adb.QueryTuples("SELECT * FROM t")
		if err != nil || len(r) != nRows {
			panic("axiom query")
		}
	}

	// correctness spot-check
	ar, _ := adb.QueryTuples("SELECT * FROM t")
	if ar[0][0].(int64) != 0 || ar[0][1].(string) != "user_000000" {
		panic("correctness")
	}

	s := bench(sqliteScan)
	a := bench(axiomQuery)
	fmt.Printf("Go read benchmark — 10K x 6, materialize every cell (median of %d)\n\n", iters)
	fmt.Printf("  go-sqlite3 (cgo, rows.Scan):   %6.2f ms   1.00x\n", s)
	ratio := a / s
	faster := ""
	if a < s {
		faster = "(FASTER)"
	}
	fmt.Printf("  AxiomDB QueryTuples (cgo pack): %6.2f ms   %.2fx  %s\n", a, ratio, faster)
}
