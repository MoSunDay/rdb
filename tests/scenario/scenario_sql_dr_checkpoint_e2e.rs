//! Multi-store scenario #128 -- checkpoint disaster recovery across the
//! SQL AND RESP planes: 20 SQL rows (CREATE TABLE + inserts) and 10 RESP
//! keys (five SET strings, five HSET hashes with two fields each) are
//! written, then a checkpoint BORN AFTER the writes is awaited (id =
//! unix ms, meta.json written LAST marks the set complete; root =
//! <node.dir>/../s3data/rdb/rocksdb/<bind>). The drill: SIGKILL, wipe
//! every live KV file under the data dir PRESERVING `raft/`, copy the
//! checkpoint files back (meta.json and the *.s3meta.json sidecars are
//! not RocksDB state), respawn on the same config, and every pre-kill
//! answer is back: COUNT(*) == 20 with the exact row values, HGETALL
//! fields intact, meta.json still in place. One REAL node, MySQL + S3.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, one, rows_ordered, run, s};
use mysql_async::Value as MVal;
use scenario_common::{FrontSpec, front_world, int_of, ok, r, rb, rf, wait_fronts};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const ROWS: i64 = 20;
const STRINGS: [&str; 5] = ["cfg", "flag", "owner", "zone", "version"];
const HASHES: [&str; 5] = ["meta", "limits", "node", "shard", "tenant"];

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Checkpoint ids (dir names `ckpt_<unix_ms>`) currently on disk.
fn ckpt_ids(ckpt_root: &Path) -> Vec<i64> {
    std::fs::read_dir(ckpt_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .into_owned()
                .strip_prefix("ckpt_")?
                .parse()
                .ok()
        })
        .collect()
}

/// Wait until a checkpoint EXISTS whose id (creation time) is at or
/// after `not_before_ms` and whose meta.json is in place.
async fn wait_ckpt_after(ckpt_root: &Path, not_before_ms: i64, ctx: &str) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let found = ckpt_ids(ckpt_root)
            .into_iter()
            .filter(|id| *id >= not_before_ms)
            .find(|id| {
                ckpt_root
                    .join(format!("ckpt_{id}"))
                    .join("meta.json")
                    .is_file()
            });
        if let Some(id) = found {
            return id;
        }
        assert!(
            Instant::now() < deadline,
            "no checkpoint at/after {not_before_ms} in 25s; {ctx}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The exact 20-row dataset, keyed by id.
fn expected_rows() -> Vec<Vec<MVal>> {
    let mut v: Vec<Vec<MVal>> = (1..=ROWS)
        .map(|i| {
            vec![
                MVal::Bytes(i.to_string().into_bytes()),
                s(&format!("DR-{:03}", i)),
                MVal::Bytes((i * 7).to_string().into_bytes()),
                s(&format!("row-{i}")),
            ]
        })
        .collect();
    v.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    v
}

/// Copy every checkpoint file except the completion marker and the s3
/// sidecars into `data_dir`; returns the number of files restored.
fn restore_files(ckpt_dir: &Path, data_dir: &Path) -> usize {
    let mut restored = 0usize;
    for entry in std::fs::read_dir(ckpt_dir)
        .expect("read ckpt dir")
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "meta.json" || name.ends_with(".s3meta.json") {
            continue; // completion marker + s3 sidecars are not RocksDB state
        }
        std::fs::copy(entry.path(), data_dir.join(&name)).expect("restore ckpt file");
        restored += 1;
    }
    restored
}

