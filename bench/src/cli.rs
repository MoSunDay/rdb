//! Command-line parsing (manual, no third-party argument parser).

/// Workload selector; `mixed` alternates set/get by op index parity, the
/// `x*` trio drives Lite streams (`xadd` produces entries, `xreadgroup`
/// delivers them, `xack` delivers + acks each one), the `kafka-*`
/// pair drives the kafka front (`--host`): `kafka-prod` sends Produce v2
/// batches, `kafka-fetch` tails Fetch v4 (ops count records on both),
/// and the `mysql-*` family drives the SQL (MySQL wire) front on
/// `--addr`: point lookup, range scan, two-table join and the KNN-shape
/// top-N scan (ops count queries; tables are pre-filled via `--rows`).
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
    MysqlPoint,
    MysqlRange,
    MysqlJoin,
    MysqlKnnScan,
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
            "mysql-point" => Some(Workload::MysqlPoint),
            "mysql-range" => Some(Workload::MysqlRange),
            "mysql-join" => Some(Workload::MysqlJoin),
            "mysql-knn-scan" => Some(Workload::MysqlKnnScan),
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
            Workload::MysqlPoint => "mysql-point",
            Workload::MysqlRange => "mysql-range",
            Workload::MysqlJoin => "mysql-join",
            Workload::MysqlKnnScan => "mysql-knn-scan",
        }
    }

    /// Whether the workload runs against the kafka front (`--host`).
    pub fn is_kafka(self) -> bool {
        matches!(self, Workload::KafkaProd | Workload::KafkaFetch)
    }

    /// Whether the workload runs against the SQL (MySQL wire) front on
    /// `--addr` (auth via `--mysql-user`/`--mysql-pass`, no RESP token).
    pub fn is_mysql(self) -> bool {
        matches!(
            self,
            Workload::MysqlPoint
                | Workload::MysqlRange
                | Workload::MysqlJoin
                | Workload::MysqlKnnScan
        )
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
    /// Rows pre-filled into each mysql workload table (mysql-*).
    pub rows: usize,
    /// Upper bound of the random pk domain, inclusive (mysql-*).
    pub key_range: usize,
    /// Span of the BETWEEN predicates (mysql-range / mysql-join).
    pub span: usize,
    /// MySQL login user (mysql-*).
    pub mysql_user: String,
    /// MySQL login password (mysql-*; empty = no password).
    pub mysql_pass: String,
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
        "                      xack | kafka-prod | kafka-fetch | mysql-point |",
        "                      mysql-range | mysql-join | mysql-knn-scan",
        "                      (default mixed); mixed alternates set/get by op",
        "                      index parity; the x* workloads drive Lite",
        "                      streams bench_<client>/c as producer (xadd) and",
        "                      consumers (xreadgroup deliver-only, xack pairs",
        "                      a deliver with an ack, counting 2 ops per",
        "                      pair); the kafka-* workloads drive the kafka",
        "                      front on --host (kafka-prod sends Produce v2",
        "                      batches, kafka-fetch tails Fetch v4; ops count",
        "                      records on both); the mysql-* workloads drive",
        "                      the SQL (MySQL wire) front on --addr (point",
        "                      lookup, BETWEEN range, two-table join, KNN-",
        "                      shape top-N scan; ops count queries; tables",
        "                      are created and pre-filled before the run)",
        "  --host <host:port>   kafka front address (required for kafka-*)",
        "  --topic <name>       kafka topic, pre-created as <topic>/q0 via a",
        "                      RESP XADD seed before kafka-* runs (default",
        "                      bench1)",
        "  --batch <n>          records per Produce request (kafka-prod only,",
        "                      default 100)",
        "  --rows <n>           rows pre-filled per mysql-* table (default",
        "                      10000)",
        "  --key-range <n>      random pk domain 1..=n (mysql-* only, default",
        "                      = --rows)",
        "  --span <n>           BETWEEN span in pks (mysql-range/mysql-join,",
        "                      default 100)",
        "  --mysql-user <s>     mysql login user (mysql-* only, default root)",
        "  --mysql-pass <s>     mysql login password (mysql-* only, default",
        "                      empty); --token is not required for mysql-*",
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
    let mut rows: Option<usize> = None;
    let mut key_range: Option<usize> = None;
    let mut span: Option<usize> = None;
    let mut mysql_user: Option<String> = None;
    let mut mysql_pass: Option<String> = None;

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
            "--rows" => rows = Some(parse_count(&flag_value(args, &mut i, name, inline)?, name)?),
            "--key-range" => {
                key_range = Some(parse_count(&flag_value(args, &mut i, name, inline)?, name)?)
            }
            "--span" => span = Some(parse_count(&flag_value(args, &mut i, name, inline)?, name)?),
            "--mysql-user" => mysql_user = Some(flag_value(args, &mut i, name, inline)?),
            "--mysql-pass" => mysql_pass = Some(flag_value(args, &mut i, name, inline)?),
            "--workload" => {
                let raw = flag_value(args, &mut i, name, inline)?;
                workload = Some(Workload::parse(&raw).ok_or_else(|| {
                    format!(
                        "unknown workload '{raw}' (ping|set|get|mixed|xadd|xreadgroup|xack|kafka-prod|kafka-fetch|mysql-point|mysql-range|mysql-join|mysql-knn-scan)"
                    )
                })?);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
        i += 1;
    }

    let addr = addr.ok_or("missing required --addr")?;
    validate_hostport(&addr, "--addr")?;
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
    // mysql-* talks to the MySQL wire front on --addr with its own
    // user/password login, so the RESP AUTH token is not wanted there.
    let token = if workload.is_mysql() {
        String::new()
    } else {
        let token = token.ok_or("missing required --token")?;
        if token.is_empty() {
            return Err("--token must not be empty".to_string());
        }
        token
    };
    // Mysql dataset flags exist only for the mysql-* workloads.
    let mysql_flags_used =
        rows.is_some() || key_range.is_some() || span.is_some() || mysql_user.is_some();
    let (rows, key_range, span, mysql_user, mysql_pass) = if workload.is_mysql() {
        let rows = rows.unwrap_or(10_000);
        let key_range = key_range.unwrap_or(rows);
        let span = span.unwrap_or(100);
        if matches!(workload, Workload::MysqlRange | Workload::MysqlJoin) && span >= key_range {
            return Err("--span must be smaller than --key-range".to_string());
        }
        let mysql_user = mysql_user.unwrap_or_else(|| "root".to_string());
        if mysql_user.is_empty() {
            return Err("--mysql-user must not be empty".to_string());
        }
        (
            rows,
            key_range,
            span,
            mysql_user,
            mysql_pass.unwrap_or_default(),
        )
    } else {
        if mysql_flags_used || mysql_pass.is_some() {
            return Err(
                "--rows/--key-range/--span/--mysql-user/--mysql-pass only apply to mysql-* workloads"
                    .to_string(),
            );
        }
        (0, 0, 0, String::new(), String::new())
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
        rows,
        key_range,
        span,
        mysql_user,
        mysql_pass,
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
    fn parses_mysql_workloads_and_flags() {
        for raw in ["mysql-point", "mysql-range", "mysql-join", "mysql-knn-scan"] {
            let cfg = parse_args(&argv(&[
                "--addr",
                "m:3306",
                "--workload",
                raw,
                "--mysql-pass",
                "p",
            ]))
            .expect("parse");
            assert_eq!(cfg.workload.as_str(), raw);
            assert!(cfg.workload.is_mysql());
            // No RESP token needed or wanted on the mysql front.
            assert!(cfg.token.is_empty());
            assert_eq!((cfg.rows, cfg.key_range, cfg.span), (10_000, 10_000, 100));
            assert_eq!(
                (cfg.mysql_user.as_str(), cfg.mysql_pass.as_str()),
                ("root", "p")
            );
        }
        // --key-range overrides the rows default; span must stay inside it.
        let cfg = parse_args(&argv(&[
            "--addr",
            "m:3306",
            "--workload",
            "mysql-range",
            "--rows",
            "5000",
            "--key-range",
            "4000",
            "--span",
            "10",
        ]))
        .expect("parse");
        assert_eq!((cfg.rows, cfg.key_range, cfg.span), (5_000, 4_000, 10));
    }

    #[test]
    fn mysql_flags_are_gated() {
        for bad in [
            // mysql flags on a RESP workload (no --token, so token errors
            // would mask the gating; token is provided).
            argv(&["--addr", "h:1", "--token", "t", "--rows", "10"]),
            argv(&["--addr", "h:1", "--token", "t", "--span", "5"]),
            argv(&["--addr", "h:1", "--token", "t", "--mysql-user", "u"]),
            // span swallowing the whole key range
            argv(&[
                "--addr",
                "m:1",
                "--workload",
                "mysql-range",
                "--key-range",
                "100",
                "--span",
                "100",
            ]),
            // kafka flags on a mysql workload
            argv(&[
                "--addr",
                "m:1",
                "--workload",
                "mysql-point",
                "--host",
                "k:2",
            ]),
        ] {
            assert!(parse_args(&bad).is_err(), "expected failure: {bad:?}");
        }
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
