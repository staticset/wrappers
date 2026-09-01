# mssql_fdw_rq

Read-only Foreign Data Wrapper for MS SQL Server with **full-query pushdown**:
the whole statement (JOINs, aggregates, ORDER BY, LIMIT/OFFSET, DISTINCT,
parameters) is translated to a single T-SQL query and executed remotely in one
round-trip. PostgreSQL acts only as a bridge that renders the final rows.

Built on the remote-query mechanism of
[supabase/wrappers PR #615](https://github.com/supabase/wrappers/pull/615)
(`FullQuery` / `RemoteQueryPolicy::Require` / `begin_remote_query`).

**Status: M1 skeleton.** The translator covers WHERE / JOIN / GROUP BY /
HAVING / ORDER BY / LIMIT / OFFSET / DISTINCT, typed parameters (`$1` →
`CAST(@P1 AS ...)` via tiberius), `ILIKE`, boolean predicates and identifier
mapping. Not wired yet: boolean-column catalog lookups for mixed queries,
bytea/time value round-trips, window functions and set operations (M2).
Unsupported constructs fail with an explicit
`UnsupportedConstruct { sql_fragment, reason }` error — never silently wrong
SQL.

## Usage

```sql
create extension wrappers;

create foreign data wrapper mssql_fdw_rq
  handler mssql_fdw_rq_handler
  validator mssql_fdw_rq_validator;

create server mssql_srv
  foreign data wrapper mssql_fdw_rq
  options (conn_string 'Server=host,1433;Database=db;TrustServerCertificate=true;encrypt=DANGER_PLAINTEXT');

create user mapping for current_user
  server mssql_srv
  options (user 'sa', password '...');          -- or password_id '<vault-secret-id>'

create foreign table dbo_orders (
  id          bigint,
  customer_id uuid,
  amount      numeric(18,2),
  created_at  timestamp
)
  server mssql_srv
  options (schema 'dbo', table 'Orders');
```

The wrapper is **read-only**: `INSERT`/`UPDATE`/`DELETE` are rejected with
`mssql_fdw_rq is read-only`.

### Options

| Object | Option | Notes |
|---|---|---|
| server | `conn_string` \| `conn_string_id` | ADO connection string (vault secret id for Supabase) |
| server | `log_remote_query` | `'true'` logs the sent T-SQL and its duration at LOG level |
| table | `schema` | remote schema, default `dbo` |
| table | `table` | remote table (required) |
| user mapping | `user` + `password` \| `password_id` | SQL Server auth; alternatively `User=`/`Password=` in `conn_string` |

## Development

```bash
docker compose -f .dev/docker-compose.yml up -d          # MSSQL 2022 + rqtest
docker exec -it mssql-fdw-rq-builder bash
cd /work/wrappers/wrappers
cargo test --lib --features "mssql_fdw_rq pg15"           # translator unit tests
cargo pgrx test pg15 --no-default-features --features "mssql_fdw_rq pg15"
```