#[tokio::test]
async fn checkpoint_restores_sql_and_resp_after_total_loss() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (mut front, a) = front_world("dr128", &spec).await;
    let node_dir: PathBuf = front.node.dir.clone();
    // front_world nests: <world>/node0/<node0 = ProcNode.dir>, while the
    // s3 root (s3_store_path) is a SIBLING of ProcNode.dir.
    let s3_root = node_dir
        .parent()
        .expect("node dir has a parent")
        .join("s3data");
    let bind = a.clone(); // the data dir under store_path is named after bind
    let ckpt_root = s3_root.join("rdb/rocksdb").join(&bind);
    let mut c = connect_root(&front.node).await;

    // a) 20 SQL rows.
    ddl(
        &mut c,
        "CREATE TABLE dr128_rows (id INT PRIMARY KEY, code VARCHAR(16) NOT NULL, \
         qty INT NOT NULL, note VARCHAR(40) NOT NULL)",
    )
    .await;
    for i in 1..=ROWS {
        run(
            &mut c,
            &format!(
                "INSERT INTO dr128_rows VALUES ({}, 'DR-{:03}', {}, 'row-{i}')",
                i,
                i,
                i * 7
            ),
        )
        .await;
    }
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM dr128_rows").await,
        vec![ROWS]
    );
    let before = rows_ordered(&mut c, "SELECT id, code, qty, note FROM dr128_rows").await;
    assert_eq!(before, expected_rows(), "pre-kill rows");

    // b) 10 RESP keys: five strings, five two-field hashes.
    for (i, k) in STRINGS.iter().enumerate() {
        ok(
            &r(
                &a,
                &[
                    b"SET",
                    format!("dr128:{k}").as_bytes(),
                    format!("v{i}").as_bytes(),
                ],
            )
            .await,
            "set",
        );
    }
    for (i, k) in HASHES.iter().enumerate() {
        let key = format!("dr128:{k}");
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[
                        b"HSET",
                        key.as_bytes(),
                        b"zone",
                        format!("z{i}").as_bytes(),
                        b"rank",
                        format!("{}", i * 11).as_bytes(),
                    ]
                )
                .await
            ),
            2,
            "hset {key}"
        );
    }
    assert_eq!(
        int_of(&r(&a, &[b"DBSIZE"]).await),
        10,
        "ten keys before the checkpoint"
    );

    // c) A checkpoint CREATED AFTER every write (id = its unix-ms birth
    //    time) with meta.json in place.
    let wrote_at = now_ms();
    let ckpt = wait_ckpt_after(&ckpt_root, wrote_at, &front.node.ctx()).await;
    assert!(
        ckpt >= wrote_at,
        "checkpoint id {ckpt} >= write time {wrote_at}"
    );
    let ckpt_dir = ckpt_root.join(format!("ckpt_{ckpt}"));
    assert!(ckpt_dir.join("meta.json").is_file(), "set is complete");

    // d) Disaster: SIGKILL, then the live KV files are wiped -- the
    //    `raft` state directory inside the data dir is PRESERVED.
    let _ = c.disconnect().await;
    front.node.kill_now();
    let data_dir = node_dir.join(&bind);
    assert!(data_dir.is_dir(), "live data dir {:?} exists", data_dir);
    for entry in std::fs::read_dir(&data_dir)
        .expect("read data dir")
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "raft" {
            continue;
        }
        if entry.path().is_dir() {
            std::fs::remove_dir_all(entry.path()).expect("wipe kv subdir");
        } else {
            std::fs::remove_file(entry.path()).expect("wipe kv file");
        }
    }
    assert!(data_dir.join("raft").is_dir(), "raft state preserved");

    // e) Restore + respawn + validate, all inside the perf gate.
    let t = scenario_common::perf::now();
    let restored = restore_files(&ckpt_dir, &data_dir);
    assert!(
        restored >= 2,
        "a real file set, not just the marker: {restored} files"
    );
    front.node.respawn();
    wait_fronts(&mut front, true, false, false).await;
    let mut c = connect_root(&front.node).await;

    // f) The pre-kill dataset answers from the restored store: SQL first.
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM dr128_rows").await,
        vec![ROWS],
        "20 rows back"
    );
    let after = rows_ordered(&mut c, "SELECT id, code, qty, note FROM dr128_rows").await;
    assert_eq!(after, expected_rows(), "row values match");
    assert_eq!(
        one(&mut c, "SELECT code FROM dr128_rows WHERE id = 13").await,
        s("DR-013")
    );
    assert_eq!(
        ints(&mut c, "SELECT SUM(qty) FROM dr128_rows").await,
        vec![(1..=ROWS).sum::<i64>() * 7],
        "aggregate over the restored rows"
    );
    // RESP: strings, hashes, and nothing else.
    for (i, k) in STRINGS.iter().enumerate() {
        assert_eq!(
            rb(&a, &[b"GET", format!("dr128:{k}").as_bytes()]).await,
            format!("v{i}"),
            "string {k} back"
        );
    }
    for (i, k) in HASHES.iter().enumerate() {
        let key = format!("dr128:{k}");
        let all = rf(&a, &[b"HGETALL", key.as_bytes()]).await;
        assert!(
            all.contains("zone") && all.contains(&format!("z{i}")) && all.contains("rank"),
            "HGETALL {key}: {all}"
        );
        assert_eq!(
            rb(&a, &[b"HGET", key.as_bytes(), b"rank"]).await,
            (i * 11).to_string(),
            "hash rank field {key}"
        );
    }
    assert_eq!(
        int_of(&r(&a, &[b"DBSIZE"]).await),
        10,
        "exactly the ten keys"
    );
    // meta.json is in place (the checkpoint set stays complete and
    // readable; it was deliberately NOT copied into the data dir).
    assert!(ckpt_dir.join("meta.json").is_file(), "meta.json in place");
    assert!(
        !data_dir.join("meta.json").exists(),
        "marker never restored as KV state"
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ckpt_dir.join("meta.json")).expect("read meta.json"))
            .expect("meta.json json");
    assert_eq!(meta["node"].as_str(), Some(bind.as_str()), "{}", meta);
    assert!(
        !meta["files"].as_array().expect("files").is_empty(),
        "files listed"
    );
    scenario_common::perf::within("dr128/restore-validate", t, Duration::from_secs(10));
    scenario_common::perf::scenario("dr128", t0);
}
