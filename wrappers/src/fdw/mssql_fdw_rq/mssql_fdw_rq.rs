use crate::stats;
use pgrx::pg_sys;
use pgrx::spi::Spi;
use std::collections::HashMap;
use std::time::Instant;
use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

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
    scan_result: Vec<tiberius::Row>,
    iter_idx: usize,
    tgt_cols: Vec<Column>,
}

impl MssqlFdwRq {
    const FDW_NAME: &'static str = "MssqlFdwRq";

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
    fn user_mapping_auth(
        server_oid: pg_sys::Oid,
    ) -> MssqlFdwRqResult<Option<tiberius::AuthMethod>> {
        let options = unsafe { Self::user_mapping_options(server_oid) };
        let user = options.get("user");
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

    /// Resolve the remote name for a relation from its foreign table options.
    fn relation_mapping(rel: &FullQueryRelation) -> MssqlFdwRqResult<RelationMapping> {
        let remote_table = rel
            .options
            .get("table")
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
            .cloned()
            .unwrap_or_else(|| "dbo".to_string());
        Ok(RelationMapping {
            local_schema: rel.local_schema.clone(),
            local_table: rel.local_table.clone(),
            remote_schema,
            remote_table,
        })
    }

    /// Local boolean column names for the referenced relations, so the
    /// translator can rewrite bare boolean predicates (`WHERE flag`) into
    /// `flag = 1`. A catalog read failure degrades to an empty set (the remote
    /// side then rejects such queries) instead of failing the whole query.
    fn bool_columns(relations: &[FullQueryRelation]) -> Vec<String> {
        Spi::connect(|client| {
            let mut out = Vec::new();
            for rel in relations {
                // names come from the local catalog; quotes are doubled so
                // the regclass literal stays a literal
                let sql = format!(
                    "SELECT a.attname::text AS attname FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = '{}.{}'::pg_catalog.regclass AND a.atttypid = 'bool'::pg_catalog.regtype AND a.attnum > 0 AND NOT a.attisdropped",
                    rel.local_schema.replace('\'', "''"),
                    rel.local_table.replace('\'', "''"),
                );
                let Ok(table) = client.select(&sql, None, &[]) else {
                    continue;
                };
                for row in table {
                    if let Ok(Some(name)) = row.get_by_name::<&str, _>("attname") {
                        out.push(name.to_lowercase());
                    }
                }
            }
            Ok::<_, pgrx::spi::SpiError>(out)
        })
        .unwrap_or_default()
    }

    fn connect(&self) -> MssqlFdwRqResult<Client<Compat<TcpStream>>> {
        let tcp = self
            .rt
            .block_on(TcpStream::connect(self.config.get_addr()))?;
        tcp.set_nodelay(true)?;
        let client = self
            .rt
            .block_on(Client::connect(self.config.clone(), tcp.compat_write()))?;
        Ok(client)
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
        if let Some(auth) = Self::user_mapping_auth(server.server_oid)? {
            config.authentication(auth);
        } else if !conn_str.to_ascii_lowercase().contains("user=") {
            return Err(MssqlFdwRqError::InvalidOption(
                "no credentials: provide a user mapping (user/password) or User=/Password= in conn_string"
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
            scan_result: Vec::new(),
            iter_idx: 0,
            tgt_cols: Vec::new(),
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
                allowed(&["conn_string", "conn_string_id", "log_remote_query"])?;
                if !names.contains(&"conn_string") && !names.contains(&"conn_string_id") {
                    return Err(MssqlFdwRqError::InvalidOption(
                        "either 'conn_string' or 'conn_string_id' is required".to_string(),
                    ));
                }
            } else if oid == FOREIGN_TABLE_RELATION_ID {
                allowed(&["schema", "table", "updatable"])?;
                check_options_contain(&options, "table")?;
            } else if oid == pg_sys::UserMappingRelationId {
                allowed(&["user", "password", "password_id"])?;
            }
        }
        Ok(())
    }

    // -- full-query pushdown (PR #615 hooks) ---------------------------------

    fn supports_full_query_pushdown(&self) -> bool {
        true
    }

    fn remote_query_policy(&self, context: &RemoteQueryContext) -> RemoteQueryPolicy {
        // bridge-FDW semantics: when every relation is foreign the query must
        // run remotely; mixed queries keep the planner free to decompose
        if context.all_referenced_relations_are_foreign {
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
        // anything that does not even mention the foreign tables.
        let mentions_any = query.relations.iter().any(|rel| {
            let lower = query.sql.to_lowercase();
            lower.contains(&rel.local_table.to_lowercase())
        });
        if !mentions_any {
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

        let relations: Vec<RelationMapping> = query
            .relations
            .iter()
            .map(Self::relation_mapping)
            .collect::<MssqlFdwRqResult<_>>()?;

        let ctx = TranslateContext {
            relations,
            bool_columns: Self::bool_columns(&query.relations),
        };
        let tsql = translator::translate(&query.sql, &ctx)?;
        let params: Vec<Box<dyn tiberius::ToSql>> = query
            .parameters
            .iter()
            .map(|p| types::value_to_sql(p.value.as_ref(), p.type_oid))
            .collect::<MssqlFdwRqResult<_>>()?;
        let param_refs: Vec<&dyn tiberius::ToSql> = params
            .iter()
            .map(|b| &**b as &dyn tiberius::ToSql)
            .collect();

        self.tgt_cols = query.columns.clone();
        self.iter_idx = 0;

        let started = Instant::now();
        if self.log_remote_query {
            pgrx::log!("mssql_fdw_rq: remote query: {tsql}");
        }
        let mut client = self.connect()?;
        let stream = self.rt.block_on(client.query(tsql.clone(), &param_refs))?;
        self.scan_result = self.rt.block_on(stream.into_first_result())?;
        let elapsed = started.elapsed();

        if self.log_remote_query {
            pgrx::log!(
                "mssql_fdw_rq: remote query done ({} rows, {} ms)",
                self.scan_result.len(),
                elapsed.as_millis()
            );
        }

        stats::inc_stats(
            Self::FDW_NAME,
            stats::Metric::RowsIn,
            self.scan_result.len() as i64,
        );
        stats::inc_stats(
            Self::FDW_NAME,
            stats::Metric::RowsOut,
            self.scan_result.len() as i64,
        );

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
            .cloned()
            .unwrap_or_else(|| "dbo".to_string());
        let remote_table = require_option("table", options)?.to_string();

        self.tgt_cols = columns.to_vec();
        self.iter_idx = 0;

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
            bracket_name(&remote_schema)?,
            bracket_name(&remote_table)?
        );

        // quals become typed T-SQL parameters (@P1, @P2, ...); values are
        // never concatenated into the SQL text (TZ §5.4)
        let mut params: Vec<Box<dyn tiberius::ToSql>> = Vec::new();
        let mut conds: Vec<String> = Vec::new();
        for qual in quals {
            if qual.use_or {
                return Err(MssqlFdwRqError::UnsupportedConstruct(
                    translator::TranslateError::UnsupportedConstruct {
                        sql_fragment: "ANY (array)".to_string(),
                        reason: "array quals are not supported in plain scans yet".to_string(),
                    },
                ));
            }
            let field = bracket_name(&qual.field)?;
            let oper = qual.operator.as_str();
            match &qual.value {
                Value::Cell(Cell::Bool(b)) if oper == "is" => {
                    conds.push(format!("{field} = {}", *b as u8));
                }
                Value::Cell(Cell::Bool(b)) if oper == "is not" => {
                    conds.push(format!("{field} <> {}", *b as u8));
                }
                Value::Cell(cell) => {
                    let n = params.len() + 1;
                    conds.push(format!(
                        "{field} {} @P{n}",
                        match oper {
                            "~~" => "LIKE".to_string(),
                            "!~~" => "NOT LIKE".to_string(),
                            o => o.to_string(),
                        }
                    ));
                    params.push(types::cell_to_sql(cell)?);
                }
                Value::Array(_) => {
                    return Err(MssqlFdwRqError::UnsupportedConstruct(
                        translator::TranslateError::UnsupportedConstruct {
                            sql_fragment: "array qual".to_string(),
                            reason: "array quals are not supported in plain scans yet".to_string(),
                        },
                    ));
                }
            }
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }

        let param_refs: Vec<&dyn tiberius::ToSql> = params
            .iter()
            .map(|b| &**b as &dyn tiberius::ToSql)
            .collect();

        let mut client = self.connect()?;
        let stream = self.rt.block_on(client.query(sql.clone(), &param_refs))?;
        self.scan_result = self.rt.block_on(stream.into_first_result())?;

        if self.log_remote_query {
            pgrx::log!("mssql_fdw_rq: remote query: {sql}");
        }

        stats::inc_stats(
            Self::FDW_NAME,
            stats::Metric::RowsIn,
            self.scan_result.len() as i64,
        );

        Ok(())
    }

    fn iter_scan(&mut self, row: &mut Row) -> MssqlFdwRqResult<Option<()>> {
        if self.iter_idx >= self.scan_result.len() {
            return Ok(None);
        }

        let src_row = &self.scan_result[self.iter_idx];
        let mut tgt_row = Row::new();
        for tgt_col in &self.tgt_cols {
            let cell = types::field_to_cell(src_row, tgt_col)?;
            tgt_row.push(&tgt_col.name, cell);
        }
        row.replace_with(tgt_row);
        self.iter_idx += 1;

        Ok(Some(()))
    }

    fn re_scan(&mut self) -> MssqlFdwRqResult<()> {
        self.iter_idx = 0;
        Ok(())
    }

    fn end_scan(&mut self) -> MssqlFdwRqResult<()> {
        self.scan_result.clear();
        Ok(())
    }

    // -- read-only: modifications are rejected (TZ §5.2) ----------------------

    fn begin_modify(&mut self, _options: &HashMap<String, String>) -> MssqlFdwRqResult<()> {
        Err(MssqlFdwRqError::InvalidOption(
            "mssql_fdw_rq is read-only".to_string(),
        ))
    }
}
