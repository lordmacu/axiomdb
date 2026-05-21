# AxiomDB — Dart / Flutter binding

A Dart binding for the AxiomDB embedded engine via `dart:ffi`. Works with
Flutter (the C library ships in the app bundle). Result materialization uses the
**packed-buffer** path: one FFI call returns the whole result as a single
buffer, decoded once in Dart with `ByteData` (compiled — AOT in Flutter release).

## Usage

```dart
import 'package:axiomdb/axiomdb.dart';

final db = AxiomDB('./myapp.db'); // or ':memory:'
db.execute("CREATE TABLE users (id INT, name TEXT)");
db.execute("INSERT INTO users VALUES (1, 'Alice')");

final rows = db.queryTuples("SELECT * FROM users");        // [[1, 'Alice']]
final recs = db.query("SELECT * FROM users");              // [{'id': 1, ...}]
final (cols, rs) = db.queryWithColumns("SELECT * FROM users");

db.begin(); db.execute("INSERT ..."); db.commit();         // or db.rollback()
db.close();
```

Cell types: `int`, `double`, `String`, `Uint8List` (BLOB), or `null`. Dart's
`int` is 64-bit, so integers are exact.

## Performance

10K rows × 6 cols, materialize every cell (macOS, median of 11):

| Binding | vs sqlite3 package |
|---|---|
| `sqlite3` package (dart:ffi → libsqlite3) | 1.00× |
| **AxiomDB `queryTuples`** | **~0.77× (FASTER)** |

AxiomDB is ~1.3× faster: one FFI call + a compiled Dart parse, vs the `sqlite3`
package's per-row/per-column FFI crossings (`sqlite3_step` + `sqlite3_column_*`).
Dart is the third binding (with Go and Python/PyO3) that beats its SQLite
baseline — same reason as Go: the baseline pays per-row FFI overhead that our
single packed call avoids.

## Flutter notes

`dart:ffi` works in Flutter; bundle `libaxiomdb_embedded` for each target
(`.so` on Android via `android/app/src/main/jniLibs/`, `.framework`/static lib
on iOS). The loader in `lib/axiomdb.dart` searches common paths — adjust
`_loadLibrary()` for your app's packaging.

## Test / bench

```bash
cargo build --release -p axiomdb-embedded   # build the shared library first
cd bindings/dart
dart pub get
dart test                    # correctness
dart run bench/bench.dart    # the table above (vs the sqlite3 package)
```
