//! Unit tests for the `standalone` quick-start (split out of
//! `src/standalone.rs` to respect the 400-line file cap).

use std::path::PathBuf;

use crate::standalone::*;

fn opts() -> StandaloneOpts {
    StandaloneOpts {
        host: "127.0.0.1".to_string(),
        port: 6379,
        dir: PathBuf::from("./rdb-standalone"),
        token: None,
    }
}

fn parse(args: &[&str]) -> Result<StandaloneOpts, String> {
    parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

#[test]
fn parse_defaults_and_forms() {
    assert_eq!(parse(&[]).unwrap(), opts());
    let p = parse(&["--host", "0.0.0.0", "--port", "7000"]).unwrap();
    assert_eq!((p.host.as_str(), p.port), ("0.0.0.0", 7000));
    let eq = parse(&["--host=::1", "--port=1", "--dir=/tmp/x", "--token=t"]).unwrap();
    assert_eq!((eq.host.as_str(), eq.port, eq.dir.to_str().unwrap()), ("::1", 1, "/tmp/x"));
    assert_eq!(eq.token.as_deref(), Some("t"));
    // Last one wins.
    assert_eq!(parse(&["--port", "1", "--port", "2"]).unwrap().port, 2);
    assert!(parse(&["--port", "abc"]).unwrap_err().contains("invalid --port"));
    // +10 derivation must stay inside the u16 port range.
    assert!(parse(&["--port", "65530"]).unwrap_err().contains("too high"));
    assert!(parse(&["--port", "65525"]).is_ok());
    assert!(parse(&["--port"]).unwrap_err().contains("needs a value"));
    assert!(parse(&["--wat", "x"]).unwrap_err().contains("unknown standalone flag"));
}

#[test]
fn derived_binds_follow_e2e_offsets() {
    let b = derived_binds("127.0.0.1", 32700);
    assert_eq!(b.resp, "127.0.0.1:32700");
    assert_eq!(b.monitor, "127.0.0.1:32702");
    assert_eq!(b.raft_http, "127.0.0.1:32703");
    assert_eq!(b.raft_tcp, "127.0.0.1:32704");
    assert_eq!(b.mysql, "127.0.0.1:32705");
    assert_eq!(b.sql_rpc, "127.0.0.1:32706");
    assert_eq!(b.kafka, "127.0.0.1:32707");
    assert_eq!(b.rocksmq, "127.0.0.1:32708");
    assert_eq!(b.es, "127.0.0.1:32709");
    assert_eq!(b.s3, "127.0.0.1:32710");
}

#[test]
fn token_generated_once_and_read_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t1 = token_for(dir.path(), None).expect("gen");
    assert_eq!(t1.len(), 32);
    assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
    // Restart reads the same token back; explicit token wins + rewrites.
    assert_eq!(token_for(dir.path(), None).unwrap(), t1);
    assert_eq!(token_for(dir.path(), Some("my-token")).unwrap(), "my-token");
    assert_eq!(token_for(dir.path(), None).unwrap(), "my-token");
    // 0600 on the persisted file; empty explicit token is refused.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir.path().join(TOKEN_FILE)).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(token_for(dir.path(), Some("  ")).is_err());
}

#[test]
fn build_conf_wires_every_plane_and_one_credential() {
    let c = build_conf(&opts(), "tok123");
    assert_eq!(c.store_path, "./rdb-standalone");
    assert_eq!(c.bind, "127.0.0.1:6379");
    assert_eq!(c.raft_tcp_address, "127.0.0.1:6383");
    assert_eq!(c.http_address, "127.0.0.1:6382");
    assert_eq!(c.mysql_bind, "127.0.0.1:6384");
    for field in [
        &c.raft_token,
        &c.mysql_password,
        &c.kafka_token,
        &c.rocksmq_token,
        &c.es_token,
        &c.s3_token,
    ] {
        assert_eq!(field, "tok123");
    }
    // The backup plane stays off in standalone mode.
    assert!(c.backup_bind.is_empty() && c.backup_store_path.is_empty());
}

#[test]
fn topology_plan_three_branches() {
    let own = "127.0.0.1:6379";
    assert_eq!(topology_plan("", own), TopologyPlan::Seed);
    assert_eq!(topology_plan("   ", own), TopologyPlan::Seed);
    assert_eq!(topology_plan(own, own), TopologyPlan::Done);
    assert_eq!(topology_plan(&format!(" {own} "), own), TopologyPlan::Done);
    for v in ["127.0.0.1:6379,127.0.0.1:7000", "127.0.0.1:7000"] {
        assert_eq!(topology_plan(v, own), TopologyPlan::Refuse(v.to_string()));
    }
}
