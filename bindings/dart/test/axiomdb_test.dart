import 'dart:io';
import 'dart:typed_data';

import 'package:axiomdb/axiomdb.dart';
import 'package:test/test.dart';

AxiomDB tmpDB() {
  final dir = Directory.systemTemp.createTempSync('axiomdb-dart-');
  addTearDown(() => dir.deleteSync(recursive: true));
  return AxiomDB('${dir.path}/t.db');
}

void main() {
  test('basic types', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT, name TEXT, score REAL, m INT)');
    db.execute("INSERT INTO t VALUES (1, 'alice', 3.5, NULL)");
    db.execute("INSERT INTO t VALUES (2, 'héllo', 2.25, 99)");

    final rows = db.queryTuples('SELECT id, name, score, m FROM t ORDER BY id');
    expect(rows.length, 2);
    expect(rows[0], [1, 'alice', 3.5, null]);
    expect(rows[1], [2, 'héllo', 2.25, 99]);
  });

  test('query maps', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT, name TEXT)');
    db.execute("INSERT INTO t VALUES (7, 'bob')");
    final rows = db.query('SELECT id, name FROM t');
    expect(rows, [
      {'id': 7, 'name': 'bob'}
    ]);
  });

  test('query with columns', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT, name TEXT)');
    db.execute("INSERT INTO t VALUES (1, 'x')");
    final (cols, rows) = db.queryWithColumns('SELECT id, name FROM t');
    expect(cols, ['id', 'name']);
    expect(rows[0], [1, 'x']);
  });

  test('empty result', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT)');
    db.execute('INSERT INTO t VALUES (1)');
    expect(db.queryTuples('SELECT id FROM t WHERE id = 999'), isEmpty);
  });

  test('blob', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE b (data BLOB)');
    db.execute("INSERT INTO b VALUES (X'010203')");
    final blob = db.queryTuples('SELECT data FROM b')[0][0];
    expect(blob, isA<Uint8List>());
    expect(blob, [1, 2, 3]);
  });

  test('transactions', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT)');
    db.begin();
    db.execute('INSERT INTO t VALUES (1)');
    db.commit();
    expect(db.queryTuples('SELECT * FROM t').length, 1);
    db.begin();
    db.execute('INSERT INTO t VALUES (2)');
    db.rollback();
    expect(db.queryTuples('SELECT * FROM t').length, 1);
  });

  test('bad sql throws', () {
    final db = tmpDB();
    addTearDown(db.close);
    expect(() => db.queryTuples('SELECT * FROM nonexistent'), throwsA(isA<AxiomDBException>()));
  });

  test('parameter binding — execute + query', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT, name TEXT, score REAL, avatar BLOB)');
    db.execute('INSERT INTO t VALUES (?, ?, ?, ?)', [1, 'alice', 3.5, null]);
    db.execute('INSERT INTO t VALUES (?, ?, ?, ?)', [2, 'bøb', 2.25, Uint8List.fromList([9, 8, 7])]);

    final rows = db.queryTuples('SELECT id, name, score, avatar FROM t WHERE id = ?', [2]);
    expect(rows.length, 1);
    expect(rows[0][0], 2);
    expect(rows[0][1], 'bøb');
    expect(rows[0][2], 2.25);
    expect(rows[0][3], [9, 8, 7]);
  });

  test('parameter binding — injection-safe', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT, name TEXT)');
    // A value containing SQL is treated as a literal, not executed.
    const evil = "x'; DROP TABLE t; --";
    db.execute('INSERT INTO t VALUES (?, ?)', [1, evil]);
    final rows = db.queryTuples('SELECT name FROM t WHERE id = ?', [1]);
    expect(rows[0][0], evil);
    // Table still exists.
    expect(db.queryTuples('SELECT COUNT(*) FROM t').first[0], 1);
  });

  test('parameter binding — null filter', () {
    final db = tmpDB();
    addTearDown(db.close);
    db.execute('CREATE TABLE t (id INT, name TEXT)');
    db.execute('INSERT INTO t VALUES (?, ?)', [1, 'a']);
    db.execute('INSERT INTO t VALUES (?, ?)', [2, 'b']);
    final maps = db.query('SELECT id, name FROM t WHERE name = ?', ['b']);
    expect(maps, [
      {'id': 2, 'name': 'b'}
    ]);
  });
}
