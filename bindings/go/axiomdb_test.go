package axiomdb

import (
	"bytes"
	"path/filepath"
	"testing"
)

func openTmp(t *testing.T) *DB {
	t.Helper()
	db, err := Open(filepath.Join(t.TempDir(), "t.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(db.Close)
	return db
}

func TestBasicTypes(t *testing.T) {
	db := openTmp(t)
	if _, err := db.Execute("CREATE TABLE t (id INT, name TEXT, score REAL, m INT)"); err != nil {
		t.Fatal(err)
	}
	db.Execute("INSERT INTO t VALUES (1, 'alice', 3.5, NULL)")
	db.Execute("INSERT INTO t VALUES (2, 'héllo', 2.25, 99)")

	rows, err := db.QueryTuples("SELECT id, name, score, m FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 2 {
		t.Fatalf("want 2 rows, got %d", len(rows))
	}
	// row 0: 1, "alice", 3.5, nil
	if rows[0][0].(int64) != 1 || rows[0][1].(string) != "alice" || rows[0][2].(float64) != 3.5 || rows[0][3] != nil {
		t.Errorf("row0 mismatch: %#v", rows[0])
	}
	// row 1: unicode + non-null int
	if rows[1][1].(string) != "héllo" || rows[1][3].(int64) != 99 {
		t.Errorf("row1 mismatch: %#v", rows[1])
	}
}

func TestQueryMaps(t *testing.T) {
	db := openTmp(t)
	db.Execute("CREATE TABLE t (id INT, name TEXT)")
	db.Execute("INSERT INTO t VALUES (7, 'bob')")
	rows, err := db.Query("SELECT id, name FROM t")
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || rows[0]["id"].(int64) != 7 || rows[0]["name"].(string) != "bob" {
		t.Errorf("map mismatch: %#v", rows)
	}
}

func TestWithColumns(t *testing.T) {
	db := openTmp(t)
	db.Execute("CREATE TABLE t (id INT, name TEXT)")
	db.Execute("INSERT INTO t VALUES (1, 'x')")
	cols, rows, err := db.QueryWithColumns("SELECT id, name FROM t")
	if err != nil {
		t.Fatal(err)
	}
	if len(cols) != 2 || cols[0] != "id" || cols[1] != "name" {
		t.Errorf("cols mismatch: %v", cols)
	}
	if rows[0][0].(int64) != 1 {
		t.Errorf("rows mismatch: %#v", rows)
	}
}

func TestEmptyResult(t *testing.T) {
	db := openTmp(t)
	db.Execute("CREATE TABLE t (id INT)")
	db.Execute("INSERT INTO t VALUES (1)")
	rows, err := db.QueryTuples("SELECT id FROM t WHERE id = 999")
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 0 {
		t.Errorf("want empty, got %#v", rows)
	}
}

func TestBlob(t *testing.T) {
	db := openTmp(t)
	db.Execute("CREATE TABLE b (data BLOB)")
	db.Execute("INSERT INTO b VALUES (X'010203')")
	rows, err := db.QueryTuples("SELECT data FROM b")
	if err != nil {
		t.Fatal(err)
	}
	blob, ok := rows[0][0].([]byte)
	if !ok || !bytes.Equal(blob, []byte{1, 2, 3}) {
		t.Errorf("blob mismatch: %#v", rows[0][0])
	}
}

func TestTransactions(t *testing.T) {
	db := openTmp(t)
	db.Execute("CREATE TABLE t (id INT)")
	db.Begin()
	db.Execute("INSERT INTO t VALUES (1)")
	db.Commit()
	if rows, _ := db.QueryTuples("SELECT * FROM t"); len(rows) != 1 {
		t.Errorf("after commit want 1 row, got %d", len(rows))
	}
	db.Begin()
	db.Execute("INSERT INTO t VALUES (2)")
	db.Rollback()
	if rows, _ := db.QueryTuples("SELECT * FROM t"); len(rows) != 1 {
		t.Errorf("after rollback want 1 row, got %d", len(rows))
	}
}

func TestBadSQL(t *testing.T) {
	db := openTmp(t)
	if _, err := db.QueryTuples("SELECT * FROM nonexistent"); err == nil {
		t.Error("expected error for bad SQL")
	}
}

func TestParamBinding(t *testing.T) {
	db := openTmp(t)
	db.Execute("CREATE TABLE t (id INT, name TEXT, score REAL, avatar BLOB)")
	if _, err := db.Execute("INSERT INTO t VALUES (?, ?, ?, ?)", 1, "alice", 3.5, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Execute("INSERT INTO t VALUES (?, ?, ?, ?)", 2, "héllo", 2.25, []byte{9, 8, 7}); err != nil {
		t.Fatal(err)
	}

	rows, err := db.QueryTuples("SELECT id, name, score, avatar FROM t WHERE id = ?", 2)
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 {
		t.Fatalf("want 1 row, got %d", len(rows))
	}
	if rows[0][0].(int64) != 2 || rows[0][1].(string) != "héllo" || rows[0][2].(float64) != 2.25 {
		t.Errorf("row mismatch: %#v", rows[0])
	}
	if blob, ok := rows[0][3].([]byte); !ok || !bytes.Equal(blob, []byte{9, 8, 7}) {
		t.Errorf("blob mismatch: %#v", rows[0][3])
	}
}

func TestParamInjectionSafe(t *testing.T) {
	db := openTmp(t)
	db.Execute("CREATE TABLE t (id INT, name TEXT)")
	evil := "x'; DROP TABLE t; --"
	if _, err := db.Execute("INSERT INTO t VALUES (?, ?)", 1, evil); err != nil {
		t.Fatal(err)
	}
	rows, err := db.QueryTuples("SELECT name FROM t WHERE id = ?", 1)
	if err != nil {
		t.Fatal(err)
	}
	if rows[0][0].(string) != evil {
		t.Errorf("want %q, got %#v", evil, rows[0][0])
	}
	// table survived — value bound, not executed
	cnt, _ := db.QueryTuples("SELECT COUNT(*) FROM t")
	if cnt[0][0].(int64) != 1 {
		t.Errorf("table mutated: count=%v", cnt[0][0])
	}
}
