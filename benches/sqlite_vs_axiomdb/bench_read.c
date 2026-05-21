/*
 * C read benchmark: AxiomDB C API vs SQLite C API.
 *
 * Proves that the per-cell accessor pattern that was catastrophic in Python
 * (ctypes marshalling ~2us/call) is native-speed in C (~ns/call). Both engines
 * materialize every cell of a 10K x 6 result, accumulating a checksum so the
 * optimizer cannot elide the reads.
 *
 * Build (macOS):
 *   cc -O2 benches/sqlite_vs_axiomdb/bench_read.c \
 *      -L target/release -laxiomdb_embedded -lsqlite3 \
 *      -Wl,-rpath,target/release -o /tmp/bench_read
 *   /tmp/bench_read
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include <unistd.h>
#include <sqlite3.h>

/* ── AxiomDB C FFI (declared here; no public header yet) ─────────────────── */
typedef struct AxiomDb AxiomDb;
typedef struct AxiomRows AxiomRows;
typedef struct AxiomCursor AxiomCursor;

extern AxiomDb *axiomdb_open(const char *path);
extern long long axiomdb_execute(AxiomDb *db, const char *sql);
extern AxiomRows *axiomdb_query(AxiomDb *db, const char *sql);
extern long long axiomdb_rows_count(const AxiomRows *rows);
extern int axiomdb_rows_columns(const AxiomRows *rows);
extern int axiomdb_rows_type(const AxiomRows *rows, long long row, int col);
extern long long axiomdb_rows_get_int(const AxiomRows *rows, long long row, int col);
extern double axiomdb_rows_get_double(const AxiomRows *rows, long long row, int col);
extern const char *axiomdb_rows_get_text(const AxiomRows *rows, long long row, int col);
extern void axiomdb_rows_free(AxiomRows *rows);
extern void axiomdb_close(AxiomDb *db);
extern unsigned char *axiomdb_query_packed(AxiomDb *db, const char *sql, size_t *out_len);
extern void axiomdb_packed_free(unsigned char *ptr, size_t len);

/* Tier 1 cursor API (zero-copy over the materialized result). */
extern AxiomCursor *axiomdb_cursor_open(AxiomDb *db, const char *sql);
extern int axiomdb_cursor_step(AxiomCursor *cur);
extern int axiomdb_cursor_columns(const AxiomCursor *cur);
extern int axiomdb_cursor_type(const AxiomCursor *cur, int col);
extern long long axiomdb_cursor_int(const AxiomCursor *cur, int col);
extern double axiomdb_cursor_double(const AxiomCursor *cur, int col);
extern const char *axiomdb_cursor_text(AxiomCursor *cur, int col, size_t *len);
extern void axiomdb_cursor_close(AxiomCursor *cur);

#define N_ROWS 10000
#define ITERS 15
#define WARMUP 3

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1.0e6;
}

static int cmp_double(const void *a, const void *b) {
    double da = *(const double *)a, db = *(const double *)b;
    return (da > db) - (da < db);
}

static double median(double *xs, int n) {
    qsort(xs, n, sizeof(double), cmp_double);
    return xs[n / 2];
}

/* ── AxiomDB: per-cell accessors (the pattern that killed Python/ctypes) ──── */
static double bench_axiom_percell(AxiomDb *db, volatile uint64_t *sink) {
    double t0 = now_ms();
    AxiomRows *rows = axiomdb_query(db, "SELECT * FROM t");
    long long n = axiomdb_rows_count(rows);
    int cols = axiomdb_rows_columns(rows);
    uint64_t acc = 0;
    for (long long r = 0; r < n; r++) {
        for (int c = 0; c < cols; c++) {
            int ty = axiomdb_rows_type(rows, r, c);
            if (ty == 1) {
                acc += (uint64_t)axiomdb_rows_get_int(rows, r, c);
            } else if (ty == 3) {
                const char *s = axiomdb_rows_get_text(rows, r, c);
                if (s) acc += strlen(s);
            } else if (ty == 2) {
                acc += (uint64_t)axiomdb_rows_get_double(rows, r, c);
            }
        }
    }
    axiomdb_rows_free(rows);
    double el = now_ms() - t0;
    *sink += acc;
    return el;
}

