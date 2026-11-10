//! Embedded turso engine (feature `turso-engine`): a second SQL engine
//! behind the existing MySQL frontend, routed per logical database.
//!
//! Routing key is the database NAME: `turso_*` databases map to one
//! turso library file each (`<turso_db_dir>/<name>.db`). The prefix is
//! the engine marker because sqlparser's `CreateDatabase` has no ENGINE
//! field to hang a real clause on. Runtime-gated by conf (`turso_engine`
//! + `turso_db_dir`); disabled builds and nodes keep the stock
//! single-engine behavior bit for bit.

mod conv;
mod engine;
pub mod meta;
pub mod route;
pub mod translate;

pub mod catalog;

pub use catalog::list_dbs;
pub use engine::{check_use, db_path, enabled, is_turso_db, run_execute, ConnHandle};
pub use route::route;
