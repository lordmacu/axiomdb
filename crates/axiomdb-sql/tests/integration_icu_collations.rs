//! Phase 24.4b — ICU4X locale-aware collations (opt-in via `icu-collations`
//! feature). All tests here run unconditionally — when the feature is
//! disabled the assertions verify the documented fallback behaviour
//! (locale-specific tailoring is silently absent; equality + ordering
//! degrade to the generic `Es` CI+AI algorithm).

mod common;

use axiomdb_sql::{
    bloom::BloomRegistry,
    session::{IcuLocale, SessionCollation},
    text_semantics::{compare_text, text_eq},
    QueryResult, SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn run_ok(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn rows(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    match run_ok(sql, storage, txn, bloom, ctx) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn mysql_alias_parses_to_icu_variant() {
    use axiomdb_sql::session::session_collation_from_name;
    assert_eq!(
        session_collation_from_name("utf8mb4_tr_0900_ai_ci").unwrap(),
        SessionCollation::Icu(IcuLocale::Turkish)
    );
    assert_eq!(
        session_collation_from_name("utf8mb4_de_0900_ai_ci").unwrap(),
        SessionCollation::Icu(IcuLocale::German)
    );
    assert_eq!(
        session_collation_from_name("utf8mb4_es_0900_ai_ci").unwrap(),
        SessionCollation::Icu(IcuLocale::Spanish)
    );
}

#[test]
fn bcp47_short_alias_parses_to_icu_variant() {
    use axiomdb_sql::session::session_collation_from_name;
    assert_eq!(
        session_collation_from_name("tr-icu").unwrap(),
        SessionCollation::Icu(IcuLocale::Turkish)
    );
    assert_eq!(
        session_collation_from_name("de-icu").unwrap(),
        SessionCollation::Icu(IcuLocale::German)
    );
}

#[test]
fn unknown_icu_locale_errors() {
    use axiomdb_sql::session::session_collation_from_name;
    assert!(session_collation_from_name("utf8mb4_xx_0900_ai_ci").is_err());
}

#[test]
fn icu_basic_equality_is_case_insensitive() {
    // The minimum guarantee both with and without the `icu-collations`
    // feature: equality on the Icu variant is still case-insensitive.
    // Locale-specific tailoring (Turkish-i, etc.) is asserted in
    // separate tests that gate on the feature flag.
    assert!(text_eq(
        SessionCollation::Icu(IcuLocale::UnicodeCi),
        "Alice",
        "alice"
    ));
    assert!(text_eq(
        SessionCollation::Icu(IcuLocale::Turkish),
        "Café",
        "cafe"
    ));
}

#[test]
fn icu_basic_compare_treats_case_as_equal() {
    // Same minimum guarantee for `compare_text`: case-equal strings
    // return Ordering::Equal under any Icu locale, regardless of
    // whether locale-specific weights are loaded.
    assert_eq!(
        compare_text(
            SessionCollation::Icu(IcuLocale::UnicodeCi),
            "Alice",
            "alice"
        ),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn create_table_with_mysql_locale_collation_compiles_and_runs() {
    // End-to-end: a CREATE TABLE that names a locale-specific MySQL
    // collation parses, the column gets the Icu collation, and an
    // equality query on it returns the expected row. Using German
    // here because Turkish's dotless-i / dotted-i distinction (which
    // ICU at Primary strength honors) would make `Istanbul` ≠
    // `istanbul` — that's the whole point of locale-specific tailoring.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ok(
        "CREATE TABLE u (id INT, name TEXT COLLATE utf8mb4_de_0900_ai_ci)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "INSERT INTO u VALUES (1, 'München')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(
        "SELECT id FROM u WHERE name = 'MUNCHEN'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        r.len(),
        1,
        "case + diacritic-insensitive match should succeed"
    );
}

// ── Feature-gated locale-specific behaviour ──────────────────────────────

#[cfg(feature = "icu-collations")]
mod feature_on {
    use super::*;

    #[test]
    fn turkish_dotless_i_locale_rules() {
        // With the ICU collator loaded under the Turkish locale, the
        // dotless `ı` (lowercase of `I`) is a DIFFERENT letter from
        // the dotted `i` (lowercase of `İ`). Generic Es would fold
        // both to a Latin `i`. ICU Turkish keeps them apart.
        //
        // Equality at Primary strength still treats accent-less Latin
        // `i` ↔ `İ` as equal in some interpretations, but `i` vs `ı`
        // (different base letters) is the canonical contrast.
        let tr = SessionCollation::Icu(IcuLocale::Turkish);
        assert!(
            !text_eq(tr, "i", "ı"),
            "Turkish locale must distinguish dotted i from dotless ı"
        );
    }

    #[test]
    fn german_eszett_equality() {
        // Under German collation, `ß` and `ss` compare equal at
        // Primary strength (DIN 5007-1).
        let de = SessionCollation::Icu(IcuLocale::German);
        assert!(text_eq(de, "Straße", "Strasse"));
        assert!(text_eq(de, "groß", "GROSS"));
    }

    #[test]
    fn swedish_letters_sort_at_end() {
        // Under Swedish collation, `å` sorts AFTER `z`, not between
        // `a` and `b`.
        let sv = SessionCollation::Icu(IcuLocale::Swedish);
        assert_eq!(
            compare_text(sv, "z", "å"),
            std::cmp::Ordering::Less,
            "z < å in Swedish"
        );
    }
}

#[cfg(not(feature = "icu-collations"))]
mod feature_off {
    use super::*;

    #[test]
    fn feature_off_falls_back_to_generic_ci() {
        // Without the feature, the Icu variant degrades to the generic
        // Es algorithm — case + accent folding without locale tailoring.
        let tr = SessionCollation::Icu(IcuLocale::Turkish);
        // `i` vs `I` is the basic Latin case fold, which Es handles.
        assert!(text_eq(tr, "i", "I"));
        // German `ß` does NOT auto-expand to `ss` under generic Unicode
        // lowercase — that's locale-specific tailoring that only the
        // ICU path supplies. Without the feature these stay distinct.
        let de = SessionCollation::Icu(IcuLocale::German);
        assert!(!text_eq(de, "straße", "strasse"));
    }
}
