//! S3 REST routing: the method x (bucket, key) dispatch table over
//! `object::`. `http.rs` owns the wire (framing, auth, serialization);
//! this file decides WHAT to do: bucket CRUD + ListObjectsV2 (v1
//! `marker` rides the same path), object GET/HEAD/PUT/DELETE and
//! single-range GET. Unknown method on a known shape is 405.

use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::http::{
    error_reply, method_not_allowed, xml_reply, Body, Ctx, Req, Response, BODY_TIMEOUT, CHUNK,
};
use super::object::{self, ObjectMeta};
use super::{http_date, parse_range, xml, Range};

/// Entry point from `http::handle_conn`: `len` is the (already
/// capped) PUT body length, `leftover` any body bytes the head read
/// swallowed. An empty key (`/<bucket>/`) counts as bucket level.
pub(crate) async fn route(ctx: &Ctx, req: Req, sock: &mut TcpStream, leftover: &mut Vec<u8>, len: u64) -> Response {
    match req.key.as_deref() {
        None | Some("") => bucket_route(ctx, &req),
        Some(key) => object_route(ctx, &req, key, sock, leftover, len).await,
    }
}

// ---- bucket level --------------------------------------------------------

fn bucket_route(ctx: &Ctx, req: &Req) -> Response {
    let resource = format!("/{}", req.bucket);
    if req.bucket.is_empty() {
        // "/" without a bucket: only ListAllMyBuckets exists.
        if req.method == "GET" {
            return xml_reply(200, xml::list_all_my_buckets(&object::list_buckets(&ctx.store)));
        }
        return method_not_allowed(&resource);
    }
    match req.method.as_str() {
        "GET" => list_objects(ctx, req, &resource),
        "PUT" => match object::create_bucket(&ctx.store, &req.bucket) {
            Ok(true) => empty_reply(200),
            Ok(false) => error_reply(
                409,
                "BucketAlreadyOwnedByYou",
                "the bucket already exists in this object store",
                &resource,
            ),
            Err(e) => internal(&e, &resource),
        },
        "HEAD" => {
            if object::bucket_exists(&ctx.store, &req.bucket) {
                empty_reply(200)
            } else {
                error_reply(404, "NoSuchBucket", "the specified bucket does not exist", &resource)
            }
        }
        "DELETE" => delete_bucket(ctx, req, &resource),
        _ => method_not_allowed(&resource),
    }
}

/// DELETE /{bucket}: 404 unknown, 409 non-empty, 204 on success.
fn delete_bucket(ctx: &Ctx, req: &Req, resource: &str) -> Response {
    if !object::bucket_exists(&ctx.store, &req.bucket) {
        return error_reply(404, "NoSuchBucket", "the specified bucket does not exist", resource);
    }
    // Non-empty check first so `remove_dir`'s refusal is only the
    // second line of defense (see object::delete_bucket).
    let occupied = match object::list(&ctx.store, &req.bucket, "", "", 1) {
        Ok(p) => !p.objects.is_empty() || !p.common_prefixes.is_empty(),
        Err(e) => return internal(&e, resource),
    };
    if occupied {
        return error_reply(409, "BucketNotEmpty", "the bucket contains objects", resource);
    }
    match object::delete_bucket(&ctx.store, &req.bucket) {
        Ok(()) => empty_reply(204),
        Err(e) => internal(&e, resource),
    }
}

/// GET /{bucket}[?list-type=2]: ListObjectsV2 (and the v1 marker
/// flavor, which rides the identical path).
fn list_objects(ctx: &Ctx, req: &Req, resource: &str) -> Response {
    if !object::bucket_exists(&ctx.store, &req.bucket) {
        return error_reply(404, "NoSuchBucket", "the specified bucket does not exist", resource);
    }
    let prefix = req.q("prefix").unwrap_or("").to_string();
    let delimiter = req.q("delimiter").unwrap_or("").to_string();
    let max_keys = match max_keys_of(req) {
        Ok(n) => n,
        Err(rep) => return rep,
    };
    // continuation-token (v2) / start-after / marker (v1) all mean
    // "first entry strictly after this key".
    let after = req.q("continuation-token").or_else(|| req.q("start-after")).or_else(|| req.q("marker")).unwrap_or("").to_string();
    let encoded = req.q("encoding-type").is_some_and(|v| v == "url");
    // object::list has no after-token, so page here over the FULL
    // folded listing (fine at this store's scale; keys+prefixes are
    // already byte-sorted by the walker).
    let page = match object::list(&ctx.store, &req.bucket, &prefix, &delimiter, usize::MAX) {
        Ok(p) => p,
        Err(e) => return internal(&e, resource),
    };
    let mut entries = merge_entries(page.objects, page.common_prefixes);
    entries.retain(|(k, _)| k.as_str() > after.as_str());
    // S3: max-keys=0 answers an empty, NOT truncated page.
    let truncated = max_keys > 0 && entries.len() > max_keys;
    entries.truncate(max_keys);
    let next_after = entries.last().map(|(k, _)| k.clone());
    let mut objects = Vec::new();
    let mut prefixes = Vec::new();
    for (k, meta) in entries {
        match meta {
            Some(m) => objects.push(m),
            None => prefixes.push(k),
        }
    }
    xml_reply(
        200,
        xml::list_bucket_result(
            &req.bucket, &prefix, &delimiter, max_keys, truncated, &objects, &prefixes,
            next_after.as_deref(), encoded,
        ),
    )
}

