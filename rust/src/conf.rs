//! Process configuration (Rust mirror of Go `internal/conf`).
//!
//! Go loads `conf.Content` from a YAML file at init time (flag `-config`,
//! default `config/config.yml`) using yaml.v2, where missing keys leave the
//! zero value in place. Here `load` returns the parsed `Config`; runtime
//! state (monitor collector, raft cache, sentinel clock, ...) is kept in
//! shared state elsewhere, so only the YAML-deserializable fields live here.

use std::collections::BTreeMap;

/// Default config path of the Go `-config` flag.
pub const DEFAULT_CONFIG_PATH: &str = "config/config.yml";

/// Static process configuration, decoded from YAML.
///
/// Every field is `#[serde(default)]` so missing YAML keys decode to zero
/// values, matching Go yaml.v2 semantics.
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct Config {
    #[serde(default, rename = "store_path")]
    pub store_path: String,
    #[serde(default, rename = "bind")]
    pub bind: String,
    #[serde(default, rename = "monitor_addr")]
    pub monitor_addr: String,
    #[serde(default, rename = "raft_bind_address")]
    pub raft_tcp_address: String,
    #[serde(default, rename = "raft_http_bind_address")]
    pub http_address: String,
    #[serde(default, rename = "raft_token")]
    pub raft_token: String,
    #[serde(default, rename = "backup_store_path")]
    pub backup_store_path: String,
    #[serde(default, rename = "backup_bind")]
    pub backup_bind: String,
    #[serde(default, rename = "backup_monitor_addr")]
    pub backup_monitor_addr: String,
    #[serde(default, rename = "backup_target_map")]
    pub backup_target_map: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default, rename = "allow_ip_list")]
    pub ip_list: Vec<String>,
    /// SQL data plane: MySQL-protocol listener (empty = disabled).
    #[serde(default, rename = "mysql_bind")]
    pub mysql_bind: String,
    /// SQL login user (empty = "root"); password may be empty.
    #[serde(default, rename = "mysql_user")]
    pub mysql_user: String,
    #[serde(default, rename = "mysql_password")]
    pub mysql_password: String,
    /// Node-to-node SQL RPC (sub-plans, 2PC); empty = disabled.
    #[serde(default, rename = "sql_rpc_bind")]
    pub sql_rpc_bind: String,
    /// Redis MULTI/EXEC transactions; disabled -> MULTI errors.
    #[serde(default, rename = "tx")]
    pub tx: TxConfig,
}

/// `[tx]` section. Serde's `default` calls [`Default::default`] (NOT the
/// derive): the manual impl keeps `enabled = true` when the section is
/// absent, matching `Config::default()` used by tests and Lite Mode.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct TxConfig {
    #[serde(default = "tx_enabled_default")]
    pub enabled: bool,
}

fn tx_enabled_default() -> bool {
    true
}

impl Default for TxConfig {
    fn default() -> Self {
        TxConfig {
            enabled: tx_enabled_default(),
        }
    }
}

/// Read and parse the YAML config file at `path`.
///
/// Go equivalent: `ioutil.ReadFile` + `yaml.Unmarshal` into `conf.Content`
/// (both fatal on error); here errors are returned as `String` with context.
pub fn load(path: &str) -> Result<Config, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read config file {path}: {e}"))?;
    serde_yaml::from_str(&text).map_err(|e| format!("parse config file {path}: {e}"))
}

