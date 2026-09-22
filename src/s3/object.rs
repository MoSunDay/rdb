//! Local-filesystem object store: buckets are directories under the
//! root, object keys are slash-separated relative paths. Every object
//! gets a `<obj>.s3meta.json` sidecar holding its etag and content
//! type (no extra index needed). Free functions + data carriers.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use md5::{Digest, Md5};

/// Root handle: just the store root path.
pub struct ObjectStore {
    pub(crate) root: PathBuf,
}

/// Object metadata as served by `head`/`list`.
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub etag: String, // md5 hex, S3-quoted: `"<hex>"`
    pub last_modified_ms: i64,
    pub content_type: String,
}

/// Result of a successful `put_file`.
pub struct PutResult {
    pub etag: String,
    pub size: u64,
}

/// One ListObjects page: `next_after` is the LAST entry of the page
/// (object key or common prefix) -- the value the caller echoes back
/// as a continuation token / marker.
pub struct ListPage {
    pub objects: Vec<ObjectMeta>,
    pub common_prefixes: Vec<String>,
    pub truncated: bool,
    pub next_after: Option<String>,
}

/// Unique-number source for staging file names.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Open (or create) the store rooted at `root`.
pub fn open(root: &Path) -> io::Result<ObjectStore> {
    fs::create_dir_all(root)?;
    Ok(ObjectStore { root: root.to_path_buf() })
}

/// S3 bucket name rules: 1..=63 chars from [a-z0-9.-], not starting
/// or ending with `-` or `.`.
pub fn valid_bucket_name(bucket: &str) -> bool {
    let byte_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-';
    (1..=63).contains(&bucket.len())
        && bucket.bytes().all(byte_ok)
        && !bucket.starts_with(['-', '.'])
        && !bucket.ends_with(['-', '.'])
}

/// Key rules: non-empty, <= 1024 bytes, no NUL, no empty/`.`/`..`
/// path components, no trailing `/`.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 1024
        && !key.contains('\0')
        && !key.ends_with('/')
        && !key.split('/').any(|c| c.is_empty() || c == "." || c == "..")
}

/// Validated bucket directory, or `None` on an illegal name.
pub fn bucket_path(store: &ObjectStore, bucket: &str) -> Option<PathBuf> {
    valid_bucket_name(bucket).then(|| store.root.join(bucket))
}

