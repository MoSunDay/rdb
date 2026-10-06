//! SASL PLAIN over the Kafka wire (Batch 2), gated on `kafka_token`:
//! empty token = the SASL face does not exist (keys 17/36 are neither
//! advertised nor dispatched -- byte-identical wire behavior to the
//! pre-SASL broker; all existing e2e run without a token).
//!
//! Flow (per connection, once): SaslHandshake(17) picks the mechanism
//! (only PLAIN is enabled), SaslAuthenticate(36) carries the PLAIN
//! token string `authzid \0 authcid \0 passwd`. The password segment
//! must equal `kafka_token` (constant-time compare; authzid/authcid
//! are arbitrary and ignored). Success marks the connection
//! authenticated -- every other API then works without re-checks.
//! Failure answers SASL_AUTHENTICATION_FAILED(58) with a FIXED message
//! (never a token fragment, not even its length) and closes the
//! connection after the reply, as does an unsupported mechanism (33).
//!
//! Pre-auth whitelist: ApiVersions(18) (every client bootstraps with
//! it before authenticating) plus the SASL pair itself. Anything else
//! on an unauthenticated connection is dropped without a reply.

use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_bytes, put_i16, put_i32, put_i64, put_string, Reader,
};
use crate::kafka::{API_KEY_SASL_AUTHENTICATE, API_KEY_SASL_HANDSHAKE};

/// The one mechanism this front enables.
pub const MECHANISM_PLAIN: &str = "PLAIN";
/// Fixed failure text (no token fragments, no lengths).
pub const AUTH_FAILED_MESSAGE: &str = "authentication failed";

/// SASL face on/off: non-empty configured token enables it.
pub fn enabled(token: &str) -> bool {
    !token.is_empty()
}

/// Per-connection SASL state threaded through the frame loop: the
/// configured token, the one-shot authenticated flag, and the
/// close-after-reply signal a failed handshake/authenticate sets.
/// Plain data + free functions (the `ds::wait` mold).
pub struct ConnAuth {
    pub token: String,
    pub authed: bool,
    pub close_after: bool,
}

/// Fresh gate for one connection: no token = born authenticated (the
/// pre-SASL behavior, zero gating); a configured token keeps every
/// non-whitelisted api closed until SaslAuthenticate succeeds.
pub fn new_conn_auth(token: String) -> ConnAuth {
    ConnAuth {
        authed: !enabled(&token),
        token,
        close_after: false,
    }
}

/// May `api_key` run on the connection behind `auth` right now? The
/// pre-auth whitelist is ApiVersions (every client bootstraps with it)
/// plus the SASL pair itself.
pub fn allowed(auth: &ConnAuth, api_key: i16) -> bool {
    auth.authed
        || api_key == crate::kafka::API_KEY_API_VERSIONS
        || enabled(&auth.token)
            && (api_key == API_KEY_SASL_HANDSHAKE || api_key == API_KEY_SASL_AUTHENTICATE)
}

/// Version gate for the SASL pair (v0-v1 both; v2+ of either is
/// flexible/tagged, above the caps -- the keys are intentionally not
/// in `implemented_apis`, so this is the only range check).
pub fn api_supported(key: i16, version: i16) -> bool {
    (key == API_KEY_SASL_HANDSHAKE || key == API_KEY_SASL_AUTHENTICATE)
        && (0..=1).contains(&version)
}

