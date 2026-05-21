import XCTest
@testable import AxiomDB

final class AxiomDBTests: XCTestCase {
    private func tmpDB() throws -> AxiomDB {
        let path = FileManager.default.temporaryDirectory
            .appendingPathComponent("axiomdb-\(UUID().uuidString).db").path
        return try AxiomDB(path)
    }

    func testBasicTypes() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE t (id INT, name TEXT, score REAL, m INT)")
        try db.execute("INSERT INTO t VALUES (1, 'alice', 3.5, NULL)")
        try db.execute("INSERT INTO t VALUES (2, 'héllo', 2.25, 99)")

        let rows = try db.queryTuples("SELECT id, name, score, m FROM t ORDER BY id")
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows[0], [.int(1), .text("alice"), .double(3.5), .null])
        XCTAssertEqual(rows[1], [.int(2), .text("héllo"), .double(2.25), .int(99)])
    }

    func testQueryDict() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE t (id INT, name TEXT)")
        try db.execute("INSERT INTO t VALUES (7, 'bob')")
        let rows = try db.query("SELECT id, name FROM t")
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0]["id"], .int(7))
        XCTAssertEqual(rows[0]["name"], .text("bob"))
    }

    func testWithColumns() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE t (id INT, name TEXT)")
        try db.execute("INSERT INTO t VALUES (1, 'x')")
        let (cols, rows) = try db.queryWithColumns("SELECT id, name FROM t")
        XCTAssertEqual(cols, ["id", "name"])
        XCTAssertEqual(rows[0], [.int(1), .text("x")])
    }

    func testEmptyResult() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE t (id INT)")
        try db.execute("INSERT INTO t VALUES (1)")
        let rows = try db.queryTuples("SELECT id FROM t WHERE id = 999")
        XCTAssertTrue(rows.isEmpty)
    }

    func testBlob() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE b (data BLOB)")
        try db.execute("INSERT INTO b VALUES (X'010203')")
        let rows = try db.queryTuples("SELECT data FROM b")
        XCTAssertEqual(rows[0][0], .blob([1, 2, 3]))
    }

    func testTransactions() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE t (id INT)")
        try db.begin()
        try db.execute("INSERT INTO t VALUES (1)")
        try db.commit()
        XCTAssertEqual(try db.queryTuples("SELECT * FROM t").count, 1)
        try db.begin()
        try db.execute("INSERT INTO t VALUES (2)")
        try db.rollback()
        XCTAssertEqual(try db.queryTuples("SELECT * FROM t").count, 1)
    }

    func testBadSQLThrows() throws {
        let db = try tmpDB()
        defer { db.close() }
        XCTAssertThrowsError(try db.queryTuples("SELECT * FROM nonexistent"))
    }

    func testParamBinding() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE t (id INT, name TEXT, score REAL, avatar BLOB)")
        try db.execute("INSERT INTO t VALUES (?, ?, ?, ?)", [.int(1), .text("alice"), .double(3.5), .null])
        try db.execute(
            "INSERT INTO t VALUES (?, ?, ?, ?)",
            [.int(2), .text("héllo"), .double(2.25), .blob([9, 8, 7])])

        let rows = try db.queryTuples(
            "SELECT id, name, score, avatar FROM t WHERE id = ?", [.int(2)])
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0], [.int(2), .text("héllo"), .double(2.25), .blob([9, 8, 7])])

        let maps = try db.query("SELECT id, name FROM t WHERE name = ?", [.text("alice")])
        XCTAssertEqual(maps.count, 1)
        XCTAssertEqual(maps[0]["id"], .int(1))
    }

    func testParamInjectionSafe() throws {
        let db = try tmpDB()
        defer { db.close() }
        try db.execute("CREATE TABLE t (id INT, name TEXT)")
        let evil = "x'; DROP TABLE t; --"
        try db.execute("INSERT INTO t VALUES (?, ?)", [.int(1), .text(evil)])
        let rows = try db.queryTuples("SELECT name FROM t WHERE id = ?", [.int(1)])
        XCTAssertEqual(rows[0][0], .text(evil))
        // table survived — value bound, not executed
        XCTAssertEqual(try db.queryTuples("SELECT COUNT(*) FROM t")[0][0], .int(1))
    }
}
