//! MySQL-front load loops: the `mysql-point` / `mysql-range` /
//! `mysql-join` / `mysql-knn-scan` workloads (see
//! plans/2026-11-02-turso-ref/03-perf-study.md §5.2).
//!
//! Mirrors the kafka client: one client = one task = one TCP connection,
//! closed-loop (one COM_QUERY in flight), one latency sample per query
//! RTT. Table creation and pre-fill live in `mysql_setup`; the wire
//! codecs in `mysql_wire` / `mysql_reply`.

use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::cli::{Config, Workload};
use crate::client::{push_sample, ClientStats};
use crate::mysql_reply::{self, ReplyKind};
use crate::mysql_wire;

/// Unsampled queries per client before the measured window, so first-plan
/// compilation and cache warmup never distort the p50/p99.
const WARMUP_QUERIES: u64 = 50;

// ---- packet framing (async transport, sequence-tracked) ------------------

/// Read one logical packet: the 4-byte header (3-byte LE length +
/// sequence byte) then the payload, concatenating 16MB-max continuations.
async fn read_packet(sock: &mut TcpStream) -> Result<Vec<u8>, String> {
    let io = |e: std::io::Error| format!("read packet: {e}");
    let mut payload = Vec::new();
    loop {
        let mut header = [0u8; 4];
        sock.read_exact(&mut header).await.map_err(io)?;
        let len = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
        let mut chunk = vec![0u8; len];
        sock.read_exact(&mut chunk).await.map_err(io)?;
        let more = len == mysql_wire::MAX_PAYLOAD;
        payload.append(&mut chunk);
        if !more {
            return Ok(payload);
        }
    }
}

/// Write `payload` as one framed packet at sequence `seq` (split at the
/// 16MB boundary the same way the server's reader reassembles it).
async fn write_packet(sock: &mut TcpStream, seq: u8, payload: &[u8]) -> Result<(), String> {
    let io = |e: std::io::Error| format!("write packet: {e}");
    let mut seq = seq;
    let mut start = 0;
    loop {
        let end = (start + mysql_wire::MAX_PAYLOAD).min(payload.len());
        let chunk = &payload[start..end];
        sock.write_all(&mysql_wire::frame_header(chunk.len(), seq))
            .await
            .map_err(io)?;
        sock.write_all(chunk).await.map_err(io)?;
        seq = seq.wrapping_add(1);
        start = end;
        if chunk.len() < mysql_wire::MAX_PAYLOAD {
            break;
        }
    }
    sock.flush().await.map_err(io)?;
    Ok(())
}

// ---- session -------------------------------------------------------------

/// Open a connection: TCP, HandshakeV10 exchange, mysql_native_password
/// auth. Fails fast on auth errors (no retry -- a wrong password never
/// heals); connection-refused is retried by the caller (`mysql_setup`).
pub async fn connect(addr: &str, user: &str, pass: &str) -> Result<TcpStream, String> {
    let mut sock = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("io: connect {addr}: {e}"))?;
    let greeting = read_packet(&mut sock).await?;
    let hello = mysql_reply::parse_greeting(&greeting)?;
    if hello.plugin != mysql_wire::AUTH_PLUGIN {
        return Err(format!(
            "server {} wants auth plugin {:?}, this client only speaks {}",
            hello.version,
            hello.plugin,
            mysql_wire::AUTH_PLUGIN
        ));
    }
    if hello.salt.len() != 20 {
        return Err(format!(
            "server {} sent a {}-byte scramble, expected 20",
            hello.version,
            hello.salt.len()
        ));
    }
    write_packet(
        &mut sock,
        1,
        &mysql_wire::handshake_response(user, &hello.salt, pass),
    )
    .await?;
    let reply = read_packet(&mut sock).await?;
    match mysql_reply::reply_kind(&reply) {
        ReplyKind::Ok => Ok(sock),
        ReplyKind::Err => Err(mysql_reply::parse_err(&reply)),
        kind => Err(format!("unexpected post-auth packet: {kind:?}")),
    }
}

