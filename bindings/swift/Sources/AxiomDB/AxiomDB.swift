import CAxiomDB
import Foundation

/// A single result cell.
public enum AxiomValue: Equatable, Sendable {
    case null
    case int(Int64)
    case double(Double)
    case text(String)
    case blob([UInt8])
}

public struct AxiomError: Error, CustomStringConvertible {
    public let message: String
    public var description: String { "AxiomDB: \(message)" }
}

/// An in-process AxiomDB database.
///
/// Materialization uses the packed-buffer path: one FFI call returns the whole
/// result as a single buffer, parsed once in Swift. Swift's C interop is direct
/// (no marshalling), and the parser uses `loadUnaligned`, so this is compiled,
/// allocation-light decoding.
///
/// Not safe for concurrent use from multiple threads; use one instance per
/// thread (like `sqlite3` with `check_same_thread`).
public final class AxiomDB {
    private var ptr: OpaquePointer?

    private static let packedMagic: UInt32 = 0x4158_4D31  // "AXM1"

    /// Opens or creates a database at `path` (`":memory:"` for ephemeral).
    public init(_ path: String) throws {
        ptr = axiomdb_open(path)
        if ptr == nil { throw AxiomError(message: "failed to open database at \(path)") }
    }

    deinit { close() }

    /// Closes the database. Safe to call multiple times.
    public func close() {
        if let p = ptr {
            axiomdb_close(p)
            ptr = nil
        }
    }

    /// Returns the last error message, or `nil`.
    public func lastError() -> String? {
        guard let p = ptr, let msg = axiomdb_last_error(p) else { return nil }
        return String(cString: msg)
    }

    /// Executes a DDL/DML statement. Returns rows affected.
    ///
    /// Pass `params` to bind `?` placeholders with real prepared-statement
    /// binding — no string interpolation, no SQL injection:
    ///
    /// ```swift
    /// try db.execute("INSERT INTO t VALUES (?, ?)", [.int(1), .text("Alice")])
    /// ```
    @discardableResult
    public func execute(_ sql: String, _ params: [AxiomValue] = []) throws -> Int64 {
        guard let p = ptr else { throw AxiomError(message: "database is closed") }
        let n: Int64
        if params.isEmpty {
            n = axiomdb_execute(p, sql)
        } else {
            let enc = Self.encodeParams(params)
            n = enc.withUnsafeBufferPointer { bp in
                axiomdb_execute_params(p, sql, bp.baseAddress, enc.count)
            }
        }
        if n < 0 { throw AxiomError(message: lastError() ?? "execute failed") }
        return n
    }

    /// Executes a SELECT, returning rows as arrays (fastest).
    /// Pass `params` to bind `?` placeholders safely (see `execute`).
    public func queryTuples(_ sql: String, _ params: [AxiomValue] = []) throws -> [[AxiomValue]] {
        try queryPacked(sql, params).rows
    }

    /// Executes a SELECT, returning column names plus rows.
    /// Pass `params` to bind `?` placeholders safely (see `execute`).
    public func queryWithColumns(_ sql: String, _ params: [AxiomValue] = []) throws -> (
        columns: [String], rows: [[AxiomValue]]
    ) {
        try queryPacked(sql, params)
    }

    /// Executes a SELECT, returning rows as dictionaries (column name → value).
    /// Pass `params` to bind `?` placeholders safely (see `execute`).
    public func query(_ sql: String, _ params: [AxiomValue] = []) throws -> [[String: AxiomValue]] {
        let (cols, rows) = try queryPacked(sql, params)
        return rows.map { row in
            var dict = [String: AxiomValue](minimumCapacity: cols.count)
            for (i, name) in cols.enumerated() { dict[name] = row[i] }
            return dict
        }
    }

    @discardableResult public func begin() throws -> Int64 { try execute("BEGIN") }
    @discardableResult public func commit() throws -> Int64 { try execute("COMMIT") }
    @discardableResult public func rollback() throws -> Int64 { try execute("ROLLBACK") }

    // MARK: - Internal

