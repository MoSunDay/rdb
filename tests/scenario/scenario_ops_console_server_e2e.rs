//! Multi-store scenario #94 -- the on-call console over the server
//! introspection surface: PING/PONG (and the payload echo), ECHO
//! round-trips, INFO slices by section (Server/Cluster/Keyspace with
//! live db0 counters; `INFO server` omits the others), COMMAND counts
//! the registry/GETKEYS resolves SET's key positions, the CONFIG stub
//! answers its fixed pair for GET and SET alike, SELECT obeys cluster
//! mode (db 0 only), and QUIT answers one +OK before the close. One
//! REAL node, RESP only.

use crate::common;
use crate::scenario_common;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use scenario_common::{front_world, int_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn ops_console_probes_the_server_surface() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("ops94", &FrontSpec::default()).await;
    assert!(r(&a, &[b"SET", b"probe:a", b"1"]).await.starts_with(b"+OK"));

    // a) Liveness: bare PONG plus the payload echo form.
    assert_eq!(rt(&a, &[b"PING"]).await, "+PONG");
    assert_eq!(
        rb(&a, &[b"PING", b"console-1"]).await,
        "console-1",
        "payload pong"
    );
    assert!(
        r(&a, &[b"PING", b"a", b"b"]).await.starts_with(b"-ERR"),
        "too many args"
    );

    // b) ECHO round-trips verbatim bytes.
    assert_eq!(rb(&a, &[b"ECHO", b"night-shift"]).await, "night-shift");
    assert_eq!(rb(&a, &[b"ECHO", b""]).await, "", "empty echo");

    // c) INFO: the full payload carries all three sections plus the
    //    live db0 counters; `INFO server` keeps Server but drops
    //    Cluster and Keyspace.
    let full = rf(&a, &[b"INFO"]).await;
    for needle in [
        "# Server\r\n",
        "redis_version:",
        "# Cluster\r\n",
        "cluster_enabled:1",
        "# Keyspace\r\n",
        "db0:keys=",
    ] {
        assert!(full.contains(needle), "INFO misses {needle:?}: {full:?}");
    }
    let srv = rf(&a, &[b"INFO", b"server"]).await;
    assert!(
        srv.contains("# Server") && !srv.contains("# Cluster") && !srv.contains("db0:"),
        "server-only slice: {srv:?}"
    );
    let ks = rf(&a, &[b"INFO", b"keyspace"]).await;
    assert!(
        ks.contains("db0:keys=") && !ks.contains("# Server"),
        "keyspace slice: {ks:?}"
    );
    assert!(
        !rf(&a, &[b"INFO", b"nonsense"]).await.contains("# "),
        "unknown slice is empty"
    );

    // d) COMMAND: the registry count, GETKEYS on SET and MSET, and
    //    INFO naming one entry (6-element RESP2 row).
    let count = int_of(&r(&a, &[b"COMMAND", b"COUNT"]).await);
    assert!(count >= 188, "registry drained? {count}");
    let one_key = rf(&a, &[b"COMMAND", b"GETKEYS", b"SET", b"k", b"v"]).await;
    assert!(
        one_key.trim_end().ends_with("k"),
        "getkeys set: {one_key:?}"
    );
    let mkeys = rf(
        &a,
        &[b"COMMAND", b"GETKEYS", b"MSET", b"k1", b"v1", b"k2", b"v2"],
    )
    .await;
    assert!(
        mkeys.contains("k1") && mkeys.contains("k2"),
        "mset keys: {mkeys:?}"
    );
    let info_get = rf(&a, &[b"COMMAND", b"INFO", b"get"]).await;
    assert!(
        info_get.contains("get") && info_get.contains("*6"),
        "info row: {info_get:?}"
    );

    // e) CONFIG: the stub answers its fixed pair for ANY args -- GET
    //    and SET alike (there is no writable knob to change).
    let pair = "*2\r\n$29\r\ncluster-require-full-coverage\r\n$2\r\nno";
    assert_eq!(
        rf(&a, &[b"CONFIG", b"GET", b"maxmemory"]).await.trim_end(),
        pair
    );
    assert_eq!(
        rf(&a, &[b"CONFIG", b"SET", b"maxmemory", b"100"])
            .await
            .trim_end(),
        pair,
        "set echoes the same pair"
    );

    // f) SELECT: cluster mode serves db 0 only.
    assert!(r(&a, &[b"SELECT", b"0"]).await.starts_with(b"+OK"));
    assert!(
        r(&a, &[b"SELECT", b"3"])
            .await
            .starts_with(b"-ERR SELECT is not allowed in cluster mode"),
        "cluster mode refusal"
    );
    assert!(
        r(&a, &[b"SELECT", b"abc"])
            .await
            .starts_with(b"-ERR value is not an integer"),
        "non-numeric"
    );

    // g) QUIT: exactly one +OK, then the server closes the socket.
    let mut sock = TcpStream::connect(&a).await.expect("connect for quit");
    let mut bye = common::lite::frame(&[b"AUTH", common::TOKEN.as_bytes()]);
    bye.extend_from_slice(&common::lite::frame(&[b"QUIT"]));
    sock.write_all(&bye).await.expect("write auth+quit");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        match tokio::time::timeout(Duration::from_secs(2), sock.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
            Err(_) => panic!("quit: reply never settled, got {buf:?}"),
        }
    }
    assert_eq!(buf, b"+OK\r\n+OK\r\n", "auth +OK then quit +OK");
}
