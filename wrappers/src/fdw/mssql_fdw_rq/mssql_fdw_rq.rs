use crate::stats;
use pgrx::pg_sys;
use pgrx::spi::Spi;
use std::collections::HashMap;
use std::time::Instant;
use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use supabase_wrappers::prelude::*;

use super::translator::{self, RelationMapping, TranslateContext};
use super::types;
use super::{MssqlFdwRqError, MssqlFdwRqResult};

/// Bracket-quote an identifier for T-SQL, rejecting anything unsafe.
fn bracket_name(name: &str) -> MssqlFdwRqResult<String> {
    if name.is_empty() || name.contains(']') {
        return Err(MssqlFdwRqError::InvalidOption(format!(
            "identifier '{name}' cannot be quoted safely for T-SQL"
        )));
    }
    Ok(format!("[{name}]"))
}

/// Does an ADO-style connection string carry a user? tiberius parses the
/// canonical `User ID=` and the `UID=` alias; a bare `user=` key does not
/// exist in its grammar.
fn conn_str_has_user(conn_str: &str) -> bool {
    conn_str.split(';').any(|kv| {
        let key = kv.trim_start().to_ascii_lowercase();
        key.starts_with("user id=") || key.starts_with("uid=") || key.starts_with("user=")
    })
}

/// Map a PostgreSQL qual operator name to its T-SQL rendering; returns the
/// whole predicate shape for the pattern operators (ILIKE needs LOWER on
/// both sides, mirroring the full-query translator).
fn sql_predicate(field: &str, oper: &str, param: &str) -> String {
    match oper {
        "~~" => format!("{field} LIKE {param}"),
        "!~~" => format!("{field} NOT LIKE {param}"),
        "~~*" => format!("LOWER({field}) LIKE LOWER({param})"),
        "!~~*" => format!("LOWER({field}) NOT LIKE LOWER({param})"),
        o => format!("{field} {o} {param}"),
    }
}

/// Render one qual into a T-SQL predicate, appending typed parameters for
/// every value (values are never concatenated into the SQL text, TZ §5.4).
fn render_qual(
    qual: &Qual,
    params: &mut Vec<Box<dyn tiberius::ToSql>>,
) -> MssqlFdwRqResult<String> {
    let field = bracket_name(&qual.field)?;
    let oper = qual.operator.as_str();

    // ScalarArrayOpExpr quals: `x = ANY (array)` (use_or) or `x <> ALL (…)`
    if let Value::Array(cells) = &qual.value {
        return render_array_qual(
            &field,
            oper,
            qual.use_or,
            qual.array_had_nulls,
            cells,
            params,
        );
    }

    match &qual.value {
        Value::Cell(Cell::Bool(b)) if oper == "is" => Ok(format!("{field} = {}", *b as u8)),
        Value::Cell(Cell::Bool(b)) if oper == "is not" => Ok(format!("{field} <> {}", *b as u8)),
        // NullTest quals arrive as is/is not with the literal cell "null"
        Value::Cell(Cell::String(s)) if oper == "is" && s == "null" => {
            Ok(format!("{field} IS NULL"))
        }
        Value::Cell(Cell::String(s)) if oper == "is not" && s == "null" => {
            Ok(format!("{field} IS NOT NULL"))
        }
        Value::Cell(cell) => {
            let n = params.len() + 1;
            let cond = sql_predicate(&field, oper, &format!("@P{n}"));
            params.push(types::cell_to_sql(cell)?);
            Ok(cond)
        }
        // handled by the array branch above
        Value::Array(_) => unreachable!("array quals are rendered by render_array_qual"),
    }
}

