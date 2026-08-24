use super::*;
use crate::sql::storage::catalog::catalog_key;
use crate::sql::storage::schema::{ColumnDef, Engine, SqlType};
use crate::sql::tx;
use crate::state::testutil;
use std::sync::Arc;

fn schema(id: u32, name: &str) -> TableSchema {
    TableSchema {
        id,
        name: name.to_string(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                sql_type: SqlType::Int,
                nullable: false,
            },
            ColumnDef {
                name: "v".into(),
                sql_type: SqlType::VarChar,
                nullable: true,
            },
        ],
        pk: "id".into(),
        engine: Engine::Row,
        indexes: vec![],
    }
}

fn shared_at(bind: &str) -> Shared {
    let conf = crate::conf::Config {
        bind: bind.to_string(),
        ..testutil::test_config()
    };
    testutil::shared_with(conf)
}

fn seed_catalog(shared: &Shared, s: &TableSchema) {
    shared
        .raft
        .write()
        .unwrap()
        .kv
        .insert(catalog_key(&s.name), serde_json::to_string(s).unwrap());
}

fn set_topology(shared: &Shared, addrs: &[&str]) {
    let joined = addrs.join(",");
    *shared.topology.write().unwrap() = crate::topology::refresh(&joined);
}

/// Publish one peer in the raft-replicated `sql_nodes` registry so
/// the coordinator can resolve its sql_rpc port (keyed by raft addr,
/// looked up by resp bind, mirroring `merged_registry`).
fn register_node(shared: &Shared, resp: &str, sql_rpc: &str) {
    let raft_addr = format!("{resp}-raft");
    let mut map = std::collections::BTreeMap::new();
    map.insert(
        raft_addr.clone(),
        crate::sql::tx::nodes::NodeBinds {
            resp: resp.to_string(),
            raft: raft_addr,
            http: format!("{resp}-http"),
            mysql: String::new(),
            sql_rpc: sql_rpc.to_string(),
        },
    );
    shared.raft.write().unwrap().kv.insert(
        crate::sql::tx::nodes::SQL_NODES_KEY.to_string(),
        serde_json::to_string(&map).unwrap(),
    );
}

fn put_version(shared: &Shared, s: &TableSchema, pk: i64, ts: u64, row: Option<Vec<Value>>) {
    let key = row::pk_encode(&Value::Int(pk)).unwrap();
    let k = row::version_key(s, row::row_slot(s, &key), &key, ts);
    let v = match row {
        Some(values) => row::encode_row(s, &values).unwrap(),
        None => row::encode_tombstone(),
    };
    let mut batch = rocksdb::WriteBatch::default();
    batch.put(k, v);
    crate::store::ops::batch_write(&shared.store, batch).unwrap();
}

fn row_of(pk: i64, v: &str) -> Vec<Value> {
    vec![Value::Int(pk), Value::Str(v.to_string())]
}

fn tref(name: &str) -> TableRef {
    TableRef::Table {
        name: name.to_string(),
        alias: None,
    }
}

#[test]
fn gatherable_needs_cluster_and_plain_table() {
    let a = shared_at("127.0.0.1:33101");
    // No cluster: single-node fast path.
    assert!(gatherable(&a, &tref("g")).is_none());
    // One node only: still local (nothing to gather).
    set_topology(&a, &["127.0.0.1:33101"]);
    assert!(gatherable(&a, &tref("g")).is_none());
    // Joins stay local-scan v1.
    set_topology(&a, &["127.0.0.1:33101", "127.0.0.1:33102"]);
    let join = TableRef::Join {
        left: Box::new(tref("g")),
        right: Box::new(tref("g")),
        on: None,
    };
    assert!(gatherable(&a, &join).is_none());
    // Plain table over 3 owners -> 3 bands.
    set_topology(&a, &["127.0.0.1:33101", "b", "c"]);
    let bs = gatherable(&a, &tref("g")).expect("gather");
    assert_eq!(bs.len(), 3);
    assert_eq!(headline(&a, &tref("g")).unwrap(), "Gather(bands=3)");
}