/* ── AxiomDB: packed buffer (one FFI call; C doesn't need it but show it) ─── */
static double bench_axiom_packed(AxiomDb *db, volatile uint64_t *sink) {
    double t0 = now_ms();
    size_t len = 0;
    unsigned char *buf = axiomdb_query_packed(db, "SELECT * FROM t", &len);
    uint64_t acc = 0;
    size_t off = 0;
    uint32_t ncols;
    memcpy(&ncols, buf + 4, 4);
    uint64_t nrows;
    memcpy(&nrows, buf + 8, 8);
    off = 16;
    for (uint32_t c = 0; c < ncols; c++) {
        uint32_t l;
        memcpy(&l, buf + off, 4);
        off += 4 + l;
    }
    for (uint64_t r = 0; r < nrows; r++) {
        for (uint32_t c = 0; c < ncols; c++) {
            unsigned char tag = buf[off++];
            if (tag == 1) {
                int64_t v;
                memcpy(&v, buf + off, 8);
                off += 8;
                acc += (uint64_t)v;
            } else if (tag == 3) {
                uint32_t l;
                memcpy(&l, buf + off, 4);
                off += 4;
                acc += l;
                off += l;
            } else if (tag == 2) {
                double v;
                memcpy(&v, buf + off, 8);
                off += 8;
                acc += (uint64_t)v;
            } else if (tag == 4) {
                uint32_t l;
                memcpy(&l, buf + off, 4);
                off += 4 + l;
            }
        }
    }
    axiomdb_packed_free(buf, len);
    double el = now_ms() - t0;
    *sink += acc;
    return el;
}

/* ── AxiomDB: Tier 1 cursor (zero-copy text, no CellValue/CString pass) ───── */
static double bench_axiom_cursor(AxiomDb *db, volatile uint64_t *sink) {
    double t0 = now_ms();
    AxiomCursor *cur = axiomdb_cursor_open(db, "SELECT * FROM t");
    int cols = axiomdb_cursor_columns(cur);
    uint64_t acc = 0;
    while (axiomdb_cursor_step(cur) == 1) {
        for (int c = 0; c < cols; c++) {
            int ty = axiomdb_cursor_type(cur, c);
            if (ty == 1) {
                acc += (uint64_t)axiomdb_cursor_int(cur, c);
            } else if (ty == 3) {
                size_t len = 0;
                const char *s = axiomdb_cursor_text(cur, c, &len);
                if (s) acc += len;
            } else if (ty == 2) {
                acc += (uint64_t)axiomdb_cursor_double(cur, c);
            }
        }
    }
    axiomdb_cursor_close(cur);
    double el = now_ms() - t0;
    *sink += acc;
    return el;
}

/* ── SQLite: per-column accessors (its native C API) ─────────────────────── */
static double bench_sqlite(sqlite3 *db, volatile uint64_t *sink) {
    double t0 = now_ms();
    sqlite3_stmt *st;
    sqlite3_prepare_v2(db, "SELECT * FROM t", -1, &st, NULL);
    uint64_t acc = 0;
    while (sqlite3_step(st) == SQLITE_ROW) {
        int cols = sqlite3_column_count(st);
        for (int c = 0; c < cols; c++) {
            int ty = sqlite3_column_type(st, c);
            if (ty == SQLITE_INTEGER) {
                acc += (uint64_t)sqlite3_column_int64(st, c);
            } else if (ty == SQLITE_TEXT) {
                const unsigned char *s = sqlite3_column_text(st, c);
                if (s) acc += strlen((const char *)s);
            } else if (ty == SQLITE_FLOAT) {
                acc += (uint64_t)sqlite3_column_double(st, c);
            }
        }
    }
    sqlite3_finalize(st);
    double el = now_ms() - t0;
    *sink += acc;
    return el;
}