/// One COM_QUERY round trip over the text protocol. Returns the row
/// count and the summed row-payload bytes of the result set (0/0 for
/// statements that answer OK without rows); the server's ERR text is
/// the error. Rows are counted, not decoded: the load loops only need
/// volume, and one packet = one row in this protocol.
pub async fn query(sock: &mut TcpStream, sql: &str) -> Result<(u64, u64), String> {
    write_packet(sock, 0, &mysql_wire::query_packet(sql)).await?;
    let first = read_packet(sock).await?;
    let columns = match mysql_reply::reply_kind(&first) {
        ReplyKind::Ok => return Ok((0, 0)),
        ReplyKind::Err => return Err(mysql_reply::parse_err(&first)),
        _ => mysql_reply::column_count(&first)?,
    };
    for _ in 0..columns {
        read_packet(sock).await?; // column definition (contents unused)
    }
    expect_eof(sock).await?;
    let mut rows = 0u64;
    let mut bytes = 0u64;
    loop {
        let packet = read_packet(sock).await?;
        if mysql_reply::reply_kind(&packet) == ReplyKind::Eof {
            return Ok((rows, bytes));
        }
        rows += 1;
        bytes += packet.len() as u64;
    }
}

/// First row's first column of a result set, as text (`SELECT COUNT(*)`
/// style verification queries); Err when the set is empty.
pub async fn query_first_field(sock: &mut TcpStream, sql: &str) -> Result<String, String> {
    write_packet(sock, 0, &mysql_wire::query_packet(sql)).await?;
    let first = read_packet(sock).await?;
    match mysql_reply::reply_kind(&first) {
        ReplyKind::Err => Err(mysql_reply::parse_err(&first)),
        _ => {
            let columns = mysql_reply::column_count(&first)?;
            for _ in 0..columns {
                read_packet(sock).await?;
            }
            expect_eof(sock).await?;
            let row = read_packet(sock).await?;
            let (field, _) = mysql_reply::lenenc_str(&row, 0)
                .ok_or_else(|| format!("no field in first row of {sql:?}"))?;
            // Drain the remainder of the set: the terminating EOF (and
            // any further rows) must not leak into the next round trip.
            loop {
                let packet = read_packet(sock).await?;
                if mysql_reply::reply_kind(&packet) == ReplyKind::Eof {
                    break;
                }
            }
            Ok(String::from_utf8_lossy(field).into_owned())
        }
    }
}

/// Consume the EOF packet that closes a column-definition block.
async fn expect_eof(sock: &mut TcpStream) -> Result<(), String> {
    let packet = read_packet(sock).await?;
    match mysql_reply::reply_kind(&packet) {
        ReplyKind::Eof | ReplyKind::Ok => Ok(()),
        kind => Err(format!(
            "expected EOF after column definitions, got {kind:?}"
        )),
    }
}

// ---- workload query shapes -----------------------------------------------

