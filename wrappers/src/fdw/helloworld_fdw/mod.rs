#![allow(clippy::module_inception)]
mod helloworld_fdw;

// not cfg-gated here: `cargo pgrx test` builds the extension with
// `cargo build` (no cfg(test)), and the pg_test module must land in it
mod tests;
