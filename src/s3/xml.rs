//! S3 XML response rendering: hand-built strings (no xml crate, the
//! responses are flat and small). All builders are pure free
//! functions returning the full document including the XML
//! declaration; the AWS S3 namespace is
//! `http://s3.amazonaws.com/doc/2006-03-01/`.

use super::object::{percent_encode_component, ObjectMeta};

const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

/// XML-escape a text node: the five predefined entities plus C0
/// control bytes (0x00-0x1f, illegal raw in XML 1.0) as `&#xNN;`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 => out.push_str(&format!("&#x{:02X};", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// GET / (ListAllMyBuckets) response.
pub fn list_all_my_buckets(buckets: &[String]) -> String {
    let mut out = format!(
        "{XML_DECL}<ListAllMyBucketsResult xmlns=\"{NS}\">\
         <Owner><ID>rdb</ID><DisplayName>rdb</DisplayName></Owner><Buckets>"
    );
    for b in buckets {
        out.push_str(&format!(
            "<Bucket><Name>{}</Name><CreationDate>1970-01-01T00:00:00.000Z</CreationDate></Bucket>",
            escape(b)
        ));
    }
    out.push_str("</Buckets></ListAllMyBucketsResult>");
    out
}

/// One `<Contents>` block (StorageClass is always STANDARD -- no
/// storage tiers here). `enc` percent-encodes the key when
/// `encoding-type=url` was requested.
fn contents(meta: &ObjectMeta, enc: bool) -> String {
    let key = if enc {
        percent_encode_component(&meta.key)
    } else {
        escape(&meta.key)
    };
    format!(
        "<Contents><Key>{key}</Key>\
         <LastModified>{}</LastModified><ETag>{}</ETag><Size>{}</Size>\
         <StorageClass>STANDARD</StorageClass></Contents>",
        super::iso8601_millis(meta.last_modified_ms),
        escape(&meta.etag),
        meta.size
    )
}

/// Inputs of one `GET /{bucket}` list reply (data carrier, not a
/// positional 10-arg call).
pub struct ListArgs<'a> {
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub delimiter: &'a str,
    pub max_keys: usize,
    pub is_truncated: bool,
    pub objects: &'a [ObjectMeta],
    pub common_prefixes: &'a [String],
    pub next_token: Option<&'a str>,
    pub encoded: bool,
    pub v1: bool,
}

/// GET /<bucket> (ListObjectsV2 / v1 list) response.
pub fn list_bucket_result(a: &ListArgs) -> String {
    let ListArgs {
        bucket,
        prefix,
        delimiter,
        max_keys,
        is_truncated,
        objects,
        common_prefixes,
        next_token,
        encoded,
        v1,
    } = *a;
    let enc = |s: &str| {
        if encoded {
            percent_encode_component(s)
        } else {
            escape(s)
        }
    };
    let mut out = format!("{XML_DECL}<ListBucketResult xmlns=\"{NS}\">");
    out.push_str(&format!("<Name>{}</Name>", escape(bucket)));
    out.push_str(&format!("<Prefix>{}</Prefix>", enc(prefix)));
    if !delimiter.is_empty() {
        out.push_str(&format!("<Delimiter>{}</Delimiter>", enc(delimiter)));
    }
    if encoded {
        out.push_str("<EncodingType>url</EncodingType>");
    }
    out.push_str(&format!("<MaxKeys>{max_keys}</MaxKeys>"));
    // KeyCount = keys + folded prefixes returned on THIS page.
    out.push_str(&format!(
        "<KeyCount>{}</KeyCount>",
        objects.len() + common_prefixes.len()
    ));
    out.push_str(&format!(
        "<IsTruncated>{}</IsTruncated>",
        if is_truncated { "true" } else { "false" }
    ));
    if let (Some(tok), true) = (next_token, is_truncated) {
        let enc_tok = percent_encode_component(tok);
        if v1 {
            // v1 pages with marker/NextMarker instead of a token.
            out.push_str(&format!("<NextMarker>{enc_tok}</NextMarker>"));
        } else {
            out.push_str(&format!(
                "<NextContinuationToken>{enc_tok}</NextContinuationToken>"
            ));
        }
    }
    for meta in objects {
        out.push_str(&contents(meta, encoded));
    }
    for cp in common_prefixes {
        out.push_str(&format!(
            "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            enc(cp)
        ));
    }
    out.push_str("</ListBucketResult>");
    out
}