/// Validated object path, or `None` on an illegal name/key.
pub fn object_path(store: &ObjectStore, bucket: &str, key: &str) -> Option<PathBuf> {
    match (valid_bucket_name(bucket), valid_key(key)) {
        (true, true) => Some(store.root.join(bucket).join(key)),
        _ => None,
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Create a bucket; `Ok(true)` when newly created, `Ok(false)` when
/// it already existed (idempotent, like S3 CreateBucket us-east-1).
pub fn create_bucket(store: &ObjectStore, bucket: &str) -> io::Result<bool> {
    let path = bucket_path(store, bucket).ok_or_else(|| invalid("invalid bucket name"))?;
    match fs::create_dir(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn bucket_exists(store: &ObjectStore, bucket: &str) -> bool {
    bucket_path(store, bucket).is_some_and(|p| p.is_dir())
}

/// Delete a bucket. Refuses with `InvalidInput` while any object
/// remains (the router 409s first; this is the second line of
/// defense), then sweeps the whole tree: leftover EMPTY directories
/// (nested-key deletes prune those, but a crash mid-PUT can leave
/// them), stray sidecars and staging files go with it -- an
/// object-free bucket is always deletable, never a 500 ENOTEMPTY.
pub fn delete_bucket(store: &ObjectStore, bucket: &str) -> io::Result<()> {
    let root = bucket_path(store, bucket).ok_or_else(|| invalid("invalid bucket name"))?;
    let mut keys = Vec::new();
    walk(&root, "", &mut keys)?;
    if !keys.is_empty() {
        return Err(invalid("bucket not empty"));
    }
    fs::remove_dir_all(&root)
}

/// All bucket names (first-level directories, `.`-prefixed skipped).
pub fn list_buckets(store: &ObjectStore) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(&store.root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Staging pair for one upload: `(tmp, final)`, parents created. The
/// tmp name lives in the SAME directory (renames must not cross
/// filesystems) and carries a global counter (no collisions).
pub fn stage_paths(store: &ObjectStore, bucket: &str, key: &str) -> io::Result<(PathBuf, PathBuf)> {
    let final_path =
        object_path(store, bucket, key).ok_or_else(|| invalid("invalid bucket or key"))?;
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = final_path.file_name().unwrap_or_default().to_string_lossy();
    Ok((final_path.with_file_name(format!("{name}.{n}.tmp")), final_path))
}

/// Sidecar path for an object path: `<obj>.s3meta.json`.
fn sidecar_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}.s3meta.json"))
}

/// Durable tmp+fsync+rename write of a small buffer (the
/// `rcache/store_snapshot.rs::write_durably` recipe).
fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!("{name}.{n}.tmp"));
    File::create(&tmp)?.write_all(bytes)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Publish a staged upload atomically: fsync the staged bytes (the
/// caller already wrote them), rename over the final path, fsync the
/// parent, and only THEN durably write the sidecar. The sidecar lands
/// after the object, so a reader that sees a sidecar never sees it
/// point at a missing file -- the crash-ordering argument of
/// `rcache/store_snapshot.rs::write_durably`, applied to (object
/// bytes, metadata) instead of (data file, meta record).
pub fn commit(dst: &Path, tmp: &Path, etag: &str, content_type: &str) -> io::Result<()> {
    File::open(tmp)?.sync_all()?;
    fs::rename(tmp, dst)?;
    if let Some(dir) = dst.parent() {
        File::open(dir)?.sync_all()?;
    }
    let meta = serde_json::json!({"etag": etag, "content_type": content_type});
    write_durably(&sidecar_path(dst), &serde_json::to_vec(&meta).unwrap_or_default())
}

/// Copy `src` into `(bucket, key)` streaming through a 64 KiB buffer
/// with a running md5; the etag is the quoted md5 hex.
pub fn put_file(store: &ObjectStore, bucket: &str, key: &str, src: &Path, content_type: &str)
-> io::Result<PutResult> {
    let mut reader = File::open(src)?;
    let (tmp, final_path) = stage_paths(store, bucket, key)?;
    let mut out = File::create(&tmp)?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        size += n as u64;
    }
    out.sync_all()?;
    drop(out);
    let etag = format!("\"{}\"", hex::encode(hasher.finalize()));
    commit(&final_path, &tmp, &etag, content_type)?;
    Ok(PutResult { etag, size })
}

/// Stat an object + its sidecar. A missing sidecar degrades to a
/// `"<size>-<mtime_secs>"` etag and `application/octet-stream`.
pub fn head(store: &ObjectStore, bucket: &str, key: &str) -> Option<ObjectMeta> {
    let path = object_path(store, bucket, key)?;
    let md = fs::metadata(&path).ok()?;
    if !md.is_file() {
        return None;
    }
    let size = md.len();
    let mtime_ms = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)?;
    let fallback = || (format!("\"{size}-{}\"", mtime_ms / 1000), "application/octet-stream".to_string());
    let (etag, content_type) = fs::read(sidecar_path(&path))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            Some((v["etag"].as_str()?.to_string(), v["content_type"].as_str()?.to_string()))
        })
        .unwrap_or_else(fallback);
    Some(ObjectMeta { key: key.to_string(), size, etag, last_modified_ms: mtime_ms, content_type })
}

/// Delete an object and its sidecar; `true` when the object existed.
/// Directories left empty by the delete are pruned back to (but never
/// including) the bucket root, so an emptied bucket stays deletable.
pub fn delete(store: &ObjectStore, bucket: &str, key: &str) -> bool {
    let Some(path) = object_path(store, bucket, key) else {
        return false;
    };
    let existed = fs::remove_file(&path).is_ok();
    let _ = fs::remove_file(sidecar_path(&path)); // best-effort
    if existed {
        prune_empty_dirs(&store.root.join(bucket), &path);
    }
    existed
}

