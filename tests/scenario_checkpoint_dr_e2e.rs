//! Multi-store scenario #35 -- checkpoint disaster recovery on the S3
//! plane: the node checkpoints its RocksDB every 500ms under
//! `<node>/s3data/rdb/rocksdb/<bind>/ckpt_<ms>/` (meta.json LAST: its
//! presence marks the set complete). The DR drill: write data, wait
//! out a checkpoint CREATED AFTER the writes (id is its unix-ms birth
//! time), SIGKILL the node, wipe the live KV files -- PRESERVING the
//! raft state directory that rides inside the same data dir -- copy
//! the checkpoint files back in place of the dead store, respawn, and
//! every plane answers with the pre-kill data. One REAL node, S3 +
//! RESP.

mod common;
mod scenario_common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use scenario_common::{front_world, int_of, ok, r, rb, rf, wait_fronts, FrontSpec};

/// Checkpoint ids (dir names `ckpt_<unix_ms>`) currently on disk.
fn ckpt_ids(ckpt_root: &Path) -> Vec<i64> {
    std::fs::read_dir(ckpt_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().into_owned().strip_prefix("ckpt_")?.parse().ok())
        .collect()
}

/// Wait until a checkpoint EXISTS whose id (creation time) is at or
/// after `not_before_ms` and whose meta.json is in place.
async fn wait_ckpt_after(ckpt_root: &Path, not_before_ms: i64) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let found = ckpt_ids(ckpt_root)
            .into_iter()
            .filter(|id| *id >= not_before_ms)
            .find(|id| ckpt_root.join(format!("ckpt_{id}")).join("meta.json").is_file());
        if let Some(id) = found {
            return id;
        }
        assert!(
            Instant::now() < deadline,
            "no fresh checkpoint in 25s (now_ms {now_ms}); ids so far: {:?}",
            ckpt_ids(ckpt_root)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn checkpoint_snapshot_restores_the_store_after_total_loss() {
    let spec = FrontSpec {
        s3: true,
        ..Default::default()
    };
    let (mut front, a) = front_world("dr35", &spec).await;
    let node_dir: PathBuf = front.node.dir.clone();
    // front_world nests: <world>/node0/<node0 = ProcNode.dir>, while the
    // s3 root (s3_store_path) is a SIBLING of ProcNode.dir.
    let s3_root = node_dir
        .parent()
        .expect("node dir has a parent")
        .join("s3data");
    let bind = a.clone(); // the data dir under store_path is named after bind

    // a) Pre-kill dataset across RESP shapes.
    ok(&r(&a, &[b"SET", b"dr:cfg", b"gold"]).await, "set gold");
    assert_eq!(int_of(&r(&a, &[b"HSET", b"dr:meta", b"zone", b"cn-1"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"dr:log", b"boot"]).await), 1);
    let ckpt_root = s3_root.join("rdb/rocksdb").join(&bind);

    // b) The first checkpoint lands (interval 500ms; the root only
    //    appears then, so no eager is_dir assert), then
    //    data written AFTER it must wait for a LATER checkpoint.
    let _first = wait_ckpt_after(&ckpt_root, 0).await;
    let wrote_more_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    ok(&r(&a, &[b"SET", b"dr:post", b"survivor"]).await, "set survivor");
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"dr:log", b"resize"]).await), 2);
    let ckpt = wait_ckpt_after(&ckpt_root, wrote_more_at).await;
    let ckpt_dir = ckpt_root.join(format!("ckpt_{ckpt}"));
    assert!(ckpt_dir.join("meta.json").is_file(), "set is complete");

    // c) Disaster: SIGKILL, then the live KV files are wiped -- the
    //    `raft` state directory inside the data dir is PRESERVED (it
    //    is not part of the KV checkpoint; losing it would orphan the
    //    single-node cluster identity).
    front.node.kill_now();
    let data_dir = node_dir.join(&bind);
    assert!(data_dir.is_dir(), "live data dir {:?} exists", data_dir);
    for entry in std::fs::read_dir(&data_dir).expect("read data dir").flatten() {
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

    // d) Restore: copy every checkpoint file except the marker back
    //    into the data dir (same path the store opened from).
    let mut restored = 0usize;
    for entry in std::fs::read_dir(&ckpt_dir).expect("read ckpt dir").flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "meta.json" || name.ends_with(".s3meta.json") {
            continue; // completion marker + s3 sidecars are not RocksDB state
        }
        std::fs::copy(entry.path(), data_dir.join(name.as_ref())).expect("restore ckpt file");
        restored += 1;
    }
    assert!(restored >= 2, "a real file set, not just the marker: {restored} files");

    // e) Respawn on the SAME config + data dir (wait_fronts polls RESP
    //    readiness for us).
    front.node.respawn();
    wait_fronts(&mut front, false, false, false).await;

    // f) The pre-kill dataset answers from the restored store.
    assert_eq!(rb(&a, &[b"GET", b"dr:cfg"]).await, "gold");
    assert_eq!(rb(&a, &[b"HGET", b"dr:meta", b"zone"]).await, "cn-1");
    assert_eq!(rb(&a, &[b"GET", b"dr:post"]).await, "survivor", "post-first-ckpt data");
    let log = rf(&a, &[b"LRANGE", b"dr:log", b"0", b"-1"]).await;
    assert!(log.contains("boot") && log.contains("resize"), "{log:?}");
}

