#ifndef CAXIOMDB_H
#define CAXIOMDB_H

#include <stdint.h>
#include <stddef.h>

/* AxiomDB embedded engine — C FFI surface used by the Swift binding.
 * The shared library (libaxiomdb_embedded) provides these symbols. */

typedef struct AxiomDb AxiomDb;

AxiomDb *axiomdb_open(const char *path);
long long axiomdb_execute(AxiomDb *db, const char *sql);
uint8_t *axiomdb_query_packed(AxiomDb *db, const char *sql, size_t *out_len);
void axiomdb_packed_free(uint8_t *ptr, size_t len);
void axiomdb_close(AxiomDb *db);
const char *axiomdb_last_error(AxiomDb *db);

/* Parameter binding: serialize ? params into a buffer; the engine prepares +
 * binds them (real prepared-statement binding, not string escaping). */
long long axiomdb_execute_params(AxiomDb *db, const char *sql,
                                 const uint8_t *params, size_t params_len);
uint8_t *axiomdb_query_packed_params(AxiomDb *db, const char *sql,
                                     const uint8_t *params, size_t params_len,
                                     size_t *out_len);

#endif /* CAXIOMDB_H */
