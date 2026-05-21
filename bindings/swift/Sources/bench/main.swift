// Benchmark: AxiomDB (packed) vs the system SQLite3 C API, row materialization.
//   cd bindings/swift && swift run -c release bench
import AxiomDB
import Foundation
import SQLite3

let N = 10_000
let iters = 11
let warm = 3
let schema = "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, active INT, score INT, email TEXT)"

func ins(_ i: Int) -> String {
    "INSERT INTO t VALUES (\(i),'user_\(String(format: "%06d", i))',\(18 + i % 62),\(i % 2 == 0 ? 1 : 0),\(100 + i % 1000),'u\(i)@b.local')"
}

func median(_ xs: [Double]) -> Double { xs.sorted()[xs.count / 2] }

func bench(_ fn: () -> Void) -> Double {
    var ts = [Double]()
    for k in 0..<(iters + warm) {
        let t0 = DispatchTime.now().uptimeNanoseconds
        fn()
        let el = Double(DispatchTime.now().uptimeNanoseconds - t0) / 1_000_000.0
        if k >= warm { ts.append(el) }
    }
    return median(ts)
}

let dir = FileManager.default.temporaryDirectory.appendingPathComponent("axswift-\(UUID().uuidString)")
try! FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
defer { try? FileManager.default.removeItem(at: dir) }

// AxiomDB setup
let adb = try! AxiomDB(dir.appendingPathComponent("a.db").path)
try! adb.execute(schema)
try! adb.begin()
for i in 0..<N { try! adb.execute(ins(i)) }
try! adb.commit()

// SQLite setup (system libsqlite3)
var sdb: OpaquePointer?
sqlite3_open(dir.appendingPathComponent("s.db").path, &sdb)
sqlite3_exec(sdb, "PRAGMA journal_mode=WAL", nil, nil, nil)
sqlite3_exec(sdb, "PRAGMA synchronous=FULL", nil, nil, nil)
sqlite3_exec(sdb, schema, nil, nil, nil)
sqlite3_exec(sdb, "BEGIN", nil, nil, nil)
for i in 0..<N { sqlite3_exec(sdb, ins(i), nil, nil, nil) }
sqlite3_exec(sdb, "COMMIT", nil, nil, nil)

let SQLITE_TRANSIENT = unsafeBitCast(-1, to: sqlite3_destructor_type.self)
_ = SQLITE_TRANSIENT

// SQLite materialization: build the SAME [[AxiomValue]] structure AxiomDB
// returns (Swift String/Int64/Double per cell), so the comparison is fair.
func sqliteScan() {
    var stmt: OpaquePointer?
    sqlite3_prepare_v2(sdb, "SELECT * FROM t", -1, &stmt, nil)
    var rows = [[AxiomValue]]()
    rows.reserveCapacity(N)
    while sqlite3_step(stmt) == SQLITE_ROW {
        let cols = sqlite3_column_count(stmt)
        var row = [AxiomValue]()
        row.reserveCapacity(Int(cols))
        for c in 0..<cols {
            switch sqlite3_column_type(stmt, c) {
            case SQLITE_INTEGER: row.append(.int(sqlite3_column_int64(stmt, c)))
            case SQLITE_TEXT: row.append(.text(String(cString: sqlite3_column_text(stmt, c))))
            case SQLITE_FLOAT: row.append(.double(sqlite3_column_double(stmt, c)))
            default: row.append(.null)
            }
        }
        rows.append(row)
    }
    sqlite3_finalize(stmt)
    precondition(rows.count == N)
}

func axiomScan() {
    let rows = try! adb.queryTuples("SELECT * FROM t")
    precondition(rows.count == N)
}

// correctness spot-check
let r0 = try! adb.queryTuples("SELECT * FROM t").first!
precondition(r0[0] == .int(0) && r0[1] == .text("user_000000"))

let s = bench(sqliteScan)
let a = bench(axiomScan)
print("Swift read benchmark — 10K x 6, materialize every cell (median of \(iters))\n")
print(String(format: "  system SQLite3 (sqlite3_column_*): %6.2f ms   1.00x", s))
let faster = a < s ? "(FASTER)" : ""
print(String(format: "  AxiomDB queryTuples (packed):      %6.2f ms   %.2fx  %@", a, a / s, faster))
