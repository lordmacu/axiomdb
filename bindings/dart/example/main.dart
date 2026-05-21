// Runnable example for the AxiomDB Dart binding.
//
//   cargo build --release -p axiomdb-embedded   # build the shared library
//   cd bindings/dart && dart pub get
//   dart run example/main.dart
import 'dart:typed_data';
import 'package:axiomdb/axiomdb.dart';

void main() {
  // 1. Open (use a file path, or ':memory:' for an ephemeral database).
  final db = AxiomDB(':memory:');

  try {
    // 2. DDL + DML. execute() returns rows affected.
    db.execute('''
      CREATE TABLE users (
        id    INT PRIMARY KEY,
        name  TEXT,
        score REAL,
        avatar BLOB
      )
    ''');
    db.execute("INSERT INTO users VALUES (1, 'Alice', 9.5, NULL)");
    db.execute("INSERT INTO users VALUES (2, 'Bob',   7.0, NULL)");

    // 3. Bulk insert inside ONE transaction (much faster than autocommit).
    db.begin();
    for (var i = 3; i <= 1000; i++) {
      db.execute("INSERT INTO users VALUES ($i, 'user_$i', ${i % 10}, NULL)");
    }
    db.commit();

    // 4a. queryTuples → List<List<Object?>> (fastest; positional).
    //     Cell types: int, double, String, Uint8List (BLOB), or null.
    final rows = db.queryTuples('SELECT id, name, score FROM users WHERE id <= 2 ORDER BY id');
    for (final row in rows) {
      final id = row[0] as int;
      final name = row[1] as String;
      final score = row[2] as double;
      print('tuple → id=$id name=$name score=$score');
    }

    // 4b. query → List<Map<String,Object?>> (by column name; ergonomic).
    final recs = db.query('SELECT id, name FROM users WHERE id = 1');
    print('map   → ${recs.first}'); // {id: 1, name: Alice}

    // 4c. queryWithColumns → (columns, rows) when you need both.
    final (cols, data) = db.queryWithColumns('SELECT id, name FROM users LIMIT 1');
    print('cols  → $cols  firstRow → ${data.first}');

    // 5. Aggregates / scalars come back as normal rows.
    final count = db.queryTuples('SELECT COUNT(*) FROM users').first[0] as int;
    print('count → $count');

    // 6. NULL is Dart null; BLOB is Uint8List.
    final avatar = db.queryTuples('SELECT avatar FROM users WHERE id = 1').first[0];
    print('avatar is null? ${avatar == null}'); // true (inserted NULL)
    db.execute("UPDATE users SET avatar = X'010203' WHERE id = 1");
    final blob = db.queryTuples('SELECT avatar FROM users WHERE id = 1').first[0] as Uint8List;
    print('blob  → $blob'); // [1, 2, 3]

    // 7. Transactions: rollback discards.
    db.begin();
    db.execute("INSERT INTO users VALUES (9999, 'temp', 0, NULL)");
    db.rollback();
    print('after rollback count → ${db.queryTuples('SELECT COUNT(*) FROM users').first[0]}');

    // 8. Errors throw AxiomDBException.
    try {
      db.queryTuples('SELECT * FROM does_not_exist');
    } on AxiomDBException catch (e) {
      print('caught → ${e.message}');
    }

    // 9. Parameter binding (?, positional). The SAFE way to handle untrusted
    //    input — real prepared-statement binding, no string interpolation, no
    //    SQL injection. Params: null, int, double, String, Uint8List (BLOB).
    db.execute('INSERT INTO users VALUES (?, ?, ?, ?)', [
      2001,
      "Robert'); DROP TABLE users; --", // treated as a literal, not executed
      8.0,
      Uint8List.fromList([0xCA, 0xFE]),
    ]);
    final bound = db.query('SELECT id, name FROM users WHERE id = ?', [2001]);
    print('param → ${bound.first}'); // {id: 2001, name: Robert'); DROP TABLE users; --}
    print('table survived → count=${db.queryTuples('SELECT COUNT(*) FROM users').first[0]}');
  } finally {
    db.close(); // release the engine; safe to call more than once
  }
}
