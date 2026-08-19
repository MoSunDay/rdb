//! Prometheus metrics + a minimal /metrics HTTP endpoint.
//!
//! Go reference: internal/monitor (rdb_command_latency histogram with
//! LinearBuckets(5, 25, 8) and labels type/mode/ack; raft_stats gauge with
//! label status; served via promhttp on monitor_addr). The rdb_* series are
//! byte-compatible; the go_*/process_* collectors of the Go default registry
//! are intentionally not reproduced.

use std::sync::Arc;
use std::time::Duration;

use prometheus::{
    CounterVec, Encoder, Gauge, GaugeVec, HistogramOpts, HistogramVec, Opts, Registry, TextEncoder,
};

pub const STATES: [&str; 5] = ["Shutdown", "Follower", "Leader", "Candidate", "Unknown"];

pub struct Collector {
    pub latency: HistogramVec,
    pub raft_status: GaugeVec,
    /// Lite Mode: message ops (labels: op = add|read|ack).
    pub lite_messages: CounterVec,
    /// Lite Mode: stream counts (labels: kind = live|reaped).
    pub lite_streams: GaugeVec,
    /// Lite Mode: group offsets awaiting the 200ms flush.
    pub lite_offset_dirty: Gauge,
    registry: Registry,
}

pub fn new_collector() -> Collector {
    // Go: prometheus.LinearBuckets(5, 25, 8) -> 5,30,55,80,105,130,155,180.
    let buckets: Vec<f64> = (0..8).map(|i| 5.0 + 25.0 * i as f64).collect();
    let latency = HistogramVec::new(
        HistogramOpts::new("rdb_command_latency", "rdb command latency(millisecond)")
            .buckets(buckets),
        &["type", "mode", "ack"],
    )
    .expect("histogram");
    let raft_status =
        GaugeVec::new(Opts::new("raft_stats", "raft stats"), &["status"]).expect("gauge");
    let lite_messages = CounterVec::new(
        Opts::new("rdb_lite_messages", "lite mode message operations"),
        &["op"],
    )
    .expect("counter");
    let lite_streams = GaugeVec::new(
        Opts::new("rdb_lite_streams", "lite mode stream counts"),
        &["kind"],
    )
    .expect("gauge");
    let lite_offset_dirty =
        Gauge::new("rdb_lite_offset_dirty", "lite group offsets pending flush").expect("gauge");
    let registry = Registry::new();
    registry
        .register(Box::new(latency.clone()))
        .expect("reg latency");
    registry
        .register(Box::new(raft_status.clone()))
        .expect("reg raft_stats");
    registry
        .register(Box::new(lite_messages.clone()))
        .expect("reg lite_messages");
    registry
        .register(Box::new(lite_streams.clone()))
        .expect("reg lite_streams");
    registry
        .register(Box::new(lite_offset_dirty.clone()))
        .expect("reg lite_offset_dirty");
    // Zero-initialize the labeled lite gauges so every series is present in
    // /metrics output before the background loop first refreshes them.
    lite_streams.with_label_values(&["live"]).set(0.0);
    lite_streams.with_label_values(&["reaped"]).set(0.0);
    Collector {
        latency,
        raft_status,
        lite_messages,
        lite_streams,
        lite_offset_dirty,
        registry,
    }
}

/// Lite Mode: count message-level operations (op = add|read|ack).
pub fn observe_lite_message(c: &Collector, op: &str, n: u64) {
    c.lite_messages.with_label_values(&[op]).inc_by(n as f64);
}

/// Lite Mode: set live/reaped stream gauges.
pub fn set_lite_streams(c: &Collector, live: f64, reaped: f64) {
    c.lite_streams.with_label_values(&["live"]).set(live);
    c.lite_streams.with_label_values(&["reaped"]).set(reaped);
}

/// Lite Mode: gauge of group offsets awaiting the periodic flush.
pub fn set_lite_offset_dirty(c: &Collector, n: f64) {
    c.lite_offset_dirty.set(n);
}

/// Go label order is (mode, firstCmd, isMoved) mapping onto (type, mode, ack):
/// type = "normal"|"backup", mode = lowercase command, ack = was-MOVED.
pub fn observe_latency(c: &Collector, type_label: &str, cmd: &str, is_moved: bool, ms: f64) {
    c.latency
        .with_label_values(&[type_label, cmd, if is_moved { "true" } else { "false" }])
        .observe(ms);
}

/// Go zeroes all five state labels then sets the parsed one to 1.
pub fn refresh_state_gauge(c: &Collector, label: &str) {
    for s in STATES {
        c.raft_status.with_label_values(&[s]).set(0.0);
    }
    c.raft_status.with_label_values(&[label]).set(1.0);
}

pub fn encode(c: &Collector) -> Result<String, String> {
    let mut buf = Vec::new();
    TextEncoder::new()
        .encode(&c.registry.gather(), &mut buf)
        .map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

/// Tiny HTTP/1.1 endpoint: GET /metrics -> 200 + text body, anything else 404.
/// One request per connection (scrapers are fine with that).
pub async fn serve(addr: &str, collector: Arc<Collector>) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("listen {} failed: {}", addr, e))?;
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => {
                // Transient accept failures (EMFILE &c) must not spin the
                // loop hot: back off briefly before retrying.
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        let collector = collector.clone();
        tokio::spawn(async move {
            // A client that connects but never finishes its request head
            // must not pin the task: bound the head read and just close.
            let head =
                match tokio::time::timeout(Duration::from_secs(5), read_head(&mut sock)).await {
                    Ok(Some(head)) => head,
                    Ok(None) | Err(_) => return,
                };
            let first_line = head.split(|b| *b == b'\n').next().unwrap_or_default();
            let is_metrics = first_line
                .split(|b| *b == b' ')
                .nth(1)
                .map(|p| p == b"/metrics" || p.starts_with(b"/metrics?"))
                .unwrap_or(false);
            let body = if is_metrics {
                encode(&collector).unwrap_or_default()
            } else {
                String::new()
            };
            let status = if is_metrics {
                "200 OK"
            } else {
                "404 Not Found"
            };
            let resp = format!(
                "HTTP/1.1 {}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
    }
}

/// Accumulate the request head until `\r\n\r\n` (or the 8 KiB cap);
/// `Ok(None)` when the peer hangs up or the socket errors first.
async fn read_head(sock: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read until end of request head (or give up on oversized input).
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_gauge() {
        let c = new_collector();
        observe_latency(&c, "normal", "get", false, 3.0);
        observe_latency(&c, "backup", "set", true, 40.0);
        refresh_state_gauge(&c, "Leader");
        let text = encode(&c).unwrap();
        assert!(text.contains("rdb_command_latency_bucket"));
        assert!(text.contains(r#"type="normal""#));
        assert!(text.contains(r#"mode="get""#));
        assert!(text.contains(r#"ack="true""#));
        assert!(text.contains(r#"raft_stats{status="Leader"} 1"#));
        assert!(text.contains(r#"raft_stats{status="Follower"} 0"#));
    }

    #[test]
    fn bucket_boundaries() {
        let c = new_collector();
        observe_latency(&c, "normal", "get", false, 0.0);
        let text = encode(&c).unwrap();
        for le in ["5", "30", "55", "80", "105", "130", "155", "180"] {
            assert!(
                text.contains(&format!(r#"le="{le}""#)),
                "missing bucket {le}"
            );
        }
    }
}