/// Full-loop gather over two REAL stores: the participant answers
/// through the sql_rpc server, bands merge disjointly in pk order,
/// read_ts and the txn overlay apply over the gathered rows.
#[tokio::test]
async fn gather_merges_bands_disjointly() {
    let a = shared_at("127.0.0.1:33101");
    let b = shared_at("127.0.0.1:33102");
    let s = schema(7, "g");
    seed_catalog(&a, &s);
    seed_catalog(&b, &s);
    set_topology(&a, &["127.0.0.1:33101", "127.0.0.1:33102"]);
    set_topology(&b, &["127.0.0.1:33101", "127.0.0.1:33102"]);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sql_rpc = listener.local_addr().unwrap().to_string();

    // Spread pks across BOTH bands by their real slot, then check
    // each landed on the node whose band owns it. Seeding happens
    // BEFORE `b` moves into the serving task.
    let r = routing(&a).unwrap();
    let bs = bands(&r);
    let mut owners = Vec::new();
    for pk in 1..=40i64 {
        let key = row::pk_encode(&Value::Int(pk)).unwrap();
        let slot = row::row_slot(&s, &key);
        let band = bs.iter().find(|x| x.lo <= slot && slot <= x.hi).unwrap();
        let target = if band.owner == a.conf.bind { &a } else { &b };
        // Two versions: ts 3 old, ts 8 new; a read at 5 sees old.
        put_version(target, &s, pk, 3, Some(row_of(pk, "old")));
        put_version(target, &s, pk, 8, Some(row_of(pk, "new")));
        owners.push(band.owner.clone());
    }
    // A pk deleted after ts 5: invisible at 10, present at 5.
    let del_pk = 41;
    let del_key = row::pk_encode(&Value::Int(del_pk)).unwrap();
    let del_slot = row::row_slot(&s, &del_key);
    let del_band = bs
        .iter()
        .find(|x| x.lo <= del_slot && x.hi >= del_slot)
        .unwrap();
    let del_target = if del_band.owner == a.conf.bind {
        &a
    } else {
        &b
    };
    put_version(del_target, &s, del_pk, 4, Some(row_of(del_pk, "gone")));
    put_version(del_target, &s, del_pk, 9, None);
    assert!(
        bs.iter().all(|x| owners.contains(&x.owner)),
        "seed must cover both bands: {bs:?}"
    );

    tokio::spawn(super::super::server::serve_on(listener, Arc::new(b)));
    register_node(&a, "127.0.0.1:33102", &sql_rpc);

    // read_ts = 5: old values, the not-yet-deleted row (deleted at ts
    // 9) included with its live-at-5 value.
    let src = materialize(&a, &tref("g"), 5, None, None).await.unwrap();
    let mut expect: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
    for pk in 1..=40i64 {
        let key = row::pk_encode(&Value::Int(pk)).unwrap();
        expect.insert(key, row_of(pk, "old"));
    }
    expect.insert(row::pk_encode(&Value::Int(41)).unwrap(), row_of(41, "gone"));
    assert_eq!(
        src.rows,
        expect.values().cloned().collect::<Vec<_>>(),
        "pk-ordered disjoint union at read_ts 5"
    );

    // read_ts = 10: new values, deleted pk gone -- exactly once each.
    let src = materialize(&a, &tref("g"), 10, None, None).await.unwrap();
    expect.clear();
    for pk in 1..=40i64 {
        let key = row::pk_encode(&Value::Int(pk)).unwrap();
        expect.insert(key, row_of(pk, "new"));
    }
    assert_eq!(src.rows, expect.values().cloned().collect::<Vec<_>>());

    // Txn overlay over gathered rows: stage an inject + a hide.
    let mut txn = Txn {
        read_ts: 10,
        writes: BTreeMap::new(),
        ..Default::default()
    };
    tx::stage_upsert(&mut txn, &s, row_of(99, "staged")).unwrap();
    tx::stage_delete(&mut txn, &s, row::pk_encode(&Value::Int(1)).unwrap()).unwrap();
    let src = materialize(&a, &tref("g"), 10, Some(&txn), None)
        .await
        .unwrap();
    let ids: Vec<i64> = src
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            _ => panic!("non-int pk"),
        })
        .collect();
    assert!(!ids.contains(&1), "staged tombstone must hide the row");
    assert_eq!(ids.iter().filter(|i| **i == 99).count(), 1, "staged insert");
    assert_eq!(ids.len(), 40, "39 gathered + 1 staged");
}

/// A remote that accepts then drops connections (transport error)
/// must fail the whole read with the 1027-style node error, never
/// return the local band alone. The request is retried once.
#[tokio::test]
async fn unreachable_node_fails_the_whole_read() {
    let a = shared_at("127.0.0.1:33101");
    let s = schema(7, "g");
    seed_catalog(&a, &s);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let flaky = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            drop(sock); // close instantly: connect ok, then EOF
        }
    });
    let peer = "127.0.0.1:33102";
    set_topology(&a, &["127.0.0.1:33101", peer]);
    register_node(&a, peer, &flaky);
    // Local rows exist: partial results would be silently wrong.
    put_version(&a, &s, 1, 3, Some(row_of(1, "local")));

    let err = materialize(&a, &tref("g"), 10, None, None)
        .await
        .expect_err("remote is down");
    assert_eq!(err.code, ErrorCode::NodeUnreachable);
    assert!(
        err.msg.contains("cluster node") && err.msg.contains("unreachable"),
        "unexpected message: {}",
        err.msg
    );
}

