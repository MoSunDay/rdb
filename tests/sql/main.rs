//! MySQL-protocol SQL data-plane e2e tests, aggregated into one crate
//! (tests-reorg 2026-11-10). sql_oracle_cluster_e2e stays a separate
//! target: it sets RAFT_BOOTSTRAP process-wide.

#[path = "../common/mod.rs"]
mod common;

mod auto_increment_e2e;
mod columnar_e2e;
mod mysql_compat_e2e;
mod sql_2pc_e2e;
mod sql_composite_pk_e2e;
mod sql_ddl_surface_cluster_e2e;
mod sql_ddl_surface_e2e;
mod sql_ddl_visibility_e2e;
mod sql_decimal_e2e;
mod sql_dist_read_e2e;
mod sql_e2e;
mod sql_failover_e2e;
mod sql_funcs_control_e2e;
mod sql_funcs_datetime_e2e;
mod sql_funcs_numeric_e2e;
mod sql_funcs_string_e2e;
mod sql_index_e2e;
mod sql_insert_select_e2e;
mod sql_join_cluster_e2e;
mod sql_outer_join_e2e;
mod sql_query_semantics_e2e;
mod sql_rename_gc_e2e;
mod sql_restart_e2e;
mod sql_setop_cluster_e2e;
mod sql_subquery_e2e;
mod sql_txn_e2e;
mod sql_types_e2e;
mod sql_upsert_cluster_e2e;
mod sql_upsert_e2e;
mod sql_vector_e2e;
mod starrocks_model_e2e;
mod txn_semantics_e2e;
