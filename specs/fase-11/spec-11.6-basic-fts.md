# Spec: 11.6 — Basic Full Text Search

## What to build
Tokenizer + inverted index + BM25 ranking. `CREATE INDEX ... USING fts` builds
an inverted index. `WHERE MATCH(col, 'query terms')` searches with ranking.

## Research: PostgreSQL tsearch (research/postgresql/src/backend/tsearch/)
- **tsvector**: compact binary with term→positions mapping
- **tsquery**: binary tree of AND/OR/NOT/PHRASE operators
- **GIN index**: term→posting list, fast intersections
- **ts_rank**: BM25-like with word distance + weight levels
- **wparser_def.c**: 40-state tokenizer recognizing 23+ token types

## Design decisions
- **Tokenizer**: whitespace + Unicode punctuation split, lowercase, NFC normalized.
  Stop words list (English, ~174 words). Position tracking for future phrase support.
- **Storage**: reuse B-Tree. Key = `[term_bytes || docid_8LE || position_4LE]`.
  B-Tree range scan on term prefix = posting list retrieval.
- **Metadata**: per-document token count stored in a metadata B-Tree alongside the
  inverted index: key = `[0xFF || docid_8LE]`, value = `token_count_4LE`.
  Global stats (N, avgdoclen) in index metapage.
- **Ranking**: Okapi BM25 with k1=1.2, b=0.75 (PostgreSQL/Elasticsearch defaults).
- **Query syntax**: `MATCH(col, 'term1 term2')` — implicit AND between terms.

## BM25 formula
```
score(q, d) = Σ_t∈q IDF(t) × (TF(t,d) × (k1 + 1)) / (TF(t,d) + k1 × (1 - b + b × |d| / avgdl))
IDF(t) = ln((N - df(t) + 0.5) / (df(t) + 0.5) + 1)
```

## SQL syntax
```sql
CREATE INDEX idx ON articles (body) USING fts;
SELECT *, MATCH(body, 'rust database') AS score FROM articles
  WHERE MATCH(body, 'rust database') > 0
  ORDER BY score DESC LIMIT 10;
```

## Acceptance criteria
- [ ] `CREATE INDEX ... USING fts` builds inverted index from existing rows
- [ ] INSERT maintains inverted index entries
- [ ] `MATCH(col, 'query')` returns BM25 score
- [ ] Multi-term queries (implicit AND)
- [ ] Results ranked by BM25 score (higher = more relevant)
- [ ] Stop words filtered from both index and query
- [ ] Case-insensitive matching
- [ ] NULL values handled (skipped in index)

## Out of scope (Phase 11.7)
- Boolean operators (AND, OR, NOT explicit syntax)
- Phrase queries ("exact phrase")
- Prefix matching (term*)
- Stemming (porter stemmer)
- Custom stop word lists
- Highlight/snippet generation
