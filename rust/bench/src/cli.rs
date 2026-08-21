//! Command-line parsing (manual, no third-party argument parser).

/// Workload selector; `mixed` alternates set/get by op index parity, the
/// `x*` trio drives Lite streams (`xadd` produces entries, `xreadgroup`
/// delivers them, `xack` delivers + acks each one).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workload {
    Ping,
    Set,
    Get,
    Mixed,
    Xadd,
    XReadGroup,
    Xack,
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
        }
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
        "                      xack (default mixed); mixed alternates set/get",
        "                      by op index parity; the x* workloads drive Lite",
        "                      streams bench_<client>/c as producer (xadd) and",
        "                      consumers (xreadgroup deliver-only, xack pairs a",
        "                      deliver with an ack, counting 2 ops per pair)",
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
fn validate_addr(addr: &str) -> Result<(), String> {
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| format!("--addr must be host:port, got '{addr}'"))?;
    if host.is_empty() {
        return Err(format!("empty host in --addr '{addr}'"));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| format!("bad port in --addr '{addr}'"))?;
    if port == 0 {
        return Err(format!("port 0 not allowed in --addr '{addr}'"));
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
            "--workload" => {
                let raw = flag_value(args, &mut i, name, inline)?;
                workload = Some(Workload::parse(&raw).ok_or_else(|| {
                    format!("unknown workload '{raw}' (ping|set|get|mixed|xadd|xreadgroup|xack)")
                })?);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
        i += 1;
    }

    let addr = addr.ok_or("missing required --addr")?;
    validate_addr(&addr)?;
    let token = token.ok_or("missing required --token")?;
    if token.is_empty() {
        return Err("--token must not be empty".to_string());
    }
    Ok(Config {
        addr,
        token,
        clients: clients.unwrap_or(16),
        duration: duration.unwrap_or(10) as u64,
        pipeline: pipeline.unwrap_or(1),
        workload: workload.unwrap_or(Workload::Mixed),
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