/// Remove `file`'s now-empty ancestor directories up to `root`
/// (`remove_dir` only succeeds on an empty dir, so this can never
/// drop a directory that still holds an object).
fn prune_empty_dirs(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || fs::remove_dir(d).is_err() {
            return;
        }
        dir = d.parent();
    }
}

/// Recursively collect relative keys: skip `.`-prefixed entries and
/// `*.s3meta.json` sidecars, never follow symlinks, files only.
fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.ends_with(".s3meta.json") {
            continue;
        }
        let rel_name = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(&entry.path(), &rel_name, out)?;
        } else if ft.is_file() {
            out.push(rel_name);
        }
    }
    Ok(())
}

/// ListObjects core: byte-sorted keys, prefix filter, delimiter
/// folding into deduplicated common prefixes, at most `max_keys`
/// entries (objects + prefixes) per page; `max_keys == 0` -> empty,
/// non-truncated page (S3 semantics).
pub fn list(store: &ObjectStore, bucket: &str, prefix: &str, delimiter: &str, max_keys: usize)
-> io::Result<ListPage> {
    let root = bucket_path(store, bucket).ok_or_else(|| invalid("invalid bucket name"))?;
    let mut keys = Vec::new();
    walk(&root, "", &mut keys)?;
    keys.sort(); // byte order (Rust String Ord is bytewise)
    let mut page =
        ListPage { objects: Vec::new(), common_prefixes: Vec::new(), truncated: false, next_after: None };
    if max_keys == 0 {
        return Ok(page);
    }
    let mut count = 0usize;
    let mut last_prefix: Option<String> = None;
    for key in keys.iter().filter(|k| k.starts_with(prefix)) {
        if count >= max_keys {
            page.truncated = true; // at least one more entry exists
            break;
        }
        let tail = &key[prefix.len()..];
        if let Some(i) = (!delimiter.is_empty()).then(|| tail.find(delimiter)).flatten() {
            let cp = format!("{}{}{}", &key[..prefix.len()], &tail[..i], delimiter);
            if last_prefix.as_deref() != Some(cp.as_str()) {
                last_prefix = Some(cp.clone());
                page.common_prefixes.push(cp.clone());
                page.next_after = Some(cp);
                count += 1;
            }
            continue;
        }
        if let Some(meta) = head(store, bucket, key) {
            page.objects.push(meta);
            page.next_after = Some(key.clone());
            count += 1;
        }
    }
    Ok(page)
}