/// Render an array qual: `= ANY` becomes `IN` (and `<> ALL` → `NOT IN`);
/// other operators expand into OR-chains (ANY) / AND-chains (ALL). NULL
/// elements of the source array are re-added literally — PostgreSQL and
/// T-SQL agree on the three-valued IN/NOT IN semantics, and dropping them
/// would flip `x <> ALL('{1,NULL}')` from "no rows" to "every x <> 1".
fn render_array_qual(
    field: &str,
    oper: &str,
    use_or: bool,
    had_nulls: bool,
    cells: &[Cell],
    params: &mut Vec<Box<dyn tiberius::ToSql>>,
) -> MssqlFdwRqResult<String> {
    // `x = ANY ('{}')` is FALSE and `x <> ALL ('{}')` is TRUE; T-SQL has no
    // empty IN list, so the degenerate cases become constants. An all-NULL
    // array never yields TRUE for either operator.
    if cells.is_empty() {
        if had_nulls {
            return Ok("1 = 0".to_string());
        }
        return Ok(if use_or {
            "1 = 0".to_string()
        } else {
            "1 = 1".to_string()
        });
    }

    let mut placeholders = Vec::with_capacity(cells.len());
    for cell in cells {
        let n = params.len() + 1;
        placeholders.push(format!("@P{n}"));
        params.push(types::cell_to_sql(cell)?);
    }

    if use_or && oper == "=" {
        let list = placeholders.join(", ");
        let list = if had_nulls {
            format!("{list}, NULL")
        } else {
            list
        };
        return Ok(format!("{field} IN ({list})"));
    }
    if !use_or && oper == "<>" {
        let list = placeholders.join(", ");
        let list = if had_nulls {
            format!("{list}, NULL")
        } else {
            list
        };
        return Ok(format!("{field} NOT IN ({list})"));
    }

    let conds: MssqlFdwRqResult<Vec<String>> = placeholders
        .iter()
        .map(|p| Ok(sql_predicate(field, oper, p)))
        .collect();
    let mut conds = conds?;
    if had_nulls {
        // UNKNOWN under three-valued logic — the same contribution the NULL
        // element makes inside PostgreSQL's ANY/ALL
        conds.push(format!("{field} {oper} NULL"));
    }
    Ok(format!(
        "({})",
        conds.join(if use_or { " OR " } else { " AND " })
    ))
}

/// Build the plain-scan statement for one table: the requested column list
/// plus one WHERE condition per qual. Pure function (unit-tested); the
/// parameters come back separately for binding by the executor.
pub(super) fn plain_scan_sql(
    remote_schema: &str,
    remote_table: &str,
    columns: &[Column],
    quals: &[Qual],
) -> MssqlFdwRqResult<(String, Vec<Box<dyn tiberius::ToSql>>)> {
    let cols = if columns.is_empty() {
        "*".to_string()
    } else {
        columns
            .iter()
            .map(|c| bracket_name(&c.name))
            .collect::<MssqlFdwRqResult<Vec<_>>>()?
            .join(", ")
    };

    let mut sql = format!(
        "SELECT {cols} FROM {}.{}",
        bracket_name(remote_schema)?,
        bracket_name(remote_table)?
    );

    let mut params: Vec<Box<dyn tiberius::ToSql>> = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    for qual in quals {
        conds.push(render_qual(qual, &mut params)?);
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }

    Ok((sql, params))
}

/// Cloneable blueprint of the running scan so a rescan can replay it:
/// [`MssqlFdwRq::spawn_streaming_query`] opens a fresh connection and
/// consumes its parameters by value, so the statement is rebuilt from
/// these inputs each time it (re-)executes.
enum ScanPlan {
    /// plain single-table scan: rebuildable with [`plain_scan_sql`]
    Plain {
        remote_schema: String,
        remote_table: String,
        columns: Vec<Column>,
        quals: Vec<Qual>,
    },
    /// full-query pushdown: the translated T-SQL plus the framework's
    /// evaluated query parameters
    Remote {
        tsql: String,
        parameters: Vec<RemoteQueryParameter>,
    },
}

impl ScanPlan {
    fn materialize(&self) -> MssqlFdwRqResult<(String, Vec<Box<dyn tiberius::ToSql>>)> {
        match self {
            Self::Plain {
                remote_schema,
                remote_table,
                columns,
                quals,
            } => plain_scan_sql(remote_schema, remote_table, columns, quals),
            Self::Remote { tsql, parameters } => {
                let params = parameters
                    .iter()
                    .map(|p| types::value_to_sql(p.value.as_ref(), p.type_oid))
                    .collect::<MssqlFdwRqResult<Vec<_>>>()?;
                Ok((tsql.clone(), params))
            }
        }
    }
}

