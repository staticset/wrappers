//! Unit tests for the pure parts of mssql_fdw_rq: the SQL translator and the
//! type mapping table. These run without PostgreSQL (TZ §7.1).

#[cfg(test)]
mod unit {
    use super::super::translator::{RelationMapping, TranslateContext, TranslateError, translate};

    fn orders_ctx() -> TranslateContext {
        TranslateContext {
            relations: vec![RelationMapping {
                local_schema: "public".into(),
                local_table: "dbo_orders".into(),
                remote_schema: "dbo".into(),
                remote_table: "Orders".into(),
            }],
            bool_columns: vec!["active".into()],
            not_null_columns: vec!["id".into()],
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
        let actual =
            translate(sql, ctx).unwrap_or_else(|e| panic!("translate({sql:?}) failed: {e}"));
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
    fn deparser_comma_list_becomes_and() {
        // pg_get_querydef prints top-level AND-chains in WHERE as `,` lists,
        // and the statement arrives multi-line with FROM ONLY
        assert_tsql(
            " SELECT count(*) AS count\n   FROM ONLY dbo_orders\n  WHERE active, (id > 5)",
            &orders_ctx(),
            "SELECT count(*) AS count FROM [dbo].[Orders] WHERE active = 1 AND (id > 5)",
        );
    }

    #[test]
    fn is_not_null_passes_through_and_advances() {
        // regression: this exact shape used to loop forever
        assert_tsql(
            "SELECT count(*) AS count FROM public.dbo_orders WHERE ((note IS NOT NULL))",
            &orders_ctx(),
            "SELECT count(*) AS count FROM [dbo].[Orders] WHERE ((note IS NOT NULL))",
        );
    }

    #[test]
    fn typed_date_literal_becomes_cast() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE order_date >= DATE '2026-05-01'",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE order_date >= CAST('2026-05-01' AS date)",
        );
    }

    #[test]
    fn untranslatable_typed_literal_rejected() {
        assert_unsupported(
            "SELECT id FROM public.dbo_orders WHERE note = JSON '{\"a\":1}'",
            &orders_ctx(),
            "json",
        );
    }

    // -- window functions (TZ §10 M2) ------------------------------------

    #[test]
    fn window_function_passes_through_with_top() {
        // the deparser prints LIMIT as `'3'::bigint`; window sort keys must
        // be NOT NULL columns (id), otherwise the query is rejected
        assert_tsql(
            " SELECT id, row_number() OVER (PARTITION BY customer_id ORDER BY id DESC) AS rn\
             \n   FROM ONLY dbo_orders\
             \n LIMIT '3'::bigint",
            &orders_ctx(),
            "SELECT TOP (3) id, row_number() OVER(PARTITION BY customer_id ORDER BY id DESC) \
             AS rn FROM [dbo].[Orders]",
        );
    }

    #[test]
    fn window_function_not_null_order_key() {
        assert_tsql(
            "SELECT id, rank() OVER (ORDER BY id) AS r FROM public.dbo_orders",
            &orders_ctx(),
            "SELECT id, rank() OVER(ORDER BY id) AS r FROM [dbo].[Orders]",
        );
    }

    #[test]
    fn window_function_nullable_order_key_rejected() {
        assert_unsupported(
            "SELECT id, rank() OVER (ORDER BY amount) AS r FROM public.dbo_orders",
            &orders_ctx(),
            "OVER",
        );
    }

    // -- NULL ordering (top-level ORDER BY) -------------------------------