int main(void) {
    const char *SCHEMA =
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, "
        "active INT, score INT, email TEXT)";

    volatile uint64_t sink = 0;
    char sql[256];

    /* ── AxiomDB setup (load once) ── */
    char axpath[] = "/tmp/axiomdb_cbench_XXXXXX";
    mkdtemp(axpath);
    char axdb[300];
    snprintf(axdb, sizeof(axdb), "%s/t", axpath);
    AxiomDb *adb = axiomdb_open(axdb);
    axiomdb_execute(adb, SCHEMA);
    axiomdb_execute(adb, "BEGIN");
    for (int i = 0; i < N_ROWS; i++) {
        snprintf(sql, sizeof(sql),
                 "INSERT INTO t VALUES (%d,'user_%06d',%d,%d,%d,'u%d@b.local')",
                 i, i, 18 + i % 62, i % 2 == 0, 100 + i % 1000, i);
        axiomdb_execute(adb, sql);
    }
    axiomdb_execute(adb, "COMMIT");

    /* ── SQLite setup (load once) ── */
    char sqpath[300];
    snprintf(sqpath, sizeof(sqpath), "%s/s.db", axpath);
    sqlite3 *sdb;
    sqlite3_open(sqpath, &sdb);
    sqlite3_exec(sdb, "PRAGMA journal_mode=WAL", 0, 0, 0);
    sqlite3_exec(sdb, "PRAGMA synchronous=FULL", 0, 0, 0);
    sqlite3_exec(sdb, SCHEMA, 0, 0, 0);
    sqlite3_exec(sdb, "BEGIN", 0, 0, 0);
    for (int i = 0; i < N_ROWS; i++) {
        snprintf(sql, sizeof(sql),
                 "INSERT INTO t VALUES (%d,'user_%06d',%d,%d,%d,'u%d@b.local')",
                 i, i, 18 + i % 62, i % 2 == 0, 100 + i % 1000, i);
        sqlite3_exec(sdb, sql, 0, 0, 0);
    }
    sqlite3_exec(sdb, "COMMIT", 0, 0, 0);

    /* ── Timed loops ── */
    double sq[ITERS], ax[ITERS], pk[ITERS], cu[ITERS];
    for (int k = 0; k < ITERS + WARMUP; k++) {
        double s = bench_sqlite(sdb, &sink);
        double a = bench_axiom_percell(adb, &sink);
        double p = bench_axiom_packed(adb, &sink);
        double c = bench_axiom_cursor(adb, &sink);
        if (k >= WARMUP) {
            sq[k - WARMUP] = s;
            ax[k - WARMUP] = a;
            pk[k - WARMUP] = p;
            cu[k - WARMUP] = c;
        }
    }

    double msq = median(sq, ITERS), max = median(ax, ITERS);
    double mpk = median(pk, ITERS), mcu = median(cu, ITERS);
    printf("C read benchmark — 10K x 6, materialize every cell (median of %d)\n\n", ITERS);
    printf("  SQLite C API (sqlite3_column_*):    %6.2f ms   1.00x  (baseline)\n", msq);
    printf("  AxiomDB C API (per-cell accessors): %6.2f ms   %.2fx\n", max, max / msq);
    printf("  AxiomDB C API (packed buffer):      %6.2f ms   %.2fx\n", mpk, mpk / msq);
    printf("  AxiomDB C API (Tier 1 cursor):      %6.2f ms   %.2fx\n", mcu, mcu / msq);
    printf("\n  (sink=%llu — prevents dead-code elimination)\n",
           (unsigned long long)sink);

    axiomdb_close(adb);
    sqlite3_close(sdb);
    return 0;
}