/// Go: `os.Getenv("RAFT_BOOTSTRAP") == "true"` (exact match only).
pub fn raft_bootstrap() -> bool {
    std::env::var("RAFT_BOOTSTRAP")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Go: `os.Getenv("RAFT_JOIN_ADDR")`; empty string means "no join".
pub fn raft_join_addr() -> String {
    std::env::var("RAFT_JOIN_ADDR").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the structure of /root/rdb/config/conf_32681.yaml with a
    /// FAKE token (never copy real secrets into tests).
    const FULL_YAML: &str = r#"
bind: 127.0.0.1:32681
store_path: /tmp/
backup_bind: 127.0.0.1:32682
backup_store_path: /tmp/backup
raft_http_bind_address: 127.0.0.1:12681
raft_token: "fake-token-0000000000000000000000000000000000000000000000000000"
raft_bind_address: 127.0.0.1:22681
monitor_addr: 127.0.0.1:42681
backup_monitor_addr: 127.0.0.1:42682

# only leader
allow_ip_list:
  - 127.0.0.1
# only leader
backup_target_map:
  127.0.0.1:22681:
    src: 127.0.0.1:32681
    target: 127.0.0.1:32684
  127.0.0.1:22683:
    src: 127.0.0.1:32683
    target: 127.0.0.1:32686
"#;

    #[test]
    fn tx_section_defaults_and_overrides() {
        // absent section -> enabled (manual Default, not the derive's false)
        let cfg: Config = serde_yaml::from_str("bind: 1.2.3.4:1").unwrap();
        assert!(cfg.tx.enabled);
        assert!(Config::default().tx.enabled);
        // explicit disable
        let cfg: Config = serde_yaml::from_str("bind: 1.2.3.4:1\ntx:\n  enabled: false\n").unwrap();
        assert!(!cfg.tx.enabled);
        // section present, flag omitted -> still enabled
        let cfg: Config = serde_yaml::from_str("bind: 1.2.3.4:1\ntx: {}\n").unwrap();
        assert!(cfg.tx.enabled);
    }

    #[test]
    fn parse_full_yaml() {
        let cfg: Config = serde_yaml::from_str(FULL_YAML).expect("full yaml parses");
        assert_eq!(cfg.bind, "127.0.0.1:32681");
        assert_eq!(cfg.store_path, "/tmp/");
        assert_eq!(cfg.backup_bind, "127.0.0.1:32682");
        assert_eq!(cfg.backup_store_path, "/tmp/backup");
        assert_eq!(cfg.http_address, "127.0.0.1:12681");
        assert_eq!(
            cfg.raft_token,
            "fake-token-0000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(cfg.raft_tcp_address, "127.0.0.1:22681");
        assert_eq!(cfg.monitor_addr, "127.0.0.1:42681");
        assert_eq!(cfg.backup_monitor_addr, "127.0.0.1:42682");
        assert_eq!(cfg.ip_list, vec!["127.0.0.1".to_string()]);
        assert_eq!(cfg.backup_target_map.len(), 2);
        let m1 = &cfg.backup_target_map["127.0.0.1:22681"];
        assert_eq!(m1["src"], "127.0.0.1:32681");
        assert_eq!(m1["target"], "127.0.0.1:32684");
        let m2 = &cfg.backup_target_map["127.0.0.1:22683"];
        assert_eq!(m2["src"], "127.0.0.1:32683");
        assert_eq!(m2["target"], "127.0.0.1:32686");
    }

    #[test]
    fn parse_minimal_yaml_defaults() {
        // Missing keys become zero values (Go yaml.v2 semantics).
        let cfg: Config = serde_yaml::from_str("bind: 127.0.0.1:1\n").expect("parses");
        assert_eq!(cfg.bind, "127.0.0.1:1");
        assert_eq!(cfg.store_path, "");
        assert_eq!(cfg.monitor_addr, "");
        assert_eq!(cfg.raft_tcp_address, "");
        assert_eq!(cfg.http_address, "");
        assert_eq!(cfg.raft_token, "");
        assert_eq!(cfg.backup_store_path, "");
        assert_eq!(cfg.backup_bind, "");
        assert_eq!(cfg.backup_monitor_addr, "");
        assert!(cfg.backup_target_map.is_empty());
        assert!(cfg.ip_list.is_empty());

        // Even a completely empty document yields all defaults.
        let empty: Config = serde_yaml::from_str("").expect("empty yaml parses");
        assert_eq!(empty, Config::default());
    }

    #[test]
    fn load_from_file_and_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yml");
        std::fs::write(&path, FULL_YAML).expect("write");
        let cfg = load(path.to_str().unwrap()).expect("load");
        assert_eq!(cfg.bind, "127.0.0.1:32681");

        let err = load("/nonexistent/no-such-config.yml").unwrap_err();
        assert!(err.contains("read config file"), "got: {err}");

        let bad = dir.path().join("bad.yml");
        std::fs::write(&bad, "bind: [unclosed\n").expect("write");
        let err = load(bad.to_str().unwrap()).unwrap_err();
        assert!(err.contains("parse config file"), "got: {err}");
    }

    /// Env mutation is process-global, so all env assertions live in ONE
    /// test (one thread) and each var is removed after use.
    #[test]
    fn env_helpers() {
        // RAFT_BOOTSTRAP: only exactly "true" enables bootstrapping.
        std::env::remove_var("RAFT_BOOTSTRAP");
        assert!(!raft_bootstrap());
        std::env::set_var("RAFT_BOOTSTRAP", "true");
        assert!(raft_bootstrap());
        std::env::set_var("RAFT_BOOTSTRAP", "True");
        assert!(!raft_bootstrap());
        std::env::set_var("RAFT_BOOTSTRAP", "1");
        assert!(!raft_bootstrap());
        std::env::set_var("RAFT_BOOTSTRAP", "");
        assert!(!raft_bootstrap());
        std::env::remove_var("RAFT_BOOTSTRAP");
        assert!(!raft_bootstrap());

        // RAFT_JOIN_ADDR: missing -> "" (caller treats empty as no-join).
        std::env::remove_var("RAFT_JOIN_ADDR");
        assert_eq!(raft_join_addr(), "");
        std::env::set_var("RAFT_JOIN_ADDR", "127.0.0.1:22681");
        assert_eq!(raft_join_addr(), "127.0.0.1:22681");
        std::env::remove_var("RAFT_JOIN_ADDR");
        assert_eq!(raft_join_addr(), "");
    }

    #[test]
    fn default_config_path_matches_go_flag() {
        assert_eq!(DEFAULT_CONFIG_PATH, "config/config.yml");
    }
}
