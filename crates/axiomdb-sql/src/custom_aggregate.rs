use axiomdb_catalog::AggregateHelperKind;
use axiomdb_core::error::DbError;

pub const INTERNAL_MEDIAN_AGGREGATE_NAME: &str = "__axiom_internal_median";

pub fn resolve_custom_aggregate_helper(
    sfunc: &str,
    stype: &str,
    finalfunc: Option<&str>,
) -> Result<AggregateHelperKind, DbError> {
    let normalized_stype = stype.to_ascii_lowercase();
    if sfunc.eq_ignore_ascii_case("median_state")
        && finalfunc.is_some_and(|f| f.eq_ignore_ascii_case("median_final"))
        && matches!(normalized_stype.as_str(), "float[]" | "real[]")
    {
        return Ok(AggregateHelperKind::Median);
    }
    Err(DbError::InvalidValue {
        reason: format!(
            "unsupported aggregate helper combination: SFUNC={sfunc}, STYPE={stype}, FINALFUNC={}",
            finalfunc.unwrap_or("<none>")
        ),
    })
}

pub fn internal_aggregate_name(kind: AggregateHelperKind) -> &'static str {
    match kind {
        AggregateHelperKind::Median => INTERNAL_MEDIAN_AGGREGATE_NAME,
    }
}

pub fn is_internal_aggregate_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(INTERNAL_MEDIAN_AGGREGATE_NAME)
}
