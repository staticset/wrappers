//! Unit tests for the pure parts of mssql_fdw_rq: the SQL translator and the
//! type mapping table. These run without PostgreSQL (TZ §7.1).

use super::translator::{RelationMapping, TranslateContext, TranslateError, translate};

fn orders_ctx() -> TranslateContext {
    TranslateContext {
        relations: vec![RelationMapping {
            local_schema: "public".into(),
            local_table: "dbo_orders".into(),
            remote_schema: "dbo".into(),
            remote_table: "Orders".into(),
        }],
        bool_columns: vec!["active".into()],
    }
}

fn two_tables_ctx() -> TranslateContext {
    let mut ctx = orders_ctx();
    ctx.relations.push(RelationMapping {
        local_schema: "public".into(),
        local_table: "dbo_customers".into(),
        remote_schema: "dbo".into(),
        remote_table: "Customers".into(),
    });
    ctx
}

fn assert_tsql(sql: &str, ctx: &TranslateContext, expected: &str) {
    let actual = translate(sql, ctx).unwrap_or_else(|e| panic!("translate({sql:?}) failed: {e}"));
    assert_eq!(actual, expected, "input: {sql}");
}

fn assert_unsupported(sql: &str, ctx: &TranslateContext, fragment: &str) {
    match translate(sql, ctx) {
        Err(TranslateError::UnsupportedConstruct { sql_fragment, .. }) => assert!(
            sql_fragment.contains(fragment),
            "fragment {sql_fragment:?} should mention {fragment:?}"
        ),
        other => panic!("expected UnsupportedConstruct for {sql:?}, got {other:?}"),
    }
}

// -- relations -----------------------------------------------------------------

#[test]
fn relation_qualified_rename() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE amount > 1000",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE amount > 1000",
    );
}

#[test]
fn relation_quoted_and_bare() {
    // quoted local names still map (matched case-insensitively)
    assert_tsql(
        "SELECT id FROM \"public\".\"dbo_orders\"",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders]",
    );
    // bare table name resolves for the public schema
    assert_tsql(
        "SELECT id FROM dbo_orders",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders]",
    );
}

// -- parameters ----------------------------------------------------------------

#[test]
fn params_renumbered_to_tiberius_style() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE customer_id = $1 AND amount < $2",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE customer_id = @P1 AND amount < @P2",
    );
}

#[test]
fn param_in_cast() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE amount > $1::numeric",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE amount > CAST(@P1 AS numeric(38, 10))",
    );
}

// -- operators -----------------------------------------------------------------

#[test]
fn concat_operator() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE note || '!' = $1",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE note + '!' = @P1",
    );
}

#[test]
fn regex_operator_rejected() {
    assert_unsupported(
        "SELECT id FROM public.dbo_orders WHERE note ~ 'abc'",
        &orders_ctx(),
        "~",
    );
}

// -- ILIKE ---------------------------------------------------------------------

#[test]
fn ilike_becomes_lower_like() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE note ILIKE '%abc%'",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE (LOWER(note) LIKE LOWER('%abc%'))",
    );
}

#[test]
fn not_ilike_becomes_negated() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE note NOT ILIKE 'a%'",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE NOT (LOWER(note) LIKE LOWER('a%'))",
    );
}

// -- LIMIT / OFFSET ------------------------------------------------------------

#[test]
fn limit_without_order_becomes_top() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders LIMIT 5",
        &orders_ctx(),
        "SELECT TOP (5) id FROM [dbo].[Orders]",
    );
}

#[test]
fn distinct_limit_becomes_distinct_top() {
    assert_tsql(
        "SELECT DISTINCT customer_id FROM public.dbo_orders LIMIT 3",
        &orders_ctx(),
        "SELECT DISTINCT TOP (3) customer_id FROM [dbo].[Orders]",
    );
}

#[test]
fn limit_with_order_becomes_fetch() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders ORDER BY id LIMIT 10",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] ORDER BY id OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY",
    );
}

#[test]
fn offset_and_limit_become_offset_fetch() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders ORDER BY id LIMIT 5 OFFSET 10",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] ORDER BY id OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY",
    );
}

#[test]
fn limit_all_rejected() {
    assert_unsupported(
        "SELECT id FROM public.dbo_orders LIMIT ALL",
        &orders_ctx(),
        "LIMIT ALL",
    );
}