/// Deterministic per-(client, op) 64-bit mix (splitmix64 finalizer): the
/// pk stream is uniform and reproducible across reruns.
fn op_rand(client_id: usize, op: u64) -> u64 {
    let mut z = (client_id as u64)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(op.wrapping_mul(0xbf58_476d_1ce4_e5b9))
        .wrapping_add(0x94d0_49bb_1331_11eb);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Uniform random pk in 1..=key_range.
fn rand_pk(cfg: &Config, client_id: usize, op: u64) -> u64 {
    op_rand(client_id, op) % cfg.key_range.max(1) as u64 + 1
}

/// Random range start so that [lo, lo+span) stays inside the key range.
fn rand_lo(cfg: &Config, client_id: usize, op: u64) -> u64 {
    let span = cfg.span.max(1) as u64;
    let width = cfg.key_range.max(1) as u64;
    op_rand(client_id, op) % width.saturating_sub(span).max(1) + 1
}

/// The query for one op of the configured mysql workload.
pub fn workload_sql(cfg: &Config, client_id: usize, op: u64) -> String {
    match cfg.workload {
        Workload::MysqlPoint => {
            format!(
                "SELECT * FROM bench_point WHERE pk = {}",
                rand_pk(cfg, client_id, op)
            )
        }
        Workload::MysqlRange => {
            let lo = rand_lo(cfg, client_id, op);
            let hi = lo + cfg.span.max(1) as u64 - 1;
            format!("SELECT * FROM bench_range WHERE pk BETWEEN {lo} AND {hi}")
        }
        Workload::MysqlJoin => {
            let lo = rand_lo(cfg, client_id, op);
            let hi = lo + cfg.span.max(1) as u64 - 1;
            format!(
                "SELECT a.pk, b.pk FROM bench_join_a a JOIN bench_join_b b \
                 ON a.k = b.k WHERE a.pk BETWEEN {lo} AND {hi}"
            )
        }
        // TODO: switch ABS(pk - ?) to DISTANCE(...) once the vector
        // distance function lands; ABS is the stand-in KNN shape today.
        Workload::MysqlKnnScan => {
            format!(
                "SELECT pk FROM bench_knn ORDER BY ABS(pk - {}) LIMIT 10",
                rand_pk(cfg, client_id, op)
            )
        }
        _ => unreachable!("non-mysql workload in mysql client"),
    }
}

// ---- load loop -----------------------------------------------------------

/// One client's session: connect, warm up unsampled, then loop COM_QUERY
/// until the shared deadline with one RTT sample per query.
pub async fn run_mysql_client(
    cfg: &Config,
    client_id: usize,
    deadline: Instant,
) -> Result<ClientStats, String> {
    let mut sock = connect(&cfg.addr, &cfg.mysql_user, &cfg.mysql_pass).await?;
    let mut stats = ClientStats {
        samples: Vec::new(),
        ops: 0,
        errors: 0,
        bytes: 0,
        first_error: None,
    };
    for op in 0..WARMUP_QUERIES {
        query(&mut sock, &workload_sql(cfg, client_id, op))
            .await
            .map_err(|e| format!("warmup query failed: {e}"))?;
    }
    let mut seq = 0u64;
    let mut op = WARMUP_QUERIES;
    while Instant::now() < deadline {
        let sql = workload_sql(cfg, client_id, op);
        let sent = Instant::now();
        match query(&mut sock, &sql).await {
            Ok((_, result_bytes)) => {
                stats.ops += 1;
                stats.bytes += result_bytes;
                push_sample(
                    &mut stats.samples,
                    &mut seq,
                    sent.elapsed().as_secs_f64() * 1000.0,
                );
            }
            Err(text) => {
                stats.errors += 1;
                if stats.first_error.is_none() {
                    stats.first_error = Some(text);
                }
            }
        }
        op += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(workload: Workload) -> Config {
        Config {
            addr: "h:1".to_string(),
            token: String::new(),
            clients: 1,
            duration: 1,
            pipeline: 1,
            workload,
            host: String::new(),
            topic: String::new(),
            batch: 0,
            rows: 10_000,
            key_range: 10_000,
            span: 100,
            mysql_user: "root".to_string(),
            mysql_pass: String::new(),
        }
    }

    #[test]
    fn sql_shapes_are_stable() {
        let c = cfg(Workload::MysqlPoint);
        assert_eq!(
            workload_sql(&c, 0, 0),
            "SELECT * FROM bench_point WHERE pk = 1538"
        );
        let c = cfg(Workload::MysqlRange);
        assert!(workload_sql(&c, 0, 1)
            .starts_with("SELECT * FROM bench_range WHERE pk BETWEEN 2110 AND 2209"));
        let c = cfg(Workload::MysqlJoin);
        assert_eq!(
            workload_sql(&c, 2, 0),
            "SELECT a.pk, b.pk FROM bench_join_a a JOIN bench_join_b b ON a.k = b.k \
             WHERE a.pk BETWEEN 9000 AND 9099"
        );
        let c = cfg(Workload::MysqlKnnScan);
        assert_eq!(
            workload_sql(&c, 0, 3),
            "SELECT pk FROM bench_knn ORDER BY ABS(pk - 7757) LIMIT 10"
        );
    }

    #[test]
    fn keys_stay_inside_the_range() {
        let c = cfg(Workload::MysqlPoint);
        for op in 0..500u64 {
            for client in 0..4usize {
                assert!(rand_pk(&c, client, op) >= 1 && rand_pk(&c, client, op) <= 10_000);
                let lo = rand_lo(&c, client, op);
                assert!(lo + 100 <= 10_001);
            }
        }
    }
}