/// `max-keys`: default 1000, clamped to 1000; negative/garbage ->
/// 400 InvalidArgument.
fn max_keys_of(req: &Req) -> Result<usize, Response> {
    let Some(raw) = req.q("max-keys") else { return Ok(1000) };
    match raw.parse::<i64>() {
        Ok(n) if n >= 0 => Ok(n.min(1000) as usize),
        _ => Err(error_reply(
            400,
            "InvalidArgument",
            "max-keys must be a non-negative integer",
            &format!("/{}", req.bucket),
        )),
    }
}

/// Merge the walker's two sorted streams into one key-ordered entry
/// list: a folded prefix `d/` occupies exactly the sort slot of its
/// first folded key (`d/1` etc. sort right after `d/`, before `d0`),
/// so plain byte comparison interleaves correctly.
fn merge_entries(objects: Vec<ObjectMeta>, common_prefixes: Vec<String>)
-> Vec<(String, Option<ObjectMeta>)> {
    let mut objs = objects.into_iter().map(|m| (m.key.clone(), Some(m))).collect::<Vec<_>>();
    let mut pfxs = common_prefixes.into_iter().map(|p| (p, None)).collect::<Vec<_>>();
    let mut merged = Vec::with_capacity(objs.len() + pfxs.len());
    while !objs.is_empty() || !pfxs.is_empty() {
        let from_obj = pfxs.is_empty() || (!objs.is_empty() && objs[0].0 <= pfxs[0].0);
        let source = if from_obj { &mut objs } else { &mut pfxs };
        merged.push(source.remove(0));
    }
    merged
}

// ---- object level --------------------------------------------------------

async fn object_route(ctx: &Ctx, req: &Req, key: &str, sock: &mut TcpStream, leftover: &mut Vec<u8>, len: u64) -> Response {
    let resource = format!("/{}/{}", req.bucket, key);
    if !object::valid_bucket_name(&req.bucket) || !object::valid_key(key) {
        return error_reply(400, "InvalidArgument", "invalid bucket name or object key", &resource);
    }
    match req.method.as_str() {
        "PUT" => put_object(ctx, req, key, sock, leftover, len, &resource).await,
        "GET" => get_object(ctx, req, key, &resource),
        "HEAD" => head_object(ctx, req, key, &resource),
        // S3 DELETE is idempotent: a missing object still answers 204.
        "DELETE" => {
            let _ = object::delete(&ctx.store, &req.bucket, key);
            empty_reply(204)
        }
        _ => method_not_allowed(&resource),
    }
}

/// PUT /{bucket}/{key}: stream the (length-capped) body into the
/// staged tmp file with a running md5, then `object::commit` renames
/// it into place with its sidecar. Replies 200 + ETag.
async fn put_object(ctx: &Ctx, req: &Req, key: &str, sock: &mut TcpStream, leftover: &mut Vec<u8>, len: u64, resource: &str) -> Response {
    let (tmp, final_path) = match object::stage_paths(&ctx.store, &req.bucket, key) {
        Ok(pair) => pair,
        Err(e) => return internal(&e, resource),
    };
    match read_body_to_file(sock, leftover, len, &tmp).await {
        Ok(etag) => {
            let content_type =
                req.header("content-type").unwrap_or("application/octet-stream").to_string();
            match object::commit(&final_path, &tmp, &etag, &content_type) {
                Ok(()) => Response {
                    status: 200,
                    content_type: "application/xml".into(),
                    headers: vec![("ETag".into(), etag)],
                    body: Body::Empty,
                },
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    internal(&e, resource)
                }
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // never leave staging behind
            error_reply(400, "IncompleteBody", &format!("request body read failed: {e}"), resource)
        }
    }
}

