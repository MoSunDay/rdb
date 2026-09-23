//! Command-line parsing (manual, no third-party argument parser).

/// Workload selector; `mixed` alternates set/get by op index parity, the
/// `x*` trio drives Lite streams (`xadd` produces entries, `xreadgroup`
/// delivers them, `xack` delivers + acks each one), and the `kafka-*`
/// pair drives the kafka front (`--host`): `kafka-prod` sends Produce v2
/// batches, `kafka-fetch` tails Fetch v4 (ops count records on both).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workload {
    Ping,
    Set,
    Get,
    Mixed,
    Xadd,
    XReadGroup,
    Xack,
    KafkaProd,
    KafkaFetch,
}

impl Workload {
    /// Lowercase names as accepted on the command line and printed in reports.
    pub fn parse(raw: &str) -> Option<Workload> {
        match raw {
            "ping" => Some(Workload::Ping),
            "set" => Some(Workload::Set),
            "get" => Some(Workload::Get),
            "mixed" => Some(Workload::Mixed),
            "xadd" => Some(Workload::Xadd),
            "xreadgroup" => Some(Workload::XReadGroup),
            "xack" => Some(Workload::Xack),
            "kafka-prod" => Some(Workload::KafkaProd),
            "kafka-fetch" => Some(Workload::KafkaFetch),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Workload::Ping => "ping",
            Workload::Set => "set",
            Workload::Get => "get",
            Workload::Mixed => "mixed",
            Workload::Xadd => "xadd",
            Workload::XReadGroup => "xreadgroup",
            Workload::Xack => "xack",
            Workload::KafkaProd => "kafka-prod",
            Workload::KafkaFetch => "kafka-fetch",
        }
    }

    /// Whether the workload runs against the kafka front (`--host`).
    pub fn is_kafka(self) -> bool {
        matches!(self, Workload::KafkaProd | Workload::KafkaFetch)
    }
}

/// Fully validated bench configuration (plain data, cloned into each task).
#[derive(Clone)]
pub struct Config {
    pub addr: String,
    pub token: String,
    pub clients: usize,
    pub duration: u64,
    pub pipeline: usize,
    pub workload: Workload,
    /// Kafka front address (kafka-* workloads only; empty otherwise).
    pub host: String,
    /// Kafka topic; `<topic>/q0` is pre-created via RESP XADD.
    pub topic: String,
    /// Records per Produce request (kafka-prod).
    pub batch: usize,
}

/// Help text; latency semantics (per batch, not per command) spelled out.
pub fn usage() -> String {
    [
        "usage: rdb-bench --addr <host:port> --token <string> [options]",
        "",
        "options:",
        "  --addr <host:port>   server RESP address (required)",
        "  --token <string>     auth token, sent as AUTH before the run (required)",
        "  --clients <n>        concurrent client connections (default 16)",
        "  --duration <secs>    run length in seconds (default 10)",
        "  --pipeline <n>       commands per round trip (default 1); latency is",
        "                      sampled once per batch RTT, so with pipeline > 1",
        "                      rtt_ms stats are per batch, not per command",
        "  --workload <w>       ping | set | get | mixed | xadd | xreadgroup |",
        "                      xack | kafka-prod | kafka-fetch (default mixed);",
        "                      mixed alternates set/get by op index parity; the",
        "                      x* workloads drive Lite streams bench_<client>/c",
        "                      as producer (xadd) and consumers (xreadgroup",
        "                      deliver-only, xack pairs a deliver with an ack,",
        "                      counting 2 ops per pair); the kafka-* workloads",
        "                      drive the kafka front on --host (kafka-prod sends",
        "                      Produce v2 batches, kafka-fetch tails Fetch v4;",
        "                      ops count records on both)",
        "  --host <host:port>   kafka front address (required for kafka-*)",
        "  --topic <name>       kafka topic, pre-created as <topic>/q0 via a",
        "                      RESP XADD seed before kafka-* runs (default",
        "                      bench1)",
        "  --batch <n>          records per Produce request (kafka-prod only,",
        "                      default 100)",
        "",
        "exit codes: 0 = ok, 1 = server error replies (e.g. -MOVED), 2 = bad usage",
    ]
    .join("\n")
}

