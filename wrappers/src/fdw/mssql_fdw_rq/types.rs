//! Type mappings between PostgreSQL and MS SQL Server (TZ §5.5), plus the
//! value conversions for tiberius: parameter binding and result row → cell.

use std::str::FromStr;

use chrono::{Datelike, Timelike};
use num_traits::ToPrimitive;
use pgrx::datum::{Date, Time, Timestamp, TimestampWithTimeZone};
use pgrx::pg_sys;
use pgrx::varlena::rust_byte_slice_to_bytea;
use pgrx::{PgBuiltInOids, PgOid};
use tiberius::ToSql;
use tiberius::numeric::Decimal;
use tiberius::time::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

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

/// Recognize a PostgreSQL type name (used for SQL typed literals like
/// `DATE '2026-01-01'`). Includes types we cannot map so the translator can
/// reject their literals explicitly.
pub(super) fn is_pg_type_name(name: &str) -> bool {
    const TYPE_NAMES: [&str; 33] = [
        "int2",
        "int4",
        "int8",
        "smallint",
        "int",
        "integer",
        "bigint",
        "real",
        "float4",
        "float8",
        "double",
        "numeric",
        "decimal",
        "money",
        "bool",
        "boolean",
        "text",
        "varchar",
        "char",
        "bpchar",
        "name",
        "uuid",
        "bytea",
        "date",
        "time",
        "timestamp",
        "timestamptz",
        "interval",
        "json",
        "jsonb",
        "xml",
        "float",
        "bit",
    ];
    TYPE_NAMES.contains(&name)
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
        PgOid::BuiltIn(PgBuiltInOids::DATEOID) => match cell {
            Cell::Date(v) => Ok(Box::new(date_to_naive(v)?)),
            other => param_type_mismatch("date", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::TIMEOID) => match cell {
            Cell::Time(v) => Ok(Box::new(time_to_naive(v)?)),
            other => param_type_mismatch("time", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPOID) => match cell {
            Cell::Timestamp(v) => Ok(Box::new(timestamp_to_naive(v)?)),
            other => param_type_mismatch("timestamp", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPTZOID) => match cell {
            Cell::Timestamptz(v) => Ok(Box::new(timestamptz_to_utc(v)?)),
            other => param_type_mismatch("timestamptz", other),
        },
        PgOid::BuiltIn(PgBuiltInOids::BYTEAOID) => match cell {
            Cell::Bytea(v) => Ok(Box::new(unsafe { bytea_to_vec(*v) })),
            other => param_type_mismatch("bytea", other),
        },
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

// ---------------------------------------------------------------------------
// Date/time conversions (parts-based, keeps sub-second precision)
// ---------------------------------------------------------------------------

fn dt_err(e: impl std::fmt::Display) -> MssqlFdwRqError {
    MssqlFdwRqError::DateTimeError(e.to_string())
}

fn date_to_naive(v: &Date) -> MssqlFdwRqResult<NaiveDate> {
    NaiveDate::from_ymd_opt(v.year(), v.month() as u32, v.day() as u32).ok_or_else(|| {
        dt_err(format!(
            "invalid date {}-{}-{}",
            v.year(),
            v.month(),
            v.day()
        ))
    })
}

fn time_to_naive(v: &Time) -> MssqlFdwRqResult<NaiveTime> {
    let sec = v.second();
    let (sec, micro) = (sec.trunc() as u32, (sec.fract() * 1e6).round() as u32);
    NaiveTime::from_hms_micro_opt(v.hour() as u32, v.minute() as u32, sec, micro)
        .ok_or_else(|| dt_err(format!("invalid time {}", v.hour())))
}

fn timestamp_to_naive(v: &Timestamp) -> MssqlFdwRqResult<NaiveDateTime> {
    let sec = v.second();
    let (sec, micro) = (sec.trunc() as u32, (sec.fract() * 1e6).round() as u32);
    let date =
        NaiveDate::from_ymd_opt(v.year(), v.month() as u32, v.day() as u32).ok_or_else(|| {
            dt_err(format!(
                "invalid date {}-{}-{}",
                v.year(),
                v.month(),
                v.day()
            ))
        })?;
    date.and_hms_micro_opt(v.hour() as u32, v.minute() as u32, sec, micro)
        .ok_or_else(|| dt_err("invalid time of day".to_string()))
}

fn timestamptz_to_utc(v: &TimestampWithTimeZone) -> MssqlFdwRqResult<DateTime<Utc>> {
    Ok(timestamp_to_naive(&Timestamp::from(*v))?.and_utc())
}

fn naive_time_to_pgrx(v: NaiveTime) -> MssqlFdwRqResult<Time> {
    let sec = f64::from(v.second()) + f64::from(v.nanosecond()) / 1e9;
    Time::new(v.hour() as u8, v.minute() as u8, sec).map_err(dt_err)
}

fn naive_dt_to_pgrx(v: NaiveDateTime) -> MssqlFdwRqResult<Timestamp> {
    let sec = f64::from(v.second()) + f64::from(v.and_utc().timestamp_subsec_nanos()) / 1e9;
    Timestamp::new(
        v.year(),
        v.month() as u8,
        v.day() as u8,
        v.hour() as u8,
        v.minute() as u8,
        sec,
    )
    .map_err(dt_err)
}

fn utc_to_pgrx(v: DateTime<Utc>) -> MssqlFdwRqResult<TimestampWithTimeZone> {
    let sec = f64::from(v.second()) + f64::from(v.timestamp_subsec_nanos()) / 1e9;
    TimestampWithTimeZone::new(
        v.year(),
        v.month() as u8,
        v.day() as u8,
        v.hour() as u8,
        v.minute() as u8,
        sec,
    )
    .map_err(dt_err)
}

/// Copy the payload of a PostgreSQL `bytea` datum into an owned buffer.
///
/// # Safety
/// `ptr` must be a valid `bytea` datum (4-byte varlena header, as produced by
/// the server for on-disk/` toast-free` datums handed to FDWs).
unsafe fn bytea_to_vec(ptr: *mut pg_sys::bytea) -> Vec<u8> {
    // SAFETY: caller guarantees a valid bytea datum with a 4-byte varlena
    // header; we only read within its varsize() bounds.
    unsafe {
        if ptr.is_null() {
            return Vec::new();
        }
        // varsize() includes the 4-byte varlena header
        let total = pgrx::varlena::varsize(ptr as *const pg_sys::varlena);
        let len = total.saturating_sub(4);
        std::slice::from_raw_parts((ptr as *const u8).add(4), len).to_vec()
    }
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
        PgOid::BuiltIn(PgBuiltInOids::DATEOID) => Box::new(None::<NaiveDate>),
        PgOid::BuiltIn(PgBuiltInOids::TIMEOID) => Box::new(None::<NaiveTime>),
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPOID) => Box::new(None::<NaiveDateTime>),
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPTZOID) => Box::new(None::<DateTime<Utc>>),
        PgOid::BuiltIn(PgBuiltInOids::BYTEAOID) => Box::new(None::<Vec<u8>>),
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
    pos: usize,
) -> MssqlFdwRqResult<Option<Cell>> {
    // Resolve the result column by name. Two full-query shapes cannot be
    // matched by name: join-path target lists carry positional names
    // (`column_N`) that never exist in the remote result, and a join may
    // select identically-named columns from two tables. Both fall back to
    // the result position, which mirrors the PostgreSQL target list order.
    let col_name = tgt_col.name.as_str();
    let name_positions: Vec<usize> = src_row
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name() == col_name)
        .map(|(i, _)| i)
        .collect();
    let idx = if name_positions.len() == 1 {
        name_positions[0]
    } else {
        pos
    };

    let ret = match PgOid::from(tgt_col.type_oid) {
        PgOid::BuiltIn(PgBuiltInOids::BOOLOID) => {
            src_row.try_get::<bool, usize>(idx)?.map(Cell::Bool)
        }
        PgOid::BuiltIn(PgBuiltInOids::INT2OID) => {
            src_row.try_get::<i16, usize>(idx)?.map(Cell::I16)
        }
        PgOid::BuiltIn(PgBuiltInOids::INT4OID) => {
            // MSSQL aggregate results are int32 even when Postgres expects
            // int4 from smallint inputs; widen where lossless
            if let Ok(v) = src_row.try_get::<i32, usize>(idx) {
                v.map(Cell::I32)
            } else {
                src_row
                    .try_get::<i16, usize>(idx)?
                    .map(|v| Cell::I32(i32::from(v)))
            }
        }
        PgOid::BuiltIn(PgBuiltInOids::INT8OID) => {
            // T-SQL COUNT()/SUM(int) return int32; Postgres expects int8
            if let Ok(v) = src_row.try_get::<i64, usize>(idx) {
                v.map(Cell::I64)
            } else if let Ok(v) = src_row.try_get::<i32, usize>(idx) {
                v.map(|x| Cell::I64(i64::from(x)))
            } else {
                src_row
                    .try_get::<i16, usize>(idx)?
                    .map(|x| Cell::I64(i64::from(x)))
            }
        }
        PgOid::BuiltIn(PgBuiltInOids::FLOAT4OID) => {
            src_row.try_get::<f32, usize>(idx)?.map(Cell::F32)
        }
        PgOid::BuiltIn(PgBuiltInOids::FLOAT8OID) => {
            if let Ok(v) = src_row.try_get::<f64, usize>(idx) {
                v.map(Cell::F64)
            } else {
                src_row
                    .try_get::<f32, usize>(idx)?
                    .map(|v| Cell::F64(f64::from(v)))
            }
        }
        PgOid::BuiltIn(PgBuiltInOids::NUMERICOID) => {
            // decimal, or an int/float aggregate result coerced to numeric
            if let Ok(v) = src_row.try_get::<Decimal, usize>(idx) {
                v.and_then(|d| d.to_f64())
                    .map(pgrx::AnyNumeric::try_from)
                    .transpose()?
                    .map(Cell::Numeric)
            } else if let Ok(v) = src_row.try_get::<i64, usize>(idx) {
                v.map(|x| Cell::Numeric(pgrx::AnyNumeric::from(i128::from(x))))
            } else if let Ok(v) = src_row.try_get::<i32, usize>(idx) {
                v.map(|x| Cell::Numeric(pgrx::AnyNumeric::from(i128::from(x))))
            } else {
                let v = src_row.try_get::<f64, usize>(idx)?;
                v.and_then(|x| pgrx::AnyNumeric::try_from(x).ok())
                    .map(Cell::Numeric)
            }
        }
        PgOid::BuiltIn(PgBuiltInOids::TEXTOID) => src_row
            .try_get::<&str, usize>(idx)?
            .map(|v| Cell::String(v.to_owned())),
        PgOid::BuiltIn(PgBuiltInOids::UUIDOID) => src_row
            .try_get::<uuid::Uuid, usize>(idx)?
            .map(|v| Cell::Uuid(pgrx::datum::Uuid::from_bytes(*v.as_bytes()))),
        PgOid::BuiltIn(PgBuiltInOids::DATEOID) => src_row
            .try_get::<NaiveDate, usize>(idx)?
            .map(|v| {
                Date::new(v.year(), v.month() as u8, v.day() as u8)
                    .map_err(dt_err)
                    .map(Cell::Date)
            })
            .transpose()?,
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPOID) => src_row
            .try_get::<NaiveDateTime, usize>(idx)?
            .map(naive_dt_to_pgrx)
            .transpose()?
            .map(Cell::Timestamp),
        PgOid::BuiltIn(PgBuiltInOids::TIMESTAMPTZOID) => src_row
            .try_get::<DateTime<Utc>, usize>(idx)?
            .map(utc_to_pgrx)
            .transpose()?
            .map(Cell::Timestamptz),
        PgOid::BuiltIn(PgBuiltInOids::TIMEOID) => src_row
            .try_get::<NaiveTime, usize>(idx)?
            .map(naive_time_to_pgrx)
            .transpose()?
            .map(Cell::Time),
        PgOid::BuiltIn(PgBuiltInOids::BYTEAOID) => src_row
            .try_get::<&[u8], usize>(idx)?
            .map(|v| Cell::Bytea(rust_byte_slice_to_bytea(v).into_pg())),
        _ => return Err(MssqlFdwRqError::UnsupportedColumnType(tgt_col.name.clone())),
    };

    Ok(ret)
}