/// Drain `leftover`, then the socket, into `tmp` in CHUNK-sized reads
/// under BODY_TIMEOUT; returns the quoted-md5 etag.
async fn read_body_to_file(sock: &mut TcpStream, leftover: &mut Vec<u8>, len: u64, tmp: &Path)
-> std::io::Result<String> {
    let mut file = tokio::fs::File::create(tmp).await?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    if !leftover.is_empty() {
        let take = std::cmp::min(len as usize, leftover.len());
        hasher.update(&leftover[..take]);
        file.write_all(&leftover[..take]).await?;
        written = take as u64;
        leftover.drain(..take);
    }
    while written < len {
        let want = std::cmp::min(CHUNK as u64, len - written) as usize;
        let n = tokio::time::timeout(BODY_TIMEOUT, sock.read(&mut buf[..want]))
            .await
            .map_err(|_| eof("body timed out"))??;
        if n == 0 {
            return Err(eof("peer closed mid-body"));
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await?;
        written += n as u64;
    }
    file.sync_all().await?;
    Ok(format!("\"{}\"", hex::encode(hasher.finalize())))
}

/// GET /{bucket}/{key}: 200 full body, or 206 + Content-Range for a
/// satisfiable single `bytes=` range, or 416 when the start lies past
/// the end. Multi-range / non-bytes specs are ignored (serve 200).
fn get_object(ctx: &Ctx, req: &Req, key: &str, resource: &str) -> Response {
    let Some(meta) = object::head(&ctx.store, &req.bucket, key) else {
        return error_reply(404, "NoSuchKey", "the specified key does not exist", resource);
    };
    let path: PathBuf = object::object_path(&ctx.store, &req.bucket, key).expect("validated key");
    let mut headers = base_object_headers(&meta);
    if let Some(spec) = req.header("range") {
        match parse_range(spec, meta.size) {
            Range::Full => {}
            Range::Part(first, last) => {
                headers.push((
                    "Content-Range".to_string(),
                    format!("bytes {first}-{last}/{}", meta.size),
                ));
                return file_reply(206, meta.content_type, headers, path, first, last - first + 1);
            }
            Range::Unsatisfiable => {
                return error_reply(416, "InvalidRange", "the requested range is not satisfiable", resource)
            }
        }
    }
    file_reply(200, meta.content_type, headers, path, 0, meta.size)
}

/// HEAD /{bucket}/{key}: 200 with the GET headers and Content-Length
/// (the body itself is suppressed by the wire layer).
fn head_object(ctx: &Ctx, req: &Req, key: &str, resource: &str) -> Response {
    match object::head(&ctx.store, &req.bucket, key) {
        Some(meta) => {
            let path: PathBuf =
                object::object_path(&ctx.store, &req.bucket, key).expect("validated key");
            file_reply(200, meta.content_type.clone(), base_object_headers(&meta), path, 0, meta.size)
        }
        None => error_reply(404, "NoSuchKey", "the specified key does not exist", resource),
    }
}

fn base_object_headers(meta: &ObjectMeta) -> Vec<(String, String)> {
    vec![
        ("ETag".to_string(), meta.etag.clone()),
        ("Last-Modified".to_string(), http_date(meta.last_modified_ms / 1000)),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
    ]
}

// Range-header parsing (`parse_range`) lives in `super` next to the
// other wire utilities; the GET arm above just carries it out.

// ---- reply helpers -------------------------------------------------------

fn empty_reply(status: u16) -> Response {
    Response {
        status,
        content_type: "application/xml".to_string(),
        headers: Vec::new(),
        body: Body::Empty,
    }
}

fn file_reply(
    status: u16,
    content_type: String,
    headers: Vec<(String, String)>,
    path: PathBuf,
    offset: u64,
    len: u64,
) -> Response {
    Response { status, content_type, headers, body: Body::File(path, offset, len) }
}

fn internal(e: &std::io::Error, resource: &str) -> Response {
    error_reply(500, "InternalError", &e.to_string(), resource)
}

fn eof(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_entries_interleave_prefixes_and_objects() {
        let meta = |k: &str| ObjectMeta {
            key: k.to_string(),
            size: 1,
            etag: "\"e\"".to_string(),
            last_modified_ms: 0,
            content_type: "application/octet-stream".to_string(),
        };
        let merged =
            merge_entries(vec![meta("a.txt"), meta("e")], vec!["d/".to_string(), "dirx/".to_string()]);
        let keys: Vec<&str> = merged.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["a.txt", "d/", "dirx/", "e"]);
        assert!(merged[0].1.is_some() && merged[1].1.is_none() && merged[3].1.is_some());
    }
}