/// Percent-encode one component set: keep [A-Za-z0-9.-_~/] (the S3
/// encoding-type=url safe set), `%XX` (uppercase hex) for the rest.
pub fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ObjectStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).expect("open");
        (dir, s)
    }

    fn put(store: &ObjectStore, bucket: &str, key: &str, bytes: &[u8]) -> PutResult {
        let src = store.root.join("src.tmp");
        fs::write(&src, bytes).expect("src");
        let r = put_file(store, bucket, key, &src, "text/plain").expect("put");
        let _ = fs::remove_file(&src);
        r
    }

    #[test]
    fn bucket_and_key_validation() {
        assert!(valid_bucket_name("a") && valid_bucket_name("rdb.check-1"));
        for bad in ["", "-a", "a-", ".a", "a.", "A", "a_b", "a/b", &"x".repeat(64)] {
            assert!(!valid_bucket_name(bad), "{bad:?}");
        }
        assert!(valid_key("a/b/c.txt"));
        for bad in ["", "/a", "a/", "a//b", "a/./b", "a/../b", "a\0b", &"k".repeat(1025)] {
            assert!(!valid_key(bad), "{bad:?}");
        }
        assert!(object_path(&store().1, "b", "../x").is_none());
    }


    #[test]
    fn list_folds_delimiters_and_truncates() {
        let (_dir, s) = store();
        put(&s, "b", "a.txt", b"a");
        put(&s, "b", "dir/1.txt", b"1");
        put(&s, "b", "dir/2.txt", b"22");
        put(&s, "b", "dirx/3.txt", b"3");
        put(&s, "b", "z.txt", b"z");
        let page = list(&s, "b", "", "/", 100).expect("list");
        let keys: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["a.txt", "z.txt"]);
        assert_eq!(page.common_prefixes, ["dir/", "dirx/"]);
        assert!(!page.truncated);
        // next_after = the LAST entry; z.txt sorts after the prefixes.
        assert_eq!(page.next_after.as_deref(), Some("z.txt"));
        let p1 = list(&s, "b", "", "/", 1).expect("list");
        assert_eq!(p1.objects[0].key, "a.txt");
        assert!(p1.truncated);
        assert_eq!(p1.next_after.as_deref(), Some("a.txt"));
        let p2 = list(&s, "b", "", "/", 3).expect("list");
        assert!(p2.truncated);
        assert_eq!(p2.next_after.as_deref(), Some("dirx/"));
        let p3 = list(&s, "b", "dir/", "", 100).expect("list");
        let keys: Vec<&str> = p3.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["dir/1.txt", "dir/2.txt"]);
        assert!(p3.common_prefixes.is_empty());
        let p0 = list(&s, "b", "", "/", 0).expect("list");
        assert!(p0.objects.is_empty() && !p0.truncated);
    }

    #[test]
    fn put_is_atomic_and_leaves_sidecar() {
        let (dir, s) = store();
        let r = put(&s, "b", "nested/obj.bin", b"hello");
        assert_eq!(r.size, 5);
        let mut hasher = Md5::new();
        hasher.update(b"hello");
        assert_eq!(r.etag, format!("\"{}\"", hex::encode(hasher.finalize())));
        let obj = dir.path().join("b/nested/obj.bin");
        assert!(obj.is_file() && dir.path().join("b/nested/obj.bin.s3meta.json").is_file());
        let meta = head(&s, "b", "nested/obj.bin").expect("head");
        assert_eq!(meta.etag, r.etag);
        assert_eq!(meta.content_type, "text/plain");
        assert!(delete(&s, "b", "nested/obj.bin"));
        assert!(!obj.is_file() && !dir.path().join("b/nested/obj.bin.s3meta.json").exists());
        assert!(head(&s, "b", "nested/obj.bin").is_none());
        // object without any sidecar degrades to the size-mtime etag
        fs::write(dir.path().join("b/raw.bin"), b"xyz").expect("raw");
        let meta = head(&s, "b", "raw.bin").expect("fallback head");
        assert_eq!(meta.content_type, "application/octet-stream");
        assert!(meta.etag.starts_with("\"3-"));
    }

    #[test]
    fn emptied_bucket_deletes_despite_dir_leftovers() {
        let (dir, s) = store();
        create_bucket(&s, "b").expect("create");
        put(&s, "b", "x/y/z.bin", b"v");
        assert!(delete(&s, "b", "x/y/z.bin"));
        // empty parents were pruned: only the bucket dir remains
        assert!(!dir.path().join("b/x").exists());
        delete_bucket(&s, "b").expect("emptied bucket deletes");
        assert!(!dir.path().join("b").exists());
        // crash-style leftovers (empty dir tree + stray sidecar, no
        // objects) are swept away with the tree
        create_bucket(&s, "b").expect("recreate");
        fs::create_dir_all(dir.path().join("b/orphan/dir")).expect("orphan dirs");
        fs::write(dir.path().join("b/stray.s3meta.json"), "{}").expect("stray sidecar");
        delete_bucket(&s, "b").expect("leftovers swept");
        // a real object still refuses the delete
        create_bucket(&s, "b").expect("recreate");
        put(&s, "b", "keep.bin", b"v");
        assert_eq!(delete_bucket(&s, "b").unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
}
