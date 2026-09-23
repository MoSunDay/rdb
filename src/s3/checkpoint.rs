//! Periodic RocksDB checkpoint publisher (the StarRocks rowset style
//! applied to a KV store): each run materializes a RocksDB
//! checkpoint under a staging dir, uploads its files as one object
//! set `rocksdb/<node>/ckpt_<unix_ms>/...`, writes `meta.json` LAST
//! (its existence marks the set complete -- the rowset-meta rule),
//! drops staging, then prunes old checkpoints down to the retention.
//! Publishing goes through the internal object store handle, so it
//! bypasses the HTTP PUT path (and its 1 GiB body cap).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::now_ms;
use super::object::{self};

/// One uploaded checkpoint file, as listed by `meta.json`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CheckpointFile {
    pub key: String,
    pub size: u64,
    pub etag: String,
}

/// The manifest of one `ckpt_*` object set.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CheckpointMeta {
    pub node: String,
    pub source: String,
    pub created_ms: i64,
    pub total_bytes: u64,
    pub files: Vec<CheckpointFile>,
}

/// Publish one checkpoint of `db` as `rocksdb/<node>/ckpt_<now_ms>/`
/// under `<root>/<bucket>/` (synchronous/blocking: the caller owns
/// the `spawn_blocking` placement). Returns the `ckpt_<now_ms>` id.
/// `retention == 0` keeps the built-in default of 2.
pub fn publish_checkpoint(
    db: &rocksdb::DB,
    root: &Path,
    bucket: &str,
    node: &str,
    source_db_path: &str,
    retention: u32,
) -> Result<String, String> {
    let ts = now_ms();
    let id = format!("ckpt_{ts}");
    let staging = root.join(".staging").join(format!("ckpt-{ts}"));
    if let Err(e) = publish_inner(db, root, bucket, node, source_db_path, &id, ts, &staging) {
        let _ = fs::remove_dir_all(&staging); // best-effort cleanup
        return Err(e);
    }
    sweep_retention(root, bucket, node, retention);
    Ok(id)
}

#[allow(clippy::too_many_arguments)] // explicit checkpoint inputs beat a param struct
fn publish_inner(
    db: &rocksdb::DB,
    root: &Path,
    bucket: &str,
    node: &str,
    source_db_path: &str,
    id: &str,
    ts: i64,
    staging: &Path,
) -> Result<(), String> {
    // A leftover staging dir of the same name makes the checkpoint
    // call refuse to run; drop it first.
    if staging.exists() {
        fs::remove_dir_all(staging).map_err(|e| format!("clean stale staging: {e}"))?;
    }
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create staging parent: {e}"))?;
    }
    rocksdb::checkpoint::Checkpoint::new(db)
        .map_err(|e| format!("open checkpoint: {e}"))?
        .create_checkpoint(staging.to_str().ok_or("staging path not utf-8")?)
        .map_err(|e| format!("create checkpoint: {e}"))?;
    let store = object::open(root).map_err(|e| format!("open object store: {e}"))?;
    // Collect first, then upload: the live LOG / LOG.old.* files of
    // the checkpoint carry no state and stay out of the set.
    let mut rels = Vec::new();
    collect_files(staging, "", &mut rels).map_err(|e| format!("walk staging: {e}"))?;
    let mut files = Vec::new();
    let mut total = 0u64;
    for rel in &rels {
        let key = format!("rocksdb/{node}/{id}/{rel}");
        let put = object::put_file(
            &store,
            bucket,
            &key,
            &staging.join(rel),
            "application/octet-stream",
        )
        .map_err(|e| format!("upload {key}: {e}"))?;
        total += put.size;
        files.push(CheckpointFile {
            key,
            size: put.size,
            etag: put.etag,
        });
    }
    let meta = CheckpointMeta {
        node: node.to_string(),
        source: source_db_path.to_string(),
        created_ms: ts,
        total_bytes: total,
        files,
    };
    let meta_bytes = serde_json::to_vec(&meta).map_err(|e| format!("encode meta: {e}"))?;
    let meta_local = staging.join("meta.json");
    fs::write(&meta_local, &meta_bytes).map_err(|e| format!("stage meta: {e}"))?;
    // meta.json lands LAST on purpose: readers treat its presence as
    // "this ckpt_* set is fully published", so it must never point at
    // a set with missing files (the rowset-meta ordering).
    object::put_file(
        &store,
        bucket,
        &format!("rocksdb/{node}/{id}/meta.json"),
        &meta_local,
        "application/json",
    )
    .map_err(|e| format!("upload meta.json: {e}"))?;
    fs::remove_dir_all(staging).map_err(|e| format!("drop staging: {e}"))?;
    Ok(())
}

/// Recursive staging walk: `(relative-unix-slash path, size)` pairs,
/// skipping `LOG` / `LOG.old.*` and non-regular entries.
fn collect_files(dir: &Path, rel: &str, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "LOG" || name.starts_with("LOG.old.") {
            continue;
        }
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if entry.file_type()?.is_dir() {
            collect_files(&entry.path(), &child, out)?;
        } else if entry.file_type()?.is_file() {
            out.push(child);
        }
    }
    Ok(())
}

