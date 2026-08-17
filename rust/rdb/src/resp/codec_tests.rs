//! Tests for the RESP2 codec, split out of `codec.rs` to keep files small.

use super::*;

fn complete(buf: &[u8]) -> (Vec<Vec<u8>>, usize) {
    match parse_command(buf) {
        ParseOutcome::Complete { args, consumed } => (args, consumed),
        _ => panic!("expected Complete"),
    }
}

fn err_msg(buf: &[u8]) -> String {
    match parse_command(buf) {
        ParseOutcome::ProtocolError { msg, .. } => msg,
        _ => panic!("expected ProtocolError"),
    }
}

fn s(v: &[Vec<u8>]) -> Vec<String> {
    v.iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

#[test]
fn roundtrip_get() {
    assert!(matches!(parse_command(b""), ParseOutcome::Incomplete));
    let buf = b"*3\r\n$3\r\nGET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
    let (args, consumed) = complete(buf);
    assert_eq!(s(&args), ["GET", "foo", "bar"]);
    assert_eq!(consumed, buf.len());
}

#[test]
fn zero_arg_and_empty_bulk() {
    // redcon parseInt("") == 0, so "*\r\n" behaves exactly like "*0\r\n".
    let (args, consumed) = complete(b"*\r\n");
    assert!(args.is_empty() && consumed == 3);
    let (args, consumed) = complete(b"*0\r\n");
    assert!(args.is_empty() && consumed == 4);
    let (args, _) = complete(b"*2\r\n$3\r\nSET\r\n$0\r\n\r\n");
    assert_eq!(s(&args), ["SET", ""]);
}

#[test]
fn pipelined_commands() {
    let buf = b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nQUIT\r\n";
    let (args, consumed) = complete(buf);
    assert_eq!(s(&args), ["PING"]);
    assert_eq!(consumed, 14);
    let (args2, consumed2) = complete(&buf[consumed..]);
    assert_eq!(s(&args2), ["QUIT"]);
    assert_eq!(consumed2, buf.len() - consumed);
}

#[test]
fn byte_by_byte_is_incomplete_until_full() {
    let buf = b"*3\r\n$3\r\nGET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
    for k in 0..buf.len() {
        assert!(
            matches!(parse_command(&buf[..k]), ParseOutcome::Incomplete),
            "prefix of {} bytes should be Incomplete",
            k
        );
    }
    assert!(matches!(parse_command(buf), ParseOutcome::Complete { .. }));
}

#[test]
fn multibulk_errors() {
    assert_eq!(err_msg(b"*3\n$3\r\nGET\r\n"), "invalid multibulk length");
    assert_eq!(err_msg(b"*x\r\n"), "invalid multibulk length");
    assert_eq!(err_msg(b"*-1\r\n"), "invalid multibulk length");
    assert_eq!(err_msg(b"*1\r\n*3\r\n"), "expected '$', got '*'");
    assert_eq!(err_msg(b"*1\r\n3\r\nGET\r\n"), "expected '$', got '3'");
}

#[test]
fn bulk_errors() {
    assert_eq!(err_msg(b"*1\r\n$3\nabc\r\n"), "invalid bulk length");
    assert_eq!(err_msg(b"*1\r\n$x\r\n"), "invalid bulk length");
    assert_eq!(err_msg(b"*1\r\n$-1\r\n"), "invalid bulk length");
    assert_eq!(err_msg(b"*1\r\n$3\r\nabcXY"), "invalid bulk length"); // data not \r\n-terminated
}

#[test]
fn dollar_first_byte_is_invalid_message() {
    assert_eq!(err_msg(b"$3\r\nfoo\r\n"), "invalid message");
}

#[test]
fn inline_basics() {
    let (args, consumed) = complete(b"PING\r\n");
    assert_eq!(s(&args), ["PING"]);
    assert_eq!(consumed, 6);
    let (args, consumed) = complete(b"PING\n"); // LF only
    assert_eq!(s(&args), ["PING"]);
    assert_eq!(consumed, 5);
    let (args, _) = complete(b"  get   key  \r\n"); // runs of spaces
    assert_eq!(s(&args), ["get", "key"]);
}

#[test]
fn inline_quotes_and_escapes() {
    let (args, _) = complete(b"set k \"hello world\"\r\n");
    assert_eq!(s(&args), ["set", "k", "hello world"]);
    let (args, _) = complete(b"set k 'a b'\r\n");
    assert_eq!(s(&args), ["set", "k", "a b"]);
    let (args, _) = complete(b"set k \"a\\\"b\"\r\n"); // escaped quote
    assert_eq!(s(&args), ["set", "k", "a\"b"]);
    // redcon translates \n \r \t inside quotes, drops the backslash.
    let (args, _) = complete(b"set k \"a\\nb\"\r\n");
    assert_eq!(args[2], b"a\nb");
    let (args, _) = complete(b"set k \"a\\\\b\"\r\n");
    assert_eq!(args[2], b"a\\b");
}

#[test]
fn inline_empty_line_completes_with_no_args() {
    let (args, consumed) = complete(b"\r\n");
    assert!(args.is_empty() && consumed == 2);
    let (args, _) = complete(b"   \r\n");
    assert!(args.is_empty());
}

#[test]
fn inline_unbalanced_quotes() {
    assert_eq!(err_msg(b"set k \"abc\r\n"), "unbalanced quotes in request");
    assert_eq!(
        err_msg(b"set k \"abc\"x\r\n"),
        "unbalanced quotes in request"
    );
    assert_eq!(err_msg(b"set k\"v\" \r\n"), "unbalanced quotes in request");
}

#[test]
fn writers_byte_exact() {
    let mut buf = Vec::new();
    append_string(&mut buf, "OK");
    assert_eq!(buf, b"+OK\r\n");
    buf.clear();
    append_error(&mut buf, "ERR x");
    assert_eq!(buf, b"-ERR x\r\n");
    buf.clear();
    append_null(&mut buf);
    assert_eq!(buf, b"$-1\r\n");
    buf.clear();
    append_bulk(&mut buf, b"abc");
    assert_eq!(buf, b"$3\r\nabc\r\n");
    buf.clear();
    append_bulk_string(&mut buf, "abc");
    assert_eq!(buf, b"$3\r\nabc\r\n");
    buf.clear();
    append_int(&mut buf, 1);
    assert_eq!(buf, b":1\r\n");
    buf.clear();
    append_int(&mut buf, -42);
    assert_eq!(buf, b":-42\r\n");
    buf.clear();
    append_array(&mut buf, 2);
    assert_eq!(buf, b"*2\r\n");
    buf.clear();
    append_raw(&mut buf, b"raw");
    assert_eq!(buf, b"raw");
}

#[test]
fn writers_append_to_existing() {
    let mut buf = b"prefix:".to_vec();
    append_array(&mut buf, 1);
    append_bulk_string(&mut buf, "x");
    assert_eq!(buf, b"prefix:*1\r\n$1\r\nx\r\n");
}
