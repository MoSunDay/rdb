//! Bearer gate for the RocksMQ HTTP front: the same posture as
//! `es_token` / `s3_token` (see `es/http.rs::authorized` and
//! `s3/http.rs::authorized`) -- an empty configured token means no
//! auth at all (the default, zero behavior change), a non-empty one
//! turns EVERY route into a 401 until `Authorization: Bearer <token>`
//! matches verbatim. Pure and stateless: headers + configured token
//! in, allow/deny out; the route layer injects it at ONE point so new
//! routes are gated by default, with only the constant [`PUBLIC_PATHS`]
//! list exempt (initially empty; health-check paths may join later).
//!
//! Not timing-hardened (a simple compare, like es/s3; the RESP AUTH
//! path's ct_eq is the only constant-time gate in the tree).

use super::api::HttpReply;

/// Fixed 401 body: constant text, never echoing any part of the
/// request or the configured token.
pub const DENIED_BODY: &str = "unauthorized";

/// Paths exempt from the bearer gate (constant; deliberately NOT
/// configurable to keep the config surface small). Empty = every
/// route is gated when a token is configured, exactly the es/s3
/// posture.
const PUBLIC_PATHS: &[&str] = &[];

/// Path-level exemption lookup (constant list, so this is total and
/// side-effect free).
pub fn is_public(path: &str) -> bool {
    PUBLIC_PATHS.contains(&path)
}

/// Bearer check: both sides are lowercased (the scheme is
/// case-insensitive and a mixed-case configured token must still
/// match); the token itself must match verbatim after that fold.
pub fn authorized(headers: &[(String, String)], token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    let expected = format!("bearer {}", token.to_ascii_lowercase());
    headers
        .iter()
        .any(|(n, v)| n == "authorization" && v.trim().to_ascii_lowercase() == expected)
}

/// The canned 401 reply every gated route answers with.
pub fn deny() -> HttpReply {
    HttpReply::text(401, DENIED_BODY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heads(v: Option<&str>) -> Vec<(String, String)> {
        match v {
            Some(v) => vec![("authorization".to_string(), v.to_string())],
            None => Vec::new(),
        }
    }

    #[test]
    fn empty_token_means_open() {
        assert!(authorized(&heads(None), ""));
        assert!(authorized(&heads(Some("Bearer anything")), ""));
        assert!(authorized(&heads(Some("garbage")), ""));
    }

    #[test]
    fn non_empty_token_requires_exact_bearer() {
        assert!(authorized(&heads(Some("Bearer tok")), "tok"));
        assert!(authorized(&heads(Some("bearer tok")), "tok")); // scheme fold
        assert!(authorized(&heads(Some("BEARER TOK")), "tok")); // token fold
        assert!(authorized(&heads(Some("  Bearer tok  ")), "tok")); // trim
        assert!(!authorized(&heads(None), "tok"));
        assert!(!authorized(&heads(Some("Bearer nope")), "tok"));
        assert!(!authorized(&heads(Some("Basic tok")), "tok")); // wrong scheme
        assert!(!authorized(&heads(Some("Bearer tok extra")), "tok"));
        assert!(!authorized(&heads(Some("Bearer ")), "tok"));
    }

    #[test]
    fn exemption_list_is_empty_so_everything_is_gated() {
        assert!(!is_public("/produce"));
        assert!(!is_public("/healthz"));
    }

    #[test]
    fn deny_reply_is_401_with_the_constant_body() {
        let r = deny();
        assert_eq!(r.status, 401);
        assert_eq!(r.body, DENIED_BODY.as_bytes());
    }
}
