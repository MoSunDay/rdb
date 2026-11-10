//! MySQL-AST -> SQLite SQL renderer (the turso translation layer;
//! T1 covered bare constant projections, T2 the minimal table
//! DDL/DML the catalog surface needs, T3 grows it into the CRUD
//! trunk plus the full value/error conversion, and the allowlist
//! batch adds JOIN trees, expression subqueries, set operations and
//! CTEs).
//!
//! Renders rdb's own parsed IR (`parse::ast`) -- never client text --
//! so engine routing stays behind one parse. Unsupported shapes reject
//! with the uniform 1235; nothing degrades silently.

mod ddl;
mod dml;
mod expr;
mod funcs;
mod join;
mod select;
mod set;
mod vector;

pub use ddl::{render_create_index, render_create_table, render_drop_index, render_drop_table};
pub use dml::{render_delete, render_insert, render_truncate, render_update};
pub use expr::{render_expr, render_value};
pub use join::render_table_ref;
pub use select::render_select;
pub use set::render_compound;
pub use vector::{blob_literal, declared_vector_dims, is_vector_decl};

/// Quote one identifier for SQLite (`"name"`, embedded quotes doubled).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