// ---------- M3: columnar fan-out (ScanColumnar) ----------

fn columnar_schema(id: u32, name: &str) -> TableSchema {
    let mut s = schema(id, name);
    s.engine = Engine::Columnar;
    s
}

/// Columnar reads fan out to EVERY member (segments commit where the
/// txn closed): the coordinator unions its own segments with each
/// peer's `ScanColumnar` reply; read_ts applies on both sides and the
/// txn's staged appends ride last.
#[tokio::test]
async fn columnar_fanout_unions_every_node() {
    let a = shared_at("127.0.0.1:33111");
    let b = shared_at("127.0.0.1:33112");
    let s = columnar_schema(9, "cg");
    seed_catalog(&a, &s);
    seed_catalog(&b, &s);
    set_topology(&a, &["127.0.0.1:33111", "127.0.0.1:33112"]);
    set_topology(&b, &["127.0.0.1:33111", "127.0.0.1:33112"]);
    // Each node commits its OWN segment (per-node registry caches).
    crate::sql::columnar::writer::commit_segment(&a, &s, 1, 5, &[row_of(1, "from-a")])
        .await
        .unwrap();
    crate::sql::columnar::writer::commit_segment(&b, &s, 2, 6, &[row_of(2, "from-b")])
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sql_rpc = listener.local_addr().unwrap().to_string();
    tokio::spawn(super::super::server::serve_on(listener, Arc::new(b)));
    register_node(&a, "127.0.0.1:33112", &sql_rpc);

    let src = materialize(&a, &tref("cg"), 10, None, None).await.unwrap();
    assert_eq!(src.scope.sides.len(), 1);
    assert_eq!(src.scope.sides[0].qualifier, "cg");
    let mut got = src.rows.clone();
    got.sort_by_key(|r| match r[0] {
        Value::Int(i) => i,
        _ => panic!("non-int id"),
    });
    assert_eq!(got, vec![row_of(1, "from-a"), row_of(2, "from-b")]);

    // read_ts cutoff applies on the remote side too (ts 6 > 5).
    let src = materialize(&a, &tref("cg"), 5, None, None).await.unwrap();
    assert_eq!(src.rows, vec![row_of(1, "from-a")]);

    // The coordinator appends the open txn's staged appends last.
    let mut txn = Txn {
        read_ts: 10,
        writes: BTreeMap::new(),
        ..Default::default()
    };
    txn.appends
        .insert("cg".to_string(), vec![row_of(3, "staged")]);
    let src = materialize(&a, &tref("cg"), 10, Some(&txn), None)
        .await
        .unwrap();
    assert_eq!(src.rows.len(), 3);
    assert_eq!(src.rows.last(), Some(&row_of(3, "staged")));
}

/// A downed member fails the WHOLE columnar read with the 1027-style
/// node error, never a partial union (same contract as band gathers).
#[tokio::test]
async fn columnar_fanout_fails_whole_read_when_node_unreachable() {
    let a = shared_at("127.0.0.1:33113");
    let s = columnar_schema(10, "cd");
    seed_catalog(&a, &s);
    crate::sql::columnar::writer::commit_segment(&a, &s, 1, 5, &[row_of(1, "local")])
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let flaky = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            drop(sock); // close instantly: connect ok, then EOF
        }
    });
    let peer = "127.0.0.1:33114";
    set_topology(&a, &["127.0.0.1:33113", peer]);
    register_node(&a, peer, &flaky);

    let err = materialize(&a, &tref("cd"), 10, None, None)
        .await
        .expect_err("remote is down");
    assert_eq!(err.code, ErrorCode::NodeUnreachable);
    assert!(
        err.msg.contains("cluster node") && err.msg.contains("unreachable"),
        "unexpected message: {}",
        err.msg
    );
}

/// EXPLAIN headline: a plain columnar table in a ready multi-node
/// cluster banners the node fan-out, not the slot bands.
#[test]
fn headline_banners_columnar_fanout() {
    let a = shared_at("127.0.0.1:33115");
    let s = columnar_schema(11, "ch");
    seed_catalog(&a, &s);
    // Single node: nothing to gather.
    assert!(headline(&a, &tref("ch")).is_none());
    set_topology(&a, &["127.0.0.1:33115", "127.0.0.1:33116", "c"]);
    assert_eq!(
        headline(&a, &tref("ch")).unwrap(),
        "Gather(columnar, nodes=3)"
    );
    // Row tables keep the bands banner.
    let r = schema(12, "rh");
    seed_catalog(&a, &r);
    assert_eq!(headline(&a, &tref("rh")).unwrap(), "Gather(bands=3)");
}