/// Constant-time equality (no early exit, length folds into the same
/// accumulator): a wrong guess costs the same as a right-length one.
pub fn token_equals(got: &[u8], want: &[u8]) -> bool {
    let mut diff = got.len() ^ want.len();
    for i in 0..got.len().max(want.len()) {
        let a = got.get(i).copied().unwrap_or(0);
        let b = want.get(i).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

/// Split a PLAIN token string into (authzid, authcid, password).
/// Malformed input (not exactly three NUL-separated segments) is a
/// failed authenticate, never a fallthrough.
pub fn parse_plain(auth: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let first = auth.iter().position(|b| b == &0)?;
    let second = first + 1 + auth[first + 1..].iter().position(|b| b == &0)?;
    Some((
        &auth[..first],
        &auth[first + 1..second],
        &auth[second + 1..],
    ))
}

/// One SASL handler outcome: the reply body, whether the connection
/// closes after that reply is flushed, and (authenticate only) whether
/// the connection is now authenticated.
pub struct SaslOutcome {
    pub body: Vec<u8>,
    pub close: bool,
    pub authed: bool,
}

/// SaslHandshake v0-v1: request = mechanism; response = error +
/// [enabled mechanisms] (+ throttle on v1). A non-PLAIN mechanism
/// answers UNSUPPORTED_SASL_MECHANISM(33) listing PLAIN (the client's
/// retry surface) and closes after the reply.
pub fn handle_handshake(body: &mut Reader<'_>, version: i16) -> Result<SaslOutcome, String> {
    let mechanism = body
        .string()
        .ok_or_else(|| "malformed saslhandshake request".to_string())?;
    let ok = mechanism == MECHANISM_PLAIN;
    Ok(SaslOutcome {
        body: handshake_body(
            version,
            if ok {
                errors::NONE
            } else {
                errors::UNSUPPORTED_SASL_MECHANISM
            },
        ),
        close: !ok,
        authed: false,
    })
}

/// Encode a SaslHandshake response body (v0-v1).
pub fn handshake_body(version: i16, error: i16) -> Vec<u8> {
    let mut out = Vec::new();
    put_i16(&mut out, error);
    put_array_len(&mut out, 1);
    put_string(&mut out, MECHANISM_PLAIN);
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    out
}

/// SaslAuthenticate v0-v1: request = auth_bytes (the PLAIN token
/// string); response = error, error_message, auth_bytes(empty),
/// session_lifetime_ms(v1+, 0 = no re-auth). Only the password
/// segment is checked; username/authzid are free-form.
pub fn handle_authenticate(
    body: &mut Reader<'_>,
    version: i16,
    token: &str,
) -> Result<SaslOutcome, String> {
    let auth = body
        .bytes()
        .ok_or_else(|| "malformed saslauthenticate request".to_string())?
        .ok_or_else(|| "null saslauthenticate auth bytes".to_string())?;
    let ok = parse_plain(auth)
        .map(|(_, _, password)| token_equals(password, token.as_bytes()))
        .unwrap_or(false);
    Ok(SaslOutcome {
        body: authenticate_body(
            version,
            if ok {
                errors::NONE
            } else {
                errors::SASL_AUTHENTICATION_FAILED
            },
            if ok { None } else { Some(AUTH_FAILED_MESSAGE) },
        ),
        close: !ok,
        authed: ok,
    })
}

/// Encode a SaslAuthenticate response body (v0-v1).
pub fn authenticate_body(version: i16, error: i16, message: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    put_i16(&mut out, error);
    crate::kafka::frame::put_nullable_string(&mut out, message);
    put_bytes(&mut out, b"");
    if version >= 1 {
        put_i64(&mut out, 0); // session_lifetime_ms: no re-auth
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_token_split() {
        assert_eq!(
            parse_plain(b"authz\0user\0secret"),
            Some((&b"authz"[..], &b"user"[..], &b"secret"[..]))
        );
        // Empty segments are legal PLAIN (username ignored).
        assert_eq!(
            parse_plain(b"\0\0pw"),
            Some((&b""[..], &b""[..], &b"pw"[..]))
        );
        // Not exactly three segments.
        assert_eq!(parse_plain(b"user\0pw"), None);
        assert_eq!(parse_plain(b"pw"), None);
        assert_eq!(parse_plain(b""), None);
    }

    #[test]
    fn constant_time_equality() {
        assert!(token_equals(
            b"fake-kafka-token-000",
            b"fake-kafka-token-000"
        ));
        assert!(!token_equals(
            b"fake-kafka-token-000",
            b"fake-kafka-token-001"
        ));
        assert!(!token_equals(b"", b"x"));
        assert!(!token_equals(b"x", b""));
        assert!(token_equals(b"", b""));
        // Embedded NULs are bytes like any other.
        assert!(token_equals(b"a\0b", b"a\0b"));
        assert!(!token_equals(b"a\0b", b"a\0c"));
    }

    #[test]
    fn handshake_body_versions() {
        let v0 = handshake_body(0, errors::NONE);
        let mut r = Reader::new(&v0);
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string().as_deref(), Some("PLAIN"));
        assert_eq!(r.remaining(), 0);
        let v1 = handshake_body(1, errors::UNSUPPORTED_SASL_MECHANISM);
        let mut r = Reader::new(&v1);
        assert_eq!(r.i16(), Some(33));
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string().as_deref(), Some("PLAIN"));
        assert_eq!(r.i32(), Some(0), "throttle tail on v1");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn authenticate_body_versions() {
        let v0 = authenticate_body(0, errors::NONE, None);
        let mut r = Reader::new(&v0);
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.nullable_string(), Some(None));
        assert_eq!(r.bytes(), Some(Some(&b""[..])));
        assert_eq!(r.remaining(), 0);
        let v1 = authenticate_body(
            1,
            errors::SASL_AUTHENTICATION_FAILED,
            Some(AUTH_FAILED_MESSAGE),
        );
        let mut r = Reader::new(&v1);
        assert_eq!(r.i16(), Some(58));
        assert_eq!(
            r.nullable_string(),
            Some(Some(AUTH_FAILED_MESSAGE.to_string()))
        );
        assert_eq!(r.bytes(), Some(Some(&b""[..])));
        assert_eq!(r.i64(), Some(0), "session lifetime on v1");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn version_gate() {
        assert!(api_supported(API_KEY_SASL_HANDSHAKE, 0));
        assert!(api_supported(API_KEY_SASL_AUTHENTICATE, 1));
        assert!(!api_supported(API_KEY_SASL_AUTHENTICATE, 2), "v2 flexible");
        assert!(!api_supported(18, 0), "ApiVersions is not SASL");
    }
}
