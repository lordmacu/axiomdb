# Spec: 11.4b — Trigram Indexes for Substring Search

## What to build
`CREATE INDEX ... USING trigram` stores 3-character n-grams for each Text value.
`WHERE col LIKE '%pattern%'` uses the index to narrow candidates before recheck.
PostgreSQL requires `pg_trgm` extension; AxiomDB includes it built-in.

## Design
- **Storage**: reuse existing B-Tree. Key = `trigram_3bytes || RecordId_10bytes`.
- **Trigram extraction**: sliding 3-char window over lowercase + padded text.
- **Query**: extract pattern trigrams → B-Tree range scan per trigram → intersect → recheck with full LIKE.
- **Recheck required**: trigrams are necessary but not sufficient (false positives expected).

## Acceptance criteria
- [ ] `CREATE INDEX idx ON t (name) USING trigram` builds trigram B-Tree
- [ ] INSERT maintains trigram index entries
- [ ] `WHERE name LIKE '%García%'` uses trigram index when available
- [ ] Results are correct (recheck eliminates false positives)
- [ ] Case-insensitive matching (trigrams extracted from lowercase)

## Out of scope
- GIN posting list compression (B-Tree is simpler, sufficient for Phase 11)
- Similarity operator (`%`) and `pg_trgm_similarity()`
- ILIKE (case-insensitive LIKE — trigrams are already lowercase)
- Regex support
