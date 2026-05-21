/// AxiomDB embedded database — Dart/Flutter binding via `dart:ffi`.
///
/// Result materialization uses the packed-buffer path: one FFI call returns the
/// whole result as a single buffer, decoded once in Dart with `ByteData`.
/// `dart:ffi` is a direct FFI (no marshalling), and Flutter release builds are
/// AOT-compiled, so this is fast, allocation-light decoding.
///
/// ```dart
/// final db = AxiomDB('./myapp.db'); // or ':memory:'
/// db.execute("CREATE TABLE users (id INT, name TEXT)");
/// db.execute("INSERT INTO users VALUES (1, 'Alice')");
/// final rows = db.queryTuples("SELECT * FROM users"); // [[1, 'Alice']]
/// db.close();
/// ```
library;

import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

final class _AxiomDb extends Opaque {}

// ── C signatures ──────────────────────────────────────────────────────────────

typedef _OpenC = Pointer<_AxiomDb> Function(Pointer<Utf8>);
typedef _OpenD = Pointer<_AxiomDb> Function(Pointer<Utf8>);
typedef _ExecC = Int64 Function(Pointer<_AxiomDb>, Pointer<Utf8>);
typedef _ExecD = int Function(Pointer<_AxiomDb>, Pointer<Utf8>);
typedef _QueryPackedC = Pointer<Uint8> Function(Pointer<_AxiomDb>, Pointer<Utf8>, Pointer<Size>);
typedef _QueryPackedD = Pointer<Uint8> Function(Pointer<_AxiomDb>, Pointer<Utf8>, Pointer<Size>);
typedef _PackedFreeC = Void Function(Pointer<Uint8>, Size);
typedef _PackedFreeD = void Function(Pointer<Uint8>, int);
typedef _CloseC = Void Function(Pointer<_AxiomDb>);
typedef _CloseD = void Function(Pointer<_AxiomDb>);
typedef _LastErrorC = Pointer<Utf8> Function(Pointer<_AxiomDb>);
typedef _LastErrorD = Pointer<Utf8> Function(Pointer<_AxiomDb>);
typedef _ExecParamsC = Int64 Function(Pointer<_AxiomDb>, Pointer<Utf8>, Pointer<Uint8>, Size);
typedef _ExecParamsD = int Function(Pointer<_AxiomDb>, Pointer<Utf8>, Pointer<Uint8>, int);
typedef _QueryParamsC = Pointer<Uint8> Function(
    Pointer<_AxiomDb>, Pointer<Utf8>, Pointer<Uint8>, Size, Pointer<Size>);
typedef _QueryParamsD = Pointer<Uint8> Function(
    Pointer<_AxiomDb>, Pointer<Utf8>, Pointer<Uint8>, int, Pointer<Size>);

// ── Library loading ───────────────────────────────────────────────────────────

DynamicLibrary _loadLibrary() {
  final ext = Platform.isMacOS
      ? 'dylib'
      : Platform.isWindows
          ? 'dll'
          : 'so';
  final name = 'libaxiomdb_embedded.$ext';
  final candidates = <String>[
    name,
    '../../target/release/$name',
    '../../target/debug/$name',
    '/usr/local/lib/$name',
  ];
  for (final p in candidates) {
    if (File(p).existsSync()) return DynamicLibrary.open(p);
  }
  return DynamicLibrary.open(name);
}

final DynamicLibrary _lib = _loadLibrary();
final _open = _lib.lookupFunction<_OpenC, _OpenD>('axiomdb_open');
final _exec = _lib.lookupFunction<_ExecC, _ExecD>('axiomdb_execute');
final _queryPacked = _lib.lookupFunction<_QueryPackedC, _QueryPackedD>('axiomdb_query_packed');
final _packedFree = _lib.lookupFunction<_PackedFreeC, _PackedFreeD>('axiomdb_packed_free');
final _closeFn = _lib.lookupFunction<_CloseC, _CloseD>('axiomdb_close');
final _lastErrorFn = _lib.lookupFunction<_LastErrorC, _LastErrorD>('axiomdb_last_error');
final _execParams = _lib.lookupFunction<_ExecParamsC, _ExecParamsD>('axiomdb_execute_params');
final _queryPackedParams =
    _lib.lookupFunction<_QueryParamsC, _QueryParamsD>('axiomdb_query_packed_params');

const int _packedMagic = 0x41584D31; // "AXM1"

