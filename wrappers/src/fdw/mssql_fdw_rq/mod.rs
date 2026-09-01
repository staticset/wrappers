#![allow(clippy::module_inception)]
mod mssql_fdw_rq;
mod translator;
mod types;

#[cfg(test)]
mod tests;

use pgrx::pg_sys::panic::ErrorReport;
use pgrx::prelude::PgSqlErrorCode;
use thiserror::Error;

use supabase_wrappers::prelude::{CreateRuntimeError, OptionsError};

#[derive(Error, Debug)]
pub(super) enum MssqlFdwRqError {
    #[error("mssql_fdw_rq: invalid option: {0}")]
    InvalidOption(String),

    #[error("mssql_fdw_rq: {0}")]
    UnsupportedConstruct(#[from] translator::TranslateError),

    #[error("mssql_fdw_rq: column '{0}' data type is not supported")]
    UnsupportedColumnType(String),

    #[error("mssql_fdw_rq: parameter type '{0}' is not supported")]
    UnsupportedParameterType(String),

    #[error("mssql_fdw_rq: column conversion failure: {0}")]
    ConversionError(#[from] std::num::TryFromIntError),

    #[error("mssql_fdw_rq: {0}")]
    TiberiusError(#[from] tiberius::error::Error),

    #[error("mssql_fdw_rq: {0}")]
    PgrxNumericError(#[from] pgrx::datum::numeric_support::error::Error),

    #[error("mssql_fdw_rq: {0}")]
    CreateRuntimeError(#[from] CreateRuntimeError),

    #[error("mssql_fdw_rq: {0}")]
    OptionsError(#[from] OptionsError),

    #[error("mssql_fdw_rq: {0}")]
    IoError(#[from] std::io::Error),
}

impl From<MssqlFdwRqError> for ErrorReport {
    fn from(value: MssqlFdwRqError) -> Self {
        ErrorReport::new(PgSqlErrorCode::ERRCODE_FDW_ERROR, format!("{value}"), "")
    }
}

pub(super) type MssqlFdwRqResult<T> = Result<T, MssqlFdwRqError>;
