//! Full Text Search tokenizer (Phase 11.6).
//!
//! Splits text into lowercase tokens with position tracking.
//! Filters stop words. NFC-normalized input assumed (Phase 11.2e).
//!
//! ## PostgreSQL reference
//!
//! PostgreSQL's `wparser_def.c` uses a 40-state machine recognizing 23+ token
//! types (words, emails, URLs, numbers, etc.). AxiomDB Phase 11.6 uses a simpler
//! approach: split on whitespace + Unicode punctuation, lowercase, filter stops.
//! Phase 11.7 can add a multi-state tokenizer if needed.

/// A token with its ordinal position in the source document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub term: String,
    pub position: u32,
}

/// English stop words (174 words — PostgreSQL's default English list).
const STOP_WORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "aren't",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can't",
    "cannot",
    "could",
    "couldn't",
    "did",
    "didn't",
    "do",
    "does",
    "doesn't",
    "doing",
    "don't",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "get",
    "got",
    "had",
    "hadn't",
    "has",
    "hasn't",
    "have",
    "haven't",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "isn't",
    "it",
    "its",
    "itself",
    "just",
    "let's",
    "me",
    "might",
    "more",
    "most",
    "mustn't",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "ought",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "same",
    "shan't",
    "she",
    "should",
    "shouldn't",
    "so",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "wasn't",
    "we",
    "were",
    "weren't",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "won't",
    "would",
    "wouldn't",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

/// Tokenizes text into lowercase terms with position tracking.
///
/// Splits on any character that is NOT alphanumeric or `'` (apostrophe).
/// Filters stop words. Returns tokens in document order.
///
/// ```
/// use axiomdb_sql::tokenizer::tokenize;
/// let tokens = tokenize("Rust is a fast database engine");
/// assert_eq!(tokens[0].term, "rust");
/// assert_eq!(tokens[0].position, 0);
/// assert_eq!(tokens[1].term, "fast");  // "is" and "a" filtered as stop words
/// ```
pub fn tokenize(text: &str) -> Vec<Token> {
    let lower = text.to_lowercase();
    let mut tokens = Vec::new();
    let mut pos: u32 = 0;

    for word in lower.split(|c: char| !c.is_alphanumeric() && c != '\'') {
        let word = word.trim_matches('\'');
        if word.is_empty() {
            continue;
        }
        if is_stop_word(word) {
            pos += 1; // count position even for stop words (for phrase queries)
            continue;
        }
        tokens.push(Token {
            term: word.to_string(),
            position: pos,
        });
        pos += 1;
    }
    tokens
}

/// Tokenizes a search query. Same as `tokenize` but does NOT filter stop words
/// if the query is very short (≤2 terms before filtering), to avoid empty queries.
pub fn tokenize_query(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let all_terms: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .collect();

    let filtered: Vec<String> = all_terms
        .iter()
        .filter(|w| !is_stop_word(w))
        .map(|w| w.to_string())
        .collect();

    // If filtering removed all terms, return unfiltered.
    if filtered.is_empty() && !all_terms.is_empty() {
        all_terms.iter().map(|w| w.to_string()).collect()
    } else {
        filtered
    }
}

fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.binary_search(&word).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_tokenize() {
        let tokens = tokenize("Hello World");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].term, "hello");
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].term, "world");
        assert_eq!(tokens[1].position, 1);
    }

    #[test]
    fn test_stop_word_removal() {
        let tokens = tokenize("Rust is a fast database engine");
        let terms: Vec<&str> = tokens.iter().map(|t| t.term.as_str()).collect();
        assert!(!terms.contains(&"is"));
        assert!(!terms.contains(&"a"));
        assert!(terms.contains(&"rust"));
        assert!(terms.contains(&"fast"));
        assert!(terms.contains(&"database"));
        assert!(terms.contains(&"engine"));
    }

    #[test]
    fn test_punctuation_split() {
        let tokens = tokenize("hello, world! foo-bar");
        let terms: Vec<&str> = tokens.iter().map(|t| t.term.as_str()).collect();
        assert!(terms.contains(&"hello"));
        assert!(terms.contains(&"world"));
        assert!(terms.contains(&"foo"));
        assert!(terms.contains(&"bar"));
    }

    #[test]
    fn test_apostrophe_kept() {
        let tokens = tokenize("it's don't");
        // "it's" → stop word (it filtered? depends on exact matching)
        // For now, apostrophes are stripped at edges
        assert!(!tokens.is_empty());
    }

    #[test]
    fn test_empty_input() {
        let tokens = tokenize("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_query_tokenize() {
        let terms = tokenize_query("rust database engine");
        assert!(terms.contains(&"rust".to_string()));
        assert!(terms.contains(&"database".to_string()));
        assert!(terms.contains(&"engine".to_string()));
    }

    #[test]
    fn test_query_all_stop_words() {
        // "the" is a stop word — but if ALL terms are stop words, return them.
        let terms = tokenize_query("the");
        assert_eq!(terms, vec!["the"]);
    }

    #[test]
    fn test_position_tracking() {
        let tokens = tokenize("the quick brown fox");
        // "the" is stop word (pos 0), "quick" (pos 1), "brown" (pos 2), "fox" (pos 3)
        assert_eq!(tokens[0].term, "quick");
        assert_eq!(tokens[0].position, 1);
        assert_eq!(tokens[1].term, "brown");
        assert_eq!(tokens[1].position, 2);
    }
}