/// Error thrown by AxiomDB operations.
class AxiomDBException implements Exception {
  final String message;
  AxiomDBException(this.message);
  @override
  String toString() => 'AxiomDBException: $message';
}

/// An in-process AxiomDB database. Use one instance per isolate/thread.
class AxiomDB {
  Pointer<_AxiomDb> _ptr;

  AxiomDB._(this._ptr);

  /// Opens or creates a database at [path] (`:memory:` for ephemeral).
  factory AxiomDB(String path) {
    final cpath = path.toNativeUtf8();
    try {
      final ptr = _open(cpath);
      if (ptr == nullptr) {
        throw AxiomDBException('failed to open database at $path');
      }
      return AxiomDB._(ptr);
    } finally {
      malloc.free(cpath);
    }
  }

  /// Closes the database. Safe to call multiple times.
  void close() {
    if (_ptr != nullptr) {
      _closeFn(_ptr);
      _ptr = nullptr;
    }
  }

  /// Returns the last error message, or null.
  String? lastError() {
    if (_ptr == nullptr) return null;
    final msg = _lastErrorFn(_ptr);
    return msg == nullptr ? null : msg.toDartString();
  }

  void _checkOpen() {
    if (_ptr == nullptr) throw AxiomDBException('database is closed');
  }

  /// Executes a DDL/DML statement. Returns rows affected.
  ///
  /// Pass [params] to bind values positionally (`?` placeholders) — this is the
  /// safe way to handle untrusted input (no SQL injection, no manual escaping):
  ///
  /// ```dart
  /// db.execute('INSERT INTO users VALUES (?, ?)', [1, 'Alice']);
  /// ```
  ///
  /// Each param maps to a cell type: `null`, `int`, `double`, `String`, or
  /// `List<int>`/`Uint8List` (BLOB).
  int execute(String sql, [List<Object?>? params]) {
    _checkOpen();
    final csql = sql.toNativeUtf8();
    try {
      if (params == null || params.isEmpty) {
        final n = _exec(_ptr, csql);
        if (n < 0) throw AxiomDBException(lastError() ?? 'execute failed');
        return n;
      }
      final encoded = _encodeParams(params);
      final pptr = malloc<Uint8>(encoded.length);
      try {
        pptr.asTypedList(encoded.length).setAll(0, encoded);
        final n = _execParams(_ptr, csql, pptr, encoded.length);
        if (n < 0) throw AxiomDBException(lastError() ?? 'execute failed');
        return n;
      } finally {
        malloc.free(pptr);
      }
    } finally {
      malloc.free(csql);
    }
  }

  /// Executes a SELECT, returning rows as lists (fastest). Cell types:
  /// int, double, String, Uint8List (BLOB), or null.
  ///
  /// Pass [params] to bind `?` placeholders safely (see [execute]).
  List<List<Object?>> queryTuples(String sql, [List<Object?>? params]) =>
      _queryPackedParse(sql, params).$2;

  /// Executes a SELECT, returning (columnNames, rows).
  ///
  /// Pass [params] to bind `?` placeholders safely (see [execute]).
  (List<String>, List<List<Object?>>) queryWithColumns(String sql, [List<Object?>? params]) =>
      _queryPackedParse(sql, params);

  /// Executes a SELECT, returning rows as maps (column name → value).
  ///
  /// Pass [params] to bind `?` placeholders safely (see [execute]).
  List<Map<String, Object?>> query(String sql, [List<Object?>? params]) {
    final (cols, rows) = _queryPackedParse(sql, params);
    return rows.map((row) {
      final m = <String, Object?>{};
      for (var i = 0; i < cols.length; i++) {
        m[cols[i]] = row[i];
      }
      return m;
    }).toList();
  }

  int begin() => execute('BEGIN');
  int commit() => execute('COMMIT');
  int rollback() => execute('ROLLBACK');

