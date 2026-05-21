// Benchmark: AxiomDB (packed) vs the sqlite3 Dart package (FFI), materialization.
//   cd bindings/dart && dart run bench/bench.dart
import 'dart:io';
import 'package:axiomdb/axiomdb.dart';
import 'package:sqlite3/sqlite3.dart' as sq;

const N = 10000;
const schema =
    'CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, active INT, score INT, email TEXT)';
String ins(int i) =>
    "INSERT INTO t VALUES ($i,'user_${i.toString().padLeft(6, '0')}',${18 + i % 62},${i % 2 == 0 ? 1 : 0},${100 + i % 1000},'u$i@b.local')";

double median(List<double> xs) {
  xs.sort();
  return xs[xs.length ~/ 2];
}

double bench(void Function() fn, {int iters = 11, int warm = 3}) {
  final ts = <double>[];
  for (var k = 0; k < iters + warm; k++) {
    final sw = Stopwatch()..start();
    fn();
    sw.stop();
    if (k >= warm) ts.add(sw.elapsedMicroseconds / 1000.0);
  }
  return median(ts);
}

void main() {
  final dir = Directory.systemTemp.createTempSync('axdart-bench-');
  try {
    final adb = AxiomDB('${dir.path}/a.db');
    adb.execute(schema);
    adb.begin();
    for (var i = 0; i < N; i++) {
      adb.execute(ins(i));
    }
    adb.commit();

    final sdb = sq.sqlite3.open('${dir.path}/s.db');
    sdb.execute('PRAGMA journal_mode=WAL');
    sdb.execute('PRAGMA synchronous=FULL');
    sdb.execute(schema);
    sdb.execute('BEGIN');
    for (var i = 0; i < N; i++) {
      sdb.execute(ins(i));
    }
    sdb.execute('COMMIT');

    // correctness spot-check
    final r0 = adb.queryTuples('SELECT * FROM t').first;
    if (r0[0] != 0 || r0[1] != 'user_000000') throw 'correctness';

    final s = bench(() {
      final rs = sdb.select('SELECT * FROM t');
      if (rs.length != N) throw 'sqlite';
    });
    final a = bench(() {
      final r = adb.queryTuples('SELECT * FROM t');
      if (r.length != N) throw 'axiom';
    });
    print('Dart read benchmark — 10K x 6, materialize every cell (median of 11)\n');
    print('  sqlite3 package (FFI):         ${s.toStringAsFixed(2)} ms   1.00x');
    final faster = a < s ? '(FASTER)' : '';
    print('  AxiomDB queryTuples (packed):  ${a.toStringAsFixed(2)} ms   ${(a / s).toStringAsFixed(2)}x  $faster');
    adb.close();
    sdb.dispose();
  } finally {
    dir.deleteSync(recursive: true);
  }
}
