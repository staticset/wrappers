# mssql_fdw_rq

Read-only Foreign Data Wrapper for MS SQL Server with **full-query pushdown**:
the whole statement (JOINs, aggregates, ORDER BY, LIMIT/OFFSET, DISTINCT,
window functions, parameters) is translated to a single T-SQL query and
executed remotely in one round-trip. PostgreSQL acts only as a bridge that
renders the final rows; results are **streamed** row by row, never fully
materialized.

Built on the remote-query mechanism of
[supabase/wrappers PR #615](https://github.com/supabase/wrappers/pull/615)
(`FullQuery` / `RemoteQueryPolicy::Require` / `begin_remote_query`).

**Status: M2.** Covered: WHERE / JOIN / GROUP BY / HAVING / ORDER BY (with
PostgreSQL-faithful NULL ordering via CASE tiebreakers) / LIMIT / OFFSET /
DISTINCT / window functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`,
`LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, aggregates `OVER (PARTITION BY …)`)
/ typed parameters (`$1` → `CAST(@P1 AS …)`) / `ILIKE` / boolean predicates /
identifier mapping / date-time-bytea round-trips / streaming execution /
Kerberos (Windows Integrated) authentication.

Anything outside the supported set fails with an explicit
`UnsupportedConstruct { sql_fragment, reason }` — never silently wrong SQL.

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
  id          bigint NOT NULL,
  customer_id uuid,
  amount      numeric(18,2),
  created_at  timestamp
)
  server mssql_srv
  options (schema 'dbo', table 'Orders');

-- everything below runs as ONE statement on SQL Server:
select c.name, sum(o.amount) as total, row_number() over (partition by c.name order by o.id desc)
from dbo_orders o join dbo_customers c on o.customer_id = c.id
where o.created_at >= date '2026-01-01'
group by c.name having sum(o.amount) > 100
order by total desc offset 10 rows fetch next 5 rows only;
```

The wrapper is **read-only**: `INSERT`/`UPDATE`/`DELETE` are rejected.

**Tip:** declare foreign-table columns `NOT NULL` when the remote column is
constrained. Sort keys of window functions must be NOT NULL columns (T-SQL's
implicit NULL ordering differs from PostgreSQL's and cannot be corrected
inside `OVER(…)`); top-level ORDER BY handles nullable keys automatically via
a CASE tiebreaker.

### Options

| Object | Option | Notes |
|---|---|---|
| server | `conn_string` \| `conn_string_id` | ADO connection string (vault secret id for Supabase) |
| server | `log_remote_query` | `'true'` logs the sent T-SQL at LOG level |
| server | `auth` | `'kerberos'` — Windows Integrated auth (see below) |
| table | `schema` | remote schema, default `dbo` |
| table | `table` | remote table (required) |
| user mapping | `user` + `password` \| `password_id` | SQL Server auth; alternatively `User=`/`Password=` in `conn_string` |

### Kerberos / Windows Integrated (M2)

Build the extension with the `mssql_fdw_rq_kerberos` cargo feature (requires
`libgssapi-dev` at build time) and set `auth 'kerberos'` on the server. The
backend then authenticates as the OS user of the PostgreSQL server process
via GSSAPI (`AuthMethod::Integrated`).

Manual checklist (not covered by CI):

1. PostgreSQL host is domain-joined or has a keytab for a domain service
   account; `KRB5_KTNAME` (or `default_keytab_name` in `krb5.conf`) points to
   it, `KRB5_CONFIG` resolves the domain (or extend `krb5.conf` with the
   AD DCs).
2. `kinit -kt <keytab> <SPN>` (or `k5start`) obtains a TGT for the postgres
   service user; verify with `kvno MSSQLSvc/<sqlserver-fqdn>:1433`.
3. SQL Server side: SPNs `MSSQLSvc/<fqdn>:1433` registered for the engine
   account, Kerberos enabled in the network protocol settings.
4. `CREATE SERVER … OPTIONS (conn_string 'Server=<fqdn>,1433;…',
   auth 'kerberos')` — no user mapping needed.
5. `select count(*) from dbo_orders;` — check `wrappers_fdw_stats` and the
   MSSQL `sys.dm_exec_sessions.auth_scheme` column shows `Kerberos`.

## Limitations

- **Set operations** (`UNION [ALL]` / `INTERSECT` / `EXCEPT`): PostgreSQL's
  planner offers FDW hooks only for base relations, so a set operation cannot
  become one remote statement (this matches postgres_fdw). Each arm is pushed
  down with its filters; the merge happens locally.
- **JOIN pushdown** uses the statement text; queries executed through SPI or
  PL/pgSQL resolve to the enclosing statement and are rejected with an
  explicit error instead of sending unrelated SQL. Top-level statements
  (psql, drivers, BI tools) are unaffected.
- Rescans (`RESCAN` plan nodes) are rejected by the streaming executor.
- LOB(MAX) types (`nvarchar(max)` etc.), `xml`, JSON and spatial types are
  rejected explicitly; arrays are not supported in v1.
- `LIMIT ALL`, `DISTINCT ON`, POSIX regex operators, subquery `LIMIT` are
  rejected explicitly.

## Development

```bash
docker compose -f .dev/docker-compose.yml up -d          # MSSQL 2022 + rqtest
docker exec -it mssql-fdw-rq-builder bash
cd /work/wrappers/wrappers
cargo test --lib --no-default-features --features "mssql_fdw_rq pg15"  # translator
USER=dev cargo pgrx test pg15 --no-default-features --features "mssql_fdw_rq pg15"
```

CI: `.github/workflows/mssql-fdw-rq.yml` runs the same suite on PostgreSQL 15
and 17 against a live MSSQL 2022 service, plus a Kerberos-feature build check.
