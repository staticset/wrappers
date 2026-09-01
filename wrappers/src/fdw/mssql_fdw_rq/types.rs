//! Type mappings between PostgreSQL and MS SQL Server (TZ §5.5), plus the
//! value conversions for tiberius: parameter binding and result row → cell.

use std::str::FromStr;

use num_traits::ToPrimitive;
use pgrx::pg_sys;
use pgrx::{PgBuiltInOids, PgOid, prelude::to_timestamp};
use tiberius::ToSql;
use tiberius::numeric::Decimal;
use tiberius::time::chrono::{NaiveDate, NaiveDateTime};

use supabase_wrappers::prelude::*;

use super::{MssqlFdwRqError, MssqlFdwRqResult};

/// Map a PostgreSQL type name (as it appears in deparsed casts, e.g. `int8`,
/// `numeric`, `timestamp with time zone`) to the T-SQL type usable as a
/// `CAST(... AS ...)` target. Returns `None` for types outside the v1 set.
pub(super) fn pg_type_to_mssql(pg_name: &str) -> Option<&'static str> {
    let name = pg_name
        .trim_start_matches("pg_catalog.")
        .trim_start_matches("public.")
        .to_lowercase();
    Some(match name.as_str() {
        "int2" | "smallint" => "smallint",
        "int4" | "int" | "integer" | "oid" => "int",
        "int8" | "bigint" => "bigint",
        "float4" | "real" => "real",
        "float8" | "double precision" => "float(53)",
        "numeric" | "decimal" => "numeric(38, 10)",
        "bool" | "boolean" => "bit",
        "text" | "varchar" | "bpchar" | "char" | "name" => "nvarchar(4000)",
        "uuid" => "uniqueidentifier",
        "bytea" => "varbinary(8000)",
        "date" => "date",
        "timestamp" | "timestamp without time zone" => "datetime2",
        "timestamptz" | "timestamp with time zone" => "datetimeoffset",
        "time" | "time without time zone" => "time",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Parameter binding: framework Value → tiberius ToSql
// ---------------------------------------------------------------------------

/// Convert an evaluated runtime parameter (or `None` = SQL NULL) into a boxed
/// [`ToSql`] whose Rust type matches what tiberius sends for the parameter's
/// PostgreSQL type.
pub(super) fn value_to_sql(
    value: Option<&Value>,
    type_oid: pg_sys::Oid,
) -> MssqlFdwRqResult<Box<dyn ToSql>> {
    let cell = match value {
        None => return null_to_sql(type_oid),
        Some(Value::Cell(cell)) => cell,
        Some(Value::Array(_)) => {
            return Err(MssqlFdwRqError::UnsupportedParameterType(
                "array-valued parameter".to_string(),
            ));
        }
    };

    match PgOid::from(type_oid) {
        PgOid::BuiltIn(PgBuiltInOids::BOOLOID) => match cell {
            Cell::Bool(v) => Ok(Box::new(*v)),
            other => param_type_mismatch("bool", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::INT2OID) => match cell {
            Cell::I8(v) => Ok(Box::new(i16::from(*v))),
            Cell::I16(v) => Ok(Box::new(*v)),
            other => param_type_mismatch("smallint", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::INT4OID) => match cell {
            Cell::I8(v) => Ok(Box::new(i32::from(*v))),
            Cell::I16(v) => Ok(Box::new(i32::from(*v))),
            Cell::I32(v) => Ok(Box::new(*v)),
            other => param_type_mismatch("int", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::INT8OID) => match cell {
            Cell::I8(v) => Ok(Box::new(i64::from(*v))),
            Cell::I16(v) => Ok(Box::new(i64::from(*v))),
            Cell::I32(v) => Ok(Box::new(i64::from(*v))),
            Cell::I64(v) => Ok(Box::new(*v)),
            other => param_type_mismatch("bigint", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::FLOAT4OID) => match cell {
            Cell::I8(v) => Ok(Box::new(f32::from(*v))),
            Cell::I16(v) => Ok(Box::new(f32::from(*v))),
            Cell::I32(v) => Ok(Box::new(*v as f32)),
            Cell::F32(v) => Ok(Box::new(*v)),
            other => param_type_mismatch("real", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::FLOAT8OID) => match cell {
            Cell::I8(v) => Ok(Box::new(f64::from(*v))),
            Cell::I16(v) => Ok(Box::new(f64::from(*v))),
            Cell::I32(v) => Ok(Box::new(f64::from(*v))),
            Cell::I64(v) => Ok(Box::new(*v as f64)),
            Cell::F32(v) => Ok(Box::new(f64::from(*v))),
            Cell::F64(v) => Ok(Box::new(*v)),
            other => param_type_mismatch("float", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::NUMERICOID) => match cell {
            Cell::Numeric(v) => numeric_to_decimal(v),
            Cell::I32(v) => Ok(Box::new(Decimal::from(i64::from(*v)))),
            Cell::I64(v) => Ok(Box::new(Decimal::from(*v))),
            other => param_type_mismatch("numeric", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::TEXTOID)
        | PgOid::BuiltIn(PgBuiltInOids::VARCHAROID)
        | PgOid::BuiltIn(PgBuiltInOids::BPCHAROID)
        | PgOid::BuiltIn(PgBuiltInOids::NAMEOID) => match cell {
            Cell::String(v) => Ok(Box::new(v.clone())),
            other => param_type_mismatch("text", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::UUIDOID) => match cell {
            Cell::Uuid(v) => Ok(Box::new(uuid::Uuid::from_bytes(*v.as_bytes()))),
            other => param_type_mismatch("uuid", other),
        },
        // date/time/bytea parameters are not bound in the skeleton yet
        other_oid => Err(MssqlFdwRqError::UnsupportedParameterType(format!(
            "oid {}",
            other_oid.value()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Qual values carry no type OID: bind straight from the cell variant
// ---------------------------------------------------------------------------

/// Convert a qual's cell value into a tiberius parameter. Used by plain table
/// scans where the framework only hands us the value (TZ §5.4: values are
/// bound, never concatenated).
pub(super) fn cell_to_sql(cell: &Cell) -> MssqlFdwRqResult<Box<dyn ToSql>> {
    Ok(match cell {
        Cell::Bool(v) => Box::new(*v),
        Cell::I8(v) => Box::new(i16::from(*v)),
        Cell::I16(v) => Box::new(*v),
        Cell::I32(v) => Box::new(*v),
        Cell::I64(v) => Box::new(*v),
        Cell::F32(v) => Box::new(*v),
        Cell::F64(v) => Box::new(*v),
        Cell::Numeric(v) => numeric_to_decimal(v)?,
        Cell::String(v) => Box::new(v.clone()),
        Cell::Uuid(v) => Box::new(uuid::Uuid::from_bytes(*v.as_bytes())),
        other => {
            return Err(MssqlFdwRqError::UnsupportedParameterType(
                cell_kind(other).to_string(),
            ));
        }
    })
}

fn numeric_to_decimal(v: &pgrx::AnyNumeric) -> MssqlFdwRqResult<Box<dyn ToSql>> {
    // exact round-trip through the decimal string representation
    let d = Decimal::from_str(&v.to_string()).map_err(|_| {
        MssqlFdwRqError::UnsupportedParameterType("numeric not representable".to_string())
    })?;
    Ok(Box::new(d))
}

fn null_to_sql(type_oid: pg_sys::Oid) -> MssqlFdwRqResult<Box<dyn ToSql>> {
    let boxed: Box<dyn ToSql> = match PgOid::from(type_oid) {
        PgOid::BuiltIn(PgBuiltInOids::BOOLOID) => Box::new(None::<bool>),
        PgOid::BuiltIn(PgBuiltInOids::INT2OID) => Box::new(None::<i16>),
        PgOid::BuiltIn(PgBuiltInOids::INT4OID) => Box::new(None::<i32>),
        PgOid::BuiltIn(PgBuiltInOids::INT8OID) => Box::new(None::<i64>),
        PgOid::BuiltIn(PgBuiltInOids::FLOAT4OID) => Box::new(None::<f32>),
        PgOid::BuiltIn(PgBuiltInOids::FLOAT8OID) => Box::new(None::<f64>),
        PgOid::BuiltIn(PgBuiltInOids::NUMERICOID) => Box::new(None::<Decimal>),
        PgOid::BuiltIn(PgBuiltInOids::TEXTOID)
        | PgOid::BuiltIn(PgBuiltInOids::VARCHAROID)
        | PgOid::BuiltIn(PgBuiltInOids::BPCHAROID)
        | PgOid::BuiltIn(PgBuiltInOids::NAMEOID) => Box::new(None::<String>),
        PgOid::BuiltIn(PgBuiltInOids::UUIDOID) => Box::new(None::<uuid::Uuid>),
        other_oid => {
            return Err(MssqlFdwRqError::UnsupportedParameterType(format!(
                "NULL parameter with oid {}",
                other_oid.value()
            )));
        }
    };
    Ok(boxed)
}

fn param_type_mismatch(expected: &str, cell: &Cell) -> MssqlFdwRqResult<Box<dyn ToSql>> {
    Err(MssqlFdwRqError::UnsupportedParameterType(format!(
        "expected {expected} parameter value, got {:?}",
        cell_kind(cell)
    )))
}

fn cell_kind(cell: &Cell) -> &'static str {
    match cell {
        Cell::Bool(_) => "bool",
        Cell::I8(_) => "i8",
        Cell::I16(_) => "i16",
        Cell::I32(_) => "i32",
        Cell::I64(_) => "i64",
        Cell::F32(_) => "f32",
        Cell::F64(_) => "f64",
        Cell::Numeric(_) => "numeric",
        Cell::String(_) => "string",
        Cell::Date(_) => "date",
        Cell::Time(_) => "time",
        Cell::Timestamp(_) => "timestamp",
        Cell::Timestamptz(_) => "timestamptz",
        Cell::Interval(_) => "interval",
        Cell::Json(_) => "json",
        Cell::Bytea(_) => "bytea",
        Cell::Uuid(_) => "uuid",
        _ => "array",
    }
}

// ---------------------------------------------------------------------------
// Result rows: tiberius Row → framework Cell (by target column OID)
// ---------------------------------------------------------------------------

pub(super) fn field_to_cell(
    src_row: &tiberius::Row,
    tgt_col: &Column,
) -> MssqlFdwRqResult<Option<Cell>> {
    let col_name = tgt_col.name.as_str();

    let ret = match PgOid::from(tgt_col.type_oid) {
        PgOid::BuiltIn(PgBuiltInOids::BOOLOID) => {
            src_row.try_get::<bool, &str>(col_name)?.map(Cell::Bool)
        }
        PgOid::BuiltIn(PgBuiltInOids::INT2OID) => {
            src_row.try_get::<i16, &str>(col_name)?.map(Cell::I16)
        }
        PgOid::BuiltIn(PgBuiltInOids::INT4OID) => {
            src_row.try_get::<i32, &str>(col_name)?.map(Cell::I32)
        }
        PgOid::BuiltIn(PgBuiltInOids::INT8OID) => {
            src_row.try_get::<i64, &str>(col_name)?.map(Cell::I64)
        }
        PgOid::BuiltIn(PgBuiltInOids::FLOAT4OID) => {
            src_row.try_get::<f32, &str>(col_name)?.map(Cell::F32)
        }
        PgOid::BuiltIn(PgBuiltInOids::FLOAT8OID) => {
            src_row.try_get::<f64, &str>(col_name)?.map(Cell::F64)
        }
        PgOid::BuiltIn(PgBuiltInOids::NUMERICOID) => src_row
            .try_get::<Decimal, &str>(col_name)?
            .and_then(|v| v.to_f64())
            .map(pgrx::AnyNumeric::try_from)
            .transpose()?
            .map(Cell::Numeric),
        PgOid::BuiltIn(PgBuiltInOids::TEXTOID) => src_row
            .try_get::<&str, &str>(col_name)?
            .map(|v| Cell::String(v.to_owned())),
        PgOid::BuiltIn(PgBuiltInOids::UUIDOID) => src_row
            .try_get::<uuid::Uuid, &str>(col_name)?
            .map(|v| Cell::Uuid(pgrx::datum::Uuid::from_bytes(*v.as_bytes()))),
        PgOid::BuiltIn(PgBuiltInOids::DATEOID) => {
            src_row.try_get::<NaiveDate, &str>(col_name)?.map(|v| {
                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let ts = to_timestamp(v.signed_duration_since(epoch).num_seconds() as f64);
                Cell::Date(pgrx::prelude::Date::from(ts))
            })
        }
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPOID) => {
            src_row.try_get::<NaiveDateTime, &str>(col_name)?.map(|v| {
                let ts = to_timestamp(v.and_utc().timestamp() as f64);
                Cell::Timestamp(ts.to_utc())
            })
        }
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPTZOID) => {
            src_row.try_get::<NaiveDateTime, &str>(col_name)?.map(|v| {
                let ts = to_timestamp(v.and_utc().timestamp() as f64);
                Cell::Timestamptz(ts)
            })
        }
        // bytea (varbinary) and time round-trips land in the next M1 iteration
        _ => return Err(MssqlFdwRqError::UnsupportedColumnType(tgt_col.name.clone())),
    };

    Ok(ret)
}