/// The S3 error document (Error/Code/Message/Resource/RequestId).
pub fn error(code: &str, message: &str, resource: &str) -> String {
    format!(
        "{XML_DECL}<Error><Code>{}</Code><Message>{}</Message>\
         <Resource>{}</Resource><RequestId>{}</RequestId></Error>",
        escape(code),
        escape(message),
        escape(resource),
        super::request_id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_entities_and_control_bytes() {
        assert_eq!(escape("a<b>&\"'z"), "a&lt;b&gt;&amp;&quot;&apos;z");
        assert_eq!(escape("x\u{1}\u{0}y"), "x&#x01;&#x00;y");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn buckets_document_shape() {
        let doc = list_all_my_buckets(&["a".to_string(), "b".to_string()]);
        assert!(doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(doc.contains(r#"xmlns="http://s3.amazonaws.com/doc/2006-03-01/""#));
        assert!(doc.contains("<Name>a</Name>") && doc.contains("<Name>b</Name>"));
    }

    #[test]
    fn list_result_folding_structure() {
        let objects = vec![ObjectMeta {
            key: "dir/1.txt".to_string(),
            size: 3,
            etag: "\"abc\"".to_string(),
            last_modified_ms: 1_789_450_496_789,
            content_type: "text/plain".to_string(),
        }];
        let doc = list_bucket_result(&ListArgs {
            bucket: "bkt",
            prefix: "dir/",
            delimiter: "/",
            max_keys: 100,
            is_truncated: true,
            objects: &objects,
            common_prefixes: &["dirx/".to_string()],
            next_token: Some("dirx/"),
            encoded: false,
            v1: false,
        });
        assert!(doc.contains("<Name>bkt</Name><Prefix>dir/</Prefix><Delimiter>/</Delimiter>"));
        assert!(doc.contains(
            "<MaxKeys>100</MaxKeys><KeyCount>2</KeyCount><IsTruncated>true</IsTruncated>"
        ));
        assert!(doc.contains("<NextContinuationToken>dirx/</NextContinuationToken>"));
        assert!(!doc.contains("<NextMarker>"));
        assert!(doc.contains("<Contents><Key>dir/1.txt</Key>"));
        assert!(doc.contains("<LastModified>2026-09-15T05:34:56.789Z</LastModified>"));
        assert!(doc.contains("<ETag>&quot;abc&quot;</ETag><Size>3</Size>"));
        assert!(doc.contains("<StorageClass>STANDARD</StorageClass>"));
        assert!(doc.contains("<CommonPrefixes><Prefix>dirx/</Prefix></CommonPrefixes>"));
        assert!(!doc.contains("<EncodingType>"));
        // encoding-type=url percent-encodes keys/prefixes
        let enc = list_bucket_result(&ListArgs {
            bucket: "bkt",
            prefix: "a b/",
            delimiter: "",
            max_keys: 10,
            is_truncated: false,
            objects: &[],
            common_prefixes: &[],
            next_token: None,
            encoded: true,
            v1: true,
        });
        assert!(enc.contains("<EncodingType>url</EncodingType>"));
        assert!(enc.contains("<Prefix>a%20b/</Prefix>"));
    }

    #[test]
    fn v1_pages_with_next_marker() {
        let doc = list_bucket_result(&ListArgs {
            bucket: "bkt",
            prefix: "",
            delimiter: "/",
            max_keys: 1,
            is_truncated: true,
            objects: &[],
            common_prefixes: &["d/".to_string()],
            next_token: Some("d/"),
            encoded: false,
            v1: true,
        });
        assert!(doc.contains("<KeyCount>1</KeyCount>"));
        assert!(doc.contains("<NextMarker>d/</NextMarker>"));
        assert!(!doc.contains("<NextContinuationToken>"));
    }

    #[test]
    fn error_document_shape() {
        let doc = error("NoSuchKey", "The specified key does not exist.", "/b/k");
        assert!(doc.contains("<Code>NoSuchKey</Code>"));
        assert!(doc.contains("<Message>The specified key does not exist.</Message>"));
        assert!(doc.contains("<Resource>/b/k</Resource>"));
        assert!(doc.contains("<RequestId>s3-"));
    }
}
