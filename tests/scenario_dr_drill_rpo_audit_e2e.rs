//! Multi-store scenario #60 -- a DR drill RPO audit: the node
//! checkpoints its RocksDB to the S3 plane every 500ms under an
//! EXPLICIT retention of one (s3_checkpoint_retention: 1; the
//! built-in default is 2): once a SECOND distinct checkpoint id has
//! ever existed, the retention sweep must leave exactly the newest
//! complete set on disk, and the live plane answers DBSIZE and
//! RANDOMKEY for the audit's key census. One REAL node, S3 + RESP.

mod common;
mod scenario_common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use scenario_common::{front_world, int_of, ok, r, rb, FrontSpec};

#[tokio::test]
async fn retention_prunes_ckpt_sets_and_keys_stay_countable() {
    let spec = FrontSpec {
        s3: true,
        extra_yaml: "s3_checkpoint_retention: 1\n".to_string(),
        ..Default::default()
    };
    let (front, a) = front_world("dr60", &spec).await;
    let node_dir: PathBuf = front.node.dir.clone();
    // front_world layout: s3data is a SIBLING of the ProcNode dir,
    // and the ckpt root is <s3>/rdb/rocksdb/<resp bind>/.
    let s3_root = node_dir.parent().expect("node dir parent").join("s3data");
    let ckpt_root = s3_root.join("rdb/rocksdb").join(&a);

    // a) The audit census: a mixed live keyset, countable by DBSIZE,
    //    spot-checkable by RANDOMKEY.
    ok(&r(&a, &[b"SET", b"{dr}cfg", b"gold"]).await, "set cfg");
    assert_eq!(int_of(&r(&a, &[b"HSET", b"{dr}svc", b"zone", b"cn-1"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{dr}log", b"boot"]).await), 1);
    let dbsize = int_of(&r(&a, &[b"DBSIZE"]).await);
    assert!(dbsize >= 3, "at least the three audit keys: {dbsize}");
    let any_key = rb(&a, &[b"RANDOMKEY"]).await;
    assert!(!any_key.is_empty(), "RANDOMKEY spots one live key");

    // b) RPO audit: the checkpoint clock runs (500ms interval after a
    //    3s startup grace; one publish costs seconds of staging copy
    //    + fsync on this FS, so the cadence is publish-bound). The
    //    drill pins an EXPLICIT retention of ONE: once a SECOND
    //    distinct id has ever existed, the sweep must leave exactly
    //    the newest set on disk -- the tightest provable prune cycle.
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut ever: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    loop {
        ever.extend(scenario_ckpt_ids(&ckpt_root));
        if ever.len() >= 2 || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ever.len() >= 2, "a second checkpoint published: {}", ever.len());
    // The sweep trails the promote by moments: give it 10s to leave
    // only the newest set.
    let sweep_by = Instant::now() + Duration::from_secs(10);
    let final_ids = loop {
        let ids = scenario_ckpt_ids(&ckpt_root);
        if (!ids.is_empty() && ids.len() <= 1) || Instant::now() > sweep_by {
            break ids;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(final_ids.len(), 1, "retention=1 keeps only the newest: {final_ids:?}");
    let newest = final_ids.iter().max().expect("newest ckpt");
    let newest_dir = ckpt_root.join(format!("ckpt_{newest}"));
    assert!(newest_dir.join("meta.json").is_file(), "the newest set is complete");
    assert!(*newest > *ever.iter().min().expect("first id"), "the survivor is the younger set");
}

/// Checkpoint ids (dir names `ckpt_<unix_ms>`) currently on disk.
fn scenario_ckpt_ids(ckpt_root: &std::path::Path) -> Vec<i64> {
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
