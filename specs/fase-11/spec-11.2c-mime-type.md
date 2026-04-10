# Spec: 11.2c — MIME_TYPE Auto-Detection

## What to build
On BLOB/Bytes INSERT, detect the content type from magic bytes (first 16 bytes).
Store as a 1-byte enum in the row alongside the blob data. Expose via
`MIME_TYPE(col)` SQL function. Zero overhead on read.

## Magic byte signatures
| Bytes | MIME | Enum |
|-------|------|------|
| `89 50 4E 47` | image/png | 1 |
| `FF D8 FF` | image/jpeg | 2 |
| `47 49 46 38` | image/gif | 3 |
| `52 49 46 46 ?? ?? ?? ?? 57 45 42 50` | image/webp | 4 |
| `25 50 44 46` | application/pdf | 5 |
| `50 4B 03 04` | application/zip | 6 |
| `1F 8B` | application/gzip | 7 |
| `7B` (starts with '{') | application/json | 8 |
| `3C` (starts with '<') | text/xml | 9 |
| otherwise | application/octet-stream | 0 |

## Acceptance criteria
- [ ] `MIME_TYPE(blob_col)` returns correct type string for PNG, JPEG, PDF, ZIP
- [ ] Unknown blobs return 'application/octet-stream'
- [ ] NULL blob returns NULL
- [ ] Zero overhead: detection happens at INSERT time, stored inline

## Out of scope
- Full MIME database (libmagic) — 10 common types only
- Per-column MIME constraint