  (List<String>, List<List<Object?>>) _queryPackedParse(String sql, [List<Object?>? params]) {
    _checkOpen();
    final csql = sql.toNativeUtf8();
    final lenPtr = malloc<Size>();
    Pointer<Uint8> pptr = nullptr;
    var pLen = 0;
    try {
      Pointer<Uint8> ptr;
      if (params == null || params.isEmpty) {
        ptr = _queryPacked(_ptr, csql, lenPtr);
      } else {
        final encoded = _encodeParams(params);
        pLen = encoded.length;
        pptr = malloc<Uint8>(pLen);
        pptr.asTypedList(pLen).setAll(0, encoded);
        ptr = _queryPackedParams(_ptr, csql, pptr, pLen, lenPtr);
      }
      if (ptr == nullptr) throw AxiomDBException(lastError() ?? 'query failed');
      final len = lenPtr.value;
      // One copy of the native buffer into an owned Uint8List, then free + parse.
      final bytes = Uint8List.fromList(ptr.asTypedList(len));
      _packedFree(ptr, len);
      return _parsePacked(bytes);
    } finally {
      malloc.free(csql);
      malloc.free(lenPtr);
      if (pptr != nullptr) malloc.free(pptr);
    }
  }
}

/// Serializes positional [params] into the AxiomDB param buffer:
/// `u32 count`, then per-param `{u8 tag, payload}`. Tags mirror the packed
/// cell encoding: 0=null, 1=int(i64), 2=double(f64), 3=text, 4=blob.
Uint8List _encodeParams(List<Object?> params) {
  final out = BytesBuilder(copy: false);
  final hdr = ByteData(4)..setUint32(0, params.length, Endian.little);
  out.add(hdr.buffer.asUint8List());
  for (final p in params) {
    if (p == null) {
      out.addByte(0);
    } else if (p is bool) {
      out.addByte(1);
      final b = ByteData(8)..setInt64(0, p ? 1 : 0, Endian.little);
      out.add(b.buffer.asUint8List());
    } else if (p is int) {
      out.addByte(1);
      final b = ByteData(8)..setInt64(0, p, Endian.little);
      out.add(b.buffer.asUint8List());
    } else if (p is double) {
      out.addByte(2);
      final b = ByteData(8)..setFloat64(0, p, Endian.little);
      out.add(b.buffer.asUint8List());
    } else if (p is String) {
      out.addByte(3);
      final bytes = utf8.encode(p);
      final l = ByteData(4)..setUint32(0, bytes.length, Endian.little);
      out.add(l.buffer.asUint8List());
      out.add(bytes);
    } else if (p is Uint8List) {
      out.addByte(4);
      final l = ByteData(4)..setUint32(0, p.length, Endian.little);
      out.add(l.buffer.asUint8List());
      out.add(p);
    } else if (p is List<int>) {
      out.addByte(4);
      final bytes = Uint8List.fromList(p);
      final l = ByteData(4)..setUint32(0, bytes.length, Endian.little);
      out.add(l.buffer.asUint8List());
      out.add(bytes);
    } else {
      throw AxiomDBException('unsupported param type: ${p.runtimeType}');
    }
  }
  return out.toBytes();
}

/// Decodes a packed (AXM1) buffer. Dart's `int` is 64-bit, so integers are exact.
(List<String>, List<List<Object?>>) _parsePacked(Uint8List buf) {
  final bd = ByteData.sublistView(buf);
  final magic = bd.getUint32(0, Endian.little);
  if (magic != _packedMagic) throw AxiomDBException('corrupt packed buffer');
  var off = 4;
  final nCols = bd.getUint32(off, Endian.little);
  off += 4;
  final nRows = bd.getUint64(off, Endian.little);
  off += 8;

  final cols = <String>[];
  for (var c = 0; c < nCols; c++) {
    final l = bd.getUint32(off, Endian.little);
    off += 4;
    cols.add(utf8.decode(buf.sublist(off, off + l)));
    off += l;
  }

  final rows = <List<Object?>>[];
  for (var r = 0; r < nRows; r++) {
    final row = List<Object?>.filled(nCols, null);
    for (var c = 0; c < nCols; c++) {
      final tag = buf[off];
      off += 1;
      if (tag == 1) {
        row[c] = bd.getInt64(off, Endian.little);
        off += 8;
      } else if (tag == 3) {
        final l = bd.getUint32(off, Endian.little);
        off += 4;
        row[c] = utf8.decode(buf.sublist(off, off + l));
        off += l;
      } else if (tag == 2) {
        row[c] = bd.getFloat64(off, Endian.little);
        off += 8;
      } else if (tag == 4) {
        final l = bd.getUint32(off, Endian.little);
        off += 4;
        row[c] = Uint8List.sublistView(buf, off, off + l);
        off += l;
      }
      // tag == 0 → null (already filled)
    }
    rows.add(row);
  }
  return (cols, rows);
}