    private func queryPacked(_ sql: String, _ params: [AxiomValue] = []) throws -> (
        columns: [String], rows: [[AxiomValue]]
    ) {
        guard let p = ptr else { throw AxiomError(message: "database is closed") }
        var len: size_t = 0
        let result: UnsafeMutablePointer<UInt8>?
        if params.isEmpty {
            result = axiomdb_query_packed(p, sql, &len)
        } else {
            let enc = Self.encodeParams(params)
            result = enc.withUnsafeBufferPointer { bp in
                axiomdb_query_packed_params(p, sql, bp.baseAddress, enc.count, &len)
            }
        }
        guard let buf = result else {
            throw AxiomError(message: lastError() ?? "query failed")
        }
        defer { axiomdb_packed_free(buf, len) }  // freed after parse
        return Self.parsePacked(UnsafeRawPointer(buf), Int(len))
    }

    /// Serializes positional `params` into the AxiomDB param buffer: `u32 count`,
    /// then per-param `{u8 tag, payload}`. Tags mirror the packed cell encoding:
    /// 0=null, 1=int(i64), 2=double(f64), 3=text, 4=blob.
    static func encodeParams(_ params: [AxiomValue]) -> [UInt8] {
        var buf = [UInt8]()
        buf.reserveCapacity(4 + params.count * 9)
        func putU32(_ v: UInt32) { withUnsafeBytes(of: v.littleEndian) { buf.append(contentsOf: $0) } }
        func putI64(_ v: Int64) { withUnsafeBytes(of: v.littleEndian) { buf.append(contentsOf: $0) } }
        func putF64(_ v: Double) {
            withUnsafeBytes(of: v.bitPattern.littleEndian) { buf.append(contentsOf: $0) }
        }
        putU32(UInt32(params.count))
        for p in params {
            switch p {
            case .null:
                buf.append(0)
            case .int(let i):
                buf.append(1)
                putI64(i)
            case .double(let d):
                buf.append(2)
                putF64(d)
            case .text(let s):
                buf.append(3)
                let bytes = Array(s.utf8)
                putU32(UInt32(bytes.count))
                buf.append(contentsOf: bytes)
            case .blob(let b):
                buf.append(4)
                putU32(UInt32(b.count))
                buf.append(contentsOf: b)
            }
        }
        return buf
    }

    /// Decodes a packed (AXM1) buffer. Reads via `loadUnaligned` (native LE on
    /// arm64/x86); strings via `String(decoding:as:)`. Every value is copied
    /// into Swift storage, so the result is valid after the buffer is freed.
    static func parsePacked(_ base: UnsafeRawPointer, _ len: Int) -> (columns: [String], rows: [[AxiomValue]]) {
        var off = 0
        func u32() -> Int { let v = base.loadUnaligned(fromByteOffset: off, as: UInt32.self); off += 4; return Int(v) }
        func u64() -> Int { let v = base.loadUnaligned(fromByteOffset: off, as: UInt64.self); off += 8; return Int(v) }
        func i64() -> Int64 { let v = base.loadUnaligned(fromByteOffset: off, as: Int64.self); off += 8; return v }
        func f64() -> Double { let v = base.loadUnaligned(fromByteOffset: off, as: Double.self); off += 8; return v }
        func text(_ n: Int) -> String {
            let s = String(decoding: UnsafeRawBufferPointer(start: base + off, count: n), as: UTF8.self)
            off += n
            return s
        }
        func blob(_ n: Int) -> [UInt8] {
            let arr = [UInt8](UnsafeRawBufferPointer(start: base + off, count: n))
            off += n
            return arr
        }

        let magic = base.loadUnaligned(fromByteOffset: 0, as: UInt32.self)
        off = 4
        precondition(magic == packedMagic, "corrupt packed buffer")
        let nCols = u32()
        let nRows = u64()

        var columns = [String]()
        columns.reserveCapacity(nCols)
        for _ in 0..<nCols { let l = u32(); columns.append(text(l)) }

        var rows = [[AxiomValue]]()
        rows.reserveCapacity(nRows)
        for _ in 0..<nRows {
            var row = [AxiomValue]()
            row.reserveCapacity(nCols)
            for _ in 0..<nCols {
                let tag = base.load(fromByteOffset: off, as: UInt8.self)
                off += 1
                switch tag {
                case 1: row.append(.int(i64()))
                case 3: let l = u32(); row.append(.text(text(l)))
                case 2: row.append(.double(f64()))
                case 4: let l = u32(); row.append(.blob(blob(l)))
                default: row.append(.null)
                }
            }
            rows.append(row)
        }
        return (columns, rows)
    }
}
