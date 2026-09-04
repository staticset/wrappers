// PG-level tests for the helloworld FDW. It needs no external service, so
// these double as framework regression tests for planner/executor paths
// shared by every FDW (see supabase-wrappers/src/scan.rs).

// `cargo pgrx test` builds the extension without cfg(test), so the module
// must be compiled in unconditionally and gated here instead.
#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Spi;

    fn setup() {
        Spi::connect_mut(|c| {
            let ddl = [
                "CREATE FOREIGN DATA WRAPPER helloworld_fwd \
                 HANDLER hello_world_fdw_handler VALIDATOR hello_world_fdw_validator"
                    .to_string(),
                "CREATE SERVER helloworld_srv FOREIGN DATA WRAPPER helloworld_fwd".to_string(),
            ];
            for sql in &ddl {
                c.update(sql, None, &[]).unwrap();
            }
        });
    }

    // Regression test: scanning a partition child calls GetForeignPlan with
    // reloptkind == RELOPT_OTHER_MEMBER_REL. scanrelid must keep the child's
    // valid RT index there (fdw_scan_tlist is NULL), otherwise the executor
    // cannot resolve the child's tuple descriptor and any partitioned
    // foreign table breaks.
    #[pg_test]
    fn partitioned_foreign_table_scan() {
        setup();
        Spi::run(
            "CREATE TABLE hw_part (id bigint, col text) PARTITION BY RANGE (id);
             CREATE FOREIGN TABLE hw_part_1 PARTITION OF hw_part \
               FOR VALUES FROM (0) TO (100) SERVER helloworld_srv;
             CREATE FOREIGN TABLE hw_part_2 PARTITION OF hw_part \
               FOR VALUES FROM (100) TO (200) SERVER helloworld_srv;",
        )
        .unwrap();

        let cnt: i64 = Spi::get_one("SELECT count(*) FROM hw_part")
            .unwrap()
            .unwrap();
        assert_eq!(cnt, 2, "each partition should return its row");

        // partition pruning: only hw_part_1 matches the bound
        let n: i64 = Spi::get_one("SELECT count(*) FROM hw_part WHERE id = 0")
            .unwrap()
            .unwrap();
        assert_eq!(n, 1);

        // sorted plan over partition children (Sort over Append over scans)
        let col: String = Spi::get_one("SELECT col FROM hw_part ORDER BY col LIMIT 1")
            .unwrap()
            .unwrap();
        assert_eq!(col, "Hello world");
    }

    // Same executor path via traditional inheritance: child tables of an
    // inherited foreign parent are also scanned as OTHER_MEMBER_REL.
    #[pg_test]
    fn inherited_foreign_table_scan() {
        setup();
        Spi::run(
            "CREATE FOREIGN TABLE hw_parent (id bigint, col text) SERVER helloworld_srv;
             CREATE FOREIGN TABLE hw_child () INHERITS (hw_parent) SERVER helloworld_srv;",
        )
        .unwrap();

        let cnt: i64 = Spi::get_one("SELECT count(*) FROM hw_parent")
            .unwrap()
            .unwrap();
        assert_eq!(cnt, 2, "parent and child should each return one row");
    }
}