#[wrappers_fdw(
    version = "0.1.0",
    author = "Rubicon",
    website = "https://github.com/staticset/wrappers/tree/feat/mssql-fdw-rq/wrappers/src/fdw/mssql_fdw_rq",
    error_type = "MssqlFdwRqError"
)]
pub(crate) struct MssqlFdwRq {
    rt: Runtime,
    config: Config,
    log_remote_query: bool,
    /// rows arrive from a background task through a bounded channel, so
    /// large results never sit fully in memory (TZ §6.1 streaming)
    rx: Option<tokio::sync::mpsc::Receiver<Result<tiberius::Row, tiberius::error::Error>>>,
    rows_out: i64,
    tgt_cols: Vec<Column>,
    /// what to re-execute when PostgreSQL rescans this ForeignScan
    rescan_plan: Option<ScanPlan>,
}

impl MssqlFdwRq {
    const FDW_NAME: &'static str = "MssqlFdwRq";

    /// Flush row counters into the statistics view. Rows of a stream that is
    /// abandoned mid-flight (a LIMIT further up the plan stops pulling, a
    /// rescan restarts the scan) would otherwise never be counted.
    fn flush_row_stats(&mut self) {
        if self.rows_out != 0 {
            stats::inc_stats(Self::FDW_NAME, stats::Metric::RowsIn, self.rows_out);
            stats::inc_stats(Self::FDW_NAME, stats::Metric::RowsOut, self.rows_out);
            self.rows_out = 0;
        }
    }

    /// Read the current user's user-mapping options for this server, if a
    /// mapping exists (the framework itself only exposes server options).
    unsafe fn user_mapping_options(server_oid: pg_sys::Oid) -> HashMap<String, String> {
        use pgrx::list::List;
        use pgrx::memcx::current_context;
        use std::ffi::{CStr, c_void};

        let mut ret = HashMap::new();
        let umapping = unsafe { pg_sys::GetUserMapping(pg_sys::GetUserId(), server_oid) };
        if umapping.is_null() {
            return ret;
        }

        current_context(|mcx| {
            // SAFETY: GetUserMapping results live in a server memory context;
            // we only copy the strings out within this call.
            unsafe {
                if let Some(list) =
                    List::<*mut c_void>::downcast_ptr_in_memcx((*umapping).options, mcx)
                {
                    for option in list.iter() {
                        let option = *option as *mut pg_sys::DefElem;
                        let name = CStr::from_ptr((*option).defname);
                        let value = CStr::from_ptr(pg_sys::defGetString(option));
                        if let (Ok(name), Ok(value)) = (name.to_str(), value.to_str()) {
                            ret.insert(name.to_owned(), value.to_owned());
                        }
                    }
                }
            }
        });
        ret
    }

    /// SQL Server credentials from the user mapping, if provided there.
    /// `username` is accepted alongside `user` — it is the spelling tools
    /// generate for tds_fdw-compatible sources (Sber Navigator's templates).
    fn user_mapping_auth(
        server_oid: pg_sys::Oid,
    ) -> MssqlFdwRqResult<Option<tiberius::AuthMethod>> {
        let options = unsafe { Self::user_mapping_options(server_oid) };
        let user = options.get("user").or_else(|| options.get("username"));
        let password = match options.get("password") {
            Some(p) => Some(p.clone()),
            None => options
                .get("password_id")
                .map(|id| get_vault_secret(id).unwrap_or_default()),
        };
        match (user, password) {
            (Some(user), Some(password)) if !user.is_empty() && !password.is_empty() => Ok(Some(
                tiberius::AuthMethod::sql_server(user.clone(), password),
            )),
            // partial or empty credentials are an error; none at all means
            // the connection string is expected to carry them
            (Some(_), None) | (None, Some(_)) | (Some(_), Some(_)) => {
                Err(MssqlFdwRqError::InvalidOption(
                    "user mapping must provide both 'user' and 'password'".to_string(),
                ))
            }
            (None, None) => Ok(None),
        }
    }