/// Value of `--name`: either the inline `--name=x` part or the next argv
/// entry (consuming it); errors when the value is missing.
fn flag_value(
    args: &[String],
    i: &mut usize,
    name: &str,
    inline: Option<String>,
) -> Result<String, String> {
    if let Some(value) = inline {
        return Ok(value);
    }
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("missing value for {name}"))
}

/// Integer flag value, rejecting zero and non-numeric input.
fn parse_count(raw: &str, name: &str) -> Result<usize, String> {
    let n: usize = raw
        .parse()
        .map_err(|_| format!("bad value for {name}: '{raw}'"))?;
    if n == 0 {
        return Err(format!("{name} must be >= 1"));
    }
    Ok(n)
}

/// Light `host:port` shape check (connect failures surface later anyway).
fn validate_hostport(value: &str, flag: &str) -> Result<(), String> {
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| format!("{flag} must be host:port, got '{value}'"))?;
    if host.is_empty() {
        return Err(format!("empty host in {flag} '{value}'"));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| format!("bad port in {flag} '{value}'"))?;
    if port == 0 {
        return Err(format!("port 0 not allowed in {flag} '{value}'"));
    }
    Ok(())
}

/// Manual `--flag value` / `--flag=value` parsing; unknown flags, missing
/// values and out-of-range numbers become usage errors.
pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut addr: Option<String> = None;
    let mut token: Option<String> = None;
    let mut clients: Option<usize> = None;
    let mut duration: Option<usize> = None;
    let mut pipeline: Option<usize> = None;
    let mut workload: Option<Workload> = None;
    let mut host: Option<String> = None;
    let mut topic: Option<String> = None;
    let mut batch: Option<usize> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg, None),
        };
        match name {
            "--addr" => addr = Some(flag_value(args, &mut i, name, inline)?),
            "--token" => token = Some(flag_value(args, &mut i, name, inline)?),
            "--clients" => {
                clients = Some(parse_count(&flag_value(args, &mut i, name, inline)?, name)?)
            }
            "--duration" => {
                duration = Some(parse_count(&flag_value(args, &mut i, name, inline)?, name)?)
            }
            "--pipeline" => {
                pipeline = Some(parse_count(&flag_value(args, &mut i, name, inline)?, name)?)
            }
            "--host" => host = Some(flag_value(args, &mut i, name, inline)?),
            "--topic" => topic = Some(flag_value(args, &mut i, name, inline)?),
            "--batch" => batch = Some(parse_count(&flag_value(args, &mut i, name, inline)?, name)?),
            "--workload" => {
                let raw = flag_value(args, &mut i, name, inline)?;
                workload = Some(Workload::parse(&raw).ok_or_else(|| {
                    format!(
                        "unknown workload '{raw}' (ping|set|get|mixed|xadd|xreadgroup|xack|kafka-prod|kafka-fetch)"
                    )
                })?);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
        i += 1;
    }

    let addr = addr.ok_or("missing required --addr")?;
    validate_hostport(&addr, "--addr")?;
    let token = token.ok_or("missing required --token")?;
    if token.is_empty() {
        return Err("--token must not be empty".to_string());
    }
    let workload = workload.unwrap_or(Workload::Mixed);
    // Kafka flags exist only for the kafka-* workloads; the workload
    // itself exists only with --host (the kafka front address).
    let host = if workload.is_kafka() {
        let host = host.ok_or("kafka-* workloads need --host <kafka host:port>")?;
        validate_hostport(&host, "--host")?;
        host
    } else {
        if host.is_some() || topic.is_some() || batch.is_some() {
            return Err("--host/--topic/--batch only apply to kafka-* workloads".to_string());
        }
        String::new()
    };
    let topic = topic.unwrap_or_else(|| "bench1".to_string());
    if topic.is_empty() {
        return Err("--topic must not be empty".to_string());
    }
    Ok(Config {
        addr,
        token,
        clients: clients.unwrap_or(16),
        duration: duration.unwrap_or(10) as u64,
        pipeline: pipeline.unwrap_or(1),
        workload,
        host,
        topic,
        batch: batch.unwrap_or(100),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_flags_equals_form_and_defaults() {
        let cfg = parse_args(&argv(&[
            "--addr=127.0.0.1:6379",
            "--token",
            "sekret",
            "--workload",
            "ping",
        ]))
        .expect("parse");
        assert_eq!(cfg.addr, "127.0.0.1:6379");
        assert_eq!(cfg.token, "sekret");
        assert_eq!(cfg.workload, Workload::Ping);
        assert_eq!((cfg.clients, cfg.duration, cfg.pipeline), (16, 10, 1));
    }

    #[test]
    fn parses_lite_stream_workloads() {
        for raw in ["xadd", "xreadgroup", "xack"] {
            let cfg = parse_args(&argv(&["--addr", "h:1", "--token", "t", "--workload", raw]))
                .expect("parse");
            assert_eq!(cfg.workload.as_str(), raw);
        }
        // `expect_err` is not an option: Config is not Debug.
        let err = match parse_args(&argv(&[
            "--addr",
            "h:1",
            "--token",
            "t",
            "--workload",
            "xgroup",
        ])) {
            Err(err) => err,
            Ok(_) => panic!("expected unknown-workload error"),
        };
        assert!(err.contains("xadd|xreadgroup|xack"), "{err}");
    }

    #[test]
    fn parses_kafka_workloads_and_flags() {
        let cfg = parse_args(&argv(&[
            "--addr",
            "h:1",
            "--token",
            "t",
            "--host",
            "k:9092",
            "--workload",
            "kafka-prod",
            "--topic",
            "tp",
            "--batch",
            "7",
        ]))
        .expect("parse");
        assert_eq!(cfg.workload, Workload::KafkaProd);
        assert!(cfg.workload.is_kafka());
        assert_eq!(
            (cfg.host.as_str(), cfg.topic.as_str(), cfg.batch),
            ("k:9092", "tp", 7)
        );
        // Defaults: topic bench1, batch 100 records per request.
        let cfg = parse_args(&argv(&[
            "--addr=h:1",
            "--token=t",
            "--host=k:2",
            "--workload=kafka-fetch",
        ]))
        .expect("parse");
        assert_eq!(cfg.workload.as_str(), "kafka-fetch");
        assert_eq!((cfg.topic.as_str(), cfg.batch), ("bench1", 100));
        assert!(!Workload::Mixed.is_kafka());
    }

    #[test]
    fn kafka_flags_are_gated() {
        for bad in [
            // kafka workload without --host
            argv(&["--addr", "h:1", "--token", "t", "--workload", "kafka-prod"]),
            // kafka workload with a malformed --host
            argv(&[
                "--addr",
                "h:1",
                "--token",
                "t",
                "--host",
                "nohost",
                "--workload",
                "kafka-fetch",
            ]),
            // kafka flags on a RESP workload
            argv(&["--addr", "h:1", "--token", "t", "--host", "k:2"]),
            argv(&["--addr", "h:1", "--token", "t", "--topic", "t1"]),
            argv(&["--addr", "h:1", "--token", "t", "--batch", "10"]),
        ] {
            assert!(parse_args(&bad).is_err(), "expected failure: {bad:?}");
        }
    }

    #[test]
    fn rejects_bad_usage() {
        for bad in [
            vec![],
            argv(&["--token", "t"]),                     // missing --addr
            argv(&["--addr", "nohost", "--token", "t"]), // no port
            argv(&["--addr", "h:0", "--token", "t"]),    // port 0
            argv(&["--addr", "h:1"]),                    // missing --token
            argv(&["--addr", "h:1", "--token", "t", "--clients", "0"]),
            argv(&["--addr", "h:1", "--token", "t", "--workload", "txn"]),
            argv(&["--addr", "h:1", "--token", "t", "--pipeline"]), // missing value
        ] {
            assert!(parse_args(&bad).is_err(), "expected failure: {bad:?}");
        }
    }
}