/// Delete all but the newest `keep` checkpoints under
/// `<root>/<bucket>/rocksdb/<node>/`. `retention == 0` means the
/// built-in default 2; otherwise at least 1 is kept.
fn sweep_retention(root: &Path, bucket: &str, node: &str, retention: u32) {
    let keep = if retention == 0 { 2 } else { retention.max(1) } as usize;
    let dir = root.join(bucket).join("rocksdb").join(node);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ckpt_"))
        .collect();
    // ckpt_<unix_ms>: numeric descending, lexicographic tiebreak.
    let ts_of = |n: &str| n.trim_start_matches("ckpt_").parse::<u128>().unwrap_or(0);
    ids.sort_by(|a, b| (ts_of(b), b).cmp(&(ts_of(a), a)));
    for old in ids.iter().skip(keep) {
        let _ = fs::remove_dir_all(dir.join(old));
    }
}

/// Background publisher: one `publish_checkpoint` every
/// (3s startup delay + interval), off-worker via `spawn_blocking`.
/// No-ops unless the S3 front is enabled AND the interval is set.
pub fn spawn_publisher(shared: Arc<crate::state::Shared>) {
    let conf = &shared.conf;
    if conf.s3_bind.is_empty() || conf.s3_checkpoint_interval_ms == 0 {
        return;
    }
    // Same snapshot rules as http::serve: bucket default "rdb", root
    // falling back to <store_path>/s3.
    let bucket = if conf.s3_bucket.is_empty() {
        "rdb".to_string()
    } else {
        conf.s3_bucket.clone()
    };
    let root: PathBuf = if conf.s3_store_path.is_empty() {
        PathBuf::from(&conf.store_path).join("s3")
    } else {
        PathBuf::from(&conf.s3_store_path)
    };
    let node = conf.bind.clone();
    let source = crate::store::rocksdb::data_path(&conf.store_path, &conf.bind)
        .display()
        .to_string();
    let interval = Duration::from_millis(conf.s3_checkpoint_interval_ms);
    let retention = conf.s3_checkpoint_retention;
    tokio::spawn(async move {
        loop {
            // Startup grace: let the fronts and raft settle first.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let db = Arc::clone(&shared.store);
            let (root, bucket, node, source) =
                (root.clone(), bucket.clone(), node.clone(), source.clone());
            match tokio::task::spawn_blocking(move || {
                publish_checkpoint(&db.db, &root, &bucket, &node, &source, retention)
            })
            .await
            {
                Ok(Ok(id)) => eprintln!("s3: checkpoint {id} published ({retention} retained)"),
                Ok(Err(e)) => eprintln!("s3: checkpoint publish failed: {e}"),
                Err(e) => eprintln!("s3: checkpoint task failed: {e}"),
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Publish a real RocksDB checkpoint into a temp object store;
    /// every meta.json entry must match a real on-disk file, and the
    /// retention sweep must leave exactly the newest N sets.
    #[test]
    fn publish_roundtrip_and_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db");
        let store = crate::store::rocksdb::open(db_path.to_str().unwrap()).expect("open db");
        for i in 0..5 {
            store
                .db
                .put(format!("k{i}"), format!("value-{i}"))
                .map_err(|e| e.to_string())
                .unwrap();
        }
        let root = dir.path().join("objroot");

        let id1 = publish_checkpoint(&store.db, &root, "bkt", "node-a", "/fake/source", 0)
            .expect("first publish");
        assert!(id1.starts_with("ckpt_"), "{id1}");
        let set = root.join("bkt/rocksdb/node-a").join(&id1);
        // meta.json is a REAL file on the filesystem...
        let raw = fs::read(set.join("meta.json")).expect("meta.json on fs");
        let meta: CheckpointMeta = serde_json::from_slice(&raw).expect("meta decodes");
        assert_eq!(meta.node, "node-a");
        assert_eq!(meta.source, "/fake/source");
        assert_eq!(meta.created_ms.to_string(), id1.trim_start_matches("ckpt_"));
        assert!(!meta.files.is_empty());
        let mut listed = 0u64;
        for f in &meta.files {
            let rel = f
                .key
                .strip_prefix(&format!("rocksdb/node-a/{id1}/"))
                .expect("key prefix");
            let path = set.join(rel);
            assert!(path.is_file(), "{} missing", path.display());
            let size = path.metadata().expect("stat").len();
            assert_eq!(size, f.size, "{}", path.display());
            listed += size;
        }
        assert_eq!(listed, meta.total_bytes);
        // staging is always dropped
        assert!(!root.join(".staging/ckpt-").exists());

        // Second publish with retention=1 keeps only the newest set.
        std::thread::sleep(Duration::from_millis(5)); // distinct ckpt id
        let id2 = publish_checkpoint(&store.db, &root, "bkt", "node-a", "/fake/source", 1)
            .expect("second publish");
        let mut left: Vec<String> = fs::read_dir(root.join("bkt/rocksdb/node-a"))
            .expect("list ckpts")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("ckpt_"))
            .collect();
        left.sort();
        assert_eq!(left.len(), 1, "retention=1 keeps one set: {left:?}");
        assert_eq!(left[0], id1.max(id2.clone()), "newest survives");
    }
}