    /// Resolve the remote name for a relation from its foreign table options
    /// (`table_name`/`schema_name` are the tds_fdw/Navigator spellings).
    fn relation_mapping(rel: &FullQueryRelation) -> MssqlFdwRqResult<RelationMapping> {
        let remote_table = rel
            .options
            .get("table")
            .or_else(|| rel.options.get("table_name"))
            .ok_or_else(|| {
                MssqlFdwRqError::InvalidOption(format!(
                    "foreign table {}.{} is missing the 'table' option",
                    rel.local_schema, rel.local_table
                ))
            })?
            .clone();
        let remote_schema = rel
            .options
            .get("schema")
            .or_else(|| rel.options.get("schema_name"))
            .cloned()
            .unwrap_or_else(|| "dbo".to_string());
        Ok(RelationMapping {
            local_schema: rel.local_schema.clone(),
            local_table: rel.local_table.clone(),
            remote_schema,
            remote_table,
        })
    }

    /// Local column flags the translator needs: boolean columns (bare bit
    /// predicates become `= 1`) and NOT NULL columns (only those may be
    /// sorted without a NULL tiebreaker). A catalog read failure degrades to
    /// empty sets with a visible notice instead of failing the whole query —
    /// otherwise bool predicates fail on the server with an opaque error.
    fn column_flags(relations: &[FullQueryRelation]) -> (Vec<String>, Vec<String>) {
        Spi::connect(|client| {
            let mut bools = Vec::new();
            let mut not_nulls = Vec::new();
            for rel in relations {
                // names come from the local catalog; quotes are doubled so
                // the regclass literal stays a literal
                let sql = format!(
                    "SELECT a.attname::text AS attname, (a.atttypid = 'bool'::pg_catalog.regtype) AS is_bool, a.attnotnull AS not_null FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = '{}.{}'::pg_catalog.regclass AND a.attnum > 0 AND NOT a.attisdropped",
                    rel.local_schema.replace('\'', "''"),
                    rel.local_table.replace('\'', "''"),
                );
                let Ok(table) = client.select(&sql, None, &[]) else {
                    continue;
                };
                for row in table {
                    let (Ok(Some(name)), Ok(Some(is_bool)), Ok(Some(not_null))) = (
                        row.get_by_name::<&str, _>("attname"),
                        row.get_by_name::<bool, _>("is_bool"),
                        row.get_by_name::<bool, _>("not_null"),
                    ) else {
                        continue;
                    };
                    let name = name.to_lowercase();
                    if is_bool {
                        bools.push(name.clone());
                    }
                    if not_null {
                        not_nulls.push(name);
                    }
                }
            }
            Ok::<_, pgrx::spi::SpiError>((bools, not_nulls))
        })
        .unwrap_or_else(|e| {
            pgrx::notice!(
                "mssql_fdw_rq: could not read column flags from the local catalog ({e}); \
                 bare boolean predicates and NULL-safe sorting are unavailable for this query"
            );
            (Vec::new(), Vec::new())
        })
    }