#[test]
fn limit_in_subquery_rejected() {
    assert_unsupported(
        "SELECT * FROM (SELECT id FROM public.dbo_orders LIMIT 5) sub",
        &orders_ctx(),
        "LIMIT",
    );
}

// -- DISTINCT ------------------------------------------------------------------

#[test]
fn distinct_passes_through() {
    assert_tsql(
        "SELECT DISTINCT customer_id FROM public.dbo_orders",
        &orders_ctx(),
        "SELECT DISTINCT customer_id FROM [dbo].[Orders]",
    );
}

#[test]
fn distinct_on_rejected() {
    assert_unsupported(
        "SELECT DISTINCT ON (customer_id) id FROM public.dbo_orders",
        &orders_ctx(),
        "DISTINCT ON",
    );
}

// -- casts ---------------------------------------------------------------------

#[test]
fn qualified_cast_type() {
    assert_tsql(
        "SELECT 1::pg_catalog.int8 FROM public.dbo_orders",
        &orders_ctx(),
        "SELECT CAST(1 AS bigint) FROM [dbo].[Orders]",
    );
}

#[test]
fn unknown_cast_type_rejected() {
    assert_unsupported(
        "SELECT id::json FROM public.dbo_orders",
        &orders_ctx(),
        "::json",
    );
}

// -- booleans ------------------------------------------------------------------

#[test]
fn is_true_becomes_bit_compare() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE active IS TRUE",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE active = 1",
    );
}

#[test]
fn is_not_false_becomes_ne_zero() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE active IS NOT FALSE",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE active <> 0",
    );
}

#[test]
fn bare_bool_column_in_predicate() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE active AND id > 5",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE active = 1 AND id > 5",
    );
}

#[test]
fn bool_literals() {
    assert_tsql(
        "SELECT id FROM public.dbo_orders WHERE active = true",
        &orders_ctx(),
        "SELECT id FROM [dbo].[Orders] WHERE active = 1",
    );
}

// -- joins / aggregates (pass-through sanity) ----------------------------------

#[test]
fn join_two_foreign_tables() {
    assert_tsql(
        "SELECT c.name FROM public.dbo_orders o JOIN public.dbo_customers c ON o.customer_id = c.id WHERE o.amount > 100",
        &two_tables_ctx(),
        "SELECT c.name FROM [dbo].[Orders] o JOIN [dbo].[Customers] c ON o.customer_id = c.id WHERE o.amount > 100",
    );
}

#[test]
fn aggregate_group_having_order() {
    assert_tsql(
        "SELECT c.name, SUM(o.amount) AS total FROM public.dbo_orders o JOIN public.dbo_customers c ON o.customer_id = c.id GROUP BY c.name HAVING SUM(o.amount) > 100 ORDER BY total DESC LIMIT 5",
        &two_tables_ctx(),
        "SELECT c.name, SUM(o.amount) AS total FROM [dbo].[Orders] o JOIN [dbo].[Customers] c ON o.customer_id = c.id GROUP BY c.name HAVING SUM(o.amount) > 100 ORDER BY total DESC OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
    );
}

// -- identifiers ---------------------------------------------------------------

#[test]
fn quoted_identifier_becomes_brackets() {
    assert_tsql(
        "SELECT \"Amount\" FROM public.dbo_orders",
        &orders_ctx(),
        "SELECT [Amount] FROM [dbo].[Orders]",
    );
}

#[test]
fn unsafe_quoted_identifier_rejected() {
    let err = translate("SELECT \"bad]name\" FROM public.dbo_orders", &orders_ctx())
        .expect_err("must fail");
    assert!(matches!(err, TranslateError::InvalidIdentifier(_)));
}

// -- type mapping --------------------------------------------------------------

#[test]
fn type_mapping_core_set() {
    use super::types::pg_type_to_mssql as m;
    assert_eq!(m("int2"), Some("smallint"));
    assert_eq!(m("int4"), Some("int"));
    assert_eq!(m("int8"), Some("bigint"));
    assert_eq!(m("numeric"), Some("numeric(38, 10)"));
    assert_eq!(m("bool"), Some("bit"));
    assert_eq!(m("uuid"), Some("uniqueidentifier"));
    assert_eq!(m("bytea"), Some("varbinary(8000)"));
    assert_eq!(m("timestamptz"), Some("datetimeoffset"));
    assert_eq!(m("pg_catalog.timestamp"), Some("datetime2"));
    assert_eq!(m("json"), None);
    assert_eq!(m("geometry"), None);
}