    #[test]
    fn order_by_not_null_key_needs_no_tiebreaker() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders ORDER BY id LIMIT 5",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] ORDER BY id OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
        );
    }

    #[test]
    fn order_by_nullable_key_gets_case_tiebreaker() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders ORDER BY amount LIMIT 5",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] ORDER BY \
             CASE WHEN amount IS NULL THEN 1 ELSE 0 END, amount OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders ORDER BY amount DESC LIMIT 5",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] ORDER BY \
             CASE WHEN amount IS NULL THEN 1 ELSE 0 END DESC, amount DESC \
             OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
        );
    }

    #[test]
    fn explicit_nulls_matching_tsql_default_dropped() {
        // ASC NULLS FIRST and DESC NULLS LAST are exactly T-SQL's defaults
        assert_tsql(
            "SELECT id FROM public.dbo_orders ORDER BY amount ASC NULLS FIRST LIMIT 5",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] ORDER BY amount \
             OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders ORDER BY amount DESC NULLS LAST LIMIT 5",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] ORDER BY amount DESC \
             OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
        );
    }

    // -- regression round 2026-09-04 (CODE_REVIEW blockers) ------------------

    // E: a composite ORDER BY key used to be captured partially — only its
    // last operand carried the NULL tiebreaker, silently reordering rows
    #[test]
    fn order_by_composite_key_rejected() {
        assert_unsupported(
            "SELECT id FROM public.dbo_orders ORDER BY amount + fee LIMIT 5",
            &orders_ctx(),
            "ORDER BY amount + fee",
        );
        assert_unsupported(
            "SELECT id FROM public.dbo_orders ORDER BY amount + fee DESC LIMIT 5",
            &orders_ctx(),
            "ORDER BY amount + fee",
        );
        assert_unsupported(
            "SELECT id FROM public.dbo_orders ORDER BY amount + fee NULLS LAST LIMIT 5",
            &orders_ctx(),
            "ORDER BY amount + fee NULLS",
        );
    }

    #[test]
    fn order_by_parenthesized_composite_gets_tiebreaker() {
        // parenthesized composites are captured whole and stay translatable
        assert_tsql(
            "SELECT id FROM public.dbo_orders ORDER BY (amount + fee) LIMIT 5",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] ORDER BY \
             CASE WHEN ( amount + fee ) IS NULL THEN 1 ELSE 0 END, ( amount + fee ) \
             OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
        );
    }

    // C: multi-word cast types were truncated to their first word, leaving
    // stray tokens in the T-SQL text (`::timestamptz` also maps wrongly)
    #[test]
    fn multiword_cast_types_not_truncated() {
        assert_tsql(
            "SELECT shipped_at::timestamp with time zone FROM public.dbo_orders",
            &orders_ctx(),
            "SELECT CAST(shipped_at AS datetimeoffset) FROM [dbo].[Orders]",
        );
        assert_tsql(
            "SELECT amount::double precision FROM public.dbo_orders",
            &orders_ctx(),
            "SELECT CAST(amount AS float(53)) FROM [dbo].[Orders]",
        );
        assert_tsql(
            "SELECT note::character varying FROM public.dbo_orders",
            &orders_ctx(),
            "SELECT CAST(note AS nvarchar(4000)) FROM [dbo].[Orders]",
        );
    }

    #[test]
    fn multiword_cast_with_modifier() {
        assert_tsql(
            "SELECT note::character varying(10) FROM public.dbo_orders",
            &orders_ctx(),
            "SELECT CAST(note AS nvarchar(4000)) FROM [dbo].[Orders]",
        );
    }

    // D: `o . active` arrives piecewise, so the qualifier tail used to hide
    // the predicate-start piece and the `= 1` rewrite never fired
    #[test]
    fn bare_bool_with_table_qualifier() {
        assert_tsql(
            "SELECT id FROM dbo_orders o WHERE o.active",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] o WHERE o.active = 1",
        );
        assert_tsql(
            "SELECT id FROM dbo_orders o WHERE NOT o.active",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] o WHERE NOT o.active = 1",
        );
    }

    // #2: E'' literals must decode escape sequences; PG deparses Windows
    // paths as E'C:\\temp\\' and the raw body compared wrong on MSSQL
    #[test]
    fn escape_string_literal_decodes_backslashes() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE note = E'C:\\\\temp\\\\'",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE note = 'C:\\temp\\'",
        );
    }

    #[test]
    fn escape_string_literal_control_escapes() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE note = E'a\\tb\\n'",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE note = 'a\tb\n'",
        );
    }

    // -- regression round 2026-09-04 (after-blockers: #5, #6, #10, #12) ------

    // #12: non-ASCII literals get the N'…' form (a legacy server collation
    // code page would otherwise mangle them to '?'); ASCII stays plain to
    // keep varchar comparison semantics
    #[test]
    fn non_ascii_literals_use_n_prefix() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE note = 'тест'",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE note = N'тест'",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE note = 'ascii'",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE note = 'ascii'",
        );
    }

    // #6: bare table names resolve from any schema when unambiguous
    #[test]
    fn bare_table_name_resolves_from_non_public_schema() {
        let ctx = TranslateContext {
            relations: vec![RelationMapping {
                local_schema: "ms".into(),
                local_table: "statuses".into(),
                remote_schema: "dbo".into(),
                remote_table: "Statuses".into(),
            }],
            ..Default::default()
        };
        assert_tsql(
            "SELECT id FROM statuses",
            &ctx,
            "SELECT id FROM [dbo].[Statuses]",
        );
    }

    // #6: two relations sharing a bare name across schemas is ambiguous —
    // reject instead of silently renaming to one of them
    #[test]
    fn ambiguous_bare_table_name_rejected() {
        let ctx = TranslateContext {
            relations: vec![
                RelationMapping {
                    local_schema: "ms".into(),
                    local_table: "statuses".into(),
                    remote_schema: "dbo".into(),
                    remote_table: "Statuses".into(),
                },
                RelationMapping {
                    local_schema: "other".into(),
                    local_table: "statuses".into(),
                    remote_schema: "ext".into(),
                    remote_table: "Statuses".into(),
                },
            ],
            ..Default::default()
        };
        assert_unsupported("SELECT id FROM statuses", &ctx, "statuses");
        // qualified names are unaffected
        assert_tsql(
            "SELECT id FROM ms.statuses",
            &ctx,
            "SELECT id FROM [dbo].[Statuses]",
        );
    }

    // The deparser prints LIKE/ILIKE as operators (~~ / !~~ / ~~* / !~~*);
    // until 2026-09-04 the lexer rejected '~' outright, so every LIKE query
    // under full-query pushdown failed. Patterns are cast by the deparser
    // ('…'::text) — the cast is dropped; non-ASCII keeps the N prefix.
    #[test]
    fn deparser_like_operators() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE (note ~~ 'a%'::text)",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (note LIKE 'a%')",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE (note !~~ 'a%'::text)",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE NOT (note LIKE 'a%')",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE (note ~~* 'a%'::text)",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (LOWER(note) LIKE LOWER('a%'))",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE (note !~~* 'a%'::text)",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE NOT (LOWER(note) LIKE LOWER('a%'))",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders WHERE (note ~~ 'От%'::text)",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (note LIKE N'От%')",
        );
    }

    #[test]
    fn regex_operator_still_rejected() {
        // a lone ~ (POSIX regex) stays rejected: only the LIKE family lexes
        assert_unsupported(
            "SELECT id FROM public.dbo_orders WHERE note ~ 'abc'",
            &orders_ctx(),
            "~",
        );
    }

    // -- Navigator integration: IMPORT FOREIGN SCHEMA type mapping -----------

    #[test]
    fn mssql_type_map_for_import() {
        use super::super::types::mssql_type_to_pg as map;

        assert_eq!(
            map("int", None, None, None, None).as_deref(),
            Some("integer")
        );
        assert_eq!(
            map("bigint", None, None, None, None).as_deref(),
            Some("bigint")
        );
        assert_eq!(
            map("bit", None, None, None, None).as_deref(),
            Some("boolean")
        );
        assert_eq!(
            map("numeric", None, Some(18), Some(2), None).as_deref(),
            Some("numeric(18, 2)")
        );
        assert_eq!(
            map("money", None, None, None, None).as_deref(),
            Some("numeric(19, 4)")
        );
        assert_eq!(
            map("float", None, Some(53), None, None).as_deref(),
            Some("double precision")
        );
        assert_eq!(
            map("float", None, Some(24), None, None).as_deref(),
            Some("real")
        );
        assert_eq!(
            map("datetime2", None, None, None, Some(7)).as_deref(),
            Some("timestamp(6) without time zone")
        );
        assert_eq!(
            map("datetimeoffset", None, None, None, Some(3)).as_deref(),
            Some("timestamp(3) with time zone")
        );
        assert_eq!(
            map("nvarchar", Some(-1), None, None, None).as_deref(),
            Some("text")
        );
        assert_eq!(
            map("nvarchar", Some(100), None, None, None).as_deref(),
            Some("varchar(100)")
        );
        assert_eq!(
            map("uniqueidentifier", None, None, None, None).as_deref(),
            Some("uuid")
        );
        assert_eq!(
            map("varbinary", Some(8000), None, None, None).as_deref(),
            Some("bytea")
        );
        // unreadable types are omitted from imported definitions
        assert_eq!(map("xml", None, None, None, None), None);
        assert_eq!(map("hierarchyid", None, None, None, None), None);
    }

    // #5: the remote-query safety net matches whole identifiers only
    #[test]
    fn mentions_relation_is_lexical() {
        use super::super::translator::mentions_relation;

        let rels = vec![RelationMapping {
            local_schema: "public".into(),
            local_table: "users".into(),
            remote_schema: "dbo".into(),
            remote_table: "Users".into(),
        }];
        assert!(mentions_relation("SELECT * FROM public.users", &rels));
        assert!(mentions_relation("SELECT * FROM users WHERE x = 1", &rels));
        assert!(!mentions_relation("SELECT * FROM appusers", &rels));
        assert!(!mentions_relation("SELECT 'users in a literal'", &rels));
        assert!(!mentions_relation("SELECT 1 WHERE users.a = 2", &rels));

        // the real deparser shape: FROM ONLY, quoted "?column?" alias,
        // non-ASCII literal (production regression 2026-09-04)
        let ms_rels = vec![RelationMapping {
            local_schema: "ms".into(),
            local_table: "statuses".into(),
            remote_schema: "dbo".into(),
            remote_table: "Statuses".into(),
        }];
        assert!(mentions_relation(
            "SELECT ('x:'::text || (count(*))::text) AS \"?column?\" \
             FROM ONLY ms.statuses WHERE statusname LIKE 'От%'",
            &ms_rels
        ));
    }

    #[test]
    fn deparser_cast_limit_form() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders LIMIT '10'::bigint",
            &orders_ctx(),
            "SELECT TOP (10) id FROM [dbo].[Orders]",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders ORDER BY id\
             \n FETCH FIRST '7'::bigint ROWS ONLY",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] ORDER BY id OFFSET 0 ROWS FETCH NEXT 7 ROWS ONLY",
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

    // -- deparser IN-list form: x = ANY (ARRAY[…]) (TZ §10 M3) -----------

    #[test]
    fn deparser_any_array_becomes_in() {
        // how PostgreSQL deparses `x IN (142, 143)` inside a full query
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid = ANY (ARRAY[142, 143]::integer[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (statusid IN (142, 143))",
        );
    }

    #[test]
    fn deparser_all_array_becomes_not_in() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid <> ALL (ARRAY[1, 2]::integer[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (statusid NOT IN (1, 2))",
        );
    }

    #[test]
    fn deparser_any_other_operator_becomes_or_chain() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE amount < ANY (ARRAY[10, 20]::numeric[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (amount < 10 OR amount < 20)",
        );
    }

    #[test]
    fn deparser_any_empty_array_is_false() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid = ANY (ARRAY[]::integer[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (1 = 0)",
        );
    }

    #[test]
    fn deparser_any_text_array_quotes_items() {
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE note = ANY (ARRAY['a''b', 'c']::text[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (note IN ('a''b', 'c'))",
        );
    }

    #[test]
    fn deparser_any_array_constant_becomes_in() {
        // constant-folded IN-list: array in output form '{v1,v2}' + cast
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid = ANY ('{142,143}'::integer[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (statusid IN (142, 143))",
        );
    }

    #[test]
    fn deparser_any_array_keeps_nulls() {
        // T-SQL IN/NOT IN share PG's three-valued semantics: NULL elements
        // must stay in the list (dropping them would flip <> ALL semantics)
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid = ANY ('{5,NULL}'::integer[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (statusid IN (5, NULL))",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid <> ALL ('{5,NULL}'::integer[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (statusid NOT IN (5, NULL))",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid = ANY (ARRAY[5, NULL])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (statusid IN (5, NULL))",
        );
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE statusid = ANY ('{NULL}'::integer[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (statusid IN (NULL))",
        );
    }

    #[test]
    fn deparser_any_array_constant_strings() {
        // PostgreSQL's array output always quotes string elements
        assert_tsql(
            "SELECT id FROM public.dbo_orders \
             WHERE note = ANY ('{\"a\"\"b\",\"c\"}'::text[])",
            &orders_ctx(),
            "SELECT id FROM [dbo].[Orders] WHERE (note IN ('a\"b', 'c'))",
        );
    }

    #[test]
    fn array_subscript_outside_any_all_still_rejected() {
        assert_unsupported(
            "SELECT id FROM public.dbo_orders WHERE note[1] = 'x'",
            &orders_ctx(),
            "[",
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
            "SELECT c.name, SUM(o.amount) AS total FROM [dbo].[Orders] o JOIN [dbo].[Customers] c ON o.customer_id = c.id GROUP BY c.name HAVING SUM(o.amount) > 100 ORDER BY CASE WHEN total IS NULL THEN 1 ELSE 0 END DESC, total DESC OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
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
        use super::super::types::pg_type_to_mssql as m;
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

    // -- plain-scan SQL building (begin_scan) --------------------------------------

    mod plain_scan {
        use pgrx::pg_sys;
        use supabase_wrappers::prelude::{Cell, Column, Qual, Value};

        use super::super::super::mssql_fdw_rq::plain_scan_sql;

        fn qual(field: &str, operator: &str, value: Value, use_or: bool) -> Qual {
            Qual {
                field: field.to_string(),
                operator: operator.to_string(),
                value,
                use_or,
                param: None,
                array_had_nulls: false,
            }
        }

        fn col(name: &str) -> Column {
            Column {
                name: name.to_string(),
                num: 1,
                type_oid: pg_sys::Oid::INVALID,
            }
        }

        fn sql_for(quals: &[Qual]) -> String {
            plain_scan_sql("dbo", "Orders", &[col("id"), col("note")], quals)
                .map(|(sql, _)| sql)
                .unwrap_or_else(|e| panic!("plain_scan_sql failed: {e}"))
        }

        #[test]
        fn equality_binds_parameter() {
            assert_eq!(
                sql_for(&[qual("id", "=", Value::Cell(Cell::I64(7)), false)]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [id] = @P1"
            );
        }

        #[test]
        fn in_list_becomes_in() {
            // `x = ANY (…)` — the production case 1 shape
            assert_eq!(
                sql_for(&[qual(
                    "id",
                    "=",
                    Value::Array(vec![Cell::I64(1), Cell::I64(26), Cell::I64(51)]),
                    true,
                )]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [id] IN (@P1, @P2, @P3)"
            );
        }

        #[test]
        fn not_in_list_becomes_not_in() {
            assert_eq!(
                sql_for(&[qual(
                    "id",
                    "<>",
                    Value::Array(vec![Cell::I64(1), Cell::I64(2)]),
                    false,
                )]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [id] NOT IN (@P1, @P2)"
            );
        }

        #[test]
        fn any_with_other_operator_becomes_or_chain() {
            assert_eq!(
                sql_for(&[qual(
                    "id",
                    "<",
                    Value::Array(vec![Cell::I64(10), Cell::I64(20)]),
                    true,
                )]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE ([id] < @P1 OR [id] < @P2)"
            );
        }

        #[test]
        fn empty_any_is_false_and_empty_all_is_true() {
            assert_eq!(
                sql_for(&[qual("id", "=", Value::Array(vec![]), true)]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE 1 = 0"
            );
            assert_eq!(
                sql_for(&[qual("id", "<>", Value::Array(vec![]), false)]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE 1 = 1"
            );
        }

        // #10: NULL elements of the source array re-enter the rendered list
        // (`x <> ALL('{1,NULL}')` matches no rows; dropping the NULL would
        // wrongly match every x <> 1)
        fn null_array_qual(use_or: bool, cells: Vec<Cell>) -> Qual {
            Qual {
                field: "id".to_string(),
                operator: if use_or { "=" } else { "<>" }.to_string(),
                value: Value::Array(cells),
                use_or,
                param: None,
                array_had_nulls: true,
            }
        }

        #[test]
        fn array_qual_with_null_element_keeps_null() {
            assert_eq!(
                sql_for(&[null_array_qual(true, vec![Cell::I64(1)])]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [id] IN (@P1, NULL)"
            );
            assert_eq!(
                sql_for(&[null_array_qual(false, vec![Cell::I64(1)])]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [id] NOT IN (@P1, NULL)"
            );
            // other-operator chains get the NULL element as an UNKNOWN cond
            let mut q = null_array_qual(true, vec![Cell::I64(10)]);
            q.operator = "<".to_string();
            assert_eq!(
                sql_for(&[q]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE ([id] < @P1 OR [id] < NULL)"
            );
        }

        #[test]
        fn all_null_array_never_matches() {
            // `= ANY('{NULL}')` and `<> ALL('{NULL}')` both yield no rows
            assert_eq!(
                sql_for(&[null_array_qual(true, vec![])]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE 1 = 0"
            );
            assert_eq!(
                sql_for(&[null_array_qual(false, vec![])]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE 1 = 0"
            );
        }

        #[test]
        fn null_test_renders_is_null() {
            // NullTest quals arrive as is/is not with the literal cell "null"
            assert_eq!(
                sql_for(&[qual(
                    "note",
                    "is",
                    Value::Cell(Cell::String("null".to_string())),
                    false
                )]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [note] IS NULL"
            );
            assert_eq!(
                sql_for(&[qual(
                    "note",
                    "is not",
                    Value::Cell(Cell::String("null".to_string())),
                    false
                )]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [note] IS NOT NULL"
            );
        }

        #[test]
        fn ilike_wraps_both_sides_in_lower() {
            assert_eq!(
                sql_for(&[qual(
                    "note",
                    "~~*",
                    Value::Cell(Cell::String("a%".to_string())),
                    false
                )]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE LOWER([note]) LIKE LOWER(@P1)"
            );
        }

        #[test]
        fn multiple_quals_join_with_and() {
            assert_eq!(
                sql_for(&[
                    qual("id", "=", Value::Cell(Cell::I64(1)), false),
                    qual(
                        "note",
                        "~~",
                        Value::Cell(Cell::String("a%".to_string())),
                        false
                    ),
                ]),
                "SELECT [id], [note] FROM [dbo].[Orders] WHERE [id] = @P1 AND [note] LIKE @P2"
            );
        }
    }
} // mod unit

// ---------------------------------------------------------------------------
// e2e tests against a live MSSQL 2022 with the rqtest database (TZ §7.2,
// §10 acceptance queries). Enabled only under `cargo pgrx test`, which adds
// the pg_test feature automatically.
// ---------------------------------------------------------------------------

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Spi;
    use supabase_wrappers::prelude::create_async_runtime;
    use tiberius::{Client, Config};
    use tokio::net::TcpStream;
    use tokio_util::compat::TokioAsyncWriteCompatExt;

    fn mssql_password() -> String {
        std::env::var("MSSQL_SA_PASSWORD").unwrap_or_else(|_| "MssqlRq_2026!Pass".to_string())
    }

    fn mssql_conn_string() -> String {
        let host = std::env::var("MSSQL_HOST").unwrap_or_else(|_| "mssql".to_string());
        let port = std::env::var("MSSQL_PORT").unwrap_or_else(|_| "1433".to_string());
        format!(
            "Server={host},{port};Database=rqtest;IntegratedSecurity=false;\
             TrustServerCertificate=true;encrypt=DANGER_PLAINTEXT"
        )
    }

    fn setup() {
        let password = mssql_password();
        Spi::connect_mut(|c| {
            let ddl = [
                "CREATE FOREIGN DATA WRAPPER mssql_fdw_rq_fwd \
                 HANDLER mssql_fdw_rq_handler VALIDATOR mssql_fdw_rq_validator"
                    .to_string(),
                format!(
                    "CREATE SERVER mssql_rq_srv FOREIGN DATA WRAPPER mssql_fdw_rq_fwd \
                     OPTIONS (conn_string '{}', log_remote_query 'true')",
                    mssql_conn_string()
                ),
                format!(
                    "CREATE USER MAPPING FOR CURRENT_USER SERVER mssql_rq_srv \
                     OPTIONS (user 'sa', password '{password}')"
                ),
                "CREATE FOREIGN TABLE rq_orders (\
                   id bigint, customer_id uuid, status text, \
                   total_amount numeric(18,2), shipping_fee numeric(10,2), \
                   order_date date, placed_at timestamp, shipped_at timestamptz\
                 ) SERVER mssql_rq_srv OPTIONS (schema 'dbo', table 'orders')"
                    .to_string(),
                "CREATE FOREIGN TABLE rq_customers (\
                   id uuid, code int, name text, tier text, credit_limit numeric(18,2), \
                   active boolean, registered_on date, created_at timestamp\
                 ) SERVER mssql_rq_srv OPTIONS (schema 'dbo', table 'customers')"
                    .to_string(),
                "CREATE FOREIGN TABLE rq_products (\
                   id int, name text, category text, price numeric(18,2), \
                   weight_kg real, rating float8, ean13 bytea, in_stock boolean, \
                   updated_at timestamp\
                 ) SERVER mssql_rq_srv OPTIONS (schema 'dbo', table 'products')"
                    .to_string(),
                "CREATE FOREIGN TABLE rq_payments (\
                   id bigint, order_id bigint, method text, amount numeric(10,2), \
                   paid_on date, paid_at time\
                 ) SERVER mssql_rq_srv OPTIONS (schema 'dbo', table 'payments')"
                    .to_string(),
                "CREATE FOREIGN TABLE rq_shipments (\
                   id bigint, order_id bigint, carrier text, track_code text, \
                   shipped_on timestamp, delivered boolean\
                 ) SERVER mssql_rq_srv OPTIONS (schema 'dbo', table 'shipments')"
                    .to_string(),
            ];
            for sql in &ddl {
                c.update(sql, None, &[]).unwrap();
            }
        });
    }

    /// Run the same T-SQL directly against MSSQL and return (name, total)
    /// pairs — the reference result required by TZ §7.2.
    fn mssql_direct(sql: &str) -> Vec<(String, String)> {
        let rt = create_async_runtime().expect("runtime");
        let config = Config::from_ado_string(&format!(
            "{};User=sa;Password={}",
            mssql_conn_string(),
            mssql_password()
        ))
        .expect("ado string");
        let tcp = rt
            .block_on(TcpStream::connect(config.get_addr()))
            .expect("tcp");
        tcp.set_nodelay(true).expect("nodelay");
        let mut client = rt
            .block_on(Client::connect(config, tcp.compat_write()))
            .expect("connect");
        let stream = rt.block_on(client.query(sql, &[])).expect("query");
        let rows = rt.block_on(stream.into_first_result()).expect("result");
        rows.iter()
            .map(|r| {
                (
                    r.try_get::<&str, _>("name")
                        .expect("name col")
                        .unwrap()
                        .to_string(),
                    r.try_get::<&str, _>("total")
                        .expect("total col")
                        .unwrap()
                        .to_string(),
                )
            })
            .collect()
    }

    fn pg_pairs(sql: &str) -> Vec<(String, String)> {
        Spi::connect(|c| {
            c.select(sql, None, &[])
                .unwrap()
                .filter_map(|r| {
                    let name = r.get_by_name::<&str, _>("name").unwrap().map(str::to_owned);
                    let total = r
                        .get_by_name::<&str, _>("total")
                        .unwrap()
                        .map(str::to_owned);
                    name.zip(total)
                })
                .collect::<Vec<_>>()
        })
    }

    #[pg_test]
    fn simple_filter_matches_reference() {
        setup();

        // 1. plain filter (TZ §10 #1); seed: total_amount = 120 + n*37,
        // so > 2000 means n >= 51 → ids 51..60
        let pg = pg_pairs(
            "SELECT id::text AS name, total_amount::text AS total \
             FROM rq_orders WHERE total_amount > 2000 ORDER BY id",
        );
        let mssql = mssql_direct(
            "SELECT CAST(id AS nvarchar(20)) AS name, \
             CAST(total_amount AS nvarchar(30)) AS total \
             FROM dbo.orders WHERE total_amount > 2000 ORDER BY id",
        );
        assert_eq!(pg, mssql);
        assert_eq!(pg.len(), 10); // ids 51..60
    }

    /// The framework deparses the TOP-LEVEL statement for join queries, which
    /// inside a #[pg_test] is the test function call. The join acceptance
    /// query therefore runs through dblink (a real top-level statement) in a
    /// dedicated database with committed catalog objects.
    fn setup_committed() {
        let password = mssql_password();
        let admin_conn = Spi::get_one::<String>(
            "SELECT format('host=localhost port=%s dbname=postgres', current_setting('port'))",
        )
        .unwrap()
        .unwrap();
        let test_conn = Spi::get_one::<String>(
            "SELECT format('host=localhost port=%s dbname=rqjoin_test', current_setting('port'))",
        )
        .unwrap()
        .unwrap();

        Spi::run("CREATE EXTENSION IF NOT EXISTS dblink").unwrap();
        // recreate the dedicated database (each dblink_exec autocommits)
        Spi::run(&format!(
            "SELECT dblink_exec('{admin_conn}', $q$DROP DATABASE IF EXISTS rqjoin_test$q$)"
        ))
        .unwrap();
        Spi::run(&format!(
            "SELECT dblink_exec('{admin_conn}', $q$CREATE DATABASE rqjoin_test$q$)"
        ))
        .unwrap();

        let ddl = [
            "CREATE EXTENSION wrappers".to_string(),
            "CREATE FOREIGN DATA WRAPPER rqj_fwd \
             HANDLER mssql_fdw_rq_handler VALIDATOR mssql_fdw_rq_validator"
                .to_string(),
            format!(
                "CREATE SERVER rqj_srv FOREIGN DATA WRAPPER rqj_fwd \
                 OPTIONS (conn_string '{}')",
                mssql_conn_string()
            ),
            format!(
                "CREATE USER MAPPING FOR CURRENT_USER SERVER rqj_srv \
                 OPTIONS (user 'sa', password '{password}')"
            ),
            "CREATE FOREIGN TABLE rqj_orders (\
               id bigint NOT NULL, customer_id uuid, status text, total_amount numeric(18,2), \
               shipping_fee numeric(10,2), order_date date, placed_at timestamp, \
               shipped_at timestamptz\
             ) SERVER rqj_srv OPTIONS (schema 'dbo', table 'orders')"
                .to_string(),
            "CREATE FOREIGN TABLE rqj_customers (\
               id uuid, code int, name text, tier text, credit_limit numeric(18,2), \
               active boolean, registered_on date, created_at timestamp\
             ) SERVER rqj_srv OPTIONS (schema 'dbo', table 'customers')"
                .to_string(),
        ];
        for sql in &ddl {
            Spi::run(&format!(
                "SELECT dblink_exec('{test_conn}', $ddl${sql}$ddl$)"
            ))
            .unwrap();
        }
    }

    #[pg_test]
    fn join_aggregate_having_offset_fetch_matches_reference() {
        setup_committed();

        // 2. JOIN + SUM + HAVING + ORDER BY + OFFSET/FETCH (TZ §10 #2),
        // compared against the same query executed directly on MSSQL (§7.2)
        let pg = Spi::connect(|c| {
            let rows = c
                .select(
                    "SELECT * FROM dblink(\
                         format('host=localhost port=%s dbname=rqjoin_test', \
                                current_setting('port')), \
                         $$SELECT c.name AS name, SUM(o.total_amount)::text AS total \
                           FROM rqj_orders o JOIN rqj_customers c ON o.customer_id = c.id \
                           WHERE o.order_date >= DATE '2026-05-01' \
                           GROUP BY c.name \
                           HAVING SUM(o.total_amount) > 2500 \
                           ORDER BY SUM(o.total_amount) DESC \
                           OFFSET 1 ROWS FETCH NEXT 4 ROWS ONLY$$\
                     ) AS t(name text, total text)",
                    None,
                    &[],
                )
                .unwrap();
            let mut v: Vec<(String, String)> = rows
                .filter_map(|r| {
                    let name = r.get_by_name::<&str, _>("name").unwrap().map(str::to_owned);
                    let total = r
                        .get_by_name::<&str, _>("total")
                        .unwrap()
                        .map(str::to_owned);
                    name.zip(total)
                })
                .collect();
            v.sort();
            v
        });

        let mssql = {
            let mut v = mssql_direct(
                "SELECT c.name AS name, \
                 CAST(CAST(SUM(o.total_amount) AS numeric(18,2)) AS nvarchar(30)) AS total \
                 FROM dbo.orders o JOIN dbo.customers c ON o.customer_id = c.id \
                 WHERE o.order_date >= '2026-05-01' \
                 GROUP BY c.name \
                 HAVING SUM(o.total_amount) > 2500 \
                 ORDER BY SUM(o.total_amount) DESC \
                 OFFSET 1 ROWS FETCH NEXT 4 ROWS ONLY",
            );
            v.sort();
            v
        };
        assert_eq!(pg, mssql);
        assert_eq!(pg.len(), 4);
    }

    #[pg_test]
    fn prepared_statement_parameter() {
        setup_committed();

        // 3. prepared statement parameter $1 -> @P1 (TZ §10 #3). Runs
        // through dblink so PREPARE/EXECUTE are real top-level statements of
        // a dedicated session — like production drivers use them. (SPI-side
        // PREPARE/EXECUTE inside a #[pg_test] hit a pgrx type-read race on
        // PG17: "cache lookup failed for type 0" from reading the EXECUTE
        // result, see pgrx datum::lookup_type_name.)
        // The customer of order 1 (code 2) owns orders 1, 26, 51. A named
        // dblink connection keeps PREPARE and EXECUTE in one session.
        let conn = "format('host=localhost port=%s dbname=rqjoin_test', current_setting('port'))";
        Spi::run(&format!("SELECT dblink_connect('rqprep', {conn})")).unwrap();
        let cid: String = Spi::connect(|c| {
            c.select(
                "SELECT * FROM dblink('rqprep', \
                 $$SELECT customer_id::text FROM rqj_orders WHERE id = 1$$) AS t(cid text)",
                None,
                &[],
            )
            .unwrap()
            .filter_map(|r| r.get_by_name::<&str, _>("cid").unwrap().map(str::to_owned))
            .collect::<Vec<_>>()
            .pop()
            .expect("customer id")
        });
        Spi::run(
            "SELECT dblink_exec('rqprep', \
             $$PREPARE p(uuid) AS SELECT count(*) FROM rqj_orders \
               WHERE customer_id = $1$$)",
        )
        .unwrap();
        let cnt = Spi::connect(|c| {
            c.select(
                &format!("SELECT * FROM dblink('rqprep', $$EXECUTE p('{cid}')$$) AS t(cnt bigint)"),
                None,
                &[],
            )
            .unwrap()
            .filter_map(|r| r.get_by_name::<i64, _>("cnt").unwrap())
            .collect::<Vec<_>>()
            .pop()
            .expect("count")
        });
        Spi::run("SELECT dblink_disconnect('rqprep')").unwrap();
        assert_eq!(cnt, 3);
    }

    #[pg_test]
    fn distinct_pushdown() {
        setup();

        // 4. DISTINCT (TZ §10 #4): 25 distinct customers
        let cnt = Spi::get_one::<i64>(
            "SELECT count(*) FROM (SELECT DISTINCT customer_id FROM rq_orders) d",
        )
        .unwrap()
        .unwrap();
        assert_eq!(cnt, 25);
    }

    #[pg_test]
    fn window_function_matches_reference() {
        setup_committed();

        // TZ §10 M2: ROW_NUMBER() OVER (PARTITION BY … ORDER BY …) as one
        // remote operator; runs through dblink (top-level statement, see
        // join test) and is compared with the same query on MSSQL directly.
        // The window sort key must be a NOT NULL column (rqj_orders.id).
        let pg = Spi::connect(|c| {
            let rows = c
                .select(
                    "SELECT * FROM dblink(\
                         format('host=localhost port=%s dbname=rqjoin_test', current_setting('port')), \
                         $$SELECT id::text AS name, rn::text AS total \
                           FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY customer_id \
                                     ORDER BY id DESC) AS rn \
                                 FROM rqj_orders) t \
                           ORDER BY id$$\
                     ) AS t(name text, total text)",
                    None,
                    &[],
                )
                .unwrap();
            rows.filter_map(|r| {
                let name = r.get_by_name::<&str, _>("name").unwrap().map(str::to_owned);
                let total = r
                    .get_by_name::<&str, _>("total")
                    .unwrap()
                    .map(str::to_owned);
                name.zip(total)
            })
            .collect::<Vec<_>>()
        });

        let mssql = {
            let rt = create_async_runtime().expect("runtime");
            let config = Config::from_ado_string(&format!(
                "{};User=sa;Password={}",
                mssql_conn_string(),
                mssql_password()
            ))
            .expect("ado");
            let tcp = rt
                .block_on(TcpStream::connect(config.get_addr()))
                .expect("tcp");
            tcp.set_nodelay(true).expect("nodelay");
            let mut client = rt
                .block_on(Client::connect(config, tcp.compat_write()))
                .expect("connect");
            let stream = rt
                .block_on(client.query(
                    "SELECT CAST(id AS nvarchar(20)) AS name, CAST(rn AS nvarchar(10)) AS total \
                     FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY customer_id \
                               ORDER BY id DESC) AS rn FROM dbo.orders) t \
                     ORDER BY id",
                    &[],
                ))
                .expect("query");
            let rows = rt.block_on(stream.into_first_result()).expect("result");
            rows.iter()
                .map(|r| {
                    (
                        r.try_get::<&str, _>("name").unwrap().unwrap().to_string(),
                        r.try_get::<&str, _>("total").unwrap().unwrap().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(pg, mssql);
        // all 60 orders, one rank each partition position
        assert_eq!(pg.len(), 60);
    }

    // H: an ORDER BY key outside the SELECT list is a resjunk target entry.
    // It must not leak into the full-query scan's tuple slot while the sort
    // itself still happens remotely.
    #[pg_test]
    fn full_query_order_by_unselected_column() {
        setup_committed();

        let dblink_rows = |query: &str| -> Vec<String> {
            Spi::connect(|c| {
                let rows = c
                    .select(
                        &format!(
                            "SELECT * FROM dblink(\
                                 format('host=localhost port=%s dbname=rqjoin_test', \
                                        current_setting('port')), \
                                 $${query}$$) AS t(id text)"
                        ),
                        None,
                        &[],
                    )
                    .unwrap();
                rows.filter_map(|r| r.get_by_name::<&str, _>("id").unwrap().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
        };

        // resjunk shape: ORDER BY total_amount, SELECT id only
        let junk = dblink_rows("SELECT id::text FROM rqj_orders ORDER BY total_amount LIMIT 3");
        // reference shape: the sort key is in the select list (no junk)
        let reference = dblink_rows(
            "SELECT id::text FROM \
             (SELECT id, total_amount FROM rqj_orders ORDER BY total_amount LIMIT 3) t",
        );

        assert_eq!(junk, reference);
        assert_eq!(junk.len(), 3);
    }

    // The deparser prints LIKE as the ~~ operator; until 2026-09-04 every
    // LIKE query under full-query pushdown failed to lex on '~'.
    #[pg_test]
    fn full_query_like_operator_pushes_down() {
        setup_committed();

        let dblink_count = |query: &str| -> i64 {
            Spi::connect(|c| {
                let mut rows = c
                    .select(
                        &format!(
                            "SELECT * FROM dblink(\
                                 format('host=localhost port=%s dbname=rqjoin_test', \
                                        current_setting('port')), \
                                 $${query}$$) AS t(cnt bigint)"
                        ),
                        None,
                        &[],
                    )
                    .unwrap();
                rows.next()
                    .and_then(|r| r.get_by_name::<i64, _>("cnt").unwrap())
                    .unwrap()
            })
        };

        let matched = dblink_count("SELECT count(*) AS cnt FROM rqj_orders WHERE status LIKE '%'");
        let not_null =
            dblink_count("SELECT count(*) AS cnt FROM rqj_orders WHERE status IS NOT NULL");

        assert_eq!(matched, not_null, "LIKE '%' matches every non-NULL status");
        assert!(matched > 0);
    }

    // Sber Navigator integration: it creates foreign tables with the
    // tds_fdw-compatible option spellings (schema_name/table_name) and its
    // templates build the server from host/port/database + a username/
    // password user mapping.
    #[pg_test]
    fn navigator_compatible_table_options() {
        setup();

        // tds_fdw spelling of table options on the scan path
        Spi::run(
            "CREATE FOREIGN TABLE rq_orders_nav (\
               id bigint, status text, total_amount numeric(18,2) \
             ) SERVER mssql_rq_srv OPTIONS (schema_name 'dbo', table_name 'orders')",
        )
        .unwrap();
        let cnt: i64 = Spi::get_one("SELECT count(*) FROM rq_orders_nav WHERE total_amount > 0")
            .unwrap()
            .unwrap();
        assert!(cnt > 0, "aliased options must drive a plain scan");
        Spi::run("DROP FOREIGN TABLE rq_orders_nav").unwrap();
    }

    // IMPORT FOREIGN SCHEMA — how BI tools introspect a source. Runs through
    // a dblink session (a real top-level statement over committed catalog
    // objects, mirroring how Navigator uses it). Historically this also
    // sidestepped a pgrx SPI-nesting race triggered by the stats collector's
    // writes, which the FDW no longer performs.
    #[pg_test]
    fn import_foreign_schema_via_top_level_session() {
        setup_committed();

        let conn = Spi::get_one::<String>(
            "SELECT format('host=localhost port=%s dbname=rqjoin_test', current_setting('port'))",
        )
        .unwrap()
        .unwrap();

        Spi::run(&format!(
            "SELECT dblink_exec('{conn}', $q$CREATE SCHEMA nav_import$q$)"
        ))
        .unwrap();
        Spi::run(&format!(
            "SELECT dblink_exec('{conn}', $q$\
             IMPORT FOREIGN SCHEMA dbo LIMIT TO (orders, customers) \
             FROM SERVER rqj_srv INTO nav_import$q$)"
        ))
        .unwrap();

        let imported: String = Spi::get_one(&format!(
            "SELECT (SELECT c FROM dblink('{conn}', \
                     $$SELECT coalesce(string_agg(c.relname, ',' ORDER BY c.relname), '<none>') \
                       FROM pg_foreign_table ft \
                       JOIN pg_class c ON c.oid = ft.ftrelid \
                       JOIN pg_namespace n ON n.oid = c.relnamespace \
                       WHERE n.nspname = 'nav_import'$$) AS t(c text))"
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            imported.as_str(),
            "customers,orders",
            "LIMIT TO must import exactly the listed tables"
        );

        let orders: i64 = Spi::get_one(&format!(
            "SELECT (SELECT count(*) FROM dblink('{conn}', \
                     $$SELECT count(*) FROM nav_import.orders$$) AS t(c bigint))"
        ))
        .unwrap()
        .unwrap();
        let customers: i64 = Spi::get_one(&format!(
            "SELECT (SELECT count(*) FROM dblink('{conn}', \
                     $$SELECT count(*) FROM nav_import.customers$$) AS t(c bigint))"
        ))
        .unwrap()
        .unwrap();
        assert!(orders > 0, "imported orders table must be queryable");
        assert!(customers > 0, "imported customers table must be queryable");
    }

    /// A second server of this FDW pointing at the same MSSQL instance.
    /// Two distinct servers keep the remote-query policy at Optional, so
    /// joins between the tables run as local joins over remote scans.
    fn setup_second_server() {
        Spi::run(&format!(
            "CREATE SERVER mssql_rq_srv2 FOREIGN DATA WRAPPER mssql_fdw_rq_fwd \
             OPTIONS (conn_string '{}')",
            mssql_conn_string()
        ))
        .unwrap();
        Spi::run(&format!(
            "CREATE USER MAPPING FOR CURRENT_USER SERVER mssql_rq_srv2 \
             OPTIONS (user 'sa', password '{}')",
            mssql_password()
        ))
        .unwrap();
        Spi::run(
            "CREATE FOREIGN TABLE rq_orders2 (\
               id bigint, customer_id uuid, status text \
             ) SERVER mssql_rq_srv2 OPTIONS (schema 'dbo', table 'orders')",
        )
        .unwrap();
    }

    // Regression: a join between foreign tables of two distinct servers of
    // this FDW. PostgreSQL never hands such a join to GetForeignJoinPaths,
    // so a Require policy can only dead-end into a hard error at plan time.
    // The FDW must downgrade to Optional and let PostgreSQL join the two
    // remote scans locally.
    #[pg_test]
    fn cross_server_join_falls_back_to_local_join() {
        setup();
        setup_second_server();

        // planning alone used to raise
        // "remote-query execution is required by this FDW ..."
        Spi::run(
            "EXPLAIN (COSTS OFF) \
             SELECT count(*) FROM rq_orders o JOIN rq_orders2 o2 ON o.id = o2.id",
        )
        .unwrap();

        let joined: i64 =
            Spi::get_one("SELECT count(*) FROM rq_orders o JOIN rq_orders2 o2 ON o.id = o2.id")
                .unwrap()
                .unwrap();
        let total: i64 = Spi::get_one("SELECT count(*) FROM rq_orders2")
            .unwrap()
            .unwrap();
        assert_eq!(joined, total, "cross-server join should return all rows");
    }

    // #9: the inner side of a nested-loop join is rescanned with unchanged
    // parameters; the FDW replays the remote stream on a fresh connection
    // instead of failing with "rescans are not supported by the streaming
    // executor".
    #[pg_test]
    fn nested_loop_rescan_replays_the_remote_stream() {
        setup();
        setup_second_server();

        let reference: i64 =
            Spi::get_one("SELECT count(*) FROM rq_orders o JOIN rq_orders2 o2 ON o.id = o2.id")
                .unwrap()
                .unwrap();

        // force a nested loop without a Materialize shield so the foreign
        // scan itself is the rescanned inner side
        Spi::run(
            "SET LOCAL enable_hashjoin = off; \
             SET LOCAL enable_mergejoin = off; \
             SET LOCAL enable_material = off;",
        )
        .unwrap();
        let nested: i64 =
            Spi::get_one("SELECT count(*) FROM rq_orders o JOIN rq_orders2 o2 ON o.id = o2.id")
                .unwrap()
                .unwrap();

        assert!(reference > 0);
        assert_eq!(nested, reference, "rescanned inner scan must replay rows");
    }

    #[pg_test(error = "option 'rowid_column' is required")]
    fn writes_are_rejected() {
        setup();

        // 5. read-only (TZ §10 #5): no writable path exists at all — the
        // framework rejects DML long before reaching our read-only guard
        Spi::run("INSERT INTO rq_orders (id, status) VALUES (1, 'x')").unwrap();
    }

    #[pg_test]
    fn type_round_trip() {
        use pgrx::datum::{Time, Timestamp};

        setup();

        // varbinary(13) -> bytea: '4600000000001' for product 1 (plain scan
        // fetches the raw value; no PG-specific functions are pushed down)
        let ean = Spi::get_one::<Vec<u8>>("SELECT ean13 FROM rq_products WHERE id = 1")
            .unwrap()
            .unwrap();
        assert_eq!((&ean[0..2], ean[12]), (b"46".as_slice(), b'1'));
        assert_eq!(ean.len(), 13);

        let weight = Spi::get_one::<f32>("SELECT weight_kg FROM rq_products WHERE id = 1")
            .unwrap()
            .unwrap();
        assert!((weight - 0.36).abs() < 1e-6);

        let rating = Spi::get_one::<f64>("SELECT rating FROM rq_products WHERE id = 1")
            .unwrap()
            .unwrap();
        assert!((rating - 3.1).abs() < 1e-9);

        // time(0) -> time: TIMEFROMPARTS(10, 1, 0) → 10:01:00
        let paid_at = Spi::get_one::<Time>("SELECT paid_at FROM rq_payments WHERE id = 1")
            .unwrap()
            .unwrap();
        assert_eq!((paid_at.hour(), paid_at.minute()), (10, 1));

        // datetime -> timestamp: order 1 shipped +1 day after 2026-02-02
        let shipped = Spi::get_one::<Timestamp>("SELECT shipped_on FROM rq_shipments WHERE id = 1")
            .unwrap()
            .unwrap();
        assert_eq!(
            (shipped.year(), shipped.month(), shipped.day()),
            (2026, 2, 3)
        );

        // datetimeoffset -> timestamptz: 40 of 60 orders shipped (COUNT
        // returns int32 in T-SQL; the FDW widens it to int8)
        let cnt =
            Spi::get_one::<i64>("SELECT count(*) FROM rq_orders WHERE shipped_at IS NOT NULL")
                .unwrap()
                .unwrap();
        assert_eq!(cnt, 40);
    }

    // Bare bit columns in predicate position become `= 1` / `= 0`. Split into
    // one #[pg_test] per statement: several full-query scans in a single
    // pg_test function trip a pgrx 0.16 result-TupleDesc staleness
    // ("cache lookup failed for type 0") once the FDW no longer performs an
    // SPI write during the scan — top-level sessions (production, dblink
    // tests) are unaffected.
    #[pg_test]
    fn bare_boolean_predicates() {
        setup();

        let active = Spi::get_one::<i64>("SELECT count(*) FROM rq_customers WHERE active")
            .unwrap()
            .unwrap();
        assert_eq!(active, 22);
    }

    #[pg_test]
    fn bare_boolean_predicates_not() {
        setup();

        let inactive = Spi::get_one::<i64>("SELECT count(*) FROM rq_customers WHERE NOT active")
            .unwrap()
            .unwrap();
        assert_eq!(inactive, 3);
    }

    #[pg_test]
    fn bare_boolean_predicates_and() {
        setup();

        let delivered = Spi::get_one::<i64>(
            "SELECT count(*) FROM rq_shipments WHERE delivered AND order_id <= 20",
        )
        .unwrap()
        .unwrap();
        assert_eq!(delivered, 15); // 20 minus ids 4, 8, 12, 16, 20
    }

    #[pg_test]
    fn in_list_filter_pushes_down() {
        setup();

        // production case 1: `IN (…)` arrives as an ANY (array) qual and must
        // push down to the plain scan as one condition, not be rejected
        let mut ids: Vec<i64> = Spi::connect(|c| {
            c.select(
                "SELECT id FROM rq_orders WHERE id IN (1, 26, 51, 999)",
                None,
                &[],
            )
            .unwrap()
            .filter_map(|r| r.get_by_name::<i64, _>("id").unwrap())
            .collect()
        });
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 26, 51]);
    }

    #[pg_test]
    fn in_list_with_aggregate_pushes_down() {
        setup();

        // production case 4: an aggregate forces the statement through the
        // full-query translator, where the deparser prints the IN-list as
        // `id = ANY (ARRAY[…]::bigint[])`
        let cnt =
            Spi::get_one::<i64>("SELECT count(*) FROM rq_orders WHERE id IN (1, 26, 51, 999)")
                .unwrap()
                .unwrap();
        assert_eq!(cnt, 3);
    }

    #[pg_test]
    fn or_across_columns_runs_remotely() {
        setup();

        // production case 2: an OR across two columns cannot become scan
        // quals; the whole statement must run as one remote query, with no
        // local Filter node above the Foreign Scan
        let plan = Spi::connect(|c| {
            c.select(
                "EXPLAIN (COSTS OFF) SELECT id FROM rq_orders \
                 WHERE id = 1 OR total_amount > 2000",
                None,
                &[],
            )
            .unwrap()
            .filter_map(|r| {
                r.get_by_name::<&str, _>("QUERY PLAN")
                    .unwrap()
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n")
        });
        assert!(plan.contains("Foreign Scan"), "plan: {plan}");
        assert!(!plan.contains("Filter"), "plan: {plan}");

        // seed: total_amount = 120 + n*37, so > 2000 means ids 51..60, plus
        // the explicit id = 1 → 11 rows in total
        let mut ids: Vec<i64> = Spi::connect(|c| {
            c.select(
                "SELECT id FROM rq_orders WHERE id = 1 OR total_amount > 2000",
                None,
                &[],
            )
            .unwrap()
            .filter_map(|r| r.get_by_name::<i64, _>("id").unwrap())
            .collect()
        });
        ids.sort_unstable();
        let expected: Vec<i64> = (1..=60i64)
            .filter(|n| *n == 1 || 120 + n * 37 > 2000)
            .collect();
        assert_eq!(ids, expected);
        assert_eq!(ids.len(), 11);
    }

    #[pg_test]
    fn join_without_output_aliases() {
        setup_committed();

        // production case 3: join-path target lists carry positional names
        // (column_N) that do not exist in the remote result; cells must be
        // matched by position. The customer with code 2 owns orders 1, 26, 51
        let rows: Vec<(i64, i32)> = Spi::connect(|c| {
            c.select(
                "SELECT * FROM dblink(\
                     format('host=localhost port=%s dbname=rqjoin_test', current_setting('port')), \
                     $$SELECT o.id, c.code FROM rqj_orders o \
                       JOIN rqj_customers c ON o.customer_id = c.id \
                       WHERE c.code = 2$$\
                 ) AS t(id bigint, code int)",
                None,
                &[],
            )
            .unwrap()
            .filter_map(|r| {
                let id = r.get_by_name::<i64, _>("id").unwrap()?;
                let code = r.get_by_name::<i32, _>("code").unwrap()?;
                Some((id, code))
            })
            .collect()
        });
        let mut ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 26, 51]);
        // a positional mix-up (id ↔ code) would surface here
        assert!(rows.iter().all(|(_, code)| *code == 2), "rows: {rows:?}");
    }

    #[pg_test]
    fn plan_is_foreign_scan() {
        setup();

        // §7.3: aggregates/joins run remotely, no local nodes above the scan
        let plan = Spi::connect(|c| {
            c.select(
                "EXPLAIN (VERBOSE) SELECT c.name, SUM(o.total_amount) \
                 FROM rq_orders o JOIN rq_customers c ON o.customer_id = c.id \
                 GROUP BY c.name",
                None,
                &[],
            )
            .unwrap()
            .filter_map(|r| {
                r.get_by_name::<&str, _>("QUERY PLAN")
                    .unwrap()
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n")
        });
        assert!(plan.contains("Foreign Scan"), "plan: {plan}");
        assert!(!plan.contains("Aggregate"), "plan: {plan}");
        assert!(!plan.contains("Sort"), "plan: {plan}");
        assert!(!plan.contains("Join"), "plan: {plan}");
    }
}