    /// Execute the query on a background task that streams rows through a
    /// bounded channel: `iter_scan` pulls one row at a time, so arbitrarily
    /// large results never materialize in memory (TZ §6.1). Dropping the
    /// receiver stops the task.
    fn spawn_streaming_query(
        &self,
        tsql: String,
        params: Vec<Box<dyn tiberius::ToSql>>,
    ) -> MssqlFdwRqResult<tokio::sync::mpsc::Receiver<Result<tiberius::Row, tiberius::error::Error>>>
    {
        use futures_util::StreamExt;

        const CHANNEL_CAPACITY: usize = 256;

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let config = self.config.clone();
        self.rt.spawn(async move {
            let tcp = match TcpStream::connect(config.get_addr()).await {
                Ok(tcp) => tcp,
                Err(e) => {
                    let _ = tx.send(Err(e.into())).await;
                    return;
                }
            };
            let _ = tcp.set_nodelay(true);
            let mut client = match Client::connect(config, tcp.compat_write()).await {
                Ok(client) => client,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            let refs: Vec<&dyn tiberius::ToSql> = params
                .iter()
                .map(|b| &**b as &dyn tiberius::ToSql)
                .collect();
            let mut row_stream = match client.query(tsql, &refs).await {
                Ok(stream) => stream.into_row_stream(),
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            while let Some(item) = row_stream.next().await {
                if tx.send(item).await.is_err() {
                    // receiver dropped: the scan was cancelled
                    break;
                }
            }
        });
        Ok(rx)
    }
}

impl ForeignDataWrapper<MssqlFdwRqError> for MssqlFdwRq {
    fn new(server: ForeignServer) -> MssqlFdwRqResult<Self> {
        let rt = create_async_runtime()?;
        let conn_str = match server.options.get("conn_string") {
            Some(conn_str) => conn_str.to_owned(),
            None => {
                let conn_str_id = require_option("conn_string_id", &server.options)?;
                get_vault_secret(conn_str_id).unwrap_or_default()
            }
        };
        let mut config = Config::from_ado_string(&conn_str)?;
        if server.options.get("auth").map(String::as_str) == Some("kerberos") {
            // Windows Integrated / Kerberos: negotiate via GSSAPI (TZ §5.1 M2)
            #[cfg(feature = "mssql_fdw_rq_kerberos")]
            config.authentication(tiberius::AuthMethod::Integrated);
            #[cfg(not(feature = "mssql_fdw_rq_kerberos"))]
            return Err(MssqlFdwRqError::InvalidOption(
                "server option auth='kerberos' requires building the extension \
                 with the mssql_fdw_rq_kerberos feature"
                    .to_string(),
            ));
        } else if let Some(auth) = Self::user_mapping_auth(server.server_oid)? {
            config.authentication(auth);
        } else if !conn_str_has_user(&conn_str) {
            return Err(MssqlFdwRqError::InvalidOption(
                "no credentials: provide a user mapping (user/password) or User ID=/UID= in conn_string"
                    .to_string(),
            ));
        }
        let log_remote_query = server
            .options
            .get("log_remote_query")
            .is_some_and(|v| v == "true");

        stats::inc_stats(Self::FDW_NAME, stats::Metric::CreateTimes, 1);

        Ok(MssqlFdwRq {
            rt,
            config,
            log_remote_query,
            rx: None,
            rows_out: 0,
            tgt_cols: Vec::new(),
            rescan_plan: None,
        })
    }

    fn validator(
        options: Vec<Option<String>>,
        catalog: Option<pg_sys::Oid>,
    ) -> MssqlFdwRqResult<()> {
        if let Some(oid) = catalog {
            // the framework hands options over as "name=value" strings
            let names: Vec<String> = options
                .iter()
                .flatten()
                .map(|o| o.split('=').next().unwrap_or_default().to_string())
                .collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let allowed = |allowed: &[&str]| -> MssqlFdwRqResult<()> {
                for name in &names {
                    if !allowed.contains(name) {
                        return Err(MssqlFdwRqError::InvalidOption(format!(
                            "unknown option '{name}'"
                        )));
                    }
                }
                Ok(())
            };
            if oid == FOREIGN_SERVER_RELATION_ID {
                // tds_version is accepted and ignored: Sber Navigator's
                // connection flow unconditionally appends `tds_version '7.1'`
                // to MS SQL server options; tiberius negotiates the protocol
                // version itself
                allowed(&[
                    "conn_string",
                    "conn_string_id",
                    "log_remote_query",
                    "auth",
                    "tds_version",
                ])?;
                if !names.contains(&"conn_string") && !names.contains(&"conn_string_id") {
                    return Err(MssqlFdwRqError::InvalidOption(
                        "either 'conn_string' or 'conn_string_id' is required".to_string(),
                    ));
                }
            } else if oid == FOREIGN_TABLE_RELATION_ID {
                // schema_name/table_name are the tds_fdw/Navigator spellings;
                // schema/table are this FDW's native ones
                allowed(&["schema", "schema_name", "table", "table_name", "updatable"])?;
                if !names.contains(&"table") && !names.contains(&"table_name") {
                    return Err(MssqlFdwRqError::InvalidOption(
                        "option 'table' (or 'table_name') is required".to_string(),
                    ));
                }
            } else if oid == pg_sys::UserMappingRelationId {
                allowed(&["user", "username", "password", "password_id"])?;
            }
        }
        Ok(())
    }

    // -- full-query pushdown (PR #615 hooks) ---------------------------------

    fn supports_full_query_pushdown(&self) -> bool {
        true
    }

    fn supports_remote_query_static() -> bool {
        true
    }

    fn remote_query_policy(&self, context: &RemoteQueryContext) -> RemoteQueryPolicy {
        // bridge-FDW semantics: when every relation is a foreign table of this
        // same server the query must run remotely; mixed queries, or queries
        // spanning several foreign servers (PostgreSQL cannot hand those to
        // GetForeignJoinPaths/GetForeignUpperPaths as one unit, so a remote
        // plan would never be built and Require would turn into a hard error)
        // keep the planner free to decompose.
        if context.all_referenced_relations_are_foreign && context.foreign_relations_share_server()
        {
            RemoteQueryPolicy::Require
        } else {
            RemoteQueryPolicy::Optional
        }
    }

    fn begin_remote_query(
        &mut self,
        query: &RemoteQuery,
        _options: &HashMap<String, String>,
    ) -> MssqlFdwRqResult<()> {
        if query.sql.trim().is_empty() {
            return Err(MssqlFdwRqError::InvalidOption(
                "no deparsed SQL was provided for remote execution".to_string(),
            ));
        }

        // For multi-relation queries the framework deparses the TOP-LEVEL
        // statement text; when the query runs through SPI or PL/pgSQL that
        // is the enclosing statement, not this query. Refuse to execute
        // anything that does not actually reference the foreign tables as
        // whole identifiers — a substring hit (`users` inside `appusers` or
        // inside a string literal) must not green-light the wrong text.
        let relations: Vec<RelationMapping> = query
            .relations
            .iter()
            .map(Self::relation_mapping)
            .collect::<MssqlFdwRqResult<_>>()?;
        if !translator::mentions_relation(&query.sql, &relations) {
            return Err(MssqlFdwRqError::UnsupportedConstruct(
                translator::TranslateError::UnsupportedConstruct {
                    sql_fragment: query.sql.chars().take(80).collect(),
                    reason: "statement text does not reference the foreign tables \
                             (full-query pushdown of joins is not available for queries \
                             executed through SPI or PL/pgSQL)"
                        .to_string(),
                },
            ));
        }

        let (bool_columns, not_null_columns) = Self::column_flags(&query.relations);
        let ctx = TranslateContext {
            relations,
            bool_columns,
            not_null_columns,
        };
        let tsql = translator::translate(&query.sql, &ctx)?;
        let params: Vec<Box<dyn tiberius::ToSql>> = query
            .parameters
            .iter()
            .map(|p| types::value_to_sql(p.value.as_ref(), p.type_oid))
            .collect::<MssqlFdwRqResult<_>>()?;

        self.tgt_cols = query.columns.clone();
        self.rescan_plan = Some(ScanPlan::Remote {
            tsql: tsql.clone(),
            parameters: query.parameters.clone(),
        });
        let started = Instant::now();
        self.rx = Some(self.spawn_streaming_query(tsql.clone(), params)?);
        self.rows_out = 0;
        if self.log_remote_query {
            pgrx::log!(
                "mssql_fdw_rq: remote query dispatched ({} ms): {}",
                started.elapsed().as_millis(),
                tsql
            );
        }

        Ok(())
    }

    // -- plain table scans (trivial single-table queries) --------------------

    fn begin_scan(
        &mut self,
        quals: &[Qual],
        columns: &[Column],
        _sorts: &[Sort],
        _limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) -> MssqlFdwRqResult<()> {
        let remote_schema = options
            .get("schema")
            .or_else(|| options.get("schema_name"))
            .cloned()
            .unwrap_or_else(|| "dbo".to_string());
        let remote_table = options
            .get("table")
            .or_else(|| options.get("table_name"))
            .ok_or_else(|| {
                MssqlFdwRqError::InvalidOption(
                    "option 'table' (or 'table_name') is required".to_string(),
                )
            })?
            .to_string();

        self.tgt_cols = columns.to_vec();
        self.rescan_plan = Some(ScanPlan::Plain {
            remote_schema: remote_schema.clone(),
            remote_table: remote_table.clone(),
            columns: columns.to_vec(),
            quals: quals.to_vec(),
        });

        let (sql, params) = plain_scan_sql(&remote_schema, &remote_table, columns, quals)?;

        if self.log_remote_query {
            pgrx::log!("mssql_fdw_rq: remote query: {sql}");
        }
        self.rx = Some(self.spawn_streaming_query(sql, params)?);
        self.rows_out = 0;

        Ok(())
    }

    fn iter_scan(&mut self, row: &mut Row) -> MssqlFdwRqResult<Option<()>> {
        // pull exactly one row from the streaming channel; the connection
        // task stays parked until the next call
        let item = match self.rx.as_mut() {
            Some(rx) => self.rt.block_on(rx.recv()),
            None => return Ok(None),
        };
        match item {
            Some(Ok(src_row)) => {
                let mut tgt_row = Row::new();
                for (pos, tgt_col) in self.tgt_cols.iter().enumerate() {
                    let cell = types::field_to_cell(&src_row, tgt_col, pos)?;
                    tgt_row.push(&tgt_col.name, cell);
                }
                row.replace_with(tgt_row);
                self.rows_out += 1;
                Ok(Some(()))
            }
            Some(Err(e)) => Err(e.into()),
            None => {
                // stream exhausted: finalize the row counters
                self.flush_row_stats();
                self.rx = None;
                Ok(None)
            }
        }
    }

    fn re_scan(&mut self) -> MssqlFdwRqResult<()> {
        // PostgreSQL replays this scan from the beginning — typically the
        // inner side of a nested-loop join. The framework already routed
        // parameter changes through end_scan/begin_scan, so here the
        // statement is unchanged: cancel the running stream (dropping the
        // receiver aborts the background query) and execute it again on a
        // fresh connection.
        let (sql, params) = match self.rescan_plan.as_ref() {
            Some(plan) => plan.materialize()?,
            None => return Ok(()),
        };
        if self.log_remote_query {
            pgrx::log!("mssql_fdw_rq: remote query rescan: {sql}");
        }
        self.rx = None;
        self.rx = Some(self.spawn_streaming_query(sql, params)?);
        // rows of the abandoned stream still count toward the statistics
        self.flush_row_stats();
        Ok(())
    }

    fn end_scan(&mut self) -> MssqlFdwRqResult<()> {
        // dropping the receiver cancels the background query task; rows
        // already pulled from it still count toward the statistics
        self.flush_row_stats();
        self.rx = None;
        Ok(())
    }

    // -- IMPORT FOREIGN SCHEMA (what BI tools use to introspect a source) --

    fn import_foreign_schema(
        &mut self,
        stmt: ImportForeignSchemaStmt,
    ) -> MssqlFdwRqResult<Vec<String>> {
        // ImportSchemaType is re-exported through the prelude

        // one row per column of every base table in the remote schema
        let sql = "SELECT c.TABLE_NAME, c.COLUMN_NAME, c.DATA_TYPE, \
                   c.CHARACTER_MAXIMUM_LENGTH, c.NUMERIC_PRECISION, \
                   c.NUMERIC_SCALE, c.DATETIME_PRECISION, c.IS_NULLABLE \
                   FROM INFORMATION_SCHEMA.COLUMNS c \
                   JOIN INFORMATION_SCHEMA.TABLES t \
                     ON t.TABLE_SCHEMA = c.TABLE_SCHEMA \
                    AND t.TABLE_NAME = c.TABLE_NAME \
                    AND t.TABLE_TYPE = 'BASE TABLE' \
                   WHERE c.TABLE_SCHEMA = @P1 \
                   ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION";
        let params: Vec<Box<dyn tiberius::ToSql>> = vec![Box::new(stmt.remote_schema.clone())];
        let mut rx = self.spawn_streaming_query(sql.to_string(), params)?;

        // (remote table → local name → ordered column definitions)
        // preserving remote order
        let mut tables: Vec<(String, String, Vec<String>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let catalog_null = || {
            MssqlFdwRqError::InvalidOption(
                "unexpected NULL in INFORMATION_SCHEMA result".to_string(),
            )
        };
        while let Some(item) = self.rt.block_on(rx.recv()) {
            let row = item?;
            let table = row
                .try_get::<&str, usize>(0)?
                .ok_or_else(catalog_null)?
                .to_string();
            let column = row
                .try_get::<&str, usize>(1)?
                .ok_or_else(catalog_null)?
                .to_string();
            let data_type = row
                .try_get::<&str, usize>(2)?
                .ok_or_else(catalog_null)?
                .to_string();
            // INFORMATION_SCHEMA numeric columns come back in varying MSSQL
            // integer widths (tinyint/smallint/int) — try each width
            let as_i64 = |row: &tiberius::Row, idx: usize| -> Option<i64> {
                if let Ok(v) = row.try_get::<i64, usize>(idx) {
                    return v;
                }
                if let Ok(v) = row.try_get::<i32, usize>(idx) {
                    return v.map(i64::from);
                }
                if let Ok(v) = row.try_get::<i16, usize>(idx) {
                    return v.map(i64::from);
                }
                if let Ok(v) = row.try_get::<u8, usize>(idx) {
                    return v.map(i64::from);
                }
                None
            };
            let as_i32 = |row: &tiberius::Row, idx: usize| -> Option<i32> {
                as_i64(row, idx).and_then(|v| i32::try_from(v).ok())
            };
            let char_len = as_i32(&row, 3);
            let num_precision = as_i32(&row, 4);
            let num_scale = as_i32(&row, 5);
            let dt_precision = as_i32(&row, 6).map(|v| v as i16);
            let not_nullable = matches!(
                row.try_get::<&str, usize>(7)?,
                Some(v) if v.eq_ignore_ascii_case("NO")
            );

            // honor LIMIT TO / EXCEPT. LIMIT TO matches case-insensitively
            // and the created table keeps the LIST entry's spelling: after
            // parsing, PostgreSQL re-filters the returned statements with a
            // case-SENSITIVE comparison against the (downcased) list, and a
            // mixed-case remote name would otherwise be dropped silently.
            let listed = stmt
                .table_list
                .iter()
                .find(|t| t.eq_ignore_ascii_case(&table));
            let (wanted, local_name) = match stmt.list_type {
                ImportSchemaType::FdwImportSchemaLimitTo => {
                    (listed.is_some(), listed.cloned().unwrap_or_default())
                }
                ImportSchemaType::FdwImportSchemaExcept => (listed.is_none(), table.clone()),
                ImportSchemaType::FdwImportSchemaAll => (true, table.clone()),
            };
            if !wanted {
                continue;
            }

            let Some(pg_type) = types::mssql_type_to_pg(
                &data_type,
                char_len,
                num_precision,
                num_scale,
                dt_precision,
            ) else {
                // unreadable column: leave it out of the definition rather
                // than out of the whole table
                continue;
            };

            let i = *index.entry(table.clone()).or_insert_with(|| {
                tables.push((table.clone(), local_name.clone(), Vec::new()));
                tables.len() - 1
            });
            tables[i].2.push(format!(
                "\"{}\" {}{}",
                column.replace('"', "\"\""),
                pg_type,
                if not_nullable { " NOT NULL" } else { "" },
            ));
        }

        // build CREATE FOREIGN TABLE statements; tables whose columns were
        // all unreadable are skipped. Following postgres_fdw/mysql_fdw, the
        // table name stays unqualified — the core server rewrites it into
        // the target local schema.
        let quote_ident = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
        let quote_literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
        let stmts: Vec<String> = tables
            .into_iter()
            .filter(|(_, _, cols)| !cols.is_empty())
            .map(|(remote_table, local_name, cols)| {
                format!(
                    "CREATE FOREIGN TABLE {} ({}) SERVER {} OPTIONS (schema_name {}, table_name {})",
                    quote_ident(&local_name),
                    cols.join(", "),
                    quote_ident(&stmt.server_name),
                    quote_literal(&stmt.remote_schema),
                    quote_literal(&remote_table),
                )
            })
            .collect();
        for s in &stmts {
            pgrx::log!("mssql_fdw_rq: import: {s}");
        }
        Ok(stmts)
    }

    // -- read-only: modifications are rejected (TZ §5.2) ----------------------

    fn begin_modify(&mut self, _options: &HashMap<String, String>) -> MssqlFdwRqResult<()> {
        Err(MssqlFdwRqError::InvalidOption(
            "mssql_fdw_rq is read-only".to_string(),
        ))
    }
}
